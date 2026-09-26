//! The kernel gate in the real runtime (`docs/m3-design.md` §6.3, §17 M3.4a): the M3 CPU,
//! the three-master bus, the RAM, the UART, the block controller, and `ModeledKernel` on
//! the minimal platform of `common::platform`.
//!
//! - The S-mode probe stores to `kgate.ENTER`; the tests pin the exact path: the store's
//!   request, the kernel's accesses and their txns, the held response, and the CPU's
//!   resumption at the next instruction.
//! - The U-mode `ecall` loop round-trips through the trampoline and the scripted
//!   operation with every register preserved.
//! - Every event boundary of both is a checkpoint that resumes identically in a fresh
//!   platform, including every held-`ENTER` state.
//! - A kernel access to `kgate` faults the session before anything is sent, and the
//!   builder refuses the platform that would make one.

mod common;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use common::layout::*;
use common::platform::{self, BuildError, MASTER_DMA, MASTER_KERNEL, Program, id};
use common::{Mem, oracle, snap};
use systemscope_contracts::component::{Delivered, PortId};
use systemscope_contracts::error::SimError;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemMsg, WriteOutcome};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_os::kernel::{ENTER_KIND, GATE_PORT, MEM_PORT, RELEASE_KIND, SHUTDOWN_KIND};
use systemscope_os::{KernelConfig, Window};
use systemscope_platform::mmbus::GRANT_KIND;
use systemscope_platform::uart::TX_KIND;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::{COMMIT_KIND, EXCEPTION_KIND};

fn config() -> KernelConfig {
    // The builder replaces the clock with the platform's.
    common::layout::config(ClockDomainId(0))
}

/// The CPU's and the kernel's views after every event, and the trace records so far.
#[derive(Default)]
struct Log {
    views: Vec<(StateView, StateView)>,
    records: usize,
    marks: Vec<usize>,
}

struct Recorder(Rc<RefCell<Log>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let mut log = self.0.borrow_mut();
        log.views.push((
            world.inspect(id::CPU).unwrap(),
            world.inspect(id::KERNEL).unwrap(),
        ));
        let n = log.records;
        log.marks.push(n);
        Control::Continue
    }

    fn on_trace(&mut self, _: &TraceRecord) {
        self.0.borrow_mut().records += 1;
    }
}

/// A checkpoint: the snapshot after some events, and the trace records by then.
struct Point {
    bytes: Vec<u8>,
    records: usize,
}

struct Run {
    rt: Runtime,
    events: Vec<Dispatched>,
    views: Vec<(StateView, StateView)>,
    points: Vec<Point>,
    trace: Trace,
}

/// More events than any scenario here takes: a run past it is looping.
const MAX_EVENTS: usize = 20_000;

/// Runs `program` to the end on `rt`, checkpointing after every event if `points`.
fn drive(mut rt: Runtime, points: bool) -> Run {
    let log = Rc::new(RefCell::new(Log::default()));
    rt.add_observer(Box::new(Recorder(Rc::clone(&log))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let mut saved = Vec::new();
    let mut save = |rt: &Runtime, records| {
        if points {
            saved.push(Point {
                bytes: rt.snapshot().unwrap(),
                records,
            });
        }
    };
    save(&rt, log.borrow().records);
    let mut events = Vec::new();
    while let Some(e) = rt.step().unwrap() {
        events.push(e);
        assert!(events.len() <= MAX_EVENTS, "the run does not end");
        let records = *log.borrow().marks.last().unwrap();
        save(&rt, records);
    }
    assert_eq!(rt.fault(), None);
    let trace = rt.take_trace().unwrap();
    let views = std::mem::take(&mut log.borrow_mut().views);
    Run {
        rt,
        events,
        views,
        points: saved,
        trace,
    }
}

fn run(program: &Program) -> Run {
    drive(platform::build(program, config()).unwrap(), false)
}

fn u(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn s(view: &StateView, name: &str) -> String {
    match view.get(name) {
        Some(Value::Str(v)) => v.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

fn field<'a>(r: &'a TraceRecord, name: &str) -> &'a Value {
    &r.fields.iter().find(|(k, _)| *k == name).unwrap().1
}

fn field_u(r: &TraceRecord, name: &str) -> u64 {
    match field(r, name) {
        Value::U64(v) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn mem_msg(ev: &Dispatched) -> Option<(PortId, &MemMsg)> {
    match &ev.delivery {
        Delivered::Message {
            port,
            msg: Message::MemV1(m),
        } => Some((*port, m)),
        _ => None,
    }
}

/// A request the kernel sent: `(txn, write, addr, bytes)`, a read's bytes as zeros.
fn kernel_request(ev: &Dispatched) -> Option<(u64, bool, u64, Vec<u8>)> {
    if ev.source != id::KERNEL || ev.target != id::BUS {
        return None;
    }
    match mem_msg(ev)?.1 {
        MemMsg::ReadReq { txn, addr, len } => Some((txn.0, false, *addr, vec![0; *len as usize])),
        MemMsg::WriteReq { txn, addr, data } => Some((txn.0, true, *addr, data.clone())),
        _ => None,
    }
}

/// Is `ev` the bus delivering a request to the kernel's `gate` port?
fn is_gate_request(ev: &Dispatched) -> bool {
    ev.target == id::KERNEL
        && matches!(
            mem_msg(ev),
            Some((GATE_PORT, MemMsg::WriteReq { .. } | MemMsg::ReadReq { .. }))
        )
}

/// Is `ev` the kernel's response to the held entry arriving at the bus?
fn is_release(ev: &Dispatched) -> bool {
    ev.source == id::KERNEL
        && ev.target == id::BUS
        && matches!(mem_msg(ev), Some((_, MemMsg::WriteResp { .. })))
}

/// Is `ev` the bus delivering a store response to the CPU?
fn is_cpu_write_response(ev: &Dispatched) -> bool {
    ev.target == id::CPU && matches!(mem_msg(ev), Some((_, MemMsg::WriteResp { .. })))
}

impl Run {
    fn cpu(&self) -> &StateView {
        &self.views.last().unwrap().0
    }

    fn kernel(&self) -> &StateView {
        &self.views.last().unwrap().1
    }

    fn records(&self, kind: &str) -> Vec<&TraceRecord> {
        self.trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.kind == kind)
            .collect()
    }

    /// The index in the trace of the first component record of `kind`.
    fn first(&self, kind: &str) -> usize {
        self.trace
            .records
            .iter()
            .position(|r| r.origin == TraceOrigin::Component && r.kind == kind)
            .unwrap()
    }

    fn uart(&self) -> Vec<u8> {
        self.records(TX_KIND)
            .iter()
            .map(|r| field_u(r, "byte") as u8)
            .collect()
    }

    fn ram(&self, addr: u64, len: usize) -> Vec<u8> {
        let components = snap::components(&self.rt.snapshot().unwrap()).unwrap();
        snap::ram_read(&components[id::RAM.0 as usize].1, addr, len).unwrap()
    }

    fn ram_words(&self, addr: u64, words: usize) -> Vec<u32> {
        self.ram(addr, 4 * words)
            .chunks(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    fn requests(&self) -> Vec<(u64, bool, u64, Vec<u8>)> {
        self.events.iter().filter_map(kernel_request).collect()
    }
}

/// The initial trap frame of the probes as a byte memory.
fn probe_memory() -> Mem {
    let mut mem = Mem::new();
    for (i, w) in platform::probe_frame().iter().enumerate() {
        for (k, b) in w.to_le_bytes().into_iter().enumerate() {
            mem.insert(u64::from(TRAP_FRAME) + 4 * i as u64 + k as u64, b);
        }
    }
    mem
}

/// The probe's frame after the scripted operation: `sepc + 4`.
fn resumed_frame() -> Vec<u32> {
    let mut words = platform::probe_frame();
    words[31] += 4;
    words
}

#[test]
fn the_probe_enters_the_kernel_and_resumes_at_the_next_instruction() {
    let program = platform::probe(TRAP_FRAME, false);
    let r = run(&program);
    let enter_pc = u64::from(platform::probe_enter_pc(false));

    // The kernel's accesses: the oracle's, in order, with txns 0, 1, 2, ...
    let mut mem = probe_memory();
    let expected = oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut mem);
    assert_eq!(expected.len(), 21, "10 reads, 10 writes, one UART byte");
    let requests = r.requests();
    let got: Vec<_> = requests
        .iter()
        .map(|(_, w, a, b)| (*w, *a, b.clone()))
        .collect();
    assert_eq!(got, expected);
    let txns: Vec<u64> = requests.iter().map(|(t, ..)| *t).collect();
    assert_eq!(txns, (0..21).collect::<Vec<_>>());
    assert_eq!(u(r.kernel(), "next_txn"), 21);
    // Every kernel request is granted as master kernel0 with its own txn.
    let grants: Vec<u64> = r
        .records(GRANT_KIND)
        .iter()
        .filter(|g| field_u(g, "master") == MASTER_KERNEL)
        .map(|g| field_u(g, "txn"))
        .collect();
    assert_eq!(grants, (0..21).collect::<Vec<_>>());

    // One ENTER reached the gate: 4 bytes at offset 0 carrying the frame address.
    let enters: Vec<usize> = (0..r.events.len())
        .filter(|&k| is_gate_request(&r.events[k]))
        .collect();
    assert_eq!(enters.len(), 1);
    let enter = enters[0];
    let Some((
        _,
        MemMsg::WriteReq {
            txn: held,
            addr,
            data,
        },
    )) = mem_msg(&r.events[enter])
    else {
        panic!("ENTER is a write");
    };
    assert_eq!(*addr, 0);
    assert_eq!(data, &TRAP_FRAME.to_le_bytes());
    // The bus's downstream txn is what the kernel holds and what the grant reports.
    let kgate_grant = r
        .records(GRANT_KIND)
        .into_iter()
        .find(|g| field_u(g, "region") == platform::REGION_KGATE)
        .unwrap();
    assert_eq!(field_u(kgate_grant, "downstream_txn"), held.0);
    assert_eq!(field_u(kgate_grant, "master"), platform::MASTER_CPU);

    // Exactly one response to it, after the kernel's last response, with Done.
    let releases: Vec<usize> = (0..r.events.len())
        .filter(|&k| is_release(&r.events[k]))
        .collect();
    assert_eq!(releases.len(), 1);
    let release = releases[0];
    assert_eq!(
        mem_msg(&r.events[release]).unwrap().1,
        &MemMsg::WriteResp {
            txn: *held,
            outcome: WriteOutcome::Done
        }
    );
    let last_kernel_response = (0..r.events.len())
        .rfind(|&k| {
            r.events[k].target == id::KERNEL
                && mem_msg(&r.events[k]).is_some_and(|(p, _)| p == MEM_PORT)
        })
        .unwrap();
    assert!(enter < last_kernel_response && last_kernel_response < release);
    let resumed = (release..r.events.len())
        .find(|&k| is_cpu_write_response(&r.events[k]))
        .unwrap();

    // While held: the CPU waits on its store and retires nothing; the kernel holds txn.
    let instret = u(&r.views[enter].0, "instret");
    for k in enter..resumed {
        let (cpu, kernel) = &r.views[k];
        assert_eq!(s(cpu, "state"), "mem_wait", "after event {k}");
        assert_eq!(u(cpu, "pc"), enter_pc, "after event {k}");
        assert_eq!(u(cpu, "instret"), instret, "after event {k}");
        if k < last_kernel_response {
            assert_eq!(s(kernel, "held_txn"), held.0.to_string(), "after event {k}");
        }
    }
    // No CPU request reaches the bus while ENTER is held.
    assert!(
        r.events[enter..resumed]
            .iter()
            .all(|e| !(e.source == id::CPU && e.target == id::BUS))
    );
    assert_eq!(s(&r.views[last_kernel_response].1, "phase"), "idle");
    assert_eq!(s(&r.views[last_kernel_response].1, "held_txn"), "none");
    let retired = (resumed..r.views.len())
        .find(|&k| u(&r.views[k].0, "instret") != instret)
        .unwrap();
    assert_eq!(
        u(&r.views[retired].0, "instret"),
        instret + 1,
        "the store retires once"
    );
    let store_commits = r
        .records(COMMIT_KIND)
        .iter()
        .filter(|c| field_u(c, "pc") == enter_pc)
        .count();
    assert_eq!(store_commits, 1);

    // The guest's progression: the stub, then every S instruction but the final ecall,
    // each retired once, in order.
    let stub = program.segments[0].1.len() as u64 / 4;
    let s_words = program.segments[1].1.len() as u64 / 4;
    let expected_pcs: Vec<u64> = (0..stub)
        .map(|i| u64::from(platform::STUB) + 4 * i)
        .chain((0..s_words - 1).map(|i| u64::from(platform::S_START) + 4 * i))
        .collect();
    let commits = r.records(COMMIT_KIND);
    let pcs: Vec<u64> = commits.iter().map(|c| field_u(c, "pc")).collect();
    assert_eq!(pcs, expected_pcs);
    assert_eq!(u(r.cpu(), "instret"), stub + s_words - 1);
    let after = commits
        .iter()
        .find(|c| field_u(c, "pc") == enter_pc + 4)
        .unwrap();
    assert_eq!(field_u(after, "rd"), 12);
    assert_eq!(field_u(after, "rd_value"), 0x55);

    // Trace order: enter, the UART byte, release, then the ENTER store retires.
    let enter_at = r.first(ENTER_KIND);
    let tx_at = r.first(TX_KIND);
    let release_at = r.first(RELEASE_KIND);
    let store_at = r
        .trace
        .records
        .iter()
        .position(|c| c.kind == COMMIT_KIND && field_u(c, "pc") == enter_pc)
        .unwrap();
    assert!(enter_at < tx_at && tx_at < release_at && release_at < store_at);
    let enter_record = r.records(ENTER_KIND)[0];
    assert_eq!(field_u(enter_record, "txn"), held.0);
    assert_eq!(field_u(enter_record, "value"), u64::from(TRAP_FRAME));
    assert_eq!(field(enter_record, "op"), &Value::Str("script".to_owned()));
    assert_eq!(field_u(r.records(RELEASE_KIND)[0], "txn"), held.0);
    assert!(r.records(SHUTDOWN_KIND).is_empty());
    assert!(
        r.records(EXCEPTION_KIND).is_empty(),
        "the store never traps"
    );

    // The final state.
    let cpu = r.cpu();
    assert_eq!(s(cpu, "halt"), "trap");
    assert_eq!(s(cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!(u(cpu, "x12"), 0x55, "the instruction after ENTER ran");
    assert_eq!(u(cpu, "x13"), 0, "action = Resume");
    assert_eq!(u(cpu, "x11"), 0, "reason");
    assert_eq!(r.uart(), b"k");
    assert_eq!(s(r.kernel(), "phase"), "idle");
    assert_eq!(s(r.kernel(), "held_txn"), "none");
    assert_eq!(r.ram_words(u64::from(TRAP_FRAME), 38), resumed_frame());
}

#[test]
fn the_probe_is_deterministic() {
    let program = platform::probe(TRAP_FRAME, true);
    let (a, b) = (run(&program), run(&program));
    assert!(a.events == b.events);
    assert_eq!(a.trace, b.trace);
    assert_eq!(a.rt.state_digest().unwrap(), b.rt.state_digest().unwrap());
    assert_eq!(a.rt.execution_digest(), b.rt.execution_digest());
}

#[test]
fn the_kernel_and_the_dma_engine_share_the_ram_deterministically() {
    let r = run(&platform::probe(TRAP_FRAME, true));
    // Same kernel accesses and txns as without DMA.
    let mut mem = probe_memory();
    let expected = oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut mem);
    let got: Vec<_> = r
        .requests()
        .into_iter()
        .map(|(_, w, a, b)| (w, a, b))
        .collect();
    assert_eq!(got, expected);
    // RAM grants to the two masters interleave while the entry is held.
    let masters: Vec<u64> = r
        .records(GRANT_KIND)
        .iter()
        .filter(|g| field_u(g, "region") == platform::REGION_RAM)
        .map(|g| field_u(g, "master"))
        .filter(|&m| m != platform::MASTER_CPU)
        .collect();
    let switches = masters.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        masters.contains(&MASTER_DMA) && switches >= 2,
        "kernel and DMA grants interleave: {masters:?}"
    );
    // Both finish: the DMA wrote the two blocks, the kernel returned past ENTER.
    let disk = platform::disk();
    assert_eq!(r.ram(STAGING, 1024), disk[..1024]);
    assert_eq!(r.ram_words(u64::from(TRAP_FRAME), 38), resumed_frame());
    let cpu = r.cpu();
    assert_eq!(s(cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!((u(cpu, "x12"), u(cpu, "x13")), (0x55, 0));
    assert_eq!(r.uart(), b"k");
}

#[test]
fn a_bad_enter_value_shuts_down() {
    let r = run(&platform::probe(TRAP_FRAME + 4, false));
    let mut mem = probe_memory();
    let expected = oracle::shutdown(u64::from(TRAP_FRAME), &mut mem);
    let got: Vec<_> = r
        .requests()
        .into_iter()
        .map(|(_, w, a, b)| (w, a, b))
        .collect();
    assert_eq!(got, expected);
    assert_eq!(r.records(SHUTDOWN_KIND).len(), 1);
    assert_eq!(r.records(RELEASE_KIND).len(), 1);
    assert!(r.uart().is_empty());
    let cpu = r.cpu();
    assert_eq!(s(cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!(u(cpu, "x13"), 1, "action = Shutdown");
    assert_eq!(u(cpu, "x11"), 1, "reason");
    let frame = r.ram_words(u64::from(TRAP_FRAME), 38);
    assert_eq!(frame[..36], platform::probe_frame()[..36]);
    assert_eq!(frame[36..], [1, 1]);
}

/// Machine code that accesses `kgate` with `access` (on `t1 = KGATE_BASE`).
fn machine_access(access: u32) -> Program {
    use platform::asm::*;
    let mut w = Vec::new();
    w.extend(li(T0, TRAP_FRAME));
    w.extend(li(T1, KGATE_BASE as u32));
    w.push(access);
    platform::machine(&w)
}

#[test]
fn a_malformed_gate_access_is_an_access_fault_and_enters_nothing() {
    use platform::asm::*;
    for (access, cause) in [
        (sb(T0, T1, 0), "StoreAccessFault"),
        (sw(T0, T1, 4), "StoreAccessFault"),
        (lw(T2, T1, 0), "LoadAccessFault"),
        (lw(T2, T1, 4), "LoadAccessFault"),
    ] {
        let r = run(&machine_access(access));
        assert_eq!(s(r.cpu(), "halt"), "trap", "{access:#x}");
        assert_eq!(s(r.cpu(), "cause"), cause, "{access:#x}");
        assert!(r.requests().is_empty());
        assert!(r.records(ENTER_KIND).is_empty());
        assert_eq!(s(r.kernel(), "phase"), "idle");
        assert_eq!(s(r.kernel(), "held_txn"), "none");
    }
    // The well-formed store from M enters and resumes like any other.
    let r = run(&machine_access(sw(T0, T1, 0)));
    assert_eq!(s(r.cpu(), "cause"), "EnvironmentCallFromM");
    assert_eq!(r.records(RELEASE_KIND).len(), 1);
    assert_eq!(r.uart(), b"k");
}

/// Runs the platform `elaborate` builds until the session faults, returning the error.
fn run_to_fault(program: &Program, config: KernelConfig) -> (SimError, Vec<Dispatched>) {
    let mut rt = platform::elaborate(program, config).unwrap();
    rt.init().unwrap();
    let mut events = Vec::new();
    loop {
        match rt.step() {
            Ok(Some(e)) => events.push(e),
            Ok(None) => panic!("the session ended without a fault"),
            Err(_) => return (rt.fault().unwrap(), events),
        }
    }
}

#[test]
fn a_kernel_access_to_kgate_faults_the_session_before_it_is_sent() {
    // A platform whose UART grant is the kgate window: the builder refuses it...
    let bad = KernelConfig {
        uart_tx: KGATE_BASE,
        ..config()
    };
    let program = platform::probe(TRAP_FRAME, false);
    assert_eq!(
        platform::build(&program, bad).err(),
        Some(BuildError::GateGranted(Window {
            base: KGATE_BASE,
            size: 1
        }))
    );
    // ...and built anyway, the kernel's whitelist faults the session at that access.
    let (fault, events) = run_to_fault(&program, bad);
    let SimError::ComponentFault(message) = &fault else {
        panic!("{fault:?}");
    };
    assert!(message.contains("whitelist"), "{message}");
    let requests: Vec<_> = events.iter().filter_map(kernel_request).collect();
    assert_eq!(
        requests.len(),
        20,
        "every frame access, but not the UART byte"
    );
    assert!(requests.iter().all(|(_, _, a, _)| *a != KGATE_BASE));
    assert!(
        !events.iter().any(is_release),
        "the held entry is never released"
    );
}

#[test]
fn the_builder_refuses_every_kgate_misconfiguration() {
    let program = platform::probe(TRAP_FRAME, false);
    let moved = KernelConfig {
        gate: Window {
            base: KGATE_BASE + 8,
            size: KGATE_SIZE,
        },
        ..config()
    };
    assert_eq!(
        platform::build(&program, moved).err(),
        Some(BuildError::GateMismatch)
    );
    let blk = KernelConfig {
        blk: Window {
            base: BLK_BASE,
            size: KGATE_BASE + 4 - BLK_BASE,
        },
        ..config()
    };
    assert!(matches!(
        platform::build(&program, blk).err(),
        Some(BuildError::GateGranted(_))
    ));
}

#[test]
fn a_faulted_kernel_access_faults_the_session() {
    // UART TX at an address no region maps: the bus answers AccessFault.
    let unmapped = KernelConfig {
        uart_tx: UART_BASE + 0x100,
        ..config()
    };
    let program = platform::probe(TRAP_FRAME, false);
    let (fault, events) = run_to_fault(&program, unmapped);
    assert!(matches!(fault, SimError::ComponentFault(_)), "{fault:?}");
    assert_eq!(events.iter().filter_map(kernel_request).count(), 21);
    assert!(!events.iter().any(is_release));
}

#[test]
fn a_user_ecall_loop_round_trips_with_every_register_preserved() {
    const ECALLS: u32 = 3;
    let r = run(&platform::ecall_loop(ECALLS));
    // The ebreak ends the run through the trampoline's shutdown.
    let cpu = r.cpu();
    assert_eq!(s(cpu, "halt"), "trap");
    assert_eq!(s(cpu, "cause"), "EnvironmentCallFromS");
    assert_eq!(u(cpu, "x11"), 0, "reason");
    // Every ecall and the ebreak entered the kernel once and was released once.
    assert_eq!(r.records(ENTER_KIND).len(), ECALLS as usize + 1);
    assert_eq!(r.records(RELEASE_KIND).len(), ECALLS as usize + 1);
    assert_eq!(r.uart(), vec![b'k'; ECALLS as usize + 1]);
    let txns: Vec<u64> = r.requests().iter().map(|(t, ..)| *t).collect();
    assert_eq!(txns, (0..21 * (u64::from(ECALLS) + 1)).collect::<Vec<_>>());
    // Every trap is from U; none is an exception from S.
    let exceptions = r.records(EXCEPTION_KIND);
    assert_eq!(exceptions.len(), ECALLS as usize + 1);
    assert!(
        exceptions
            .iter()
            .all(|e| field(e, "from") == &Value::Str("U".to_owned()))
    );
    // The frame at the ebreak: every U register as U left it, the loop counters at
    // their end, sepc the ebreak's plus 4 (the kernel's), scause 3.
    let frame = r.ram_words(u64::from(TRAP_FRAME), 38);
    for reg in 1..=31u32 {
        let want = match reg {
            8 | 9 => ECALLS,
            _ => platform::user_value(reg),
        };
        assert_eq!(frame[reg as usize - 1], want, "x{reg}");
    }
    assert_eq!(frame[31], platform::user_ebreak_pc() + 4);
    assert_eq!(frame[33], 3);
    // The ecalls returned past themselves: each trap pc is the one ecall, then ebreak.
    let pcs: Vec<u64> = exceptions.iter().map(|e| field_u(e, "pc")).collect();
    let ecall_pc = u64::from(platform::user_ebreak_pc()) - 12;
    let mut want = vec![ecall_pc; ECALLS as usize];
    want.push(u64::from(platform::user_ebreak_pc()));
    assert_eq!(pcs, want);
}

/// Where a checkpoint falls relative to the kernel entry it may be in.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Boundary {
    /// No entry in progress, before the first.
    BeforeEntry,
    /// The ENTER store is on its way through the bus.
    EnterInFlight,
    /// Held, the kernel waking to issue step `n`.
    Issue(u64),
    /// Held, step `n` sent; `at_bus` once the bus has it.
    Wait { step: u64, at_bus: bool },
    /// Released: the response is on its way to the bus.
    ReleaseToBus,
    /// Released: the bus has it, the CPU does not.
    ReleaseToCpu,
    /// No entry in progress, after at least one.
    AfterEntry,
}

/// The boundary after `k` events of `r`.
fn boundary(r: &Run, k: usize) -> Boundary {
    let (done, rest) = r.events.split_at(k);
    if k > 0 {
        let kernel = &r.views[k - 1].1;
        match s(kernel, "phase").as_str() {
            "issue" => return Boundary::Issue(u(kernel, "step")),
            "wait" => {
                let txn: u64 = s(kernel, "mem_txn").parse().unwrap();
                let at_bus = done
                    .iter()
                    .any(|e| kernel_request(e).is_some_and(|(t, ..)| t == txn));
                return Boundary::Wait {
                    step: u(kernel, "step"),
                    at_bus,
                };
            }
            _ => {}
        }
    }
    let entries = done.iter().filter(|e| is_gate_request(e)).count();
    let released = done.iter().filter(|e| is_release(e)).count();
    if entries > released {
        return Boundary::ReleaseToBus;
    }
    let waiting = k > 0 && s(&r.views[k - 1].0, "state") == "mem_wait";
    let next_response = rest.iter().position(is_cpu_write_response);
    if let (true, Some(p)) = (waiting, next_response) {
        // The CPU waits on this store alone: nothing else of its own is in flight.
        let only_store = rest[..p].iter().all(|e| {
            e.target != id::CPU
                && !(e.source == id::CPU && matches!(mem_msg(e), Some((_, MemMsg::ReadReq { .. }))))
        });
        if only_store {
            if rest[..p].iter().any(is_gate_request) {
                return Boundary::EnterInFlight;
            }
            let store = done.iter().rposition(|e| {
                e.source == id::CPU && matches!(mem_msg(e), Some((_, MemMsg::WriteReq { .. })))
            });
            let gate = done.iter().rposition(is_gate_request);
            if let (Some(a), Some(b)) = (store, gate)
                && b > a
            {
                return Boundary::ReleaseToCpu;
            }
        }
    }
    if entries == 0 {
        Boundary::BeforeEntry
    } else {
        Boundary::AfterEntry
    }
}

/// Runs `program` checkpointing after every event, restores every checkpoint into a
/// fresh platform, and requires the rest of the run, the trace, the final snapshot, and
/// both digests to equal the uninterrupted run's, and the restored queue to be the
/// checkpoint's: nothing is reissued, lost, or answered twice. Returns the boundaries
/// the checkpoints fell on.
fn resume_everywhere(program: &Program) -> BTreeSet<Boundary> {
    let r = drive(platform::build(program, config()).unwrap(), true);
    let last = r.rt.snapshot().unwrap();
    let digests = (r.rt.state_digest().unwrap(), r.rt.execution_digest());
    let mut seen = BTreeSet::new();
    for (k, point) in r.points.iter().enumerate() {
        seen.insert(boundary(&r, k));
        let mut fresh = platform::build(program, config()).unwrap();
        fresh.restore(&point.bytes).unwrap();
        assert_eq!(
            fresh.snapshot().unwrap(),
            point.bytes,
            "checkpoint after {k}"
        );
        fresh
            .resume_trace(Trace {
                header: r.trace.header.clone(),
                records: r.trace.records[..point.records].to_vec(),
            })
            .unwrap();
        let rest: Vec<Dispatched> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None, "checkpoint after {k}");
        assert!(rest == r.events[k..], "checkpoint after {k}");
        assert_eq!(fresh.snapshot().unwrap(), last, "checkpoint after {k}");
        assert_eq!(fresh.take_trace().unwrap(), r.trace, "checkpoint after {k}");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            digests,
            "checkpoint after {k}"
        );
    }
    seen
}

#[test]
fn every_event_of_the_probe_resumes_identically() {
    let seen = resume_everywhere(&platform::probe(TRAP_FRAME, false));
    let mut want = vec![
        Boundary::BeforeEntry,
        Boundary::EnterInFlight,
        Boundary::ReleaseToBus,
        Boundary::ReleaseToCpu,
        Boundary::AfterEntry,
    ];
    for step in 0..21 {
        want.push(Boundary::Issue(step));
        want.push(Boundary::Wait {
            step,
            at_bus: false,
        });
        want.push(Boundary::Wait { step, at_bus: true });
    }
    for b in want {
        assert!(seen.contains(&b), "no checkpoint at {b:?}: {seen:?}");
    }
}

#[test]
fn every_event_of_the_contended_probe_resumes_identically() {
    let seen = resume_everywhere(&platform::probe(TRAP_FRAME, true));
    assert!(seen.contains(&Boundary::ReleaseToCpu));
    assert!(seen.contains(&Boundary::Wait {
        step: 20,
        at_bus: true
    }));
}

#[test]
fn every_event_of_the_user_ecall_loop_resumes_identically() {
    let seen = resume_everywhere(&platform::ecall_loop(1));
    assert!(seen.contains(&Boundary::EnterInFlight));
    assert!(seen.contains(&Boundary::ReleaseToCpu));
    assert!(seen.contains(&Boundary::AfterEntry));
}
