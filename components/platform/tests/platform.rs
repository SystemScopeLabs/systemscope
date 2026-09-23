//! The address bus and RAM in a real runtime (`docs/m1-design.md` §7.1, §7.2): ordering
//! semantics, bus faults in the trace, and checkpoint/restore at every event boundary.
//!
//! A stateless test script drives the bus. Its responses are read back from the runtime's
//! dispatch records, so no test-only CPU model exists.

mod common;

use common::{Script, read, write};
use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::event::Phase;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::bus::FAULT_KIND;
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, Segment};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;

const P: u64 = 4096;
/// Main memory: four pages, with a gap before `ROM`.
const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: u64 = 4 * P;
/// A small second RAM, with an image, far below main memory.
const ROM_BASE: u64 = 0x2000;
const ROM_SIZE: u64 = 0x100;
/// Unmapped.
const GAP: u64 = 0x4000_0000;

const SCRIPT: ComponentId = ComponentId(0);
const BUS: ComponentId = ComponentId(1);

/// Script → bus → {ram, rom}, on one 1 GHz clock. The RAM answers after `ram_cycles`.
fn build(requests: &[(u64, MemMsg)], ram_cycles: u64) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let script = t.add_component(
        "soc.script",
        Box::new(Script {
            clock,
            requests: requests.to_vec(),
        }),
    );
    let bus = AddressBus::new(vec![
        Region {
            name: "ram",
            base: RAM_BASE,
            size: RAM_SIZE,
        },
        Region {
            name: "rom",
            base: ROM_BASE,
            size: ROM_SIZE,
        },
    ])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = Ram::new(
        RamConfig {
            size: RAM_SIZE,
            latency: cycles(clock, ram_cycles),
        },
        &RamImage {
            image_hash: [0xaa; 32],
            // Page 1 starts non-zero and page 2 holds one byte.
            segments: vec![
                Segment {
                    offset: P,
                    bytes: vec![0x5a; 16],
                },
                Segment {
                    offset: 2 * P + 9,
                    bytes: vec![0x77],
                },
            ],
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let rom = Ram::new(
        RamConfig {
            size: ROM_SIZE,
            latency: cycles(clock, 1),
        },
        &RamImage {
            image_hash: [0xbb; 32],
            segments: vec![Segment {
                offset: 0,
                bytes: vec![1, 2, 3, 4],
            }],
        },
    )
    .unwrap();
    let rom = t.add_component("soc.rom", Box::new(rom));
    assert_eq!((script, bus), (SCRIPT, BUS));
    t.connect((script, "mem"), (bus, "cpu"), Some(cycles(clock, 1)));
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(clock, 1)));
    t.connect((bus, "rom"), (rom, "mem"), None);
    t.elaborate(SessionConfig::default()).unwrap()
}

fn cycles(domain: ClockDomainId, k: u64) -> LinkLatency {
    LinkLatency::Cycles { domain, k }
}

fn run(rt: &mut Runtime) -> Vec<Dispatched> {
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    events
}

/// The `mem.v1` message of a dispatched event.
fn mem(ev: &Dispatched) -> &MemMsg {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } => msg,
        other => panic!("expected a mem.v1 message, got {other:?}"),
    }
}

/// The responses the script received, in arrival order.
fn responses(events: &[Dispatched]) -> Vec<MemMsg> {
    events
        .iter()
        .filter(|ev| ev.target == SCRIPT)
        .map(|ev| mem(ev).clone())
        .collect()
}

fn data(txn: u64, data: &[u8]) -> MemMsg {
    MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Data {
            data: data.to_vec(),
        },
    }
}

fn done(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
}

fn fault(msg: &MemMsg) -> MemMsg {
    let fault = MemFault::AccessFault;
    match *msg {
        MemMsg::ReadReq { txn, .. } => MemMsg::ReadResp {
            txn,
            outcome: ReadOutcome::Fault { fault },
        },
        MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp {
            txn,
            outcome: WriteOutcome::Fault { fault },
        },
        _ => unreachable!(),
    }
}

/// The position of the first event matching `f`.
fn position(events: &[Dispatched], f: impl Fn(&Dispatched) -> bool) -> usize {
    events.iter().position(f).expect("no such event")
}

fn is(ev: &Dispatched, target: ComponentId, msg: &MemMsg) -> bool {
    ev.target == target && mem(ev) == msg
}

// ---------------------------------------------------------------------------------------
// Ordering semantics.

/// A read samples memory when the RAM accepts it; a write accepted before the read's
/// response is not visible in that response.
#[test]
fn reads_sample_memory_at_acceptance() {
    let a = read(1, RAM_BASE + P, 4);
    let b = write(2, RAM_BASE + P, &[0xb0; 4]);
    let events = run(&mut build(&[(0, a), (1, b)], 6));
    let ram = ComponentId(2);
    // The write reaches the RAM before the read's response leaves it.
    let write_accepted = position(&events, |ev| {
        ev.target == ram && matches!(mem(ev), MemMsg::WriteReq { .. })
    });
    let read_answered = position(&events, |ev| is(ev, BUS, &data(1, &[0x5a; 4])));
    assert!(write_accepted < read_answered);
    assert_eq!(responses(&events), [data(1, &[0x5a; 4]), done(2)]);
}

/// A write is visible from acceptance, before its response is delivered.
#[test]
fn writes_are_visible_from_acceptance() {
    let w = write(1, RAM_BASE + P - 2, &[0xc1, 0xc2, 0xc3, 0xc4]);
    let r = read(2, RAM_BASE + P - 2, 4);
    let events = run(&mut build(&[(0, w), (1, r)], 6));
    let expected = data(2, &[0xc1, 0xc2, 0xc3, 0xc4]);
    let ram = ComponentId(2);
    let read_accepted = position(&events, |ev| {
        ev.target == ram && matches!(mem(ev), MemMsg::ReadReq { .. })
    });
    let write_answered = position(&events, |ev| is(ev, SCRIPT, &done(1)));
    assert!(read_accepted < write_answered);
    assert_eq!(responses(&events), [done(1), expected]);
}

// ---------------------------------------------------------------------------------------
// Faults and tracing.

#[test]
fn bus_faults_are_answered_in_complete_and_traced() {
    let bad = [
        read(10, GAP, 4),
        write(11, RAM_BASE + RAM_SIZE - 2, &[1, 2, 3]),
        read(12, u64::MAX, 2),
    ];
    let requests: Vec<_> = bad.iter().cloned().map(|m| (0, m)).collect();
    let mut rt = build(&requests, 2);
    rt.start_trace().unwrap();
    let events = run(&mut rt);
    let trace = rt.take_trace().unwrap();

    // Each request is answered by the bus itself, in the tick it arrived, in `Complete`.
    for msg in &bad {
        let arrived = &events[position(&events, |ev| is(ev, BUS, msg))];
        let answered = &events[position(&events, |ev| is(ev, SCRIPT, &fault(msg)))];
        assert_eq!(arrived.key.phase, Phase::Request);
        assert_eq!(answered.source, BUS);
        assert_eq!(answered.key.phase, Phase::Complete);
    }
    // Nothing reached either memory.
    assert!(
        events
            .iter()
            .all(|ev| ev.target == SCRIPT || ev.target == BUS)
    );

    let records: Vec<_> = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component)
        .collect();
    let expected = [
        (10, GAP, 4),
        (11, RAM_BASE + RAM_SIZE - 2, 3),
        (12, u64::MAX, 2),
    ];
    assert_eq!(records.len(), expected.len());
    for (record, (txn, addr, len)) in records.into_iter().zip(expected) {
        assert_eq!(record.kind, FAULT_KIND);
        assert_eq!(record.kind, "platform.bus.fault");
        assert_eq!(record.component, BUS);
        let TraceAt::Event(key) = record.at else {
            panic!("fault traced outside an event");
        };
        assert_eq!(key.phase, Phase::Request);
        assert_eq!(
            record.fields,
            [
                ("txn", Value::U64(txn)),
                ("addr", Value::U64(addr)),
                ("len", Value::U64(len)),
            ]
        );
    }
}

#[test]
fn normal_traffic_traces_nothing_but_dispatches() {
    let requests = [
        (0, write(1, RAM_BASE, &[1; 8])),
        (0, read(2, ROM_BASE, 4)),
        (1, read(3, RAM_BASE + 4, 8)),
    ];
    let mut rt = build(&requests, 2);
    rt.start_trace().unwrap();
    let events = run(&mut rt);
    let trace = rt.take_trace().unwrap();
    assert!(
        trace
            .records
            .iter()
            .all(|r| r.origin == TraceOrigin::Runtime && r.kind == "runtime.dispatch")
    );
    // One dispatch record per event.
    assert_eq!(trace.records.len(), events.len());
    let mut got = responses(&events);
    got.sort_by_key(|m| match m {
        MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => *txn,
        _ => unreachable!(),
    });
    assert_eq!(
        got,
        [
            done(1),
            data(2, &[1, 2, 3, 4]),
            data(3, &[1, 1, 1, 1, 0, 0, 0, 0])
        ]
    );
}

// ---------------------------------------------------------------------------------------
// Checkpoints.

/// Traffic that exercises every piece of snapshot state: overlapping outstanding txns on
/// both regions, cross-page writes, an image page zeroed so the snapshot omits it, pages
/// allocated and freed, bus faults, and reads that observe all of it.
fn traffic() -> Vec<(u64, MemMsg)> {
    vec![
        (0, read(1, RAM_BASE + P, 16)),
        (0, write(2, RAM_BASE + P, &[0; 16])),
        (0, read(3, ROM_BASE, 4)),
        (1, write(4, RAM_BASE + P - 3, &[9, 8, 7, 6, 5, 4])),
        (1, read(5, GAP, 1)),
        (2, write(6, ROM_BASE + 2, &[0, 0])),
        (2, read(7, RAM_BASE + P - 3, 6)),
        (3, write(8, RAM_BASE + 3 * P, &[0xee; 4])),
        (3, write(9, RAM_BASE + 2 * P + 9, &[0])),
        (4, read(10, RAM_BASE + 2 * P, 16)),
        (4, write(11, RAM_BASE + 3 * P, &[0; 4])),
        (5, read(12, RAM_BASE + P, 16)),
        (5, write(13, RAM_BASE + RAM_SIZE - 1, &[1, 2])),
        (6, read(14, ROM_BASE, 4)),
        (6, read(15, RAM_BASE + 3 * P, 4)),
    ]
}

/// The end state of a run.
#[derive(Debug, PartialEq, Eq)]
struct End {
    state: [u8; 32],
    execution: [u8; 32],
    trace_bytes: Vec<u8>,
    trace: [u8; 32],
}

fn end(mut rt: Runtime) -> End {
    let trace = rt.take_trace().unwrap();
    End {
        state: rt.state_digest().unwrap(),
        execution: rt.execution_digest(),
        trace_bytes: trace.canonical_bytes(),
        trace: trace.digest(),
    }
}

fn reference() -> (End, Vec<Dispatched>) {
    let mut rt = build(&traffic(), 3);
    rt.start_trace().unwrap();
    let events = run(&mut rt);
    (end(rt), events)
}

/// Stops after `k` events, then continues in a freshly elaborated runtime.
fn resumed(k: usize) -> End {
    let (snapshot, prefix): (Vec<u8>, Trace) = {
        let mut rt = build(&traffic(), 3);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        (rt.snapshot().unwrap(), rt.take_trace().unwrap())
    };
    let mut rt = build(&traffic(), 3);
    rt.restore(&snapshot).unwrap();
    rt.resume_trace(prefix).unwrap();
    while rt.step().unwrap().is_some() {}
    assert_eq!(rt.fault(), None);
    end(rt)
}

#[test]
fn the_traffic_behaves_as_expected() {
    let (_, events) = reference();
    let mut got = responses(&events);
    got.sort_by_key(|m| match m {
        MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => *txn,
        _ => unreachable!(),
    });
    let traffic = traffic();
    assert_eq!(
        got,
        [
            data(1, &[0x5a; 16]),
            done(2),
            data(3, &[1, 2, 3, 4]),
            done(4),
            fault(&traffic[4].1),
            done(6),
            data(7, &[9, 8, 7, 6, 5, 4]),
            done(8),
            done(9),
            data(10, &[0; 16]),
            done(11),
            data(12, &[6, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            fault(&traffic[12].1),
            data(14, &[1, 2, 0, 0]),
            data(15, &[0; 4]),
        ]
    );
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let (expected, events) = reference();
    assert!(events.len() > 40, "{} events", events.len());
    for k in 0..=events.len() {
        assert_eq!(resumed(k), expected, "checkpoint after {k} events");
    }
}

/// A checkpoint whose snapshot holds a forwarded request not yet answered (an outstanding
/// bus txn) and a RAM response still in the queue, taken after page 1 of the image was
/// zeroed.
#[test]
fn a_checkpoint_can_hold_outstanding_txns_and_pending_responses() {
    let (_, events) = reference();
    // Just after the RAM accepted the zeroing write (txn 2): its response is in the queue
    // and the bus still has it outstanding.
    let ram = ComponentId(2);
    let k = 1 + position(&events, |ev| {
        ev.target == ram && matches!(mem(ev), MemMsg::WriteReq { txn: TxnId(2), .. })
    });
    let mut rt = build(&traffic(), 3);
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    let bytes = rt.snapshot().unwrap();
    let mut fresh = build(&traffic(), 3);
    fresh.restore(&bytes).unwrap();
    assert_eq!(fresh.state_digest().unwrap(), rt.state_digest().unwrap());
    let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
    assert!(rest.iter().any(|ev| is(ev, BUS, &done(2))));
    // The zeroed image page stays zero after restore, though the fresh RAM's image
    // reloaded it.
    assert!(rest.iter().any(|ev| is(
        ev,
        SCRIPT,
        &data(12, &[6, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    )));
}
