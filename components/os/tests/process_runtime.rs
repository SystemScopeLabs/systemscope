//! The process model on the real platform (`docs/m3-design.md` §6.4, §6.6, §7.3, §8.3,
//! §17 M3.4b): the M3 CPU, the three-master bus, the RAM, and `ModeledKernel` with a
//! process plan, booted by the §7.3 trampoline from staged user images.
//!
//! - User programs check their own state after every switch and end with `ebreak`: a
//!   failed check executes an illegal instruction instead, so each process's fault cause
//!   is its verdict, `Breakpoint` for a pass.
//! - Same-VA isolation, the text, megapage, and stack protections, and the switch go
//!   through the CPU's own Sv32 walk and `satp`, not the kernel's view.
//! - The real toolchain fixture `user.elf` runs as two processes.
//! - Every event boundary of a two-process run restores into a fresh platform and
//!   continues identically.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::layout::*;
use common::platform::asm::*;
use common::platform::{self, id};
use common::procs::{Kind, PageMem, ksnap, plan_in, staged_at, two_segment, walk};
use common::snap;
use systemscope_contracts::component::Delivered;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_os::kernel::{ENTER_KIND, RELEASE_KIND, SHUTDOWN_KIND};
use systemscope_os::procop::{CREATE_KIND, FAULT_KIND, SWITCH_KIND};
use systemscope_os::{KernelConfig, ProcessPlan, UserLayout};
use systemscope_platform::uart::TX_KIND;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::EXCEPTION_KIND;

const SP: u32 = 2;
const STACK_TOP: u32 = 0x7FFF_F000;
const DATA: u32 = 0x0001_1000;
const POOL_PPN: u32 = (POOL / 4096) as u32;
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

/// `beq a, b, +8` over an illegal instruction: continue if `a == b`, else fault.
fn check(w: &mut Vec<u32>, a: u32, b: u32) {
    w.push(beq(a, b, 8));
    w.push(ILLEGAL);
}

/// A worker: stores `marker` to its data page and its stack, yields, checks that its
/// data, stack, registers, file bytes, and `.bss` are its own, yields again, and ends
/// with `ebreak`. Its data segment is `[0, tag]` then 64 zero bytes.
fn worker(tag: u32, marker: u32) -> Vec<u8> {
    let mut w = Vec::new();
    w.extend(li(T1, DATA));
    w.extend(li(T2, marker));
    w.push(sw(T2, T1, 0));
    w.push(addi(A0, 0, tag as i32));
    w.extend(li(T5, STACK_TOP));
    check(&mut w, SP, T5);
    w.push(sw(T2, SP, -4));
    w.push(ECALL);
    w.push(lw(T3, T1, 0));
    check(&mut w, T3, T2);
    w.push(addi(T4, 0, tag as i32));
    check(&mut w, A0, T4);
    w.push(lw(T3, SP, -4));
    check(&mut w, T3, T2);
    w.push(lw(T3, T1, 4));
    check(&mut w, T3, T4);
    w.push(lw(T3, T1, 16));
    check(&mut w, T3, 0);
    w.push(ECALL);
    w.push(EBREAK);
    let data: Vec<u8> = [0u32, tag].iter().flat_map(|x| x.to_le_bytes()).collect();
    two_segment(&w, &data, 64)
}

/// A program of `words` then `ebreak`, with a small data segment.
fn program(words: &[u32]) -> Vec<u8> {
    let mut w = words.to_vec();
    w.push(EBREAK);
    two_segment(&w, &[1, 2, 3, 4], 0)
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
}

struct Recorder(Rc<RefCell<Log>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let mut log = self.0.borrow_mut();
        log.cpu = Some(world.inspect(id::CPU).unwrap());
        log.kernel = Some(world.inspect(id::KERNEL).unwrap());
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
    /// The trace records by the end of each event.
    marks: Vec<usize>,
    cpu: StateView,
    kernel: StateView,
    trace: Trace,
}

/// Runs to the end, or while `more` says so.
fn drive(mut rt: Runtime, mut more: impl FnMut(&[Dispatched]) -> bool) -> Run {
    let log = Rc::new(RefCell::new(Log::default()));
    rt.add_observer(Box::new(Recorder(Rc::clone(&log))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let mut events = Vec::new();
    while more(&events) {
        let Some(e) = rt.step().unwrap() else { break };
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
        trace,
    }
}

/// The kernel's snapshot bytes in a freshly initialised platform, before boot.
fn fresh_kernel(files: &[Vec<u8>], layout: UserLayout) -> Vec<u8> {
    let mut rt = build(files, layout);
    rt.init().unwrap();
    snap::components(&rt.snapshot().unwrap()).unwrap()[id::KERNEL.0 as usize]
        .1
        .clone()
}

fn run(files: &[Vec<u8>], layout: UserLayout) -> Run {
    drive(build(files, layout), |_| true)
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

fn fs(r: &TraceRecord, name: &str) -> String {
    match field(r, name) {
        Value::Str(v) => v.clone(),
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

impl Run {
    fn records(&self, kind: &str) -> Vec<&TraceRecord> {
        self.trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.kind == kind)
            .collect()
    }

    fn switches(&self) -> Vec<(u64, u64)> {
        self.records(SWITCH_KIND)
            .iter()
            .map(|r| (fu(r, "from"), fu(r, "to")))
            .collect()
    }

    /// `(pid, cause, epc, tval)` of every fault.
    fn faults(&self) -> Vec<(u64, String, u64, u64)> {
        self.records(FAULT_KIND)
            .iter()
            .map(|r| (fu(r, "pid"), fs(r, "cause"), fu(r, "epc"), fu(r, "tval")))
            .collect()
    }

    fn roots(&self) -> Vec<u32> {
        self.records(CREATE_KIND)
            .iter()
            .map(|r| fu(r, "root") as u32)
            .collect()
    }

    fn ram(&self, addr: u64, len: usize) -> Vec<u8> {
        let components = snap::components(&self.rt.snapshot().unwrap()).unwrap();
        snap::ram_read(&components[id::RAM.0 as usize].1, addr, len).unwrap()
    }

    fn ram_word(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.ram(addr, 4).try_into().unwrap())
    }

    /// The first `frames` pool frames as a page memory, for the independent walk.
    fn pool(&self, frames: usize) -> PageMem {
        let mut m = PageMem::default();
        m.write(POOL, &self.ram(POOL, 4096 * frames));
        m
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

/// The run ended by the kernel's shutdown: the trampoline's `SRST` from S with `reason`.
fn assert_shut_down(r: &Run, reason: u64) {
    assert_eq!(vs(&r.cpu, "halt"), "trap");
    assert_eq!(vs(&r.cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!(vs(&r.cpu, "x11"), reason.to_string());
    let s = r.records(SHUTDOWN_KIND);
    assert_eq!(s.len(), 1);
    assert_eq!(fu(s[0], "reason"), reason);
}

/// Every trap reached the kernel from U, and every kernel access stayed in its grants.
fn assert_clean(r: &Run, pool_frames: u64) {
    for e in r.records(EXCEPTION_KIND) {
        assert_eq!(field(e, "from"), &Value::Str("U".to_owned()), "{e:?}");
    }
    let reqs = r.kernel_requests();
    let txns: Vec<u64> = reqs.iter().map(|q| q.0).collect();
    assert_eq!(txns, (0..reqs.len() as u64).collect::<Vec<_>>());
    let frame = u64::from(TRAP_FRAME)..u64::from(TRAP_FRAME) + 0x98;
    for &(_, write, addr, len) in &reqs {
        let end = addr + len as u64;
        let in_frame = frame.contains(&addr) && end <= frame.end;
        if write {
            assert!(
                in_frame || (addr >= POOL && end <= POOL + 4096 * pool_frames),
                "{addr:#x}"
            );
        } else {
            assert!(
                in_frame || (addr >= STAGING && end <= STAGING + STAGING_SIZE),
                "{addr:#x}"
            );
        }
    }
    assert_eq!(r.records(ENTER_KIND).len(), r.records(RELEASE_KIND).len());
}

fn worker_ebreak(tag: u32, marker: u32) -> u32 {
    // The ebreak is the last text word; the text is at the start of the file's segment.
    let file = worker(tag, marker);
    let plan = plan_in(std::slice::from_ref(&file), small());
    let copy = plan.images[0].image.segments[0].pages[0].copy.unwrap();
    0x0001_0000 + copy.len - 4
}

#[test]
fn two_processes_switch_a_b_a_on_the_real_cpu() {
    let files = [worker(0xA, 0xAAAA), worker(0xB, 0xBBBB)];
    let r = run(&files, UserLayout::M3);
    assert_eq!(
        r.switches(),
        [(0, 1), (1, 2), (2, 1), (1, 2), (2, 1), (1, 2)]
    );
    let eb = u64::from(worker_ebreak(0xA, 0xAAAA));
    assert_eq!(
        r.faults(),
        [
            (1, "Breakpoint".to_owned(), eb, eb),
            (2, "Breakpoint".to_owned(), eb, eb)
        ],
        "each process saw only its own data, stack, and registers"
    );
    assert_shut_down(&r, 1);
    assert_clean(&r, 3072);
    assert_eq!(vs(&r.kernel, "free_frames"), "3072");
    assert_eq!(
        vs(&r.kernel, "processes"),
        format!(
            "1:faulted(0x3,{eb:#x},{eb:#x}):{:#x};2:faulted(0x3,{eb:#x},{eb:#x}):{:#x}",
            POOL_PPN,
            POOL_PPN + 9
        )
    );
    // Same VA, different frames, by an independent walk of the final RAM: each data
    // page holds its own marker, and the frames are disjoint.
    let roots = r.roots();
    assert_eq!(roots, [POOL_PPN, POOL_PPN + 9]);
    let mem = r.pool(20);
    let a = walk(&mem, roots[0], DATA, Kind::Store).unwrap();
    let b = walk(&mem, roots[1], DATA, Kind::Store).unwrap();
    assert_ne!(a >> 12, b >> 12);
    assert_eq!((mem.word(a), mem.word(b)), (0xAAAA, 0xBBBB));
    assert_eq!((mem.word(a + 4), mem.word(b + 4)), (0xA, 0xB));
    let sa = walk(&mem, roots[0], STACK_TOP - 4, Kind::Load).unwrap();
    let sb = walk(&mem, roots[1], STACK_TOP - 4, Kind::Load).unwrap();
    assert_eq!((mem.word(sa), mem.word(sb)), (0xAAAA, 0xBBBB));
    // Nothing reached the UART.
    assert!(r.records(TX_KIND).is_empty());
}

#[test]
fn three_processes_rotate_fifo() {
    let files = [worker(1, 0x1111), worker(2, 0x2222), worker(3, 0x3333)];
    let r = run(&files, small());
    assert_eq!(
        r.switches(),
        [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 1),
            (1, 2),
            (2, 3),
            (3, 1),
            (1, 2),
            (2, 3)
        ]
    );
    assert!(
        r.faults().iter().all(|f| f.1 == "Breakpoint"),
        "{:?}",
        r.faults()
    );
    assert_shut_down(&r, 1);
    assert_clean(&r, 3072);
}

#[test]
fn a_store_to_text_faults_only_that_process() {
    let mut w = Vec::new();
    w.extend(li(T1, 0x0001_0000));
    w.push(sw(T1, T1, 0));
    let files = [program(&w), worker(0xB, 0xBBBB)];
    let r = run(&files, small());
    let f = r.faults();
    assert_eq!(
        f[0],
        (1, "StorePageFault".to_owned(), 0x0001_0008, 0x0001_0000)
    );
    assert_eq!(f[1].1, "Breakpoint");
    assert_eq!(r.switches(), [(0, 1), (1, 2), (2, 2), (2, 2)]);
    assert_shut_down(&r, 1);
    assert_clean(&r, 3072);
}

#[test]
fn user_access_to_the_kernel_and_mmio_megapages_faults() {
    let mut a = Vec::new();
    a.extend(li(T1, RAM_BASE as u32 + 0x10));
    a.push(lw(T2, T1, 0));
    let mut b = Vec::new();
    b.extend(li(T1, UART_BASE as u32));
    b.push(addi(T2, 0, 0x41));
    b.push(sb(T2, T1, 0));
    let mut c = Vec::new();
    c.extend(li(T1, TRAP_FRAME));
    c.push(sw(0, T1, 0));
    let files = [program(&a), program(&b), program(&c)];
    let r = run(&files, small());
    assert_eq!(
        r.faults(),
        [
            (1, "LoadPageFault".to_owned(), 0x0001_0008, RAM_BASE + 0x10),
            (2, "StorePageFault".to_owned(), 0x0001_000C, UART_BASE),
            (
                3,
                "StorePageFault".to_owned(),
                0x0001_0008,
                u64::from(TRAP_FRAME)
            ),
        ]
    );
    assert!(
        r.records(TX_KIND).is_empty(),
        "U-mode never reached the UART"
    );
    assert_shut_down(&r, 1);
    assert_clean(&r, 3072);
}

#[test]
fn the_stack_is_exactly_its_pages() {
    let base = STACK_TOP - 2 * 4096;
    let mut w = Vec::new();
    w.extend(li(T5, STACK_TOP));
    check(&mut w, SP, T5);
    w.push(sw(SP, SP, -4)); // the top word
    w.extend(li(T1, base));
    w.push(sw(SP, T1, 0)); // the lowest stack word
    w.push(lw(T2, T1, 0));
    check(&mut w, T2, SP);
    w.push(lw(T2, T1, -4)); // one word below the stack: a fault
    let layout = UserLayout {
        stack_pages: 2,
        ..UserLayout::M3
    };
    let r = run(&[program(&w)], layout);
    let pc = 0x0001_0000 + 4 * (w.len() as u64 - 1);
    assert_eq!(
        r.faults(),
        [(1, "LoadPageFault".to_owned(), pc, u64::from(base) - 4)]
    );
    // A load at STACK_TOP (just above the stack) faults too.
    let up = vec![lw(T2, SP, 0)];
    let r = run(&[program(&up)], layout);
    assert_eq!(
        r.faults(),
        [(
            1,
            "LoadPageFault".to_owned(),
            0x0001_0000,
            u64::from(STACK_TOP)
        )]
    );
    // And execution of the stack or data (no X) is a fetch fault.
    let mut jump = Vec::new();
    jump.extend(li(T1, DATA));
    jump.push(0x0003_0067); // jalr x0, 0(t1)
    let r = run(&[program(&jump)], layout);
    assert_eq!(
        r.faults(),
        [(
            1,
            "InstructionPageFault".to_owned(),
            u64::from(DATA),
            u64::from(DATA)
        )]
    );
}

/// The `user.elf` fixture, built by the pinned toolchain (M3.1).
const USER_ELF: &[u8] = include_bytes!("../../../elf/tests/fixtures/user/user.elf");

/// The fixture's `sw t1, 0(t2)` after the linker relaxed `la t2, scratch` to
/// `addi t2, gp, -2044`.
const FIXTURE_GP_STORE: u32 = 0x0001_00A4;

#[test]
fn the_real_user_fixture_runs_as_two_processes() {
    // The pinned toolchain relaxed the fixture's `la t2, scratch` against `gp`, which
    // the fixture never sets and the §6.6 initial context leaves 0. Each process
    // therefore loads its .data word through its own pc-relative mapping, then faults
    // on the store to `gp - 2044`: the fault kill, deterministically, per process.
    assert_eq!(
        u32::from_le_bytes(USER_ELF[0xA0..0xA4].try_into().unwrap()),
        0x8041_8393,
        "addi t2, gp, -2044"
    );
    let files = [USER_ELF.to_vec(), USER_ELF.to_vec()];
    let r = run(&files, UserLayout::M3);
    let tval = u64::from(0u32.wrapping_sub(2044));
    let pc = u64::from(FIXTURE_GP_STORE);
    assert_eq!(
        r.faults(),
        [
            (1, "StorePageFault".to_owned(), pc, tval),
            (2, "StorePageFault".to_owned(), pc, tval),
        ]
    );
    assert_eq!(r.switches(), [(0, 1), (1, 2)]);
    assert_shut_down(&r, 1);
    assert_clean(&r, 3072);
    // The last trap frame is process 2's at the fault: the .data word loaded through
    // the real Sv32 walk, t0 its address, t2 the gp-relative address.
    let f = u64::from(TRAP_FRAME);
    assert_eq!(r.ram_word(f + 4 * 4), 0x0001_10E8, "t0");
    assert_eq!(r.ram_word(f + 4 * 5), 0x5353, "t1");
    assert_eq!(u64::from(r.ram_word(f + 4 * 6)), tval, "t2");
    assert_eq!(r.ram_word(f + 4 * 2), 0, "gp");
    assert_eq!(r.ram_word(f + 0x7C), FIXTURE_GP_STORE);
    // Each address space holds the real linker layout: the ELF headers, text, and
    // .rodata in the R+X page, the .data word at its page offset, zero .bss.
    let roots = r.roots();
    let mem = r.pool(20);
    for &root in &roots {
        let text = walk(&mem, root, 0x0001_0000, Kind::Fetch).unwrap();
        assert_eq!(mem.read(text, 0xE5), USER_ELF[..0xE5]);
        assert!(mem.read(text, 0xE5).windows(21).any(|w| w
            == b"hello from user mode
"));
        let counter = walk(&mem, root, 0x0001_10E8, Kind::Store).unwrap();
        assert_eq!(mem.word(counter), 0x5353);
        assert!(mem.read(counter + 4, 64).iter().all(|&b| b == 0), ".bss");
        assert!(mem.read(counter & !0xFFF, 0xE8).iter().all(|&b| b == 0));
    }
    let (a, b) = (
        walk(&mem, roots[0], 0x0001_10E8, Kind::Load).unwrap(),
        walk(&mem, roots[1], 0x0001_10E8, Kind::Load).unwrap(),
    );
    assert_ne!(a >> 12, b >> 12);
}

#[test]
fn the_platform_run_is_deterministic() {
    let files = [worker(0xA, 0xAAAA), worker(0xB, 0xBBBB)];
    let a = run(&files, small());
    let b = run(&files, small());
    assert!(a.events == b.events);
    assert_eq!(a.trace, b.trace);
    assert_eq!(a.rt.snapshot().unwrap(), b.rt.snapshot().unwrap());
    assert_eq!(a.rt.state_digest().unwrap(), b.rt.state_digest().unwrap());
    assert_eq!(a.rt.execution_digest(), b.rt.execution_digest());
}

/// The every-event checkpoint scenario: create A and B, run A, switch to B, switch back
/// to A, both end. After every event the platform is snapshotted and restored into a
/// fresh one, which must snapshot to the same bytes, hold the same kernel metadata
/// (by the independent codec), and dispatch the same next events; on a stride and at
/// every kernel-stage boundary it runs to the end and must match the uninterrupted run
/// exactly: events, trace, final snapshot (RAM included), and both digests.
#[test]
fn every_event_restores_and_continues_identically() {
    const WINDOW: usize = 6;
    const STRIDE: usize = 509;
    let files = [worker(0xA, 0xAAAA), worker(0xB, 0xBBBB)];
    let layout = small();
    let full = run(&files, layout);
    let last = full.rt.snapshot().unwrap();
    let digests = (full.rt.state_digest().unwrap(), full.rt.execution_digest());
    let total = full.events.len();
    assert_eq!(
        full.switches(),
        [(0, 1), (1, 2), (2, 1), (1, 2), (2, 1), (1, 2)]
    );

    // Replay, checkpointing after every event.
    let mut rt = build(&files, layout);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let kernel_id = id::KERNEL.0 as usize;
    let prefix = ksnap::prefix_len(&fresh_kernel(&files, layout), 3072);
    let mut stages = Vec::new();
    let mut full_runs = 0;
    for k in 0..=total {
        if k > 0 {
            let e = rt.step().unwrap().unwrap();
            assert!(e == full.events[k - 1]);
        }
        let bytes = rt.snapshot().unwrap();
        let comps = snap::components(&bytes).unwrap();
        let ks = ksnap::Snap::decode(&comps[kernel_id].1, prefix).unwrap();
        // A new stage, or the Issue and Wait of a stage's first access.
        let key = match &ks.op {
            Some(o) if o.step == 0 => (Some(std::mem::discriminant(&o.stage)), 0, ks.state),
            Some(o) => (Some(std::mem::discriminant(&o.stage)), 1, 0),
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
            let rest: Vec<Dispatched> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
            assert_eq!(fresh.fault(), None, "checkpoint {k}");
            assert!(rest == full.events[k..], "checkpoint {k}");
            assert_eq!(fresh.snapshot().unwrap(), last, "checkpoint {k}");
            assert_eq!(fresh.take_trace().unwrap(), full.trace, "checkpoint {k}");
            assert_eq!(
                (fresh.state_digest().unwrap(), fresh.execution_digest()),
                digests,
                "checkpoint {k}"
            );
            full_runs += 1;
        } else {
            for i in 0..WINDOW {
                let e = fresh.step().unwrap().unwrap();
                assert!(e == full.events[k + i], "checkpoint {k}, event {i}");
            }
        }
    }
    assert_eq!(rt.snapshot().unwrap(), last);
    // Every stage of the process path had a boundary: creation, frame read, dispatch,
    // shutdown, each in Issue and Wait.
    assert!(stages.len() > 20, "{}", stages.len());
    assert!(full_runs > 40, "{full_runs}");
}

#[test]
fn a_real_run_leaves_kernel_metadata_only_in_the_kernel_snapshot() {
    let files = [worker(0xA, 0xAAAA), worker(0xB, 0xBBBB)];
    let layout = small();
    let r = run(&files, layout);
    let comps = snap::components(&r.rt.snapshot().unwrap()).unwrap();
    let kernel = &comps[id::KERNEL.0 as usize].1;
    let fresh = fresh_kernel(&files, layout);
    let s = ksnap::Snap::decode(kernel, ksnap::prefix_len(&fresh, 3072)).unwrap();
    assert_eq!(s.life, 2, "down after the last fault");
    assert!(s.pcbs.iter().all(|p| p.state.0 == 3 && p.tables.is_empty()));
    assert!(s.bitmap.iter().all(|&b| b == 0));
    // The kernel's snapshot is its metadata: no page of user memory.
    assert!(kernel.len() - ksnap::prefix_len(&fresh, 3072) < 1024);
}
