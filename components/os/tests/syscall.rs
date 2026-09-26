//! The syscall ABI driven through `MockCtx` (`docs/m3-design.md` §6.5, §6.6, §6.8, §7.2,
//! §17 M3.5): every syscall and error path from a trap frame in memory, the exact kernel
//! accesses each one makes, the user-copy walk, UART output, the frame words a syscall
//! may and may not touch, bus faults, and the snapshot of a syscall in flight: its
//! contents, every restore rejection, and restore at every step boundary of a
//! two-process scenario without a reissue.

mod common;

use common::layout::*;
use common::procs::config;
use common::procs::ksnap::{self, Snap, Stage};
use common::procs::*;
use common::{restore_into, snapshot_of};
use systemscope_contracts::component::Component;
use systemscope_contracts::event::Phase;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::Value;
use systemscope_os::ModeledKernel;
use systemscope_os::kernel::{ISSUE, SHUTDOWN_KIND};
use systemscope_os::process::ProcState;
use systemscope_os::procop::{
    EXIT_KIND, FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND, SYSCALL_EXIT_KIND,
};

const STACK_TOP: u32 = 0x7FFF_F000;
const TEXT: u32 = 0x0001_0000;
const DATA: u32 = 0x0001_1000;
const F: u64 = TRAP_FRAME as u64;

// The ABI numbers and errors, from the design text (§6.5), not from the crate.
const WRITE: u32 = 64;
const EXIT: u32 = 93;
const EXIT_GROUP: u32 = 94;
const YIELD: u32 = 124;
const GETPID: u32 = 172;
const EBADF: u32 = 9u32.wrapping_neg();
const EFAULT: u32 = 14u32.wrapping_neg();
const ENOSYS: u32 = 38u32.wrapping_neg();

/// A program with three text words, `data`, and `bss` zero bytes after it.
fn prog(data: &[u8], bss: u32) -> Vec<u8> {
    two_segment(&[0x13, 0x13, 0x13], data, bss)
}

/// A program whose data segment spans three pages: 0x11000–0x13FFF.
fn wide() -> Vec<u8> {
    prog(b"hello, world", 8192)
}

fn booted(files: &[Vec<u8>]) -> Harness {
    let mut h = Harness::new(config(), files, plan_of(files));
    h.boot();
    h
}

fn procs(h: &Harness) -> &systemscope_os::process::Processes {
    h.k.processes().unwrap()
}

fn root(h: &Harness, pid: u32) -> u32 {
    procs(h).pcb(pid).unwrap().root
}

fn state(h: &Harness, pid: u32) -> ProcState {
    procs(h).pcb(pid).unwrap().state
}

fn queue(h: &Harness) -> Vec<u32> {
    procs(h).queue().iter().copied().collect()
}

fn frame_bytes(h: &Harness) -> Vec<u8> {
    h.mem.read(F, 0x98)
}

/// The physical address of user `va` in `pid`'s address space, by the independent walk.
fn pa(h: &Harness, pid: u32, va: u32) -> u64 {
    walk(&h.mem, root(h, pid), va, Kind::Load).unwrap()
}

/// Fills `pid`'s user bytes `[va, va + len)` with a pattern, as its own stores would.
fn fill(h: &mut Harness, pid: u32, va: u32, len: u32) {
    for v in va..va + len {
        let byte = (v.wrapping_mul(7) ^ (v >> 8)) as u8;
        let at = pa(h, pid, v);
        h.mem.write(at, &[byte]);
    }
}

fn user_bytes(h: &Harness, pid: u32, va: u32, len: u32) -> Vec<u8> {
    (va..va + len)
        .map(|v| h.mem.read(pa(h, pid, v), 1)[0])
        .collect()
}

/// One syscall operation's accesses and outcome.
struct Call {
    stop: Stop,
    seen: Vec<Seen>,
}

impl Call {
    fn uart(&self) -> Vec<u8> {
        uart(&self.seen)
    }

    /// The accesses after the frame read.
    fn after_frame(&self) -> &[Seen] {
        &self.seen[10..]
    }
}

fn uart(seen: &[Seen]) -> Vec<u8> {
    seen.iter()
        .filter(|s| s.0 && s.1 == UART_BASE)
        .flat_map(|s| s.2.clone())
        .collect()
}

/// Runs syscall `nr` from `sepc`, faulting access `inject` of the operation.
fn call_at(h: &mut Harness, nr: u32, args: [u32; 3], sepc: u32, inject: Inject) -> Call {
    h.write_syscall(nr, args, sepc, 0, 0x5);
    let before = h.seen.len();
    let stop = h.op(TRAP_FRAME, inject);
    Call {
        stop,
        seen: h.seen[before..].to_vec(),
    }
}

fn call(h: &mut Harness, nr: u32, args: [u32; 3]) -> Call {
    call_at(h, nr, args, TEXT + 8, None)
}

/// The frame read: ten reads of 16, 16, …, 8 bytes at the frame.
fn assert_frame_read(seen: &[Seen]) {
    let reads: Vec<(bool, u64, usize)> = seen[..10].iter().map(|s| (s.0, s.1, s.2.len())).collect();
    let want: Vec<(bool, u64, usize)> = (0..10)
        .map(|i| (false, F + 16 * i, if i == 9 { 8 } else { 16 }))
        .collect();
    assert_eq!(reads, want);
}

/// The Return: `a0 = value`, then `sepc = sepc + 4`, and nothing else.
fn return_writes(value: u32, sepc: u32) -> Vec<Seen> {
    vec![
        (true, F + 0x24, value.to_le_bytes().to_vec()),
        (true, F + 0x7C, sepc.wrapping_add(4).to_le_bytes().to_vec()),
    ]
}

/// The frame after a returning syscall is the frame before with only `a0` and `sepc`
/// changed.
fn assert_only_a0_and_sepc(before: &[u8], after: &[u8], value: u32, sepc: u32) {
    let mut want = before.to_vec();
    want[0x24..0x28].copy_from_slice(&value.to_le_bytes());
    want[0x7C..0x80].copy_from_slice(&sepc.wrapping_add(4).to_le_bytes());
    assert_eq!(after, want);
}

/// `(pid, nr, a0, a1, a2)` of every `os.syscall.enter`, and `(pid, nr, ret)` of every
/// `os.syscall.exit`.
type Entered = Vec<(u64, u64, u64, u64, u64)>;
type Exited = Vec<(u64, u64, u64)>;

fn syscall_traces(h: &Harness) -> (Entered, Exited) {
    let enter = h
        .traced(SYSCALL_ENTER_KIND)
        .iter()
        .map(|t| {
            (
                tu(t, "pid"),
                tu(t, "nr"),
                tu(t, "a0"),
                tu(t, "a1"),
                tu(t, "a2"),
            )
        })
        .collect();
    let exit = h
        .traced(SYSCALL_EXIT_KIND)
        .iter()
        .map(|t| (tu(t, "pid"), tu(t, "nr"), tu(t, "ret")))
        .collect();
    (enter, exit)
}

fn exits(h: &Harness) -> Vec<(u64, i64)> {
    h.traced(EXIT_KIND)
        .iter()
        .map(|t| match tfield(t, "status") {
            Value::I64(s) => (tu(t, "pid"), *s),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn switches(h: &Harness) -> Vec<(u64, u64)> {
    h.traced(SWITCH_KIND)
        .iter()
        .map(|t| (tu(t, "from"), tu(t, "to")))
        .collect()
}

// --- getpid -------------------------------------------------------------------------

#[test]
fn getpid_returns_the_running_pid_and_touches_only_a0_and_sepc() {
    let files = [prog(b"a", 0), prog(b"b", 0)];
    let mut h = booted(&files);
    h.write_syscall(GETPID, [7, 8, 9], TEXT + 8, 0x20, 3);
    let before = frame_bytes(&h);
    let n = h.seen.len();
    assert_eq!(h.op(TRAP_FRAME, None), Stop::Released);
    let seen = h.seen[n..].to_vec();
    assert_frame_read(&seen);
    assert_eq!(seen[10..], return_writes(1, TEXT + 8));
    assert_only_a0_and_sepc(&before, &frame_bytes(&h), 1, TEXT + 8);
    assert_eq!(procs(&h).current(), Some(1));
    assert_eq!(queue(&h), [2]);
    assert_eq!(state(&h, 1), ProcState::Running);
    let (enter, exit) = syscall_traces(&h);
    assert_eq!(enter, [(1, 172, 7, 8, 9)]);
    assert_eq!(exit, [(1, 172, 1)]);
    assert_eq!(switches(&h), [(0, 1)], "getpid switches nothing");
}

#[test]
fn getpid_follows_the_running_process_across_yields() {
    let files = [prog(b"a", 0), prog(b"b", 0), prog(b"c", 0)];
    let mut h = booted(&files);
    let mut got = Vec::new();
    for _ in 0..7 {
        assert_eq!(call(&mut h, GETPID, [0; 3]).stop, Stop::Released);
        got.push(h.frame(0x24));
        assert_eq!(call(&mut h, YIELD, [0; 3]).stop, Stop::Released);
    }
    assert_eq!(got, [1, 2, 3, 1, 2, 3, 1]);
}

#[test]
fn getpid_is_the_same_after_a_restore() {
    let files = [prog(b"a", 0), prog(b"b", 0)];
    let mut h = booted(&files);
    call(&mut h, YIELD, [0; 3]);
    let bytes = snapshot_of(&h.k);
    let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
    restore_into(&mut k, &bytes).unwrap();
    h.k = k;
    assert_eq!(call(&mut h, GETPID, [0; 3]).stop, Stop::Released);
    assert_eq!(h.frame(0x24), 2);
}

// --- sched_yield --------------------------------------------------------------------

#[test]
fn a_lone_process_yields_to_itself_with_a0_zero_and_sepc_plus_4() {
    let files = [prog(b"a", 0)];
    let mut h = booted(&files);
    h.write_syscall(YIELD, [0xAA, 0xBB, 0xCC], TEXT + 4, 0, 9);
    let before = frame_bytes(&h);
    assert_eq!(h.op(TRAP_FRAME, None), Stop::Released);
    assert_eq!(procs(&h).current(), Some(1));
    assert_eq!(queue(&h), Vec::<u32>::new(), "never queued twice");
    // The dispatch rewrites the context in full: a0 = 0, sepc + 4, the rest as it was.
    let mut want = before.clone();
    want[0x24..0x28].copy_from_slice(&0u32.to_le_bytes());
    want[0x7C..0x80].copy_from_slice(&(TEXT + 8).to_le_bytes());
    want[0x8C..0x90].copy_from_slice(&(0x8000_0000 | root(&h, 1)).to_le_bytes());
    want[0x90..0x98].fill(0);
    assert_eq!(frame_bytes(&h), want);
    assert_eq!(switches(&h), [(0, 1), (1, 1)]);
    assert_eq!(syscall_traces(&h).1, [(1, 124, 0)]);
}

#[test]
fn yield_rotates_two_and_three_processes_fifo() {
    for n in [2u32, 3] {
        let files: Vec<Vec<u8>> = (0..n).map(|i| prog(&[b'a' + i as u8], 0)).collect();
        let mut h = booted(&files);
        let mut order = vec![procs(&h).current().unwrap()];
        for round in 0..2 * n {
            let from = procs(&h).current().unwrap();
            let q_before = queue(&h);
            assert_eq!(call(&mut h, YIELD, [round, 0xB05, 0]).stop, Stop::Released);
            let mut want: Vec<u32> = q_before[1..].to_vec();
            want.push(from);
            assert_eq!(queue(&h), want, "the caller goes to the tail");
            assert_eq!(state(&h, from), ProcState::Ready);
            let saved = procs(&h).pcb(from).unwrap().context.unwrap();
            assert_eq!((saved.regs[9], saved.pc), (0, TEXT + 12));
            assert_eq!(saved.regs[10], 0xB05, "a1 as the caller left it");
            order.push(procs(&h).current().unwrap());
            assert_eq!(
                h.frame(0x8C),
                0x8000_0000 | root(&h, *order.last().unwrap())
            );
        }
        let want: Vec<u32> = (0..=2 * n).map(|i| i % n + 1).collect();
        assert_eq!(order, want);
    }
}

// --- exit, exit_group ---------------------------------------------------------------

#[test]
fn exit_of_a_non_last_process_frees_it_and_dispatches_the_next() {
    let files = [prog(b"a", 0), prog(b"b", 0), prog(b"c", 0)];
    let mut h = booted(&files);
    let free = procs(&h).frames().free_count();
    let own = procs(&h).pcb(1).unwrap().frames().len();
    assert_eq!(call(&mut h, EXIT, [5, 0, 0]).stop, Stop::Released);
    assert_eq!(state(&h, 1), ProcState::Exited { status: 5 });
    let pcb = procs(&h).pcb(1).unwrap();
    assert!(pcb.context.is_none() && pcb.tables.is_empty() && pcb.regions.is_empty());
    assert_eq!(procs(&h).frames().free_count(), free + own);
    assert_eq!(procs(&h).current(), Some(2));
    assert_eq!(queue(&h), [3]);
    assert_eq!(exits(&h), [(1, 5)]);
    assert_eq!(switches(&h), [(0, 1), (1, 2)]);
    assert!(
        syscall_traces(&h).1.is_empty(),
        "exit does not return: no os.syscall.exit"
    );
    // B's initial context was dispatched, not A's frame.
    assert_eq!((h.frame(0x04), h.frame(0x7C)), (STACK_TOP, TEXT));
}

#[test]
fn the_last_exit_shuts_down_with_reason_0_only_when_every_status_was_0() {
    for (statuses, reason) in [([0, 0], 0u32), ([0, 1], 1), ([7, 0], 1), ([u32::MAX, 0], 1)] {
        let files = [prog(b"a", 0), prog(b"b", 0)];
        let mut h = booted(&files);
        assert_eq!(call(&mut h, EXIT, [statuses[0], 0, 0]).stop, Stop::Released);
        assert_eq!(
            call(&mut h, EXIT_GROUP, [statuses[1], 0, 0]).stop,
            Stop::Released
        );
        assert_eq!(procs(&h).current(), None);
        assert_eq!((h.frame(0x90), h.frame(0x94)), (1, reason), "{statuses:?}");
        let s = h.traced(SHUTDOWN_KIND);
        assert_eq!(tu(s[0], "reason"), u64::from(reason));
        assert_eq!(procs(&h).frames().free_count(), procs(&h).frames().count());
        // After the shutdown, the trampoline's SRST ends the run; another ENTER is a bug.
        assert!(matches!(call(&mut h, GETPID, [0; 3]).stop, Stop::Fault(_)));
    }
}

#[test]
fn exit_status_is_the_full_signed_word() {
    for status in [0i32, 1, -1, 255, 256, i32::MIN, i32::MAX] {
        let files = [prog(b"a", 0), prog(b"b", 0)];
        let mut h = booted(&files);
        assert_eq!(
            call(&mut h, EXIT, [status as u32, 0, 0]).stop,
            Stop::Released
        );
        assert_eq!(state(&h, 1), ProcState::Exited { status });
        assert_eq!(exits(&h), [(1, i64::from(status))]);
        // And it survives a snapshot round trip.
        let bytes = snapshot_of(&h.k);
        let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
        restore_into(&mut k, &bytes).unwrap();
        assert_eq!(
            k.processes().unwrap().pcb(1).unwrap().state,
            ProcState::Exited { status }
        );
    }
}

#[test]
fn exit_group_is_exactly_exit() {
    // §6.5: "Same as exit (one thread per process)".
    let run = |nr: u32| {
        let files = [prog(b"a", 0), prog(b"b", 0)];
        let mut h = booted(&files);
        let a = call(&mut h, nr, [3, 4, 5]);
        let b = call(&mut h, nr, [0, 0, 0]);
        // Everything but the number itself: in the frame the kernel read, in the frame
        // left in memory, and in the enter traces.
        let mut traces = h.ctx.traced.clone();
        for t in &mut traces {
            if t.0 == SYSCALL_ENTER_KIND {
                t.1.retain(|f| f.0 != "nr");
            }
        }
        let mut seen = [a.seen, b.seen].concat();
        for s in &mut seen {
            if !s.0 && s.1 == F + 0x40 {
                s.2[..4].fill(0);
            }
        }
        h.mem.set_word(F + 0x40, 0);
        (seen, snapshot_of(&h.k), traces, h.mem)
    };
    let (exit, group) = (run(EXIT), run(EXIT_GROUP));
    assert!(exit == group);
}

// --- write --------------------------------------------------------------------------

/// The accesses of a successful `write` of `n` bytes at `buf` by `pid`: the walk (a
/// level-1 then a level-0 PTE read per page), then per chunk of at most 16 bytes that
/// crosses no page, a read of the chunk and one UART write per byte, then the Return.
fn write_accesses(h: &Harness, pid: u32, buf: u32, n: u32, sepc: u32) -> Vec<(bool, u64)> {
    let r = u64::from(root(h, pid));
    let mut out = Vec::new();
    let mut page = buf & !0xFFF;
    while u64::from(page) < u64::from(buf) + u64::from(n) {
        let l1 = h.mem.word(r * 4096 + 4 * u64::from(page >> 22));
        out.push((false, r * 4096 + 4 * u64::from(page >> 22)));
        out.push((
            false,
            u64::from(l1 >> 10) * 4096 + 4 * u64::from(page >> 12 & 0x3FF),
        ));
        page += 4096;
    }
    let mut va = buf;
    while va < buf + n {
        let len = (buf + n - va).min(16).min(4096 - va % 4096);
        out.push((false, pa(h, pid, va)));
        out.extend((0..len).map(|_| (true, UART_BASE)));
        va += len;
    }
    out.extend(return_writes(n, sepc).into_iter().map(|s| (s.0, s.1)));
    out
}

fn assert_writes(files: &[Vec<u8>], fd: u32, buf: u32, count: u32, n: u32) {
    let mut h = booted(files);
    fill(&mut h, 1, DATA, 3 * 4096);
    h.write_syscall(WRITE, [fd, buf, count], TEXT + 4, 0, 1);
    let before = frame_bytes(&h);
    let want = write_accesses(&h, 1, buf, n, TEXT + 4);
    let expected = user_bytes(&h, 1, buf, n);
    let at = h.seen.len();
    assert_eq!(h.op(TRAP_FRAME, None), Stop::Released);
    let seen = &h.seen[at..];
    assert_frame_read(seen);
    let got: Vec<(bool, u64)> = seen[10..].iter().map(|s| (s.0, s.1)).collect();
    assert_eq!(got, want, "buf {buf:#x} count {count}");
    assert_eq!(uart(seen), expected, "the guest's bytes, in order, once");
    assert_only_a0_and_sepc(&before, &frame_bytes(&h), n, TEXT + 4);
    assert_eq!(syscall_traces(&h).1, [(1, 64, u64::from(n))]);
    assert_eq!(procs(&h).current(), Some(1));
}

#[test]
fn write_outputs_one_byte_and_many() {
    let files = [wide()];
    assert_writes(&files, 1, DATA, 1, 1);
    assert_writes(&files, 1, DATA + 3, 5, 5);
    assert_writes(&files, 2, DATA, 16, 16);
    assert_writes(&files, 1, DATA + 1, 17, 17);
    assert_writes(&files, 1, DATA + 100, 300, 300);
}

#[test]
fn write_crosses_pages_in_order() {
    let files = [wide()];
    assert_writes(&files, 1, DATA + 4090, 12, 12);
    assert_writes(&files, 1, DATA + 4095, 2, 2);
    assert_writes(&files, 1, DATA + 4096 - 16, 16, 16);
    assert_writes(&files, 1, DATA + 4096, 20, 20);
}

#[test]
fn write_is_capped_at_write_max() {
    let files = [wide()];
    assert_writes(&files, 1, DATA, 4096, 4096);
    assert_writes(&files, 1, DATA + 0x800, 4096, 4096);
    assert_writes(&files, 1, DATA + 0x800, 4097, 4096);
    assert_writes(&files, 1, DATA + 1, u32::MAX, 4096);
}

#[test]
fn a_text_page_is_readable_for_write() {
    let files = [wide()];
    let mut h = booted(&files);
    let c = call(&mut h, WRITE, [1, TEXT, 12]);
    assert_eq!(c.stop, Stop::Released);
    assert_eq!(c.uart(), words(&[0x13, 0x13, 0x13]));
    assert_eq!(h.frame(0x24), 12);
}

#[test]
fn a_zero_count_returns_0_without_touching_the_buffer() {
    let files = [wide()];
    for buf in [DATA, 0, 0x8000_0000, 0xFFFF_FFFF] {
        let mut h = booted(&files);
        let c = call(&mut h, WRITE, [1, buf, 0]);
        assert_eq!(c.stop, Stop::Released);
        assert_eq!(c.after_frame(), return_writes(0, TEXT + 8));
    }
}

#[test]
fn a_bad_fd_is_ebadf_before_the_buffer_is_looked_at() {
    let files = [wide()];
    for fd in [0, 3, 7, 0x8000_0001, u32::MAX] {
        for buf in [DATA, 0] {
            let mut h = booted(&files);
            let c = call(&mut h, WRITE, [fd, buf, 4]);
            assert_eq!(c.stop, Stop::Released);
            assert_eq!(c.after_frame(), return_writes(EBADF, TEXT + 8), "fd {fd}");
        }
    }
}

#[test]
fn a_bad_buffer_is_efault_before_any_byte_is_output() {
    let files = [wide()];
    let cases: [(&str, u32, u32, usize); 8] = [
        // (what, buf, count, PTE reads before the refusal)
        ("page 0: no leaf", 0, 4, 2),
        ("the kernel megapage", 0x8000_0000, 4, 1),
        ("the trap frame", TRAP_FRAME, 4, 1),
        ("the MMIO megapage", 0x1000_0000, 1, 1),
        ("the page above the stack", STACK_TOP, 1, 2),
        ("an empty slot", 0x4000_0000, 1, 1),
        ("the second page unmapped", DATA + 3 * 4096 - 2, 4, 4),
        (
            "unmapped after a mapped first page, at the cap",
            DATA + 2 * 4096 + 1,
            5000,
            4,
        ),
    ];
    for (what, buf, count, ptes) in cases {
        let mut h = booted(&files);
        let c = call(&mut h, WRITE, [1, buf, count]);
        assert_eq!(c.stop, Stop::Released, "{what}");
        let reads = &c.after_frame()[..ptes];
        assert!(reads.iter().all(|s| !s.0 && s.2.len() == 4), "{what}");
        assert_eq!(
            c.after_frame()[ptes..],
            return_writes(EFAULT, TEXT + 8),
            "{what}"
        );
        assert!(c.uart().is_empty(), "{what}: no partial output");
        // A bad pointer is a syscall error, not a CPU page fault: the caller runs on.
        assert_eq!(procs(&h).current(), Some(1), "{what}");
        assert!(h.traced(FAULT_KIND).is_empty(), "{what}");
    }
}

#[test]
fn a_pointer_pte_at_level_0_is_efault_after_two_reads() {
    // The walk ends at level 0: a pointer there is a page fault for the CPU, so the user
    // copy refuses it without reading a third PTE. The test rewrites the level-0 PTE of
    // the data page in its own memory to a pointer back to the root.
    let files = [wide()];
    let mut h = booted(&files);
    let r = root(&h, 1);
    let l1 = h.mem.word(u64::from(r) * 4096 + 4 * u64::from(DATA >> 22));
    let pte = u64::from(l1 >> 10) * 4096 + 4 * u64::from(DATA >> 12 & 0x3FF);
    h.mem.set_word(pte, r << 10 | 1);
    let c = call(&mut h, WRITE, [1, DATA, 4]);
    assert_eq!(c.stop, Stop::Released);
    let reads: Vec<u64> = c.after_frame()[..2].iter().map(|s| s.1).collect();
    assert_eq!(
        reads,
        [u64::from(r) * 4096 + 4 * u64::from(DATA >> 22), pte]
    );
    assert_eq!(c.after_frame()[2..], return_writes(EFAULT, TEXT + 8));
    assert!(c.uart().is_empty());
}

#[test]
fn a_range_past_the_top_of_the_address_space_is_efault_without_a_walk() {
    let files = [wide()];
    for (buf, count) in [(0xFFFF_FFF0, 32), (0xFFFF_FFFF, 2), (0xFFFF_F001, 4096)] {
        let mut h = booted(&files);
        let c = call(&mut h, WRITE, [1, buf, count]);
        assert_eq!(c.after_frame(), return_writes(EFAULT, TEXT + 8));
    }
    // The last byte of the address space alone is a range; it is unmapped.
    let mut h = booted(&files);
    let c = call(&mut h, WRITE, [2, 0xFFFF_FFFF, 1]);
    assert_eq!(c.after_frame().len(), 1 + 2);
    assert_eq!(h.frame(0x24), EFAULT);
}

#[test]
fn a_bus_fault_on_a_kernel_access_during_write_faults_the_session_without_a_repeat() {
    // §6.3: a Fault on the kernel's own access to a region the configuration says exists
    // is a session fault, never a guest-visible error.
    let files = [wide()];
    let total = {
        let mut h = booted(&files);
        call(&mut h, WRITE, [1, DATA + 4090, 12]).seen.len()
    };
    for inject in 10..total {
        let mut h = booted(&files);
        let c = call_at(&mut h, WRITE, [1, DATA + 4090, 12], TEXT + 8, Some(inject));
        assert_eq!(
            c.stop,
            Stop::Fault("modeled kernel: its own access faulted"),
            "access {inject}"
        );
        assert_eq!(c.seen.len(), inject + 1, "nothing after the faulted access");
        let out = uart(&c.seen[..inject]);
        assert_eq!(out, user_bytes(&h, 1, DATA + 4090, out.len() as u32));
        assert_eq!(h.k.held(), Some(HELD), "no release");
        // The a0 write is the second-to-last access; the sepc write, the last, is never
        // applied when it faults.
        let a0 = if inject + 1 == total { 12 } else { 1 };
        assert_eq!(
            (h.frame(0x24), h.frame(0x7C)),
            (a0, TEXT + 8),
            "access {inject}"
        );
    }
}

// --- unsupported numbers ------------------------------------------------------------

#[test]
fn every_other_number_is_enosys_and_the_caller_runs_on() {
    let files = [prog(b"a", 0), prog(b"b", 0)];
    for nr in [
        0,
        1,
        57,
        63,
        65,
        92,
        95,
        123,
        125,
        171,
        173,
        999,
        0x5352_5354,
        u32::MAX,
    ] {
        let mut h = booted(&files);
        h.write_syscall(nr, [1, DATA, 1], TEXT, 0, 2);
        let before = frame_bytes(&h);
        let at = h.seen.len();
        assert_eq!(h.op(TRAP_FRAME, None), Stop::Released);
        assert_eq!(h.seen[at + 10..], return_writes(ENOSYS, TEXT), "nr {nr}");
        assert_only_a0_and_sepc(&before, &frame_bytes(&h), ENOSYS, TEXT);
        // No switch: the M3.4b "any ecall yields" is gone.
        assert_eq!((procs(&h).current(), queue(&h)), (Some(1), vec![2]));
        assert_eq!(switches(&h), [(0, 1)]);
        assert_eq!(
            syscall_traces(&h).1,
            [(1, u64::from(nr), u64::from(ENOSYS))]
        );
    }
}

// --- the scenario, the snapshot, and restore ----------------------------------------

/// The `sepc` of scenario trap `i`: each process's calls are at consecutive words from
/// its entry.
fn local_sepc(i: usize) -> u32 {
    TEXT + 4 * ((i / 4) * 2 + i % 2) as u32
}

/// A: write "A", yield, write "a", exit(0). B: write "B", yield, write "b",
/// exit_group(0). Each process's data holds its two letters.
fn scenario_files() -> [Vec<u8>; 2] {
    [prog(b"Aa", 0), prog(b"Bb", 0)]
}

/// The traps of the scenario: `(nr, a0, a1, a2)` in the order the processes run.
const SCENARIO: [(u32, u32, u32, u32); 8] = [
    (WRITE, 1, DATA, 1),
    (YIELD, 0, 0, 0),
    (WRITE, 1, DATA, 1),
    (YIELD, 0, 0, 0),
    (WRITE, 2, DATA + 1, 1),
    (EXIT, 0, 0, 0),
    (WRITE, 1, DATA + 1, 1),
    (EXIT_GROUP, 0, 0, 0),
];

#[test]
fn two_processes_write_abab_through_yield_and_exit() {
    let files = scenario_files();
    let mut h = booted(&files);
    let mut sepcs = Vec::new();
    for (i, &(nr, a0, a1, a2)) in SCENARIO.iter().enumerate() {
        let pid = procs(&h).current().unwrap();
        assert_eq!(h.frame(0x8C), 0x8000_0000 | root(&h, pid));
        let sepc = local_sepc(i);
        assert_eq!(
            call_at(&mut h, nr, [a0, a1, a2], sepc, None).stop,
            Stop::Released
        );
        if nr == WRITE {
            assert_eq!(h.frame(0x24), 1);
            sepcs.push(h.frame(0x7C));
        }
    }
    let out = uart(&h.seen);
    assert_eq!(out, b"ABab");
    assert_eq!(sepcs, [TEXT + 4, TEXT + 4, TEXT + 12, TEXT + 12]);
    assert_eq!(switches(&h), [(0, 1), (1, 2), (2, 1), (1, 2)]);
    assert_eq!(exits(&h), [(1, 0), (2, 0)]);
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 0));
    assert_eq!(state(&h, 1), ProcState::Exited { status: 0 });
    assert_eq!(state(&h, 2), ProcState::Exited { status: 0 });
    assert_eq!(procs(&h).frames().free_count(), procs(&h).frames().count());
    let (enter, exit) = syscall_traces(&h);
    let nrs: Vec<u64> = enter.iter().map(|e| e.1).collect();
    assert_eq!(nrs, [64, 124, 64, 124, 64, 93, 64, 94]);
    let pids: Vec<u64> = enter.iter().map(|e| e.0).collect();
    assert_eq!(pids, [1, 1, 2, 2, 1, 1, 2, 2]);
    assert_eq!(
        exit,
        [
            (1, 64, 1),
            (1, 124, 0),
            (2, 64, 1),
            (2, 124, 0),
            (1, 64, 1),
            (2, 64, 1)
        ]
    );
}

#[test]
fn the_snapshot_of_a_write_holds_its_stage_not_the_bytes() {
    let files = [wide()];
    let mut h = booted(&files);
    fill(&mut h, 1, DATA, 2 * 4096);
    let pre = ksnap::prefix_len(
        &snapshot_of(&Harness::new(config(), &files, plan_of(&files)).k),
        3072,
    );
    h.write_syscall(WRITE, [1, DATA + 4094, 20], TEXT + 4, 0, 1);
    h.enter(TRAP_FRAME).unwrap();
    let mut stages = Vec::new();
    let data_page = |h: &Harness, va| (pa(h, 1, va) >> 12) as u32;
    let pages = vec![data_page(&h, DATA), data_page(&h, DATA + 4096)];
    loop {
        h.issue().unwrap();
        let s = Snap::decode(&snapshot_of(&h.k), pre).unwrap();
        stages.push(s.op.clone().unwrap());
        if h.respond(false).unwrap() {
            break;
        }
    }
    let buffer = (TEXT + 4, DATA + 4094, 20);
    let r = root(&h, 1);
    let l0 = h.mem.word(u64::from(r) * 4096) >> 10;
    // The walk: two PTEs per page; the running PID and its root are not repeated.
    assert_eq!(
        stages[10].stage,
        Stage::Walk {
            buffer,
            pages: vec![],
            table: r,
            level: 1
        }
    );
    assert_eq!(
        stages[11].stage,
        Stage::Walk {
            buffer,
            pages: vec![],
            table: l0,
            level: 0
        }
    );
    assert_eq!(
        stages[13].stage,
        Stage::Walk {
            buffer,
            pages: pages[..1].to_vec(),
            table: l0,
            level: 0
        }
    );
    // The output: the first chunk is the two bytes up to the page end.
    assert_eq!(
        stages[14],
        ksnap::Op {
            stage: Stage::Output {
                buffer,
                pages: pages.clone(),
                done: 0
            },
            step: 0,
            data: vec![],
        }
    );
    assert_eq!(
        stages[15].data,
        user_bytes(&h, 1, DATA + 4094, 2),
        "the chunk read"
    );
    assert_eq!(
        stages[17].stage,
        Stage::Output {
            buffer,
            pages,
            done: 2
        }
    );
    let n = stages.len();
    assert_eq!(
        stages[n - 2].stage,
        Stage::Return {
            sepc: TEXT + 4,
            value: 20
        }
    );
    assert_eq!((stages[n - 2].step, stages[n - 1].step), (0, 1));
}

/// A booted kernel restoring `bytes` rejects them and is left as it was.
fn assert_rejected(files: &[Vec<u8>], bytes: &[u8], why: &str) {
    let mut h = booted(files);
    let before = snapshot_of(&h.k);
    assert!(restore_into(&mut h.k, bytes).is_err(), "accepted: {why}");
    assert_eq!(
        snapshot_of(&h.k),
        before,
        "a failed restore changed state: {why}"
    );
}

/// The snapshots taken in `Issue` at every access of a syscall from `h`'s frame.
fn stage_snaps(h: &mut Harness, pre: usize) -> Vec<Snap> {
    h.enter(TRAP_FRAME).unwrap();
    let mut at = Vec::new();
    loop {
        at.push(Snap::decode(&snapshot_of(&h.k), pre).unwrap());
        h.issue().unwrap();
        if h.respond(false).unwrap() {
            return at;
        }
    }
}

fn restores(files: &[Vec<u8>], s: &Snap) -> bool {
    let mut k = ModeledKernel::with_processes(config(), plan_of(files)).unwrap();
    restore_into(&mut k, &s.encode()).is_ok() && snapshot_of(&k) == s.encode()
}

/// A program whose text is execute-only (`PF_X` alone, which §8.3 allows) and whose
/// data is `xyz`.
fn execute_only() -> Vec<u8> {
    elf32(
        TEXT,
        &[
            Seg::new(TEXT, PF_X, words(&[0x13, 0x13, 0x13]), 12),
            Seg::new(DATA, PF_R | PF_W, b"xyz".to_vec(), 3),
        ],
    )
}

#[test]
fn an_execute_only_page_is_not_a_write_buffer() {
    let files = [execute_only()];
    let mut h = booted(&files);
    let c = call(&mut h, WRITE, [1, TEXT, 4]);
    assert_eq!(
        c.after_frame().len(),
        2 + 2,
        "two PTE reads, then the Return"
    );
    assert_eq!(c.after_frame()[2..], return_writes(EFAULT, TEXT + 8));
    assert!(c.uart().is_empty());
    let c = call(&mut h, WRITE, [1, DATA, 3]);
    assert_eq!(c.uart(), b"xyz");
}

#[test]
fn restore_checks_the_slot_the_running_process_and_readability() {
    // A walk at level 0 in the stack's slot, the second table: accepted, and not with the
    // first table.
    let files = [wide(), prog(b"b", 0)];
    let pre = ksnap::prefix_len(
        &snapshot_of(&Harness::new(config(), &files, plan_of(&files)).k),
        3072,
    );
    let mut h = booted(&files);
    h.write_syscall(WRITE, [1, STACK_TOP - 8, 4], TEXT, 0, 1);
    let at = stage_snaps(&mut h, pre);
    let stack0 = at
        .iter()
        .find(|s| matches!(&s.op.as_ref().unwrap().stage, Stage::Walk { level: 0, .. }))
        .unwrap()
        .clone();
    assert!(restores(&files, &stack0));
    let data_table = h
        .mem
        .word(u64::from(root(&h, 1)) * 4096 + 4 * u64::from(DATA >> 22))
        >> 10;
    let mut wrong = stack0.clone();
    if let Stage::Walk { table, .. } = stage_mut(&mut wrong) {
        assert_ne!(*table, data_table);
        *table = data_table;
    }
    assert_rejected(
        &files,
        &wrong.encode(),
        "the stack walked in the data table",
    );

    // A Return with no process running, the rest of the table consistent: PID 1 Ready,
    // queued, with a context.
    let ret = at
        .iter()
        .find(|s| matches!(&s.op.as_ref().unwrap().stage, Stage::Return { .. }))
        .unwrap()
        .clone();
    assert!(restores(&files, &ret));
    let mut idle = ret.clone();
    idle.current = None;
    idle.pcbs[0].state = (0, vec![]);
    idle.pcbs[0].context = Some(vec![0; 33]);
    idle.queue.push(1);
    assert_rejected(&files, &idle.encode(), "a Return with no process running");

    // Output from an execute-only page: its frame is mapped, but not readable.
    let files = [execute_only()];
    let pre = ksnap::prefix_len(
        &snapshot_of(&Harness::new(config(), &files, plan_of(&files)).k),
        3072,
    );
    let mut h = booted(&files);
    h.write_syscall(WRITE, [1, DATA, 3], TEXT, 0, 1);
    let at = stage_snaps(&mut h, pre);
    let output = at
        .iter()
        .find(|s| {
            matches!(&s.op.as_ref().unwrap().stage, Stage::Output { .. })
                && s.op.as_ref().unwrap().step == 1
        })
        .unwrap()
        .clone();
    assert!(restores(&files, &output));
    let text = (walk(&h.mem, root(&h, 1), TEXT, Kind::Fetch).unwrap() >> 12) as u32;
    let mut forged = output.clone();
    if let Stage::Output { buffer, pages, .. } = stage_mut(&mut forged) {
        *buffer = (buffer.0, TEXT, 3);
        *pages = vec![text];
    }
    assert_rejected(&files, &forged.encode(), "output from an execute-only page");
}

type Mutant = (&'static str, Box<dyn Fn(&mut Snap)>);

fn stage_mut(s: &mut Snap) -> &mut Stage {
    &mut s.op.as_mut().unwrap().stage
}

#[test]
fn restore_rejects_every_impossible_syscall_stage() {
    let files = [wide(), prog(b"b", 0)];
    let pre = ksnap::prefix_len(
        &snapshot_of(&Harness::new(config(), &files, plan_of(&files)).k),
        3072,
    );
    // Snapshots of a walk at level 0 of the second page, an output after one chunk, and
    // a Return, all in Issue.
    let mut h = booted(&files);
    h.write_syscall(WRITE, [1, DATA + 4094, 20], TEXT + 4, 0, 1);
    h.enter(TRAP_FRAME).unwrap();
    let mut at = Vec::new();
    loop {
        at.push(Snap::decode(&snapshot_of(&h.k), pre).unwrap());
        h.issue().unwrap();
        if h.respond(false).unwrap() {
            break;
        }
    }
    let pick = |f: &dyn Fn(&Snap) -> bool| at.iter().find(|s| f(s)).unwrap().clone();
    let walk0 = pick(
        &|s| matches!(&s.op.as_ref().unwrap().stage, Stage::Walk { pages, level: 0, .. } if pages.len() == 1),
    );
    let output = pick(&|s| {
        matches!(&s.op.as_ref().unwrap().stage, Stage::Output { done: 2, .. })
            && s.op.as_ref().unwrap().step == 1
    });
    let ret = pick(&|s| matches!(&s.op.as_ref().unwrap().stage, Stage::Return { .. }));
    for good in [&walk0, &output, &ret] {
        let mut k = ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
        restore_into(&mut k, &good.encode()).unwrap();
        assert_eq!(snapshot_of(&k), good.encode());
    }
    let buffer_of = |s: &mut Snap| -> (u32, u32, u32) {
        match stage_mut(s) {
            Stage::Walk { buffer, .. } | Stage::Output { buffer, .. } => *buffer,
            _ => unreachable!(),
        }
    };
    let set_buffer = |s: &mut Snap, b: (u32, u32, u32)| match stage_mut(s) {
        Stage::Walk { buffer, .. } | Stage::Output { buffer, .. } => *buffer = b,
        _ => unreachable!(),
    };
    let both: Vec<Mutant> = vec![
        (
            "n = 0",
            Box::new(move |s| {
                let b = buffer_of(s);
                set_buffer(s, (b.0, b.1, 0))
            }),
        ),
        (
            "n > WRITE_MAX",
            Box::new(move |s| {
                let b = buffer_of(s);
                set_buffer(s, (b.0, b.1, 4097))
            }),
        ),
        (
            "a range past 2^32",
            Box::new(move |s| {
                let b = buffer_of(s);
                set_buffer(s, (b.0, 0xFFFF_FFF0, 20))
            }),
        ),
        (
            "a buffer on other pages",
            Box::new(move |s| {
                let b = buffer_of(s);
                set_buffer(s, (b.0, DATA + 2 * 4096 - 2, 20))
            }),
        ),
        (
            "no process running",
            Box::new(|s| {
                s.current = None;
                s.pcbs[0].state = (0, vec![]);
            }),
        ),
        (
            "the other process running",
            Box::new(|s| {
                s.current = Some(2);
                s.pcbs[1].state = (1, vec![]);
                s.pcbs[1].context = None;
                s.pcbs[0].state = (0, vec![]);
                s.queue = vec![1];
            }),
        ),
        ("after the shutdown", Box::new(|s| s.life = 2)),
    ];
    let walks: Vec<Mutant> = vec![
        (
            "level 2",
            Box::new(|s| {
                if let Stage::Walk { level, .. } = stage_mut(s) {
                    *level = 2
                }
            }),
        ),
        (
            "level 1 off the root",
            Box::new(|s| {
                if let Stage::Walk { level, table, .. } = stage_mut(s) {
                    *level = 1;
                    *table += 1
                }
            }),
        ),
        (
            "level 0 in another table",
            Box::new(|s| {
                if let Stage::Walk { table, .. } = stage_mut(s) {
                    *table += 1
                }
            }),
        ),
        (
            "level 0 in the root",
            Box::new(|s| {
                let root = s.pcbs[0].root;
                if let Stage::Walk { table, .. } = stage_mut(s) {
                    *table = root
                }
            }),
        ),
        (
            "a wrong page found",
            Box::new(|s| {
                if let Stage::Walk { pages, .. } = stage_mut(s) {
                    pages[0] += 1
                }
            }),
        ),
        (
            "every page found",
            Box::new(|s| {
                if let Stage::Walk { pages, .. } = stage_mut(s) {
                    pages.push(pages[0] + 1)
                }
            }),
        ),
        (
            "a step past the read",
            Box::new(|s| s.op.as_mut().unwrap().step = 1),
        ),
        (
            "working data before the read",
            Box::new(|s| s.op.as_mut().unwrap().data = vec![0; 4]),
        ),
    ];
    let outputs: Vec<Mutant> = vec![
        (
            "done mid-chunk",
            Box::new(|s| {
                if let Stage::Output { done, .. } = stage_mut(s) {
                    *done = 3
                }
            }),
        ),
        (
            "done at the end",
            Box::new(|s| {
                if let Stage::Output { done, .. } = stage_mut(s) {
                    *done = 20
                }
            }),
        ),
        (
            "a page missing",
            Box::new(|s| {
                if let Stage::Output { pages, .. } = stage_mut(s) {
                    pages.pop();
                }
            }),
        ),
        (
            "a wrong page",
            Box::new(|s| {
                if let Stage::Output { pages, .. } = stage_mut(s) {
                    pages[1] -= 1
                }
            }),
        ),
        (
            "the chunk's bytes missing",
            Box::new(|s| s.op.as_mut().unwrap().data.clear()),
        ),
        (
            "a step past the chunk",
            Box::new(|s| s.op.as_mut().unwrap().step = 17),
        ),
    ];
    let returns: Vec<Mutant> = vec![
        (
            "a step past the sepc write",
            Box::new(|s| s.op.as_mut().unwrap().step = 2),
        ),
        (
            "no process running",
            Box::new(|s| {
                s.current = None;
                s.pcbs[0].state = (0, vec![]);
            }),
        ),
        ("before boot", Box::new(|s| s.life = 0)),
    ];
    for (good, group) in [
        (&walk0, &both),
        (&output, &both),
        (&walk0, &walks),
        (&output, &outputs),
        (&ret, &returns),
    ] {
        for (why, m) in group {
            let mut s = good.clone();
            m(&mut s);
            assert_rejected(&files, &s.encode(), why);
        }
    }
}

/// Every handler boundary of the scenario, as a snapshot, restores into a fresh kernel
/// that then does exactly what the original did: the same requests with the same txns
/// (so no guest read, UART byte, or frame word twice), the same traces, the same final
/// snapshot and memory. In `Wait`, a wake to the restored kernel faults: it never
/// reissues.
#[test]
fn every_step_boundary_of_the_scenario_restores_and_never_reissues() {
    let files = scenario_files();
    let run = |k: Option<&[u8]>, cut: usize| {
        let mut h = Harness::new(config(), &files, plan_of(&files));
        let mut snaps = vec![snapshot_of(&h.k)];
        let mut requests = Vec::new();
        let mut boundary = 0usize;
        let mut restored = k.is_none();
        let ops = std::iter::once(None).chain(SCENARIO.iter().enumerate().map(Some));
        for t in ops {
            if let Some((i, &(nr, a0, a1, a2))) = t {
                h.write_syscall(nr, [a0, a1, a2], local_sepc(i), 0, i as u32);
            }
            h.enter(TRAP_FRAME).unwrap();
            boundary += 1;
            snaps.push(snapshot_of(&h.k));
            loop {
                if !restored && boundary == cut {
                    restore_into(&mut h.k, k.unwrap()).unwrap();
                    restored = true;
                }
                requests.push(h.issue().unwrap());
                boundary += 1;
                snaps.push(snapshot_of(&h.k));
                if !restored && boundary == cut {
                    restore_into(&mut h.k, k.unwrap()).unwrap();
                    restored = true;
                    let mut probe =
                        ModeledKernel::with_processes(config(), plan_of(&files)).unwrap();
                    restore_into(&mut probe, k.unwrap()).unwrap();
                    let mut ctx = common::MockCtx::new();
                    assert!(ctx.wake(&mut probe, ISSUE, Phase::Request).is_err());
                    assert!(ctx.sent.is_empty());
                }
                let done = h.respond(false).unwrap();
                boundary += 1;
                snaps.push(snapshot_of(&h.k));
                if done {
                    break;
                }
            }
        }
        (
            snaps,
            requests,
            boundary,
            h.ctx.traced.clone(),
            h.mem.clone(),
        )
    };
    let (snaps, requests, total, traces, mem) = run(None, usize::MAX);
    let uart_bytes: Vec<u8> = requests
        .iter()
        .filter_map(|m| match m {
            MemMsg::WriteReq { addr, data, .. } if *addr == UART_BASE => Some(data[0]),
            _ => None,
        })
        .collect();
    assert_eq!(uart_bytes, b"ABab");
    let pre = ksnap::prefix_len(&snaps[0], 3072);
    let mut cuts = Vec::new();
    for (c, bytes) in snaps.iter().enumerate().take(total).skip(1) {
        let s = Snap::decode(bytes, pre).unwrap();
        let creating = matches!(&s.op, Some(o) if matches!(o.stage, Stage::Create { .. }));
        if !creating || c % 101 == 0 {
            cuts.push(c);
        }
    }
    let syscall_stages = cuts
        .iter()
        .filter(|&&c| {
            let s = Snap::decode(&snaps[c], pre).unwrap();
            matches!(&s.op, Some(o) if matches!(o.stage, Stage::Walk { .. } | Stage::Output { .. } | Stage::Return { .. }))
        })
        .count();
    assert!(syscall_stages >= 48, "{syscall_stages}");
    for c in cuts {
        let (snaps2, requests2, _, traces2, mem2) = run(Some(&snaps[c]), c);
        assert_eq!(requests2, requests, "cut {c}");
        assert_eq!(snaps2.last(), snaps.last(), "cut {c}");
        assert_eq!(mem2, mem, "cut {c}");
        // The restored run's traces are the original's after the cut point: the handlers
        // before it ran in the original kernel of this run, so the whole list matches.
        assert_eq!(traces2, traces, "cut {c}");
    }
}

#[test]
fn inspect_names_the_syscall_stages() {
    let files = [wide()];
    let mut h = booted(&files);
    h.write_syscall(WRITE, [1, DATA, 2], TEXT, 0, 0);
    h.enter(TRAP_FRAME).unwrap();
    let mut names = Vec::new();
    loop {
        h.issue().unwrap();
        names.push(match h.k.inspect().get("op").cloned() {
            Some(Value::Str(s)) => s,
            other => panic!("{other:?}"),
        });
        if h.respond(false).unwrap() {
            break;
        }
    }
    names.dedup();
    assert_eq!(names, ["read_frame", "walk", "output", "return"]);
}
