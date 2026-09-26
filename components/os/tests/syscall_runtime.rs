//! Syscalls on the real platform (`docs/m3-design.md` §6.5, §6.6, §6.8, §7.3, §17 M3.5):
//! the M3 CPU's `ecall` from U traps through the §7.3 trampoline to `ModeledKernel`,
//! which decodes the syscall from the trap frame, reads the user buffer through its own
//! Sv32 walk and the bus, writes the bytes to the UART over the bus, and returns in the
//! frame, yields, or ends the process.
//!
//! - The user programs are hand-assembled and gp-independent: every address is built with
//!   `lui`/`addi`, never relative to `gp`, which the §6.6 initial context leaves 0 (the
//!   M3.1 `user.elf` relaxation problem is M3.6's). They check each return value and
//!   execute an illegal instruction on a mismatch, so a wrong result is a fault the test
//!   sees.
//! - Every event boundary of the two-process `ABab` run restores into a fresh platform
//!   and continues identically, with a bounded event count (a test limit, not a
//!   simulator watchdog).

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::layout::*;
use common::platform::asm::*;
use common::platform::{self, id};
use common::procs::{ksnap, plan_in, staged_at, two_segment};
use common::snap;
use systemscope_contracts::component::Delivered;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_os::kernel::{ENTER_KIND, RELEASE_KIND, SHUTDOWN_KIND};
use systemscope_os::procop::{
    CREATE_KIND, EXIT_KIND, FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND, SYSCALL_EXIT_KIND,
};
use systemscope_os::{KernelConfig, ProcessPlan, UserLayout};
use systemscope_platform::uart::TX_KIND;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::EXCEPTION_KIND;

const TEXT: u32 = 0x0001_0000;
const DATA: u32 = 0x0001_1000;
const POOL_FRAMES: u64 = 3072;
/// A bound on every run's events, so a broken kernel fails the test instead of hanging
/// it. Test-only: the simulator itself has no such limit.
const MAX_EVENTS: usize = 400_000;
const ILLEGAL: u32 = 0;

fn config() -> KernelConfig {
    common::layout::config(ClockDomainId(0))
}

/// One stack page: fewer frames to zero, so fewer events.
fn small() -> UserLayout {
    UserLayout {
        stack_pages: 1,
        ..UserLayout::M3
    }
}

/// A user program under assembly.
#[derive(Default)]
struct Asm(Vec<u32>);

impl Asm {
    /// `beq a, b, +8` over an illegal instruction: continue if `a == b`, else fault.
    fn check(&mut self, a: u32, b: u32) {
        self.0.push(beq(a, b, 8));
        self.0.push(ILLEGAL);
    }

    /// Faults unless `a0 == value`.
    fn check_a0(&mut self, value: u32) {
        self.0.extend(li(T5, value));
        self.check(A0, T5);
    }

    /// `ecall` with `a7 = nr` and `a0`–`a2` = `args`.
    fn syscall(&mut self, nr: u32, args: [u32; 3]) {
        for (r, v) in [A0, A1, A2].into_iter().zip(args) {
            self.0.extend(li(r, v));
        }
        self.0.extend(li(A7, nr));
        self.0.push(ECALL);
    }

    fn program(&self, data: &[u8], bss: u32) -> Vec<u8> {
        two_segment(&self.0, data, bss)
    }

    /// The address of the next instruction.
    fn here(&self) -> u32 {
        TEXT + 4 * self.0.len() as u32
    }
}

/// The scenario's process: `getpid` (checked), `write(1, "X")`, `sched_yield`, a check
/// that a callee-saved register survived the switch, `write(fd, "x")`, then `exit(0)` or
/// `exit_group(0)` with an illegal instruction after it. Its data is its two letters.
/// Returns the program and the `sepc` of each of its `ecall`s.
fn letters(pid: u32, letters: &[u8; 2], fd: u32, end: u32) -> (Vec<u8>, Vec<u32>) {
    let mut a = Asm::default();
    let mut ecalls = Vec::new();
    a.0.extend(li(S1, 0x5EED_0000 | pid));
    a.syscall(172, [0, 0, 0]);
    ecalls.push(a.here() - 4);
    a.check_a0(pid);
    a.syscall(64, [1, DATA, 1]);
    ecalls.push(a.here() - 4);
    a.check_a0(1);
    a.syscall(124, [0, 0, 0]);
    ecalls.push(a.here() - 4);
    a.check_a0(0);
    a.0.extend(li(T5, 0x5EED_0000 | pid));
    a.check(S1, T5);
    a.syscall(64, [fd, DATA + 1, 1]);
    ecalls.push(a.here() - 4);
    a.check_a0(1);
    a.syscall(end, [0, 0, 0]);
    ecalls.push(a.here() - 4);
    a.0.push(ILLEGAL);
    (a.program(letters, 0), ecalls)
}

fn scenario() -> ([Vec<u8>; 2], [Vec<u32>; 2]) {
    let (a, ea) = letters(1, b"Aa", 1, 93);
    let (b, eb) = letters(2, b"Bb", 2, 94);
    ([a, b], [ea, eb])
}

fn setup(files: &[Vec<u8>], layout: UserLayout) -> (platform::Program, ProcessPlan) {
    let staged: Vec<(u32, Vec<u8>)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (staged_at(i), f.clone()))
        .collect();
    (platform::firmware(&staged), plan_in(files, layout))
}

fn build(files: &[Vec<u8>], layout: UserLayout) -> Runtime {
    let (program, plan) = setup(files, layout);
    platform::build_procs(&program, config(), plan).unwrap()
}

#[derive(Default)]
struct Log {
    cpu: Option<StateView>,
    kernel: Option<StateView>,
    records: usize,
    marks: Vec<usize>,
    /// `(satp, running)` after every event with the CPU in U.
    user: Vec<(u64, String)>,
}

struct Recorder(Rc<RefCell<Log>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let mut log = self.0.borrow_mut();
        let cpu = world.inspect(id::CPU).unwrap();
        let kernel = world.inspect(id::KERNEL).unwrap();
        if cpu.get("priv") == Some(&Value::U64(0)) {
            log.user.push((vu(&cpu, "satp"), vs(&kernel, "running")));
        }
        log.cpu = Some(cpu);
        log.kernel = Some(kernel);
        let n = log.records;
        log.marks.push(n);
        Control::Continue
    }

    fn on_trace(&mut self, _: &TraceRecord) {
        self.0.borrow_mut().records += 1;
    }
}

struct Run {
    rt: Runtime,
    events: Vec<Dispatched>,
    marks: Vec<usize>,
    cpu: StateView,
    kernel: StateView,
    user: Vec<(u64, String)>,
    trace: Trace,
}

fn run(files: &[Vec<u8>], layout: UserLayout) -> Run {
    let mut rt = build(files, layout);
    let log = Rc::new(RefCell::new(Log::default()));
    rt.add_observer(Box::new(Recorder(Rc::clone(&log))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let mut events = Vec::new();
    while let Some(e) = rt.step().unwrap() {
        events.push(e);
        assert!(events.len() <= MAX_EVENTS, "the run does not end");
    }
    assert_eq!(rt.fault(), None);
    let trace = rt.take_trace().unwrap();
    let mut log = log.borrow_mut();
    Run {
        rt,
        events,
        marks: std::mem::take(&mut log.marks),
        cpu: log.cpu.take().unwrap(),
        kernel: log.kernel.take().unwrap(),
        user: std::mem::take(&mut log.user),
        trace,
    }
}

fn fresh_kernel(files: &[Vec<u8>], layout: UserLayout) -> Vec<u8> {
    let mut rt = build(files, layout);
    rt.init().unwrap();
    snap::components(&rt.snapshot().unwrap()).unwrap()[id::KERNEL.0 as usize]
        .1
        .clone()
}

fn field<'a>(r: &'a TraceRecord, name: &str) -> &'a Value {
    &r.fields.iter().find(|(k, _)| *k == name).unwrap().1
}

fn fu(r: &TraceRecord, name: &str) -> u64 {
    match field(r, name) {
        Value::U64(v) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn vs(view: &StateView, name: &str) -> String {
    match view.get(name) {
        Some(Value::Str(v)) => v.clone(),
        Some(Value::U64(v)) => v.to_string(),
        other => panic!("{name}: {other:?}"),
    }
}

fn vu(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

impl Run {
    fn records(&self, kind: &str) -> Vec<&TraceRecord> {
        self.trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.kind == kind)
            .collect()
    }

    fn uart(&self) -> Vec<u8> {
        self.records(TX_KIND)
            .iter()
            .map(|r| fu(r, "byte") as u8)
            .collect()
    }

    fn switches(&self) -> Vec<(u64, u64)> {
        self.records(SWITCH_KIND)
            .iter()
            .map(|r| (fu(r, "from"), fu(r, "to")))
            .collect()
    }

    fn entered(&self) -> Vec<(u64, u64)> {
        self.records(SYSCALL_ENTER_KIND)
            .iter()
            .map(|r| (fu(r, "pid"), fu(r, "nr")))
            .collect()
    }

    fn returned(&self) -> Vec<(u64, u64, u64)> {
        self.records(SYSCALL_EXIT_KIND)
            .iter()
            .map(|r| (fu(r, "pid"), fu(r, "nr"), fu(r, "ret")))
            .collect()
    }

    fn exits(&self) -> Vec<(u64, i64)> {
        self.records(EXIT_KIND)
            .iter()
            .map(|r| match field(r, "status") {
                Value::I64(s) => (fu(r, "pid"), *s),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// `(cause, pc)` of every CPU exception.
    fn exceptions(&self) -> Vec<(String, u64)> {
        self.records(EXCEPTION_KIND)
            .iter()
            .map(|r| match (field(r, "cause"), field(r, "pc")) {
                (Value::Str(c), Value::U64(e)) => (c.clone(), *e),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn roots(&self) -> Vec<u64> {
        self.records(CREATE_KIND)
            .iter()
            .map(|r| fu(r, "root"))
            .collect()
    }

    /// The kernel's requests: `(txn, write, addr, len)`.
    fn kernel_requests(&self) -> Vec<(u64, bool, u64, usize)> {
        self.events
            .iter()
            .filter(|e| e.source == id::KERNEL && e.target == id::BUS)
            .filter_map(|e| match &e.delivery {
                Delivered::Message {
                    msg: Message::MemV1(m),
                    ..
                } => match m {
                    MemMsg::ReadReq { txn, addr, len } => {
                        Some((txn.0, false, *addr, *len as usize))
                    }
                    MemMsg::WriteReq { txn, addr, data } => Some((txn.0, true, *addr, data.len())),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

fn assert_shut_down(r: &Run, reason: u64) {
    assert_eq!(vs(&r.cpu, "halt"), "trap");
    assert_eq!(vs(&r.cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!(vs(&r.cpu, "x11"), reason.to_string());
    let s = r.records(SHUTDOWN_KIND);
    assert_eq!(s.len(), 1);
    assert_eq!(fu(s[0], "reason"), reason);
}

/// Every trap came from U; the kernel's txns are fresh and consecutive; it read only the
/// trap frame, staging, and the frame pool (page tables and user bytes), and wrote only
/// the trap frame, the pool, and single bytes to UART TX; while the CPU ran in U, `satp`
/// was always the running process's root.
fn assert_clean(r: &Run) {
    for e in r.records(EXCEPTION_KIND) {
        assert_eq!(field(e, "from"), &Value::Str("U".to_owned()), "{e:?}");
    }
    let reqs = r.kernel_requests();
    let txns: Vec<u64> = reqs.iter().map(|q| q.0).collect();
    assert_eq!(txns, (0..reqs.len() as u64).collect::<Vec<_>>());
    let frame = u64::from(TRAP_FRAME)..u64::from(TRAP_FRAME) + 0x98;
    let pool = POOL..POOL + 4096 * POOL_FRAMES;
    for &(_, write, addr, len) in &reqs {
        let end = addr + len as u64;
        let within = |r: &std::ops::Range<u64>| r.contains(&addr) && end <= r.end;
        if write {
            assert!(
                within(&frame) || within(&pool) || (addr == UART_BASE && len == 1),
                "{addr:#x}"
            );
        } else {
            assert!(
                within(&frame)
                    || within(&pool)
                    || (addr >= STAGING && end <= STAGING + STAGING_SIZE),
                "{addr:#x}"
            );
        }
    }
    assert_eq!(r.records(ENTER_KIND).len(), r.records(RELEASE_KIND).len());
    let roots = r.roots();
    assert!(!r.user.is_empty());
    for (satp, running) in &r.user {
        let pid: usize = running.parse().unwrap();
        assert_eq!(
            *satp,
            0x8000_0000 | roots[pid - 1],
            "U ran under its own root"
        );
    }
}

#[test]
fn two_processes_write_abab_through_yield_and_exit_on_the_real_cpu() {
    let (files, ecalls) = scenario();
    let r = run(&files, UserLayout::M3);
    assert_eq!(r.uart(), b"ABab");
    assert!(r.records(FAULT_KIND).is_empty(), "every check passed");
    assert_eq!(r.switches(), [(0, 1), (1, 2), (2, 1), (1, 2)]);
    assert_eq!(
        r.entered(),
        [
            (1, 172),
            (1, 64),
            (1, 124),
            (2, 172),
            (2, 64),
            (2, 124),
            (1, 64),
            (1, 93),
            (2, 64),
            (2, 94)
        ]
    );
    assert_eq!(
        r.returned(),
        [
            (1, 172, 1),
            (1, 64, 1),
            (1, 124, 0),
            (2, 172, 2),
            (2, 64, 1),
            (2, 124, 0),
            (1, 64, 1),
            (2, 64, 1)
        ]
    );
    assert_eq!(r.exits(), [(1, 0), (2, 0)]);
    // Every ecall trapped from its own sepc, in the order the processes ran.
    let order = [0, 0, 0, 1, 1, 1, 0, 0, 1, 1];
    let idx = [0, 1, 2, 0, 1, 2, 3, 4, 3, 4];
    let want: Vec<(String, u64)> = order
        .iter()
        .zip(idx)
        .map(|(&p, i)| ("EnvironmentCallFromU".to_owned(), u64::from(ecalls[p][i])))
        .collect();
    assert_eq!(r.exceptions(), want);
    assert_shut_down(&r, 0);
    assert_clean(&r);
    assert_eq!(vs(&r.kernel, "free_frames"), "3072");
    assert_eq!(vs(&r.kernel, "running"), "none");
    assert_eq!(vs(&r.kernel, "queue"), "");
    let roots = r.roots();
    assert_eq!(
        vs(&r.kernel, "processes"),
        format!("1:exited(0):{:#x};2:exited(0):{:#x}", roots[0], roots[1])
    );
}

#[test]
fn syscall_errors_return_to_the_caller_on_the_real_cpu() {
    let mut a = Asm::default();
    // A kernel address, an unmapped page, and a range past 2^32: -EFAULT.
    a.syscall(64, [1, RAM_BASE as u32, 4]);
    a.check_a0(14u32.wrapping_neg());
    a.syscall(64, [1, 0x4000_0000, 1]);
    a.check_a0(14u32.wrapping_neg());
    a.syscall(64, [2, 0xFFFF_FFF0, 32]);
    a.check_a0(14u32.wrapping_neg());
    // A bad fd: -EBADF, before the buffer is looked at.
    a.syscall(64, [7, 0, 1]);
    a.check_a0(9u32.wrapping_neg());
    // Unknown numbers: -ENOSYS.
    for nr in [999, 63, 65, 0, u32::MAX] {
        a.syscall(nr, [1, DATA, 1]);
        a.check_a0(38u32.wrapping_neg());
    }
    // A zero count: 0.
    a.syscall(64, [1, 0, 0]);
    a.check_a0(0);
    // A write across the page boundary of the data segment.
    a.syscall(64, [1, DATA + 4094, 4]);
    a.check_a0(4);
    a.syscall(93, [0, 0, 0]);
    a.0.push(ILLEGAL);
    let mut data = vec![0u8; 4100];
    data[4094..4098].copy_from_slice(b"WXYZ");
    let r = run(&[a.program(&data, 0)], small());
    assert!(
        r.records(FAULT_KIND).is_empty(),
        "every error was as expected"
    );
    assert_eq!(r.uart(), b"WXYZ", "no partial output of a refused write");
    assert_eq!(r.switches(), [(0, 1)], "an error never switches");
    assert_eq!(r.exits(), [(1, 0)]);
    assert_shut_down(&r, 0);
    assert_clean(&r);
}

#[test]
fn a_nonzero_exit_status_shuts_down_with_reason_1() {
    for status in [1u32, u32::MAX, 0x8000_0000] {
        let mut a = Asm::default();
        a.syscall(64, [1, DATA, 1]);
        a.syscall(93, [status, 0, 0]);
        a.0.push(ILLEGAL);
        let r = run(&[a.program(b"E", 0)], small());
        assert_eq!(r.uart(), b"E");
        assert_eq!(r.exits(), [(1, i64::from(status as i32))]);
        assert_shut_down(&r, 1);
        assert_clean(&r);
    }
}

#[test]
fn a_fault_after_a_write_kills_only_that_process() {
    let mut a = Asm::default();
    a.syscall(64, [1, DATA, 1]);
    a.0.push(lw(T1, 0, 0));
    let fault_pc = a.here() - 4;
    let (b, _) = letters(2, b"Bb", 1, 93);
    let r = run(&[a.program(b"F", 0), b], small());
    assert_eq!(r.uart(), b"FBb");
    let faults = r.records(FAULT_KIND);
    assert_eq!(faults.len(), 1);
    assert_eq!(fu(faults[0], "pid"), 1);
    assert_eq!(fu(faults[0], "epc"), u64::from(fault_pc));
    assert_eq!(r.exits(), [(2, 0)]);
    assert_eq!(r.switches(), [(0, 1), (1, 2), (2, 2)]);
    assert_shut_down(&r, 1);
    assert_clean(&r);
}

#[test]
fn a_write_of_write_max_bytes_on_the_real_cpu() {
    let mut a = Asm::default();
    a.syscall(64, [1, DATA + 0x800, 5000]);
    a.check_a0(4096);
    a.syscall(94, [0, 0, 0]);
    a.0.push(ILLEGAL);
    let data: Vec<u8> = (0..8192u32).map(|i| b'a' + (i % 26) as u8).collect();
    let r = run(&[a.program(&data, 0)], small());
    assert_eq!(r.uart(), data[0x800..0x800 + 4096]);
    assert_shut_down(&r, 0);
    assert_clean(&r);
}

#[test]
fn the_syscall_run_is_deterministic() {
    let (files, _) = scenario();
    let a = run(&files, small());
    let b = run(&files, small());
    assert!(a.events == b.events);
    assert_eq!(a.trace, b.trace);
    assert_eq!(a.rt.snapshot().unwrap(), b.rt.snapshot().unwrap());
    assert_eq!(a.rt.state_digest().unwrap(), b.rt.state_digest().unwrap());
    assert_eq!(a.rt.execution_digest(), b.rt.execution_digest());
}

/// The every-event checkpoint scenario for syscalls: A and B write, yield, write, and
/// exit. After every event the platform is snapshotted and restored into a fresh one,
/// which must snapshot to the same bytes and dispatch the same next events; on a stride
/// and at every kernel-stage boundary (each syscall stage in Issue and Wait, so after the
/// guest-buffer read, after each UART write, before and after the a0 and sepc writes,
/// at the yield and exit dispatches, and before the release) it runs to the end and must
/// match the uninterrupted run exactly: events, bus traffic and txns, UART bytes, trace,
/// final snapshot (RAM and trap frame included), and both digests.
#[test]
fn every_event_of_a_syscall_run_restores_and_continues_identically() {
    const WINDOW: usize = 6;
    const STRIDE: usize = 509;
    let (files, _) = scenario();
    let layout = small();
    let full = run(&files, layout);
    assert_eq!(full.uart(), b"ABab");
    let last = full.rt.snapshot().unwrap();
    let digests = (full.rt.state_digest().unwrap(), full.rt.execution_digest());
    let total = full.events.len();
    assert!(total < MAX_EVENTS);

    let mut rt = build(&files, layout);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let kernel_id = id::KERNEL.0 as usize;
    let prefix = ksnap::prefix_len(&fresh_kernel(&files, layout), 3072);
    let mut stages = Vec::new();
    let mut syscall_runs = 0;
    for k in 0..=total {
        if k > 0 {
            let e = rt.step().unwrap().unwrap();
            assert!(e == full.events[k - 1]);
        }
        let bytes = rt.snapshot().unwrap();
        let comps = snap::components(&bytes).unwrap();
        let ks = ksnap::Snap::decode(&comps[kernel_id].1, prefix).unwrap();
        // Each step of a syscall stage in Issue and in Wait; the first step of the others.
        let syscall = matches!(
            &ks.op,
            Some(o) if matches!(
                o.stage,
                ksnap::Stage::Walk { .. } | ksnap::Stage::Output { .. } | ksnap::Stage::Return { .. }
            )
        );
        let key = match &ks.op {
            Some(o) if syscall => (Some(format!("{:?}", o.stage)), o.step, ks.state),
            Some(o) if o.step == 0 => (
                Some(format!("{:?}", std::mem::discriminant(&o.stage))),
                0,
                ks.state,
            ),
            Some(o) => (
                Some(format!("{:?}", std::mem::discriminant(&o.stage))),
                1,
                0,
            ),
            None => (None, 0, ks.state),
        };
        let boundary = stages.last() != Some(&key);
        if boundary {
            stages.push(key);
        }
        let mut fresh = build(&files, layout);
        fresh.restore(&bytes).unwrap();
        assert_eq!(fresh.snapshot().unwrap(), bytes, "checkpoint {k}");
        if boundary || k % STRIDE == 0 || k + WINDOW >= total {
            let records = if k == 0 { 0 } else { full.marks[k - 1] };
            fresh
                .resume_trace(Trace {
                    header: full.trace.header.clone(),
                    records: full.trace.records[..records].to_vec(),
                })
                .unwrap();
            let mut rest = Vec::new();
            while let Some(e) = fresh.step().unwrap() {
                rest.push(e);
                assert!(
                    k + rest.len() <= MAX_EVENTS,
                    "checkpoint {k}: the run does not end"
                );
            }
            assert_eq!(fresh.fault(), None, "checkpoint {k}");
            assert!(rest == full.events[k..], "checkpoint {k}");
            assert_eq!(fresh.snapshot().unwrap(), last, "checkpoint {k}");
            assert_eq!(fresh.take_trace().unwrap(), full.trace, "checkpoint {k}");
            assert_eq!(
                (fresh.state_digest().unwrap(), fresh.execution_digest()),
                digests,
                "checkpoint {k}"
            );
            if syscall {
                syscall_runs += 1;
            }
        } else {
            for i in 0..WINDOW {
                let e = fresh.step().unwrap().unwrap();
                assert!(e == full.events[k + i], "checkpoint {k}, event {i}");
            }
        }
    }
    assert_eq!(rt.snapshot().unwrap(), last);
    // Four writes of one byte (walk 2 PTEs, 1 chunk read, 1 UART byte, a0, sepc) and the
    // returns of two getpids and two yields' exits: every step in Issue and Wait.
    assert!(syscall_runs >= 4 * 6 * 2, "{syscall_runs}");
}
