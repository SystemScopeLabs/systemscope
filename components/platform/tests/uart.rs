//! `SimpleUart` registers, faults, output order, and snapshots (`docs/m1-design.md`
//! §7.3), driven directly through a mock context, then behind an `AddressBus` in a real
//! runtime with no CPU.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{MockCtx, Script, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{Component, ComponentId, Delivered};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceOrigin, Value};
use systemscope_platform::uart::{INSPECT_TAIL, PORT, SIZE, STATUS, TX, TX_KIND};
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, SimpleUart, UartConfig};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: ClockDomainId(0),
    k: 2,
};

fn uart() -> SimpleUart {
    SimpleUart::new(UartConfig { latency: LATENCY })
}

/// Delivers a request in `Transfer` and returns the response the UART sends, which must be
/// on its port, after the latency, in `Complete`.
fn serve_traced(uart: &mut SimpleUart, msg: MemMsg) -> (MemMsg, Vec<common::Traced>) {
    let mut ctx = MockCtx::new(Phase::Transfer);
    ctx.deliver(uart, PORT, msg).unwrap();
    let sent = ctx.take_one();
    assert_eq!(
        (sent.port, sent.when, sent.phase),
        (
            PORT,
            ScheduleWhen::Cycles {
                domain: ClockDomainId(0),
                k: 2
            },
            Phase::Complete
        )
    );
    (sent.msg, ctx.traced)
}

fn serve(uart: &mut SimpleUart, msg: MemMsg) -> MemMsg {
    serve_traced(uart, msg).0
}

fn tx(uart: &mut SimpleUart, byte: u8) {
    assert_eq!(serve(uart, write(0, TX, &[byte])), done(0));
}

fn done(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
}

fn data(txn: u64, data: &[u8]) -> MemMsg {
    MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Data {
            data: data.to_vec(),
        },
    }
}

/// The `AccessFault` response to a request.
fn fault(msg: &MemMsg) -> MemMsg {
    let fault = MemFault::AccessFault;
    match msg {
        MemMsg::ReadReq { txn, .. } => MemMsg::ReadResp {
            txn: *txn,
            outcome: ReadOutcome::Fault { fault },
        },
        MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp {
            txn: *txn,
            outcome: WriteOutcome::Fault { fault },
        },
        _ => unreachable!(),
    }
}

/// The canonical snapshot of a UART with `LATENCY` and this output.
fn canonical(output: &[u8]) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u8(1);
    w.u32(0);
    w.u64(2);
    w.bytes(output);
    w.into_bytes()
}

// ---------------------------------------------------------------------------------------
// Registers.

#[test]
fn a_one_byte_tx_write_appends_the_byte_and_traces_it() {
    let mut u = uart();
    assert!(u.output().is_empty());
    let (resp, traced) = serve_traced(&mut u, write(7, TX, b"H"));
    assert_eq!(resp, done(7));
    assert_eq!(u.output(), b"H");
    assert_eq!(traced, [(TX_KIND, vec![("byte", Value::U64(0x48))])]);
    assert_eq!(TX_KIND, "platform.uart.tx");
    for &b in b"i!" {
        tx(&mut u, b);
    }
    assert_eq!(u.output(), b"Hi!");
}

#[test]
fn every_byte_value_is_kept_raw() {
    let mut u = uart();
    for b in 0..=255u8 {
        tx(&mut u, b);
    }
    let all: Vec<u8> = (0..=255).collect();
    assert_eq!(u.output(), all);
}

#[test]
fn status_reads_as_little_endian_one_at_widths_1_2_and_4() {
    let mut u = uart();
    let (resp, traced) = serve_traced(&mut u, read(1, STATUS, 1));
    assert_eq!(resp, data(1, &[0x01]));
    assert!(traced.is_empty());
    assert_eq!(serve(&mut u, read(2, STATUS, 2)), data(2, &[0x01, 0x00]));
    assert_eq!(
        serve(&mut u, read(4, STATUS, 4)),
        data(4, &[0x01, 0x00, 0x00, 0x00])
    );
    // STATUS does not depend on the output.
    tx(&mut u, b'x');
    assert_eq!(
        serve(&mut u, read(5, STATUS, 4)),
        data(5, &[0x01, 0x00, 0x00, 0x00])
    );
    assert_eq!(u.output(), b"x");
}

#[test]
fn every_other_access_faults_and_changes_nothing() {
    let mut u = uart();
    tx(&mut u, b'A');
    let mut bad = vec![
        // TX is write-only, one byte.
        read(1, TX, 1),
        read(2, TX, 2),
        read(3, TX, 4),
        write(4, TX, &[1, 2]),
        write(5, TX, &[1, 2, 3, 4]),
        write(6, TX, &[1, 2, 3, 4, 5, 6, 7, 8]),
        // STATUS is read-only, 1, 2, or 4 bytes.
        write(7, STATUS, &[1]),
        write(8, STATUS, &[1, 0]),
        write(9, STATUS, &[1, 0, 0, 0]),
        read(10, STATUS, 3),
        read(11, STATUS, 8),
        // Crossing from a register into reserved bytes, or from TX into STATUS.
        read(12, 0x3, 2),
        read(13, 0x0, 8),
        write(14, 0x3, &[1, 2]),
        // Address overflow: the last byte would lie past u64::MAX.
        read(15, u64::MAX, 2),
        write(16, u64::MAX, &[1, 2]),
        read(17, u64::MAX, 1),
        write(18, u64::MAX, &[1]),
        // Far outside the window.
        read(19, 0x100, 1),
        write(20, 0x1000_0000, &[1]),
    ];
    // Reserved offsets 1-3 and 5 and above, at every width.
    let mut txn = 100;
    for offset in [1, 2, 3, 5, 6, 7, 8, 9, 12] {
        for len in [1u32, 2, 4] {
            bad.push(read(txn, offset, len));
            bad.push(write(txn + 1, offset, &vec![0x5a; len as usize]));
            txn += 2;
        }
    }
    for msg in &bad {
        let before = snapshot_of(&u);
        let (resp, traced) = serve_traced(&mut u, msg.clone());
        assert_eq!(resp, fault(msg), "{msg:?}");
        assert!(traced.is_empty(), "{msg:?}");
        assert_eq!(snapshot_of(&u), before, "{msg:?}");
    }
    assert_eq!(u.output(), b"A");
}

#[test]
fn protocol_violations_fault_the_session() {
    let mut u = uart();
    let mut ctx = MockCtx::new(Phase::Transfer);
    for msg in [read(1, TX, 0), write(2, TX, &[]), read(3, STATUS, 0)] {
        assert!(matches!(
            ctx.deliver(&mut u, PORT, msg),
            Err(SimError::ComponentFault(_))
        ));
    }
    for msg in [done(4), data(5, &[1])] {
        assert!(matches!(
            ctx.deliver(&mut u, PORT, msg),
            Err(SimError::ComponentFault(_))
        ));
    }
    assert!(ctx.sent.is_empty());
    assert!(u.output().is_empty());
}

#[test]
fn latency_is_the_configured_one() {
    let d = Duration::from_ns(5);
    let mut u = SimpleUart::new(UartConfig {
        latency: LinkLatency::After(d),
    });
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver(&mut u, PORT, write(1, TX, b"z")).unwrap();
    let sent = ctx.take_one();
    assert_eq!(sent.when, ScheduleWhen::After(d));
    assert_eq!(sent.phase, Phase::Complete);
}

#[test]
fn the_window_is_eight_bytes() {
    assert_eq!((TX, STATUS, SIZE), (0x0, 0x4, 8));
    assert_eq!(uart().type_name(), "platform.uart");
    let ports = uart().ports();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].name, "mem");
}

// ---------------------------------------------------------------------------------------
// Inspect and snapshots.

fn field(view: &StateView, name: &str) -> Value {
    view.get(name).unwrap().clone()
}

#[test]
fn inspect_shows_the_length_and_a_bounded_tail() {
    let mut u = uart();
    assert_eq!(field(&u.inspect(), "tx_len"), Value::U64(0));
    assert_eq!(field(&u.inspect(), "tx_tail"), Value::Bytes(Vec::new()));
    tx(&mut u, 1);
    tx(&mut u, 2);
    assert_eq!(field(&u.inspect(), "tx_len"), Value::U64(2));
    assert_eq!(field(&u.inspect(), "tx_tail"), Value::Bytes(vec![1, 2]));
    for i in 0..200u32 {
        tx(&mut u, i as u8);
    }
    assert_eq!(field(&u.inspect(), "tx_len"), Value::U64(202));
    assert_eq!(
        field(&u.inspect(), "tx_tail"),
        Value::Bytes(u.output()[202 - INSPECT_TAIL..].to_vec())
    );
    assert_eq!(u.inspect().fields.len(), 2);
}

#[test]
fn the_snapshot_is_the_latency_and_the_output() {
    let mut u = uart();
    assert_eq!(snapshot_of(&u), canonical(&[]));
    tx(&mut u, b'A');
    tx(&mut u, 0);
    tx(&mut u, 0xff);
    assert_eq!(snapshot_of(&u), canonical(&[b'A', 0, 0xff]));
}

/// Write A, write B, snapshot, write C, restore: the output is exactly "AB".
#[test]
fn restore_drops_the_suffix_and_never_duplicates() {
    let mut u = uart();
    tx(&mut u, b'A');
    tx(&mut u, b'B');
    let snap = snapshot_of(&u);
    tx(&mut u, b'C');
    assert_eq!(u.output(), b"ABC");
    restore_into(&mut u, &snap).unwrap();
    assert_eq!(u.output(), b"AB");
    // Restoring twice, or into a fresh UART, gives the same.
    restore_into(&mut u, &snap).unwrap();
    assert_eq!(u.output(), b"AB");
    let mut fresh = uart();
    restore_into(&mut fresh, &snap).unwrap();
    assert_eq!(fresh.output(), b"AB");
    assert_eq!(snapshot_of(&fresh), snap);
}

#[test]
fn restore_rejects_a_different_latency_and_bad_bytes() {
    let snap = snapshot_of(&uart());
    let mut other = SimpleUart::new(UartConfig {
        latency: LinkLatency::Cycles {
            domain: ClockDomainId(0),
            k: 3,
        },
    });
    assert_eq!(
        restore_into(&mut other, &snap),
        Err(RestoreError::InvalidState(
            "uart: snapshot has a different latency"
        ))
    );
    let mut u = uart();
    tx(&mut u, b'k');
    assert!(restore_into(&mut u, &snap[..snap.len() - 1]).is_err());
    let mut long = canonical(b"x");
    long.push(0);
    assert!(restore_into(&mut u, &long).is_err());
}

// ---------------------------------------------------------------------------------------
// Properties.

/// A random request: often a one-byte TX write, otherwise anything non-empty near the
/// window.
fn request() -> impl Strategy<Value = MemMsg> {
    prop_oneof![
        3 => any::<u8>().prop_map(|b| write(0, TX, &[b])),
        1 => (0u64..12, 1u32..9).prop_map(|(addr, len)| read(0, addr, len)),
        1 => (0u64..12, proptest::collection::vec(any::<u8>(), 1..9))
            .prop_map(|(addr, data)| write(0, addr, &data)),
    ]
}

/// The output a sequence of requests should produce: the data of every one-byte write at
/// offset 0, in order.
fn model(requests: &[MemMsg]) -> Vec<u8> {
    requests
        .iter()
        .filter_map(|m| match m {
            MemMsg::WriteReq { addr: 0, data, .. } if data.len() == 1 => Some(data[0]),
            _ => None,
        })
        .collect()
}

fn apply(u: &mut SimpleUart, requests: &[MemMsg]) {
    for msg in requests {
        let before = u.output().len();
        let resp = serve(u, msg.clone());
        let appended = u.output().len() - before;
        match resp {
            MemMsg::WriteResp {
                outcome: WriteOutcome::Done,
                ..
            } => assert_eq!(appended, 1),
            _ => assert_eq!(appended, 0, "{msg:?}"),
        }
    }
}

proptest! {
    #[test]
    fn output_is_every_tx_byte_in_order(
        bytes in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        let mut u = uart();
        for &b in &bytes {
            tx(&mut u, b);
        }
        prop_assert_eq!(u.output(), bytes.as_slice());
        prop_assert_eq!(snapshot_of(&u), canonical(&bytes));
    }

    #[test]
    fn faulting_requests_never_touch_the_output(
        requests in proptest::collection::vec(request(), 0..100),
    ) {
        let mut u = uart();
        apply(&mut u, &requests);
        let expected = model(&requests);
        prop_assert_eq!(u.output(), expected.as_slice());
    }

    #[test]
    fn restore_brings_back_exactly_the_prefix(
        prefix in proptest::collection::vec(request(), 0..60),
        suffix in proptest::collection::vec(request(), 0..60),
    ) {
        let mut u = uart();
        apply(&mut u, &prefix);
        let snap = snapshot_of(&u);
        apply(&mut u, &suffix);
        let mut all = prefix.clone();
        all.extend(suffix);
        let expected = model(&all);
        prop_assert_eq!(u.output(), expected.as_slice());
        restore_into(&mut u, &snap).unwrap();
        let expected = model(&prefix);
        prop_assert_eq!(u.output(), expected.as_slice());
        prop_assert_eq!(snapshot_of(&u), snap);
    }
}

// ---------------------------------------------------------------------------------------
// Behind the address bus, in a runtime.

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: u64 = 0x1000;
const UART_BASE: u64 = 0x1000_0000;

const SCRIPT: ComponentId = ComponentId(0);
const BUS: ComponentId = ComponentId(1);
const UART: ComponentId = ComponentId(3);

/// Records the UART's `inspect()` after every event.
struct Watch(Rc<RefCell<Vec<StateView>>>);

impl Observer for Watch {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0.borrow_mut().push(world.inspect(UART).unwrap());
        Control::Continue
    }
}

/// Script → bus → {ram, uart}, on one 1 GHz clock, with the §9 memory map.
fn build(requests: &[(u64, MemMsg)]) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
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
            name: "uart",
            base: UART_BASE,
            size: SIZE,
        },
    ])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = Ram::new(
        RamConfig {
            size: RAM_SIZE,
            latency: cycles(1),
        },
        &RamImage {
            image_hash: [0xaa; 32],
            segments: Vec::new(),
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let uart = t.add_component(
        "soc.uart",
        Box::new(SimpleUart::new(UartConfig { latency: cycles(0) })),
    );
    assert_eq!((script, bus, uart), (SCRIPT, BUS, UART));
    t.connect((script, "mem"), (bus, "cpu"), Some(cycles(1)));
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
    t.connect((bus, "uart"), (uart, "mem"), Some(cycles(1)));
    t.elaborate(SessionConfig::default()).unwrap()
}

fn mem(ev: &Dispatched) -> &MemMsg {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } => msg,
        other => panic!("expected a mem.v1 message, got {other:?}"),
    }
}

fn traffic() -> Vec<(u64, MemMsg)> {
    vec![
        (0, write(1, UART_BASE, b"H")),
        (1, write(2, RAM_BASE + 8, &[9, 8, 7, 6])),
        (2, write(3, UART_BASE + TX, b"i")),
        (3, read(4, UART_BASE + STATUS, 4)),
        (4, write(5, UART_BASE + 1, b"x")),
        (5, write(6, UART_BASE + SIZE, b"y")),
        (6, read(7, RAM_BASE + 8, 4)),
        (7, read(8, UART_BASE + STATUS, 1)),
        (8, write(9, UART_BASE, &[0x00])),
        (9, write(10, UART_BASE, &[0xff])),
    ]
}

#[test]
fn absolute_addresses_reach_the_uart_as_offsets() {
    let mut rt = build(&traffic());
    let views = Rc::new(RefCell::new(Vec::new()));
    rt.add_observer(Box::new(Watch(Rc::clone(&views))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);

    // The UART sees offsets 0 and 4, never absolute addresses; the bus faults the request
    // past the window itself.
    let at_uart: Vec<&MemMsg> = events
        .iter()
        .filter(|ev| ev.target == UART)
        .map(mem)
        .collect();
    assert_eq!(
        at_uart,
        [
            &write(1, 0, b"H"),
            &write(3, 0, b"i"),
            &read(4, 4, 4),
            &write(5, 1, b"x"),
            &read(8, 4, 1),
            &write(9, 0, &[0x00]),
            &write(10, 0, &[0xff]),
        ]
    );
    let mut got: Vec<MemMsg> = events
        .iter()
        .filter(|ev| ev.target == SCRIPT)
        .map(|ev| mem(ev).clone())
        .collect();
    got.sort_by_key(|m| match m {
        MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => *txn,
        _ => unreachable!(),
    });
    let traffic = traffic();
    assert_eq!(
        got,
        [
            done(1),
            done(2),
            done(3),
            data(4, &[1, 0, 0, 0]),
            fault(&traffic[4].1),
            fault(&traffic[5].1),
            data(7, &[9, 8, 7, 6]),
            data(8, &[1]),
            done(9),
            done(10),
        ]
    );

    let last = views.borrow().last().unwrap().clone();
    assert_eq!(field(&last, "tx_len"), Value::U64(4));
    assert_eq!(
        field(&last, "tx_tail"),
        Value::Bytes(vec![b'H', b'i', 0x00, 0xff])
    );
    let bytes: Vec<Value> = rt
        .take_trace()
        .unwrap()
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == UART)
        .map(|r| {
            assert_eq!(r.kind, TX_KIND);
            r.fields[0].1.clone()
        })
        .collect();
    assert_eq!(
        bytes,
        [b'H', b'i', 0x00, 0xff].map(|b| Value::U64(u64::from(b)))
    );
}

/// A checkpoint just after the UART accepted `H`, with its `WriteResp` still in the
/// runtime's queue, resumes with one `H` and one response, not two.
#[test]
fn a_restore_with_a_pending_response_never_repeats_the_byte() {
    let requests = vec![(0, write(1, UART_BASE, b"H"))];
    let mut rt = build(&requests);
    rt.init().unwrap();
    let mut k = 0;
    loop {
        let ev = rt.step().unwrap().unwrap();
        k += 1;
        if ev.target == UART {
            break;
        }
    }
    let bytes = rt.snapshot().unwrap();
    let rest: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();

    let mut fresh = build(&requests);
    let views = Rc::new(RefCell::new(Vec::new()));
    fresh.add_observer(Box::new(Watch(Rc::clone(&views))));
    fresh.restore(&bytes).unwrap();
    let resumed: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
    assert_eq!(fresh.fault(), None);
    assert_eq!(resumed, rest);
    assert!(k > 0);
    assert!(resumed.iter().all(|ev| ev.target != UART));
    let responses: Vec<_> = resumed
        .iter()
        .filter(|ev| ev.target == SCRIPT)
        .map(mem)
        .collect();
    assert_eq!(responses, [&done(1)]);
    for view in views.borrow().iter() {
        assert_eq!(field(view, "tx_len"), Value::U64(1));
        assert_eq!(field(view, "tx_tail"), Value::Bytes(b"H".to_vec()));
    }
    assert_eq!(fresh.state_digest().unwrap(), rt.state_digest().unwrap());
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let reference = {
        let mut rt = build(&traffic());
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (
            events.len(),
            rt.state_digest().unwrap(),
            rt.execution_digest(),
        )
    };
    for k in 0..=reference.0 {
        let mut rt = build(&traffic());
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let mut fresh = build(&traffic());
        fresh.restore(&bytes).unwrap();
        while fresh.step().unwrap().is_some() {}
        assert_eq!(fresh.fault(), None);
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.1, reference.2),
            "checkpoint after {k} events"
        );
    }
}
