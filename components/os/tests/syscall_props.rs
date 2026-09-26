//! Properties of the syscall ABI (`docs/m3-design.md` §6.5, §6.6, §6.8, §17 M3.5) over
//! random programs and random syscall sequences, checked against an independent oracle
//! written from the design text: the ABI table, `write`'s checks in their order, the
//! bytes a process's pages hold, the FIFO queue, and the shutdown reason. The oracle
//! shares no code with the kernel; it knows each program's pages from the program it
//! built, and the user bytes from the pattern the test stored in them. Plus the kernel's
//! user-copy walk against the CPU's `sv32_translate` on random page-table entries.

mod common;

use std::collections::VecDeque;

use common::layout::*;
use common::procs::config;
use common::procs::*;
use common::{MockCtx, restore_into, snapshot_of};
use proptest::prelude::*;
use systemscope_contracts::event::Phase;
use systemscope_os::kernel::ISSUE;
use systemscope_os::process::ProcState;
use systemscope_os::{ModeledKernel, UserLayout, syscall};
use systemscope_rv32i::privilege::Privilege;
use systemscope_rv32i::sv32::{Access, sv32_translate};

const TEXT: u32 = 0x0001_0000;
const DATA: u32 = 0x0001_1000;
const F: u64 = TRAP_FRAME as u64;
const LAYOUT: UserLayout = UserLayout::M3;

/// A random program: 1–4 text words, 1–40 data bytes, and up to 9000 bytes of `.bss`, so
/// its data segment spans one to three pages.
#[derive(Clone, Debug)]
struct Prog {
    text: Vec<u32>,
    data: Vec<u8>,
    bss: u32,
}

impl Prog {
    fn file(&self) -> Vec<u8> {
        two_segment(&self.text, &self.data, self.bss)
    }

    /// Every page the program maps, all of them readable: the text page, the data pages,
    /// and the stack (§6.4, §11.2).
    fn pages(&self) -> Vec<u32> {
        let data = (self.data.len() as u32 + self.bss).div_ceil(4096);
        let stack = LAYOUT.stack_top - 4096 * LAYOUT.stack_pages;
        std::iter::once(TEXT)
            .chain((0..data).map(|i| DATA + 4096 * i))
            .chain((0..LAYOUT.stack_pages).map(|i| stack + 4096 * i))
            .collect()
    }
}

fn prog() -> impl Strategy<Value = Prog> {
    (
        proptest::collection::vec(any::<u32>(), 1..=4),
        proptest::collection::vec(any::<u8>(), 1..=40),
        0u32..9000,
    )
        .prop_map(|(text, data, bss)| Prog { text, data, bss })
}

/// The byte the test stores at `va` of `pid` after boot.
fn pattern(pid: u32, va: u32) -> u8 {
    (va.wrapping_mul(0x9E37_79B1) >> 13) as u8 ^ (pid as u8).wrapping_mul(31)
}

/// A syscall as the guest makes it: `a7` and `a0`–`a2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Call {
    nr: u32,
    args: [u32; 3],
}

fn buf() -> impl Strategy<Value = u32> {
    prop_oneof![
        4 => (0u32..3 * 4096).prop_map(|o| DATA + o),
        1 => (0u32..4096).prop_map(|o| TEXT + o),
        2 => (0u32..5 * 4096).prop_map(|o| LAYOUT.stack_top - 4 * 4096 - 4096 + o),
        1 => Just(0),
        1 => Just(0x8000_0000),
        1 => (0u32..64).prop_map(|o| 0xFFFF_FFC0 + o),
        1 => any::<u32>(),
    ]
}

fn count() -> impl Strategy<Value = u32> {
    prop_oneof![
        4 => 0u32..40,
        2 => 0u32..5000,
        1 => 4090u32..4100,
        1 => any::<u32>(),
    ]
}

fn call() -> impl Strategy<Value = Call> {
    let status = prop_oneof![3 => Just(0u32), 1 => any::<u32>()];
    let arg = any::<u32>();
    prop_oneof![
        6 => (prop_oneof![6 => Just(1u32), 3 => Just(2), 1 => any::<u32>()], buf(), count())
            .prop_map(|(fd, b, n)| Call { nr: 64, args: [fd, b, n] }),
        4 => (arg, arg).prop_map(|(a, b)| Call { nr: 124, args: [a, b, 0] }),
        3 => (arg, arg).prop_map(|(a, b)| Call { nr: 172, args: [a, b, 0] }),
        1 => status.clone().prop_map(|s| Call { nr: 93, args: [s, 0, 0] }),
        1 => status.prop_map(|s| Call { nr: 94, args: [s, 0, 0] }),
        1 => (prop_oneof![
            Just(0u32), Just(63), Just(65), Just(92), Just(95), Just(123), Just(125),
            Just(171), Just(173), Just(u32::MAX), any::<u32>(),
        ], arg).prop_map(|(nr, a)| Call { nr, args: [a, 0, 0] }),
    ]
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    proptest::collection::vec(call(), 0..16)
}

/// What the oracle expects of one call.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    /// The caller runs on with `a0 = value`, after `bytes` went to the UART.
    Return { value: u32, bytes: Vec<u8> },
    /// The caller went to the queue tail with `a0 = 0`.
    Yield,
    /// The caller exited.
    Exit(i32),
    /// The number is not a syscall the model knows (a subset of `Return`, for the
    /// determinism property).
    NoSys,
}

/// The oracle: the ABI table of §6.5 and the scheduler of §6.6.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Oracle {
    /// `(pid, state)`: 'R'eady, 'U' running, or `E` with the status.
    states: Vec<(u32, char, i32)>,
    queue: VecDeque<u32>,
    current: Option<u32>,
    /// The shutdown reason, once down.
    down: Option<u32>,
}

impl Oracle {
    fn boot(n: usize) -> Oracle {
        let mut o = Oracle {
            states: (1..=n as u32).map(|p| (p, 'R', 0)).collect(),
            queue: (1..=n as u32).collect(),
            current: None,
            down: None,
        };
        o.next();
        o
    }

    fn set(&mut self, pid: u32, s: char, status: i32) {
        self.states[pid as usize - 1] = (pid, s, status);
    }

    fn next(&mut self) {
        match self.queue.pop_front() {
            Some(p) => {
                self.set(p, 'U', 0);
                self.current = Some(p);
            }
            None => {
                self.current = None;
                let clean = self.states.iter().all(|s| s.1 == 'E' && s.2 == 0);
                self.down = Some(if clean { 0 } else { 1 });
            }
        }
    }

    /// `write(fd, buf, count)` by `pid`, whose pages are `pages`.
    fn write(pid: u32, pages: &[u32], [fd, buf, count]: [u32; 3]) -> Expect {
        let ret = |value| Expect::Return {
            value,
            bytes: Vec::new(),
        };
        if fd != 1 && fd != 2 {
            return ret(9u32.wrapping_neg());
        }
        let n = count.min(4096);
        if n == 0 {
            return ret(0);
        }
        let end = u64::from(buf) + u64::from(n);
        if end > 1 << 32 {
            return ret(14u32.wrapping_neg());
        }
        let mut page = u64::from(buf) & !0xFFF;
        while page < end {
            if !pages.contains(&(page as u32)) {
                return ret(14u32.wrapping_neg());
            }
            page += 4096;
        }
        Expect::Return {
            value: n,
            bytes: (buf..=(end - 1) as u32)
                .map(|va| pattern(pid, va))
                .collect(),
        }
    }

    fn call(&mut self, progs: &[Prog], c: Call) -> Expect {
        let pid = self.current.unwrap();
        match c.nr {
            64 => Oracle::write(pid, &progs[pid as usize - 1].pages(), c.args),
            124 => {
                self.set(pid, 'R', 0);
                self.queue.push_back(pid);
                self.next();
                Expect::Yield
            }
            172 => Expect::Return {
                value: pid,
                bytes: Vec::new(),
            },
            93 | 94 => {
                let status = c.args[0] as i32;
                self.set(pid, 'E', status);
                self.next();
                Expect::Exit(status)
            }
            _ => Expect::NoSys,
        }
    }
}

/// The kernel's scheduler state in the oracle's terms.
fn observed(h: &Harness) -> Oracle {
    let p = h.k.processes().unwrap();
    let states = p
        .pcbs()
        .iter()
        .map(|pcb| match pcb.state {
            ProcState::Ready => (pcb.pid, 'R', 0),
            ProcState::Running => (pcb.pid, 'U', 0),
            ProcState::Exited { status } => (pcb.pid, 'E', status),
            ProcState::Faulted { .. } => (pcb.pid, 'F', 0),
        })
        .collect();
    let down = (h.frame(0x90) == 1).then(|| h.frame(0x94));
    Oracle {
        states,
        queue: p.queue().clone(),
        current: p.current(),
        down,
    }
}

/// Stores [`pattern`] in every page of every process, through the test's own walk.
fn fill(h: &mut Harness, progs: &[Prog]) {
    for (i, p) in progs.iter().enumerate() {
        let pid = i as u32 + 1;
        let root = h.k.processes().unwrap().pcb(pid).unwrap().root;
        for page in p.pages() {
            let pa = walk(&h.mem, root, page, Kind::Load).unwrap();
            let bytes: Vec<u8> = (page..page + 4096).map(|va| pattern(pid, va)).collect();
            h.mem.write(pa, &bytes);
        }
    }
}

/// One call as the kernel ran it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ran {
    call: Call,
    pid: u32,
    expect: Expect,
    before: Vec<u8>,
    after: Vec<u8>,
    seen: Vec<Seen>,
    /// The caller's saved context after a yield.
    saved: Option<(Vec<u32>, u32)>,
}

impl Ran {
    fn uart(&self) -> Vec<u8> {
        self.seen
            .iter()
            .filter(|s| s.0 && s.1 == UART_BASE)
            .flat_map(|s| s.2.clone())
            .collect()
    }
}

/// A handler boundary of a run: the operation it is in (0 is boot) and the call.
type Boundary = (usize, Option<Call>);

/// A run of boot and `calls` (stopping at shutdown). At every handler boundary,
/// `restore(boundary)` may return snapshot bytes: the kernel is then replaced by a
/// fresh one restored from them, and the run continues with it.
struct Run {
    h: Harness,
    ran: Vec<Ran>,
    states: Vec<(Oracle, Oracle)>,
    boundaries: Vec<Boundary>,
    snaps: Vec<Vec<u8>>,
}

fn run(progs: &[Prog], calls: &[Call], mut cut: impl FnMut(usize) -> bool) -> Run {
    let files: Vec<Vec<u8>> = progs.iter().map(Prog::file).collect();
    let mut h = Harness::new(config(), &files, plan_of(&files));
    let mut o = Oracle::boot(progs.len());
    let mut boundaries = Vec::new();
    let mut snaps = Vec::new();
    let mut ran = Vec::new();
    let mut states = Vec::new();
    let mut step = |h: &mut Harness, which: Boundary| {
        let bytes = snapshot_of(&h.k);
        if cut(boundaries.len()) {
            let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
            restore_into(&mut k, &bytes).unwrap();
            if h.pending.is_some() {
                // In Wait: a wake to the restored kernel is a fault and sends nothing.
                let mut probe = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
                restore_into(&mut probe, &bytes).unwrap();
                let mut ctx = MockCtx::new();
                assert!(ctx.wake(&mut probe, ISSUE, Phase::Request).is_err());
                assert!(ctx.sent.is_empty());
            }
            h.k = k;
        }
        boundaries.push(which);
        snaps.push(bytes);
    };
    let mut drive = |h: &mut Harness, which: Boundary| {
        h.enter(TRAP_FRAME).unwrap();
        step(h, which);
        loop {
            h.issue().unwrap();
            step(h, which);
            let done = h.respond(false).unwrap();
            step(h, which);
            if done {
                break;
            }
        }
    };
    drive(&mut h, (0, None));
    fill(&mut h, progs);
    states.push((o.clone(), observed(&h)));
    for (i, &c) in calls.iter().enumerate() {
        if o.down.is_some() {
            break;
        }
        let pid = o.current.unwrap();
        let expect = o.call(progs, c);
        h.write_syscall(c.nr, c.args, TEXT + 4 * (i as u32 % 1000), 0, i as u32);
        let before = h.mem.read(F, 0x98);
        let at = h.seen.len();
        drive(&mut h, (i + 1, Some(c)));
        // A yield to another process leaves the caller's context in its PCB; a lone
        // process's yield dispatches it at once, into the frame.
        let saved = (expect == Expect::Yield).then(|| {
            match h.k.processes().unwrap().pcb(pid).unwrap().context {
                Some(ctx) => (ctx.regs.to_vec(), ctx.pc),
                None => {
                    let word = |o: usize| h.mem.word(F + o as u64);
                    ((0..31).map(|i| word(4 * i)).collect(), word(0x7C))
                }
            }
        });
        ran.push(Ran {
            call: c,
            pid,
            expect,
            before,
            after: h.mem.read(F, 0x98),
            seen: h.seen[at..].to_vec(),
            saved,
        });
        states.push((o.clone(), observed(&h)));
    }
    Run {
        h,
        ran,
        states,
        boundaries,
        snaps,
    }
}

fn progs(n: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<Prog>> {
    proptest::collection::vec(prog(), n)
}

/// A frame with `a0 = value` and `sepc + 4`, the rest of `before` unchanged.
fn returned(before: &[u8], value: u32) -> Vec<u8> {
    let mut want = before.to_vec();
    want[0x24..0x28].copy_from_slice(&value.to_le_bytes());
    let sepc = u32::from_le_bytes(before[0x7C..0x80].try_into().unwrap());
    want[0x7C..0x80].copy_from_slice(&sepc.wrapping_add(4).to_le_bytes());
    want
}

/// The same run restored at the `pick`th boundary among those `filter` accepts, which
/// must end exactly as the uncut run: the same accesses (so no UART byte, guest read, or
/// frame word twice), memory, traces, and final snapshot.
fn assert_cut_is_invisible(
    progs: &[Prog],
    calls: &[Call],
    pick: prop::sample::Index,
    filter: impl Fn(&Boundary) -> bool,
) -> Result<(), TestCaseError> {
    let clean = run(progs, calls, |_| false);
    let at: Vec<usize> = (0..clean.boundaries.len())
        .filter(|&i| filter(&clean.boundaries[i]))
        .collect();
    if at.is_empty() {
        return Ok(());
    }
    let cut = at[pick.index(at.len())];
    let restored = run(progs, calls, |i| i == cut);
    prop_assert_eq!(&restored.h.seen, &clean.h.seen);
    prop_assert_eq!(&restored.h.mem, &clean.h.mem);
    prop_assert_eq!(&restored.h.ctx.traced, &clean.h.ctx.traced);
    prop_assert_eq!(&restored.snaps, &clean.snaps);
    prop_assert_eq!(&restored.states, &clean.states);
    Ok(())
}

fn is_nr(b: &Boundary, nrs: &[u32]) -> bool {
    b.1.is_some_and(|c| nrs.contains(&c.nr))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// 1. Arbitrary bytes, and a snapshot taken inside a syscall with any bytes flipped,
    /// never panic a restore: it succeeds or reports an error.
    #[test]
    fn arbitrary_snapshot_bytes_never_panic(
        progs in progs(1..=2),
        calls in calls(),
        pick in any::<prop::sample::Index>(),
        flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
        noise in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let files: Vec<Vec<u8>> = progs.iter().map(Prog::file).collect();
        let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
        let _ = restore_into(&mut k, &noise);
        let r = run(&progs, &calls, |_| false);
        let mut bytes = r.snaps[pick.index(r.snaps.len())].clone();
        for (at, x) in flips {
            let i = at.index(bytes.len());
            bytes[i] ^= x | 1;
        }
        let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
        let _ = restore_into(&mut k, &bytes);
        bytes.truncate(pick.index(bytes.len()));
        let _ = restore_into(&mut k, &bytes);
    }

    /// 2. `getpid` returns the PID the oracle has running.
    #[test]
    fn getpid_is_the_running_pid(progs in progs(1..=4), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for c in r.ran.iter().filter(|c| c.call.nr == 172) {
            prop_assert_eq!(&c.after, &returned(&c.before, c.pid));
        }
    }

    /// 3. `sched_yield` moves the caller to the queue tail and runs the head: after every
    /// call the kernel's queue, running PID, and states are the oracle's.
    #[test]
    fn yield_is_fifo(progs in progs(1..=4), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for (want, got) in &r.states {
            prop_assert_eq!(got, want);
        }
    }

    /// 4. `exit` and `exit_group` end the caller for good: never queued or running again,
    /// its frames and context released, and the shutdown reason the oracle's.
    #[test]
    fn exit_removes_the_process(progs in progs(1..=4), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        let p = r.h.k.processes().unwrap();
        for c in r.ran.iter().filter(|c| matches!(c.expect, Expect::Exit(_))) {
            let pcb = p.pcb(c.pid).unwrap();
            prop_assert!(pcb.frames().is_empty() && pcb.context.is_none());
            prop_assert!(!p.queue().contains(&c.pid) && p.current() != Some(c.pid));
            prop_assert!(r.ran.iter().rfind(|l| l.pid == c.pid).unwrap().call.nr == c.call.nr);
            prop_assert!(!c.seen.iter().any(|s| s.0 && s.1 == F + 0x24), "no return value");
        }
        let (want, got) = r.states.last().unwrap();
        prop_assert_eq!(got.down, want.down);
    }

    /// 5. An unsupported number returns `-ENOSYS` with nothing else changed, the same way
    /// every time.
    #[test]
    fn unsupported_numbers_are_deterministic(progs in progs(1..=3), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for c in r.ran.iter().filter(|c| c.expect == Expect::NoSys) {
            prop_assert_eq!(&c.after, &returned(&c.before, 38u32.wrapping_neg()));
            prop_assert_eq!(c.seen.len(), 12, "the frame read and the Return only");
        }
    }

    /// 6. `write` returns what the oracle computes from the fd, the count capped at
    /// `WRITE_MAX`, and whether every page of the range is mapped readable.
    #[test]
    fn write_returns_the_length_or_the_error(progs in progs(1..=2), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for c in r.ran.iter().filter(|c| c.call.nr == 64) {
            let Expect::Return { value, .. } = &c.expect else { unreachable!() };
            prop_assert_eq!(&c.after, &returned(&c.before, *value));
        }
    }

    /// 7. The UART receives exactly the guest's bytes of the range, in order, and nothing
    /// for an error.
    #[test]
    fn output_is_the_guest_bytes(progs in progs(1..=2), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for c in &r.ran {
            let want = match &c.expect {
                Expect::Return { bytes, .. } => bytes.clone(),
                _ => Vec::new(),
            };
            prop_assert_eq!(c.uart(), want);
        }
    }

    /// 8. A restore at any boundary of a `write` never repeats or drops a byte.
    #[test]
    fn restore_during_write_never_duplicates_output(
        progs in progs(1..=2),
        calls in calls(),
        pick in any::<prop::sample::Index>(),
    ) {
        assert_cut_is_invisible(&progs, &calls, pick, |b| is_nr(b, &[64]))?;
    }

    /// 9. A restore at any boundary of a `sched_yield` never queues the caller twice.
    #[test]
    fn restore_during_yield_never_duplicates_the_queue_entry(
        progs in progs(1..=3),
        calls in calls(),
        pick in any::<prop::sample::Index>(),
    ) {
        assert_cut_is_invisible(&progs, &calls, pick, |b| is_nr(b, &[124]))?;
        let r = run(&progs, &calls, |_| false);
        for (_, got) in &r.states {
            let mut q: Vec<u32> = got.queue.iter().copied().collect();
            q.sort_unstable();
            q.dedup();
            prop_assert_eq!(q.len(), got.queue.len());
        }
    }

    /// 10. A restore at any boundary of an `exit` never brings the process back.
    #[test]
    fn restore_during_exit_never_resurrects(
        progs in progs(1..=3),
        calls in calls(),
        pick in any::<prop::sample::Index>(),
    ) {
        assert_cut_is_invisible(&progs, &calls, pick, |b| is_nr(b, &[93, 94]))?;
    }

    /// 11. The same programs and calls give the same accesses, output, traces, memory,
    /// snapshots, and snapshot digest.
    #[test]
    fn the_same_input_gives_the_same_output_and_digest(progs in progs(1..=3), calls in calls()) {
        let (a, b) = (run(&progs, &calls, |_| false), run(&progs, &calls, |_| false));
        prop_assert_eq!(&a.ran, &b.ran);
        prop_assert_eq!(&a.h.seen, &b.h.seen);
        prop_assert_eq!(&a.h.ctx.traced, &b.h.ctx.traced);
        prop_assert_eq!(&a.h.mem, &b.h.mem);
        prop_assert_eq!(&a.snaps, &b.snaps);
        prop_assert_eq!(fnv(a.snaps.last().unwrap()), fnv(b.snaps.last().unwrap()));
    }

    /// 12. No syscall touches a frame register it does not own: a returning one writes
    /// only `a0` and `sepc`, and a yield saves the caller's registers as they were but
    /// `a0 = 0`, with `pc = sepc + 4`.
    #[test]
    fn no_unrelated_register_is_mutated(progs in progs(1..=3), calls in calls()) {
        let r = run(&progs, &calls, |_| false);
        for c in &r.ran {
            match &c.expect {
                Expect::Return { value, .. } => prop_assert_eq!(&c.after, &returned(&c.before, *value)),
                Expect::NoSys => prop_assert_eq!(&c.after, &returned(&c.before, 38u32.wrapping_neg())),
                Expect::Yield => {
                    let (regs, pc) = c.saved.clone().unwrap();
                    let mut want: Vec<u32> = (0..31)
                        .map(|i| u32::from_le_bytes(c.before[4 * i..4 * i + 4].try_into().unwrap()))
                        .collect();
                    want[9] = 0;
                    prop_assert_eq!(regs, want);
                    let sepc = u32::from_le_bytes(c.before[0x7C..0x80].try_into().unwrap());
                    prop_assert_eq!(pc, sepc.wrapping_add(4));
                    let writes: Vec<u64> = c.seen.iter().filter(|s| s.0).map(|s| s.1).collect();
                    prop_assert!(writes.iter().all(|&a| (F..F + 0x98).contains(&a)));
                }
                Expect::Exit(_) => {
                    let writes = c.seen.iter().filter(|s| s.0);
                    prop_assert!(writes.clone().all(|s| (F..F + 0x98).contains(&s.1)));
                }
            }
        }
    }

}

/// PTE flag bytes: a clean pointer, a readable user leaf, or anything.
fn flags() -> impl Strategy<Value = u8> {
    prop_oneof![1 => Just(0x01u8), 1 => Just(0x53), 2 => any::<u8>()]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// The kernel's user-copy walk agrees with the CPU's Sv32 walk for a U-mode load
    /// with `SUM = MXR = 0` on any PTE pair, reading the same PTE addresses (§15.1).
    #[test]
    fn the_user_copy_walk_is_the_cpus_load_walk(
        root in 0u32..1 << 22,
        va in any::<u32>(),
        ptes in (any::<u32>(), any::<u32>(), any::<bool>(), flags(), flags()),
    ) {
        let (mut l1, mut l0, align, f1, f0) = ptes;
        // Bias toward pointers, leaves, and aligned megapages, so every rule is reached.
        l1 = (l1 & !0xFF) | u32::from(f1);
        l0 = (l0 & !0xFF) | u32::from(f0);
        if align {
            l1 &= !(0x3FF << 10);
        }
        let read = |seen: &mut Vec<u64>, a: u64| {
            seen.push(a);
            if seen.len() == 1 { l1 } else { l0 }
        };
        let mut ours = Vec::new();
        let kernel = syscall::translate(root, va, |a| read(&mut ours, a));
        let mut theirs = Vec::new();
        let cpu = sv32_translate(0x8000_0000 | root, Privilege::User, false, false, Access::Load, va, |a| {
            Some(read(&mut theirs, a))
        });
        prop_assert_eq!(kernel, cpu.ok());
        prop_assert_eq!(ours, theirs);
    }
}

#[test]
fn the_oracle_agrees_on_a_known_case() {
    let progs = [
        Prog {
            text: vec![0x13],
            data: b"A".to_vec(),
            bss: 0,
        },
        Prog {
            text: vec![0x13],
            data: b"B".to_vec(),
            bss: 5000,
        },
    ];
    let calls = [
        Call {
            nr: 64,
            args: [1, DATA, 2],
        },
        Call {
            nr: 124,
            args: [0; 3],
        },
        Call {
            nr: 64,
            args: [2, DATA + 4094, 4],
        },
        Call {
            nr: 93,
            args: [0; 3],
        },
        Call {
            nr: 94,
            args: [3, 0, 0],
        },
    ];
    let r = run(&progs, &calls, |_| false);
    let out: Vec<u8> = r.ran.iter().flat_map(Ran::uart).collect();
    let mut want = vec![pattern(1, DATA), pattern(1, DATA + 1)];
    want.extend((DATA + 4094..DATA + 4098).map(|va| pattern(2, va)));
    assert_eq!(out, want);
    assert_eq!(r.states.last().unwrap().1.down, Some(1));
    assert!(r.h.k.processes().unwrap().current().is_none());
    assert!(r.h.pending.is_none());
}
