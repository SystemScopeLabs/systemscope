//! `MultiMasterBus` (`docs/m2-design.md` §10): configuration, ports, identity, per-region
//! round-robin arbitration, phases, access faults, session faults, snapshots, inspect, and
//! trace, driven directly through a mock context and checked against an independent
//! arbitration model; then in a real runtime with two scripted masters and RAM targets,
//! for timing, dispatch-order independence, and every-event checkpoints.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{
    MockCtx, Script, SendKind, Sent, Traced, Wake, read, restore_into, snapshot_of, write,
};
use proptest::prelude::*;
use systemscope_contracts::component::Component;
use systemscope_contracts::component::{ComponentId, Delivered, PortId, Role};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::{
    self, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::SnapshotWriter;
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::bus::FAULT_KIND;
use systemscope_platform::mmbus::{GRANT_KIND, SNAPSHOT_SCHEMA};
use systemscope_platform::{
    AddressBus, MultiMasterBus, MultiMasterBusConfig, MultiMasterBusConfigError, Ram, RamConfig,
    RamImage, Region,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;

const CLOCK: ClockDomainId = ClockDomainId(3);

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: u64 = 0x1000;
const UART_BASE: u64 = 0x1000_0000;
const UART_SIZE: u64 = 0x8;

const RAM: u16 = 0;
const UART: u16 = 1;
const CPU: u16 = 0;
const DMA: u16 = 1;

fn region(name: &'static str, base: u64, size: u64) -> Region {
    Region { name, base, size }
}

fn regions() -> Vec<Region> {
    vec![
        region("ram", RAM_BASE, RAM_SIZE),
        region("uart", UART_BASE, UART_SIZE),
    ]
}

const MASTERS: [&str; 3] = ["cpu", "dma0", "dma1"];

fn config(masters: usize) -> MultiMasterBusConfig {
    MultiMasterBusConfig {
        masters: MASTERS[..masters].to_vec(),
        regions: regions(),
        clock: CLOCK,
    }
}

fn bus_of(masters: usize) -> MultiMasterBus {
    MultiMasterBus::new(config(masters)).unwrap()
}

/// `cpu` and `dma0` over `ram` and `uart`.
fn bus() -> MultiMasterBus {
    bus_of(2)
}

/// The address of `offset` in region `r`.
fn addr(r: u16, offset: u64) -> u64 {
    [RAM_BASE, UART_BASE][usize::from(r)] + offset
}

fn request(bus: &mut MultiMasterBus, master: u16, msg: MemMsg) -> Result<MockCtx, SimError> {
    let mut ctx = MockCtx::new(Phase::Request);
    let port = bus.master_port(master);
    ctx.deliver(bus, port, msg)?;
    Ok(ctx)
}

/// Queues a routed request, checking that it only enqueues and wakes the bus for
/// `Transfer` now.
fn enqueue(bus: &mut MultiMasterBus, master: u16, msg: MemMsg) {
    let ctx = request(bus, master, msg).unwrap();
    assert_eq!(ctx.sent, []);
    assert_eq!(ctx.traced, []);
    assert_eq!(
        ctx.wakes,
        [Wake {
            when: ScheduleWhen::Now,
            phase: Phase::Transfer,
            token: 0
        }]
    );
}

fn try_arbitrate(bus: &mut MultiMasterBus) -> Result<MockCtx, SimError> {
    let mut ctx = MockCtx::new(Phase::Transfer);
    bus.handle_event(&Delivered::Wake { token: 0 }, &mut ctx)?;
    Ok(ctx)
}

fn arbitrate(bus: &mut MultiMasterBus) -> MockCtx {
    try_arbitrate(bus).unwrap()
}

fn respond(bus: &mut MultiMasterBus, region: u16, msg: MemMsg) -> Result<MockCtx, SimError> {
    let mut ctx = MockCtx::new(Phase::Complete);
    let port = bus.region_port(region);
    ctx.deliver(bus, port, msg)?;
    Ok(ctx)
}

/// A grant: region, master, original txn, downstream txn.
type Grant = (u16, u16, u64, u64);

fn txn_of(msg: &MemMsg) -> TxnId {
    match msg {
        MemMsg::ReadReq { txn, .. }
        | MemMsg::WriteReq { txn, .. }
        | MemMsg::ReadResp { txn, .. }
        | MemMsg::WriteResp { txn, .. } => *txn,
    }
}

/// The grants of one arbitration pass, checking that each trace record matches exactly
/// one request sent on its region's port at `Now`, `Transfer`, with the downstream txn,
/// and that nothing else was sent or scheduled.
fn grants_of(bus: &MultiMasterBus, ctx: &MockCtx) -> Vec<Grant> {
    let grants: Vec<Grant> = ctx
        .traced
        .iter()
        .map(|(kind, fields)| {
            assert_eq!(*kind, GRANT_KIND);
            let names: Vec<&str> = fields.iter().map(|f| f.0).collect();
            assert_eq!(names, ["region", "master", "txn", "downstream_txn"]);
            let v = |i: usize| match fields[i].1 {
                Value::U64(v) => v,
                ref other => panic!("{other:?}"),
            };
            (
                u16::try_from(v(0)).unwrap(),
                u16::try_from(v(1)).unwrap(),
                v(2),
                v(3),
            )
        })
        .collect();
    assert_eq!(ctx.sent.len(), grants.len());
    assert_eq!(ctx.wakes, []);
    for (sent, g) in ctx.sent.iter().zip(&grants) {
        assert_eq!(sent.port, bus.region_port(g.0));
        assert_eq!(txn_of(&sent.msg), TxnId(g.3));
        assert_eq!(
            (sent.when, sent.phase),
            (ScheduleWhen::Now, Phase::Transfer)
        );
    }
    grants
}

/// A successful response to a forwarded request.
fn ok_for(req: &MemMsg) -> MemMsg {
    match req {
        MemMsg::ReadReq { txn, len, .. } => MemMsg::ReadResp {
            txn: *txn,
            outcome: ReadOutcome::Data {
                data: vec![0xab; *len as usize],
            },
        },
        MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp {
            txn: *txn,
            outcome: WriteOutcome::Done,
        },
        other => panic!("{other:?}"),
    }
}

fn with_txn(msg: &MemMsg, txn: u64) -> MemMsg {
    let mut msg = msg.clone();
    match &mut msg {
        MemMsg::ReadReq { txn: t, .. }
        | MemMsg::WriteReq { txn: t, .. }
        | MemMsg::ReadResp { txn: t, .. }
        | MemMsg::WriteResp { txn: t, .. } => *t = TxnId(txn),
    }
    msg
}

fn access_fault(req: &MemMsg) -> MemMsg {
    let fault = MemFault::AccessFault;
    match req {
        MemMsg::ReadReq { txn, .. } => MemMsg::ReadResp {
            txn: *txn,
            outcome: ReadOutcome::Fault { fault },
        },
        MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp {
            txn: *txn,
            outcome: WriteOutcome::Fault { fault },
        },
        other => panic!("{other:?}"),
    }
}

fn component_fault(result: Result<impl Sized, SimError>) -> &'static str {
    match result {
        Err(SimError::ComponentFault(why)) => why,
        Err(other) => panic!("expected a component fault, got {other:?}"),
        Ok(_) => panic!("expected a component fault, got success"),
    }
}

fn next_cycle_wake() -> Wake {
    Wake {
        when: ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 1,
        },
        phase: Phase::Transfer,
        token: 0,
    }
}

/// Completes the active transaction of `region` with a success, checking the relay, and
/// returns the relayed response and the context.
fn complete(bus: &mut MultiMasterBus, region: u16, forwarded: &MemMsg) -> (Sent, MockCtx) {
    let mut ctx = respond(bus, region, ok_for(forwarded)).unwrap();
    assert_eq!(ctx.traced, []);
    let relayed = ctx.take_one();
    assert_eq!(
        (relayed.when, relayed.phase),
        (ScheduleWhen::Now, Phase::Complete)
    );
    (relayed, ctx)
}

// ---------------------------------------------------------------------------------------
// Configuration and ports
// ---------------------------------------------------------------------------------------

#[test]
fn ports_are_the_masters_then_the_regions_in_configured_order() {
    let bus = bus_of(3);
    let ports: Vec<(&str, Role)> = bus.ports().iter().map(|p| (p.name, p.role)).collect();
    assert_eq!(
        ports,
        [
            ("cpu", Role::Target),
            ("dma0", Role::Target),
            ("dma1", Role::Target),
            ("ram", Role::Initiator),
            ("uart", Role::Initiator),
        ]
    );
    assert!(bus.ports().iter().all(|p| p.protocol == mem_v1::PROTOCOL));
    assert_eq!(
        (0..3).map(|m| bus.master_port(m)).collect::<Vec<_>>(),
        [PortId(0), PortId(1), PortId(2)]
    );
    assert_eq!(
        (bus.region_port(RAM), bus.region_port(UART)),
        (PortId(3), PortId(4))
    );
    assert_eq!(bus.type_name(), "platform.multi_master_bus");
    assert_eq!(bus.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(SNAPSHOT_SCHEMA, 1);
}

#[test]
fn invalid_configurations_are_rejected() {
    let with = |masters: Vec<&'static str>, regions: Vec<Region>| {
        MultiMasterBus::new(MultiMasterBusConfig {
            masters,
            regions,
            clock: CLOCK,
        })
        .err()
    };
    let ram = region("ram", RAM_BASE, RAM_SIZE);
    assert_eq!(
        with(vec![], vec![ram]),
        Some(MultiMasterBusConfigError::NoMasters)
    );
    assert_eq!(
        with(vec!["cpu"], vec![region("z", 0, 0)]),
        Some(MultiMasterBusConfigError::ZeroSize("z"))
    );
    assert_eq!(
        with(vec!["cpu"], vec![region("w", u64::MAX, 2)]),
        Some(MultiMasterBusConfigError::Wraps("w"))
    );
    assert_eq!(
        with(vec!["cpu"], vec![region("end", u64::MAX, 1)]),
        None,
        "a region may end at the last byte"
    );
    assert_eq!(
        with(
            vec!["cpu"],
            vec![ram, region("b", RAM_BASE + RAM_SIZE - 1, 4)]
        ),
        Some(MultiMasterBusConfigError::Overlap("ram", "b"))
    );
    assert_eq!(
        with(vec!["cpu"], vec![ram, region("b", RAM_BASE + RAM_SIZE, 4)]),
        None,
        "adjacent regions do not overlap"
    );
    assert_eq!(
        with(vec!["cpu", "cpu"], vec![ram]),
        Some(MultiMasterBusConfigError::DuplicateName("cpu"))
    );
    assert_eq!(
        with(vec!["cpu", "ram"], vec![ram]),
        Some(MultiMasterBusConfigError::DuplicateName("ram"))
    );
    assert_eq!(
        with(vec!["cpu"], vec![ram, region("ram", 0, 1)]),
        Some(MultiMasterBusConfigError::DuplicateName("ram"))
    );
    let many: Vec<&'static str> = (0..u16::MAX)
        .map(|i| &*Box::leak(format!("m{i}").into_boxed_str()))
        .collect();
    assert_eq!(
        with(many, vec![]),
        Some(MultiMasterBusConfigError::TooManyPorts)
    );
}

#[test]
fn a_new_bus_is_idle_and_init_sends_nothing() {
    let mut bus = bus();
    let mut ctx = MockCtx::new(Phase::Request);
    bus.init(&mut ctx).unwrap();
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    for r in [RAM, UART] {
        assert_eq!(bus.active_master(r), None);
        assert_eq!(bus.rr_cursor(r), 0);
        assert_eq!((bus.queued(r, CPU), bus.queued(r, DMA)), (0, 0));
    }
    assert_eq!(bus.next_downstream_txn(), 0);
}

// ---------------------------------------------------------------------------------------
// Identity, routing, relay
// ---------------------------------------------------------------------------------------

#[test]
fn one_master_one_request_is_granted_as_an_offset_with_a_fresh_txn() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(7, RAM_BASE + 0x40, 4));
    assert_eq!(bus.queued(RAM, CPU), 1);
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, CPU, 7, 0)]);
    assert_eq!(ctx.sent[0].msg, read(0, 0x40, 4));
    assert_eq!(bus.active_master(RAM), Some(CPU));
    assert_eq!(bus.queued(RAM, CPU), 0);
    assert_eq!(bus.rr_cursor(RAM), 1);
    assert_eq!(bus.next_downstream_txn(), 1);

    let (relayed, ctx) = complete(&mut bus, RAM, &ctx.sent[0].msg);
    assert_eq!(relayed.port, bus.master_port(CPU));
    assert_eq!(relayed.msg, with_txn(&ok_for(&read(0, 0, 4)), 7));
    assert_eq!(ctx.wakes, [], "nothing queued: no wake");
    assert_eq!(bus.active_master(RAM), None);
}

#[test]
fn writes_are_forwarded_with_their_data_and_relayed() {
    let mut bus = bus();
    enqueue(&mut bus, DMA, write(3, UART_BASE + 4, &[1, 2]));
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(UART, DMA, 3, 0)]);
    assert_eq!(ctx.sent[0].msg, write(0, 4, &[1, 2]));
    let (relayed, _) = complete(&mut bus, UART, &ctx.sent[0].msg);
    assert_eq!(relayed.port, bus.master_port(DMA));
    assert_eq!(
        relayed.msg,
        MemMsg::WriteResp {
            txn: TxnId(3),
            outcome: WriteOutcome::Done
        }
    );
}

#[test]
fn target_faults_are_relayed_like_any_response() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(9, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    let fault = access_fault(&ctx.sent[0].msg);
    let mut ctx = respond(&mut bus, RAM, fault).unwrap();
    assert_eq!(ctx.take_one().msg, access_fault(&read(9, 0, 4)));
}

/// `docs/m2-design.md` §10.2: identity is `(master, txn)`. Two masters may use one txn at
/// once, to one region or to two; each gets its own response.
#[test]
fn the_same_txn_from_different_masters_is_two_transactions() {
    // Same region.
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(5, RAM_BASE, 4));
    enqueue(&mut bus, DMA, write(5, RAM_BASE + 8, &[1]));
    let first = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &first), [(RAM, CPU, 5, 0)]);
    let (relayed, ctx) = complete(&mut bus, RAM, &first.sent[0].msg);
    assert_eq!(
        (relayed.port, txn_of(&relayed.msg)),
        (bus.master_port(CPU), TxnId(5))
    );
    assert!(matches!(relayed.msg, MemMsg::ReadResp { .. }));
    assert_eq!(ctx.wakes, [next_cycle_wake()]);
    let second = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &second), [(RAM, DMA, 5, 1)]);
    let (relayed, _) = complete(&mut bus, RAM, &second.sent[0].msg);
    assert_eq!(
        (relayed.port, txn_of(&relayed.msg)),
        (bus.master_port(DMA), TxnId(5))
    );
    assert!(matches!(relayed.msg, MemMsg::WriteResp { .. }));

    // Different regions, concurrently: the targets see unique downstream ids, and the
    // responses, completed in the other order, go back to their own masters.
    let mut bus = crate::bus();
    enqueue(&mut bus, CPU, read(5, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(5, UART_BASE, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, CPU, 5, 0), (UART, DMA, 5, 1)]);
    assert_ne!(txn_of(&ctx.sent[0].msg), txn_of(&ctx.sent[1].msg));
    let (to_dma, _) = complete(&mut bus, UART, &ctx.sent[1].msg);
    let (to_cpu, _) = complete(&mut bus, RAM, &ctx.sent[0].msg);
    assert_eq!(
        (to_dma.port, txn_of(&to_dma.msg)),
        (bus.master_port(DMA), TxnId(5))
    );
    assert_eq!(
        (to_cpu.port, txn_of(&to_cpu.msg)),
        (bus.master_port(CPU), TxnId(5))
    );
}

#[test]
fn a_master_reusing_a_queued_or_active_txn_faults_the_session() {
    // Queued, same region.
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    let before = snapshot_of(&bus);
    let why = component_fault(request(&mut bus, CPU, read(1, RAM_BASE + 4, 4)));
    assert!(why.contains("reuses"), "{why}");
    assert_eq!(snapshot_of(&bus), before);
    // Queued in another region: identity spans the regions.
    component_fault(request(&mut bus, CPU, write(1, UART_BASE, &[0])));
    // Active.
    let ctx = arbitrate(&mut bus);
    assert_eq!(bus.active_master(RAM), Some(CPU));
    component_fault(request(&mut bus, CPU, read(1, UART_BASE, 1)));
    // Even an unroutable request with a live txn is a protocol violation.
    component_fault(request(&mut bus, CPU, read(1, 0, 1)));
    assert_eq!(bus.queued(UART, CPU), 0);
    // Once the response is relayed, the txn is free again.
    complete(&mut bus, RAM, &ctx.sent[0].msg);
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
}

#[test]
fn downstream_txns_come_from_one_counter_across_regions() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(10, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(10, UART_BASE, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(
        grants_of(&bus, &ctx)
            .iter()
            .map(|g| g.3)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    enqueue(&mut bus, DMA, read(11, RAM_BASE, 4));
    complete(&mut bus, RAM, &ctx.sent[0].msg);
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, DMA, 11, 2)]);
    assert_eq!(bus.next_downstream_txn(), 3);
}

// ---------------------------------------------------------------------------------------
// Arbitration
// ---------------------------------------------------------------------------------------

#[test]
fn the_first_tie_goes_to_master_0_and_the_second_to_master_1() {
    let mut bus = bus();
    enqueue(&mut bus, DMA, read(1, RAM_BASE, 4));
    enqueue(&mut bus, CPU, read(1, RAM_BASE + 4, 4));
    enqueue(&mut bus, CPU, read(2, RAM_BASE + 8, 4));
    enqueue(&mut bus, DMA, read(2, RAM_BASE + 12, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, CPU, 1, 0)]);
    assert_eq!(bus.rr_cursor(RAM), 1);
    complete(&mut bus, RAM, &ctx.sent[0].msg);
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, DMA, 1, 1)]);
    assert_eq!(bus.rr_cursor(RAM), 0);
}

#[test]
fn a_lone_master_1_is_granted_and_the_cursor_wraps() {
    let mut bus = bus();
    enqueue(&mut bus, DMA, read(4, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, DMA, 4, 0)]);
    assert_eq!(bus.rr_cursor(RAM), 0);
}

#[test]
fn the_cursor_does_not_advance_on_an_empty_pass() {
    let mut bus = bus();
    let before = snapshot_of(&bus);
    let ctx = arbitrate(&mut bus);
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    assert_eq!(snapshot_of(&bus), before);
    // Nor from a non-zero cursor.
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    complete(&mut bus, RAM, &ctx.sent[0].msg);
    assert_eq!(bus.rr_cursor(RAM), 1);
    let before = snapshot_of(&bus);
    let ctx = arbitrate(&mut bus);
    assert!(ctx.order.is_empty());
    assert_eq!(snapshot_of(&bus), before);
    assert_eq!(bus.rr_cursor(RAM), 1);
}

#[test]
fn a_busy_region_grants_nothing_and_keeps_its_cursor() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    let first = arbitrate(&mut bus);
    enqueue(&mut bus, DMA, read(1, RAM_BASE, 4));
    enqueue(&mut bus, CPU, read(2, RAM_BASE, 4));
    let before = snapshot_of(&bus);
    let ctx = arbitrate(&mut bus);
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    assert_eq!(snapshot_of(&bus), before);
    assert_eq!(bus.rr_cursor(RAM), 1);
    complete(&mut bus, RAM, &first.sent[0].msg);
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, DMA, 1, 1)]);
}

#[test]
fn one_masters_requests_to_one_region_keep_their_order() {
    let mut bus = bus();
    for txn in [30, 10, 20] {
        enqueue(&mut bus, CPU, read(txn, RAM_BASE + txn, 1));
    }
    let mut order = Vec::new();
    for _ in 0..3 {
        let ctx = arbitrate(&mut bus);
        let g = grants_of(&bus, &ctx);
        order.push(g[0].2);
        assert_eq!(ctx.sent[0].msg, read(g[0].3, g[0].2, 1));
        complete(&mut bus, RAM, &ctx.sent[0].msg);
    }
    assert_eq!(order, [30, 10, 20]);
}

#[test]
fn independent_regions_are_granted_in_the_same_pass() {
    let mut bus = bus();
    enqueue(&mut bus, DMA, write(1, UART_BASE, &[0x41]));
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(2, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    // Region order, one grant each: RAM's tie goes to the CPU; UART has only the DMA.
    assert_eq!(grants_of(&bus, &ctx), [(RAM, CPU, 1, 0), (UART, DMA, 1, 1)]);
    assert_eq!(
        (bus.active_master(RAM), bus.active_master(UART)),
        (Some(CPU), Some(DMA))
    );
    assert_eq!((bus.rr_cursor(RAM), bus.rr_cursor(UART)), (1, 0));
    // Freeing one region does not touch the other.
    let (_, ctx2) = complete(&mut bus, UART, &ctx.sent[1].msg);
    assert_eq!(ctx2.wakes, []);
    assert_eq!(bus.active_master(RAM), Some(CPU));
}

/// Several wakes can land in one `(tick, Transfer)`, one per request of that tick. The
/// first grants everything grantable; the others do nothing.
#[test]
fn later_wakes_of_one_transfer_find_nothing_to_do() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(2, UART_BASE, 4));
    let first = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &first).len(), 2);
    let after = snapshot_of(&bus);
    for _ in 0..2 {
        let ctx = arbitrate(&mut bus);
        assert!(ctx.order.is_empty() && ctx.traced.is_empty());
        assert_eq!(snapshot_of(&bus), after);
    }
}

/// Two masters that always have a request queued alternate; three go 0, 1, 2, 0, ...
#[test]
fn backlogged_masters_are_served_round_robin() {
    for masters in [2u16, 3] {
        let mut bus = bus_of(usize::from(masters));
        let mut next = vec![0u64; usize::from(masters)];
        for m in 0..masters {
            for _ in 0..2 {
                enqueue(&mut bus, m, read(next[usize::from(m)], RAM_BASE, 4));
                next[usize::from(m)] += 1;
            }
        }
        let mut served = Vec::new();
        for _ in 0..(4 * masters) {
            let ctx = arbitrate(&mut bus);
            let g = grants_of(&bus, &ctx)[0];
            served.push(g.1);
            // Keep the granted master backlogged.
            enqueue(&mut bus, g.1, read(next[usize::from(g.1)], RAM_BASE, 4));
            next[usize::from(g.1)] += 1;
            let (_, ctx) = complete(&mut bus, RAM, &ctx.sent[0].msg);
            assert_eq!(ctx.wakes, [next_cycle_wake()]);
        }
        let expected: Vec<u16> = (0..(4 * masters)).map(|i| i % masters).collect();
        assert_eq!(served, expected, "{masters} masters");
    }
}

// ---------------------------------------------------------------------------------------
// Phases and timing
// ---------------------------------------------------------------------------------------

/// `Complete` relays and frees, never grants: with contenders queued it wakes the bus in
/// the next bus cycle's `Transfer`, not at `Now`.
#[test]
fn a_completion_frees_the_region_and_regrants_only_next_cycle() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(1, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    let (relayed, done) = complete(&mut bus, RAM, &ctx.sent[0].msg);
    assert_eq!(relayed.port, bus.master_port(CPU));
    assert_eq!(done.order, [SendKind::Mem, SendKind::Wake]);
    assert_eq!(done.wakes, [next_cycle_wake()]);
    assert_eq!(bus.active_master(RAM), None);
    assert_eq!(bus.queued(RAM, DMA), 1, "not granted in COMPLETE");
}

#[test]
fn requests_arrive_only_in_request_and_responses_only_in_complete() {
    for phase in [
        Phase::Transfer,
        Phase::Complete,
        Phase::Commit,
        Phase::Observe,
    ] {
        let mut bus = bus();
        let mut ctx = MockCtx::new(phase);
        let why = component_fault(ctx.deliver(&mut bus, PortId(0), read(1, RAM_BASE, 4)));
        assert!(why.contains("outside REQUEST"), "{why}");
        assert_eq!(bus.queued(RAM, CPU), 0);
    }
    for phase in [
        Phase::Request,
        Phase::Transfer,
        Phase::Commit,
        Phase::Observe,
    ] {
        let mut bus = bus();
        enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
        let fwd = arbitrate(&mut bus).sent[0].msg.clone();
        let mut ctx = MockCtx::new(phase);
        let port = bus.region_port(RAM);
        let why = component_fault(ctx.deliver(&mut bus, port, ok_for(&fwd)));
        assert!(why.contains("outside COMPLETE"), "{why}");
        assert_eq!(bus.active_master(RAM), Some(CPU));
    }
    for phase in [
        Phase::Request,
        Phase::Complete,
        Phase::Commit,
        Phase::Observe,
    ] {
        let mut bus = bus();
        enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
        let mut ctx = MockCtx::new(phase);
        component_fault(bus.handle_event(&Delivered::Wake { token: 0 }, &mut ctx));
        assert_eq!(bus.queued(RAM, CPU), 1);
    }
}

// ---------------------------------------------------------------------------------------
// Access faults
// ---------------------------------------------------------------------------------------

fn fault_record(txn: u64, addr: u64, len: u64, master: u16) -> Traced {
    (
        FAULT_KIND,
        vec![
            ("txn", Value::U64(txn)),
            ("addr", Value::U64(addr)),
            ("len", Value::U64(len)),
            ("master", Value::U64(u64::from(master))),
        ],
    )
}

/// Unmapped, crossing, and overflowing requests are answered by the bus to the
/// originating master with its own txn, and change nothing else: no queue, no counter,
/// no cursor, no active transaction, no wake.
#[test]
fn unrouted_requests_are_answered_with_an_access_fault_and_change_nothing() {
    let adjacent = MultiMasterBusConfig {
        masters: vec!["cpu", "dma0"],
        regions: vec![region("a", 0x1000, 0x100), region("b", 0x1100, 0x100)],
        clock: CLOCK,
    };
    let cases: Vec<(&str, MultiMasterBusConfig, MemMsg, u64)> = vec![
        ("unmapped", config(2), read(6, 0x4000_0000, 4), 4),
        ("below", config(2), write(6, RAM_BASE - 1, &[1]), 1),
        (
            "past the end",
            config(2),
            read(6, RAM_BASE + RAM_SIZE - 2, 4),
            4,
        ),
        ("into an adjacent region", adjacent, read(6, 0x10fe, 4), 4),
        ("overflow", config(2), read(6, u64::MAX - 1, 4), 4),
        ("overflow write", config(2), write(6, u64::MAX, &[1, 2]), 2),
    ];
    for (name, cfg, msg, len) in cases {
        let mut bus = MultiMasterBus::new(cfg).unwrap();
        // An active transaction and a non-zero cursor that must survive.
        let base = bus.config().regions[0].base;
        enqueue(&mut bus, CPU, read(1, base, 1));
        arbitrate(&mut bus);
        let before = snapshot_of(&bus);
        let addr = match &msg {
            MemMsg::ReadReq { addr, .. } | MemMsg::WriteReq { addr, .. } => *addr,
            _ => unreachable!(),
        };
        let mut ctx = request(&mut bus, DMA, msg.clone()).unwrap();
        let sent = ctx.take_one();
        assert_eq!(sent.port, bus.master_port(DMA), "{name}");
        assert_eq!(sent.msg, access_fault(&msg), "{name}");
        assert_eq!(
            (sent.when, sent.phase),
            (ScheduleWhen::Now, Phase::Complete)
        );
        assert_eq!(ctx.wakes, [], "{name}");
        assert_eq!(ctx.traced, [fault_record(6, addr, len, DMA)], "{name}");
        assert_eq!(snapshot_of(&bus), before, "{name}");
    }
}

#[test]
fn the_last_byte_of_the_address_space_is_routable() {
    let mut bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: vec!["cpu"],
        regions: vec![region("top", u64::MAX - 7, 8)],
        clock: CLOCK,
    })
    .unwrap();
    enqueue(&mut bus, CPU, read(1, u64::MAX - 3, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(ctx.sent[0].msg, read(0, 4, 4));
}

// ---------------------------------------------------------------------------------------
// Session faults
// ---------------------------------------------------------------------------------------

#[test]
fn responses_that_do_not_match_the_active_transaction_fault_the_session() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(5, RAM_BASE, 4));
    enqueue(&mut bus, DMA, write(5, UART_BASE, &[1]));
    let ctx = arbitrate(&mut bus);
    let (to_ram, to_uart) = (ctx.sent[0].msg.clone(), ctx.sent[1].msg.clone());
    let before = snapshot_of(&bus);
    let cases: Vec<(&str, u16, MemMsg)> = vec![
        ("unknown txn", RAM, with_txn(&ok_for(&to_ram), 99)),
        ("original txn", RAM, with_txn(&ok_for(&to_ram), 5)),
        ("wrong region port", UART, ok_for(&to_ram)),
        ("wrong kind", RAM, with_txn(&ok_for(&to_uart), 0)),
    ];
    for (name, region, msg) in cases {
        let why = component_fault(respond(&mut bus, region, msg));
        assert!(!why.is_empty(), "{name}");
        assert_eq!(snapshot_of(&bus), before, "{name}");
    }
    // A duplicate or stale response after the relay.
    complete(&mut bus, RAM, &to_ram);
    component_fault(respond(&mut bus, RAM, ok_for(&to_ram)));
    // A response on an idle region.
    complete(&mut bus, UART, &to_uart);
    component_fault(respond(&mut bus, UART, ok_for(&to_uart)));
}

#[test]
fn messages_in_the_wrong_direction_or_protocol_fault_the_session() {
    let mut bus = bus();
    let resp = ok_for(&read(0, 0, 4));
    component_fault(request(&mut bus, CPU, resp));
    let mut ctx = MockCtx::new(Phase::Complete);
    let port = bus.region_port(RAM);
    component_fault(ctx.deliver(&mut bus, port, read(0, 0, 4)));
    let mut ctx = MockCtx::new(Phase::Request);
    component_fault(ctx.deliver(&mut bus, PortId(4), read(0, 0, 4)));
    component_fault(ctx.deliver_msg(
        &mut bus,
        PortId(0),
        Message::Irq(IrqMsg::Level { asserted: true }),
    ));
    let mut ctx = MockCtx::new(Phase::Transfer);
    component_fault(bus.handle_event(&Delivered::Wake { token: 1 }, &mut ctx));
    // Zero-length requests are protocol violations, not access faults.
    let why = component_fault(request(&mut bus, CPU, read(0, RAM_BASE, 0)));
    assert!(why.contains("zero-length"), "{why}");
    component_fault(request(&mut bus, CPU, write(0, 0, &[])));
    assert_eq!(snapshot_of(&bus), snapshot_of(&crate::bus()));
}

/// The downstream counter never wraps: a grant that would need id `u64::MAX + 1` faults
/// before anything is sent or changed.
#[test]
fn an_exhausted_downstream_counter_faults_before_sending() {
    let mut bus = bus();
    let pending = read(4, 0x10, 4);
    let bytes = Forge::new(2)
        .next(u64::MAX - 1)
        .queue(RAM, CPU, pending.clone())
        .queue(RAM, DMA, read(8, 0x20, 4));
    restore_into(&mut bus, &bytes.bytes()).unwrap();
    // The last id is still allocatable.
    let ctx = arbitrate(&mut bus);
    assert_eq!(grants_of(&bus, &ctx), [(RAM, CPU, 4, u64::MAX - 1)]);
    assert_eq!(bus.next_downstream_txn(), u64::MAX);
    complete(&mut bus, RAM, &ctx.sent[0].msg);
    let before = snapshot_of(&bus);
    let mut ctx = MockCtx::new(Phase::Transfer);
    let why = component_fault(bus.handle_event(&Delivered::Wake { token: 0 }, &mut ctx));
    assert!(why.contains("exhausted"), "{why}");
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    assert_eq!(snapshot_of(&bus), before);
}

// ---------------------------------------------------------------------------------------
// Inspect and trace
// ---------------------------------------------------------------------------------------

fn field(view: &StateView, name: &str) -> Value {
    view.get(name).cloned().unwrap_or_else(|| panic!("{name}"))
}

#[test]
fn inspect_shows_each_regions_active_master_cursor_and_fifo_lengths() {
    let mut bus = bus();
    let view = bus.inspect();
    let names: Vec<&str> = view.fields.iter().map(|f| f.0).collect();
    assert_eq!(
        names,
        ["masters", "regions", "next_downstream_txn", "ram", "uart"]
    );
    assert_eq!(field(&view, "masters"), Value::U64(2));
    assert_eq!(field(&view, "regions"), Value::U64(2));
    let idle = Value::Str("active=none rr_cursor=0 queued=0,0".into());
    assert_eq!(
        (field(&view, "ram"), field(&view, "uart")),
        (idle.clone(), idle)
    );

    enqueue(&mut bus, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(1, RAM_BASE, 4));
    enqueue(&mut bus, DMA, read(2, RAM_BASE, 4));
    enqueue(&mut bus, CPU, read(2, RAM_BASE, 4));
    arbitrate(&mut bus);
    let view = bus.inspect();
    assert_eq!(field(&view, "next_downstream_txn"), Value::U64(1));
    assert_eq!(
        field(&view, "ram"),
        Value::Str("active=0 rr_cursor=1 queued=1,2".into())
    );
}

#[test]
fn a_grant_is_traced_once_and_idle_passes_trace_nothing() {
    let mut bus = bus();
    enqueue(&mut bus, DMA, read(12, RAM_BASE, 4));
    let ctx = arbitrate(&mut bus);
    assert_eq!(
        ctx.traced,
        [(
            GRANT_KIND,
            vec![
                ("region", Value::U64(0)),
                ("master", Value::U64(1)),
                ("txn", Value::U64(12)),
                ("downstream_txn", Value::U64(0)),
            ]
        )]
    );
    assert_eq!(arbitrate(&mut bus).traced, []);
    let (_, ctx) = complete(&mut bus, RAM, &ctx.sent[0].msg);
    assert_eq!(ctx.traced, []);
}

// ---------------------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------------------

/// Hand-written snapshot bytes, independent of the bus.
#[derive(Clone)]
struct Forge {
    regions: Vec<Region>,
    masters: Vec<&'static str>,
    clock: u32,
    next: u64,
    /// Per region: active, cursor, FIFOs.
    state: Vec<(Option<ForgedActive>, u16, Vec<Vec<MemMsg>>)>,
}

/// An active transaction: master, original txn, downstream txn, whether it is a write.
type ForgedActive = (u16, u64, u64, bool);

impl Forge {
    fn new(masters: usize) -> Forge {
        Forge {
            regions: regions(),
            masters: MASTERS[..masters].to_vec(),
            clock: CLOCK.0,
            next: 0,
            state: vec![(None, 0, vec![Vec::new(); masters]); 2],
        }
    }

    fn next(mut self, next: u64) -> Forge {
        self.next = next;
        self
    }

    fn active(mut self, r: u16, master: u16, original: u64, downstream: u64, w: bool) -> Forge {
        self.state[usize::from(r)].0 = Some((master, original, downstream, w));
        self
    }

    fn cursor(mut self, r: u16, cursor: u16) -> Forge {
        self.state[usize::from(r)].1 = cursor;
        self
    }

    fn queue(mut self, r: u16, master: u16, msg: MemMsg) -> Forge {
        self.state[usize::from(r)].2[usize::from(master)].push(msg);
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut w = SnapshotWriter::new();
        w.len(self.regions.len());
        for r in &self.regions {
            w.str(r.name);
            w.u64(r.base);
            w.u64(r.size);
        }
        w.len(self.masters.len());
        for m in &self.masters {
            w.str(m);
        }
        w.u32(self.clock);
        w.u64(self.next);
        for (active, cursor, queues) in &self.state {
            match active {
                None => w.u8(0),
                Some((m, o, d, wr)) => {
                    w.u8(1);
                    w.u16(*m);
                    w.u64(*o);
                    w.u64(*d);
                    w.bool(*wr);
                }
            }
            w.u16(*cursor);
            for q in queues {
                w.len(q.len());
                for msg in q {
                    msg.encode(&mut w);
                }
            }
        }
        w.into_bytes()
    }
}

/// The snapshot layout, pinned against hand-written bytes: the CPU active on RAM, the
/// DMA queued behind it, the RAM cursor at 1, and a queued UART write.
#[test]
fn the_snapshot_layout_is_pinned() {
    let mut bus = bus();
    enqueue(&mut bus, CPU, read(7, RAM_BASE + 0x10, 4));
    enqueue(&mut bus, DMA, write(7, RAM_BASE + 0x20, &[1, 2]));
    arbitrate(&mut bus);
    // The UART write is queued after the pass (its wake has not run yet).
    enqueue(&mut bus, DMA, write(8, UART_BASE + 1, &[3]));
    let expected = Forge::new(2)
        .next(1)
        .active(RAM, CPU, 7, 0, false)
        .cursor(RAM, 1)
        .queue(RAM, DMA, write(7, 0x20, &[1, 2]))
        .queue(UART, DMA, write(8, 1, &[3]));
    assert_eq!(snapshot_of(&bus), expected.bytes());
}

/// Named states from `docs/m2-design.md` §10.6 and the M2.4 checklist, each reached by
/// driving the bus.
fn checkpoint_states() -> Vec<(&'static str, MultiMasterBus)> {
    let mut states = Vec::new();
    states.push(("empty", bus()));

    let mut b = bus();
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    states.push(("one queued", b));

    let mut b = bus_of(3);
    enqueue(&mut b, 2, read(1, RAM_BASE, 4));
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut b, DMA, write(1, UART_BASE, &[1]));
    enqueue(&mut b, DMA, read(2, RAM_BASE, 2));
    states.push(("several masters queued", b));

    let mut b = bus();
    enqueue(&mut b, DMA, read(1, RAM_BASE, 4));
    arbitrate(&mut b);
    states.push(("active", b));

    let mut b = bus();
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut b, DMA, read(1, RAM_BASE + 4, 4));
    enqueue(&mut b, CPU, write(2, RAM_BASE + 8, &[9, 9]));
    arbitrate(&mut b);
    states.push(("active and contenders", b));

    let mut b = bus();
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut b, DMA, read(1, UART_BASE, 4));
    arbitrate(&mut b);
    states.push(("two regions active", b));

    let mut b = bus();
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    enqueue(&mut b, DMA, read(1, RAM_BASE, 4));
    let fwd = arbitrate(&mut b).sent[0].msg.clone();
    complete(&mut b, RAM, &fwd);
    states.push(("relayed, before the next-cycle grant", b));

    let mut b = bus();
    enqueue(&mut b, CPU, read(1, RAM_BASE, 4));
    let fwd = arbitrate(&mut b).sent[0].msg.clone();
    complete(&mut b, RAM, &fwd);
    states.push(("cursor not 0", b));
    states
}

/// Drives a bus until it has nothing queued or active: arbitrate, then complete every
/// active region in region order. Returns everything it saw.
fn drain(bus: &mut MultiMasterBus) -> Vec<(Vec<Sent>, Vec<Traced>, Vec<Wake>)> {
    let mut log = Vec::new();
    let regions = u16::try_from(bus.config().regions.len()).unwrap();
    let mut inflight: Vec<Option<MemMsg>> = vec![None; usize::from(regions)];
    // A bus may start with active transactions whose requests are already in flight
    // (runtime events, not bus state); a response only needs their downstream txn and
    // kind, which the snapshot holds.
    for r in 0..regions {
        if bus.active_master(r).is_some() {
            inflight[usize::from(r)] = Some(active_request(bus, r));
        }
    }
    for _ in 0..64 {
        let ctx = arbitrate(bus);
        for s in &ctx.sent {
            let r = s.port.0 - u16::try_from(bus.config().masters.len()).unwrap();
            inflight[usize::from(r)] = Some(s.msg.clone());
        }
        log.push((ctx.sent, ctx.traced, ctx.wakes));
        let mut any = false;
        for r in 0..regions {
            if let Some(fwd) = inflight[usize::from(r)].take() {
                let ctx = respond(bus, r, ok_for(&fwd)).unwrap();
                log.push((ctx.sent, ctx.traced, ctx.wakes));
                any = true;
            }
        }
        if !any {
            break;
        }
    }
    log
}

/// A request shaped like region `r`'s active transaction, read back from the snapshot.
fn active_request(bus: &MultiMasterBus, r: u16) -> MemMsg {
    // Parse this region's active entry from the canonical bytes.
    let bytes = snapshot_of(bus);
    let mut d = systemscope_contracts::canonical::Decoder::new(&bytes);
    for _ in 0..d.len().unwrap() {
        d.str().unwrap();
        d.u64().unwrap();
        d.u64().unwrap();
    }
    let masters = d.len().unwrap();
    for _ in 0..masters {
        d.str().unwrap();
    }
    d.u32().unwrap();
    d.u64().unwrap();
    for i in 0.. {
        let active = match d.u8().unwrap() {
            0 => None,
            _ => {
                d.u16().unwrap();
                d.u64().unwrap();
                let down = d.u64().unwrap();
                let w = d.bool().unwrap();
                Some((down, w))
            }
        };
        if i == r {
            let (down, w) = active.unwrap();
            return if w {
                write(down, 0, &[0])
            } else {
                read(down, 0, 4)
            };
        }
        d.u16().unwrap();
        for _ in 0..masters {
            for _ in 0..d.len().unwrap() {
                MemMsg::decode(&mut d).unwrap();
            }
        }
    }
    unreachable!()
}

#[test]
fn snapshots_round_trip_and_continue_identically() {
    for (name, mut original) in checkpoint_states() {
        let bytes = snapshot_of(&original);
        let mut copy = bus_of(original.config().masters.len());
        restore_into(&mut copy, &bytes).unwrap();
        assert_eq!(snapshot_of(&copy), bytes, "{name}");
        assert_eq!(copy.inspect(), original.inspect(), "{name}");
        // The next-cycle wake of the relayed state is a runtime event; either way the
        // continuation is the same.
        let a = drain(&mut original);
        let b = drain(&mut copy);
        assert_eq!(a, b, "{name}");
        assert_eq!(snapshot_of(&original), snapshot_of(&copy), "{name}");
    }
}

#[test]
fn restore_rejects_invalid_snapshots_and_changes_nothing() {
    let valid = Forge::new(2)
        .next(3)
        .active(RAM, CPU, 7, 2, false)
        .cursor(RAM, 1)
        .queue(RAM, DMA, read(7, 0x10, 4));
    let other_regions = |f: Forge, regions: Vec<Region>| Forge { regions, ..f };
    let cases: Vec<(&str, Forge)> = vec![
        (
            "region renamed",
            other_regions(
                valid.clone(),
                vec![
                    region("mem", RAM_BASE, RAM_SIZE),
                    region("uart", UART_BASE, UART_SIZE),
                ],
            ),
        ),
        (
            "region moved",
            other_regions(
                valid.clone(),
                vec![
                    region("ram", RAM_BASE + 0x1000, RAM_SIZE),
                    region("uart", UART_BASE, UART_SIZE),
                ],
            ),
        ),
        (
            "region resized",
            other_regions(
                valid.clone(),
                vec![
                    region("ram", RAM_BASE, RAM_SIZE * 2),
                    region("uart", UART_BASE, UART_SIZE),
                ],
            ),
        ),
        (
            "regions reordered",
            other_regions(
                valid.clone(),
                vec![
                    region("uart", UART_BASE, UART_SIZE),
                    region("ram", RAM_BASE, RAM_SIZE),
                ],
            ),
        ),
        (
            "masters reordered",
            Forge {
                masters: vec!["dma0", "cpu"],
                ..valid.clone()
            },
        ),
        (
            "master renamed",
            Forge {
                masters: vec!["cpu", "dma1"],
                ..valid.clone()
            },
        ),
        (
            "different clock",
            Forge {
                clock: CLOCK.0 + 1,
                ..valid.clone()
            },
        ),
        ("cursor at the master count", valid.clone().cursor(UART, 2)),
        (
            "active master unknown",
            valid.clone().active(UART, 2, 1, 1, false),
        ),
        (
            "active downstream not allocated",
            valid.clone().active(RAM, CPU, 7, 3, false),
        ),
        (
            "two actives share a downstream txn",
            valid.clone().active(UART, DMA, 1, 2, true),
        ),
        (
            "active and active reuse (master, txn)",
            valid.clone().active(UART, CPU, 7, 1, true),
        ),
        (
            "active and queued reuse (master, txn)",
            valid.clone().queue(UART, CPU, read(7, 0, 1)),
        ),
        (
            "two queued reuse (master, txn)",
            valid.clone().queue(UART, DMA, read(7, 0, 1)),
        ),
        (
            "queued past its region",
            valid.clone().queue(UART, CPU, read(1, UART_SIZE - 1, 2)),
        ),
        (
            "queued at an absolute address",
            valid.clone().queue(RAM, CPU, read(1, RAM_BASE, 4)),
        ),
        (
            "queued zero-length",
            valid.clone().queue(RAM, CPU, read(1, 0, 0)),
        ),
        (
            "queued response",
            valid.clone().queue(RAM, CPU, ok_for(&read(1, 0, 4))),
        ),
    ];
    for (name, forge) in cases {
        // Restore into a bus with state of its own, which must survive.
        let mut bus = bus();
        enqueue(&mut bus, DMA, write(3, UART_BASE, &[1]));
        let before = snapshot_of(&bus);
        assert!(restore_into(&mut bus, &forge.bytes()).is_err(), "{name}");
        assert_eq!(snapshot_of(&bus), before, "{name}");
    }
    // The valid one restores, and a third master's snapshot does not fit two.
    restore_into(&mut bus(), &valid.bytes()).unwrap();
    assert!(restore_into(&mut bus(), &Forge::new(3).bytes()).is_err());
    // Non-canonical encodings: an active tag of 2, a write flag of 2, a trailing byte.
    // Each idle region is a tag, a cursor, and two empty FIFO lengths (u32).
    let mut bytes = Forge::new(2).bytes();
    let header = bytes.len() - 2 * (1 + 2 + 2 * 4);
    bytes[header] = 2;
    assert!(restore_into(&mut bus(), &bytes).is_err(), "active tag 2");
    let mut bytes = Forge::new(2).active(RAM, CPU, 1, 0, false).next(1).bytes();
    bytes[header + 1 + 2 + 8 + 8] = 2;
    assert!(restore_into(&mut bus(), &bytes).is_err(), "write flag 2");
    let mut bytes = valid.bytes();
    bytes.push(0);
    assert!(restore_into(&mut bus(), &bytes).is_err(), "trailing byte");
}

// ---------------------------------------------------------------------------------------
// Independent arbitration model
// ---------------------------------------------------------------------------------------

/// Written from `docs/m2-design.md` §10.3–§10.5 without the bus's code: per region, a busy
/// flag with its owner, one FIFO of original txns per master, and a cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelRegion {
    busy: Option<(usize, u64)>,
    fifos: Vec<Vec<u64>>,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    masters: usize,
    regions: Vec<ModelRegion>,
}

impl Model {
    fn new(masters: usize, regions: usize) -> Model {
        Model {
            masters,
            regions: vec![
                ModelRegion {
                    busy: None,
                    fifos: vec![Vec::new(); masters],
                    cursor: 0,
                };
                regions
            ],
        }
    }

    fn enqueue(&mut self, region: usize, master: usize, txn: u64) {
        self.regions[region].fifos[master].push(txn);
    }

    /// One arbitration pass; the grants as (region, master, txn).
    fn transfer(&mut self) -> Vec<(usize, usize, u64)> {
        let mut grants = Vec::new();
        for (r, reg) in self.regions.iter_mut().enumerate() {
            if reg.busy.is_some() {
                continue;
            }
            let mut m = reg.cursor;
            for _ in 0..self.masters {
                if !reg.fifos[m].is_empty() {
                    let txn = reg.fifos[m].remove(0);
                    reg.busy = Some((m, txn));
                    reg.cursor = if m + 1 == self.masters { 0 } else { m + 1 };
                    grants.push((r, m, txn));
                    break;
                }
                m = if m + 1 == self.masters { 0 } else { m + 1 };
            }
        }
        grants
    }

    /// Completes `region`: its owner and txn, and whether anything is still queued there.
    fn complete(&mut self, region: usize) -> Option<(usize, u64, bool)> {
        let reg = &mut self.regions[region];
        let (m, txn) = reg.busy.take()?;
        Some((m, txn, reg.fifos.iter().any(|f| !f.is_empty())))
    }
}

/// Where a generated request goes.
#[derive(Clone, Copy, Debug)]
enum Target {
    Ram(u16),
    Uart(u8),
    Unmapped,
    Crossing,
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        4 => (0u16..0x100).prop_map(Target::Ram),
        3 => (0u8..8).prop_map(Target::Uart),
        1 => Just(Target::Unmapped),
        1 => Just(Target::Crossing),
    ]
}

#[derive(Clone, Debug)]
enum Op {
    /// One `Request` phase: (master, target, len, write) in dispatch order.
    Batch(Vec<(usize, Target, u8, bool)>),
    Transfer,
    Complete(u16),
    Checkpoint,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => prop::collection::vec((0usize..3, target(), 1u8..=8, any::<bool>()), 1..5)
            .prop_map(Op::Batch),
        3 => Just(Op::Transfer),
        3 => (0u16..2).prop_map(Op::Complete),
        1 => Just(Op::Checkpoint),
    ]
}

fn make_request(t: Target, len: u8, is_write: bool, txn: u64) -> (MemMsg, Option<usize>) {
    let (addr, routed) = match t {
        Target::Ram(off) => (RAM_BASE + u64::from(off), Some(0)),
        // Clamp to the 8-byte window so the request fits.
        Target::Uart(off) => (UART_BASE + u64::from(off.min(8 - len.min(8))), Some(1)),
        Target::Unmapped => (0x4000_0000, None),
        Target::Crossing => (RAM_BASE + RAM_SIZE - 1, None),
    };
    // A 1-byte request at the last RAM byte would fit; crossing needs at least two.
    let len = if matches!(t, Target::Crossing) {
        len.max(2)
    } else {
        len
    };
    let msg = if is_write {
        write(txn, addr, &vec![0x5a; usize::from(len)])
    } else {
        read(txn, addr, u32::from(len))
    };
    (msg, routed)
}

fn assert_matches_model(bus: &MultiMasterBus, model: &Model) {
    for (r, reg) in model.regions.iter().enumerate() {
        let r16 = u16::try_from(r).unwrap();
        assert_eq!(
            bus.active_master(r16).map(usize::from),
            reg.busy.map(|b| b.0)
        );
        assert_eq!(usize::from(bus.rr_cursor(r16)), reg.cursor);
        for m in 0..model.masters {
            assert_eq!(
                bus.queued(r16, u16::try_from(m).unwrap()),
                reg.fifos[m].len()
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every sequence of request batches, arbitration passes, completions, and
    /// checkpoints grants what the model grants, relays each response to its owner with
    /// its original txn, and wakes for the next cycle exactly when contenders remain.
    #[test]
    fn every_sequence_matches_the_model(
        masters in 1usize..=3,
        ops in prop::collection::vec(op(), 0..48),
    ) {
        let mut bus = bus_of(masters);
        let mut model = Model::new(masters, 2);
        // Every master counts txns from 0, so masters collide on purpose.
        let mut next_txn = vec![0u64; masters];
        let mut inflight: Vec<Option<MemMsg>> = vec![None, None];
        let mut downstream = 0u64;
        for op in ops {
            match op {
                Op::Batch(items) => {
                    for (m, t, len, is_write) in items {
                        let m = m % masters;
                        let txn = next_txn[m];
                        next_txn[m] += 1;
                        let (msg, routed) = make_request(t, len, is_write, txn);
                        let master = u16::try_from(m).unwrap();
                        match routed {
                            Some(r) => {
                                enqueue(&mut bus, master, msg);
                                model.enqueue(r, m, txn);
                            }
                            None => {
                                let mut ctx = request(&mut bus, master, msg.clone()).unwrap();
                                let sent = ctx.take_one();
                                prop_assert_eq!(sent.port, bus.master_port(master));
                                prop_assert_eq!(sent.msg, access_fault(&msg));
                                prop_assert!(ctx.wakes.is_empty());
                            }
                        }
                    }
                }
                Op::Transfer => {
                    let ctx = arbitrate(&mut bus);
                    let got = grants_of(&bus, &ctx);
                    let want = model.transfer();
                    prop_assert_eq!(got.len(), want.len());
                    for ((g, w), sent) in got.iter().zip(&want).zip(&ctx.sent) {
                        prop_assert_eq!((usize::from(g.0), usize::from(g.1), g.2), *w);
                        prop_assert_eq!(g.3, downstream);
                        downstream += 1;
                        inflight[usize::from(g.0)] = Some(sent.msg.clone());
                    }
                }
                Op::Complete(r) => match inflight[usize::from(r)].take() {
                    Some(fwd) => {
                        let mut ctx = respond(&mut bus, r, ok_for(&fwd)).unwrap();
                        let (m, txn, more) = model.complete(usize::from(r)).unwrap();
                        let relayed = ctx.take_one();
                        prop_assert_eq!(relayed.port, bus.master_port(u16::try_from(m).unwrap()));
                        prop_assert_eq!(txn_of(&relayed.msg), TxnId(txn));
                        prop_assert_eq!(relayed.phase, Phase::Complete);
                        let wakes = if more { vec![next_cycle_wake()] } else { vec![] };
                        prop_assert_eq!(ctx.wakes, wakes);
                    }
                    None => {
                        // Nothing active there: any response is unknown.
                        let before = snapshot_of(&bus);
                        let bogus = ok_for(&read(downstream, 0, 1));
                        prop_assert!(respond(&mut bus, r, bogus).is_err());
                        prop_assert_eq!(snapshot_of(&bus), before);
                    }
                },
                Op::Checkpoint => {
                    let bytes = snapshot_of(&bus);
                    let mut copy = bus_of(masters);
                    restore_into(&mut copy, &bytes).unwrap();
                    prop_assert_eq!(snapshot_of(&copy), bytes);
                    bus = copy;
                }
            }
            assert_matches_model(&bus, &model);
            prop_assert_eq!(bus.next_downstream_txn(), downstream);
        }
    }

    /// `docs/m2-design.md` §10.4 and §16 risk 1: requests from different masters in one
    /// `Request` phase give the same queues, grants, cursors, and final state whatever
    /// their relative dispatch order. Each master's own requests keep their order.
    #[test]
    fn the_dispatch_order_across_masters_does_not_change_arbitration(
        masters in 2usize..=3,
        batches in prop::collection::vec(
            prop::collection::vec((0usize..3, 0u16..2), 0..8),
            1..5,
        ),
        keys in prop::collection::vec(0u8..8, 3),
    ) {
        // Assign each master's txns in its own arrival order, then build a second
        // dispatch order that regroups masters but keeps each master's order.
        let mut next_txn = vec![0u64; masters];
        let batches: Vec<Vec<(u16, u16, u64)>> = batches
            .into_iter()
            .map(|batch| {
                batch
                    .into_iter()
                    .map(|(m, r)| {
                        let m = m % masters;
                        let txn = next_txn[m];
                        next_txn[m] += 1;
                        (u16::try_from(m).unwrap(), r, txn)
                    })
                    .collect()
            })
            .collect();
        let permuted: Vec<Vec<(u16, u16, u64)>> = batches
            .iter()
            .map(|b| {
                let mut b = b.clone();
                b.sort_by_key(|&(m, _, _)| keys[usize::from(m)]);
                b
            })
            .collect();
        let reversed: Vec<Vec<(u16, u16, u64)>> = batches
            .iter()
            .map(|b| {
                let mut b = b.clone();
                b.sort_by_key(|&(m, _, _)| std::cmp::Reverse(m));
                b
            })
            .collect();

        let run = |order: &[Vec<(u16, u16, u64)>]| {
            let mut bus = bus_of(masters);
            let mut after_request = Vec::new();
            let mut grants = Vec::new();
            let mut cursors = Vec::new();
            for batch in order {
                for &(m, r, txn) in batch {
                    enqueue(&mut bus, m, read(txn, addr(r, u64::from(m)), 1));
                }
                after_request.push(snapshot_of(&bus));
                // One cycle: arbitrate, then complete what is active.
                let ctx = arbitrate(&mut bus);
                grants.extend(grants_of(&bus, &ctx));
                cursors.push((bus.rr_cursor(RAM), bus.rr_cursor(UART)));
                for s in &ctx.sent {
                    let r = s.port.0 - u16::try_from(masters).unwrap();
                    respond(&mut bus, r, ok_for(&s.msg)).unwrap();
                }
            }
            for step in drain(&mut bus) {
                grants.extend(step.1.iter().filter(|t| t.0 == GRANT_KIND).map(|t| {
                    let v = |i: usize| match t.1[i].1 {
                        Value::U64(v) => v,
                        _ => unreachable!(),
                    };
                    (
                        u16::try_from(v(0)).unwrap(),
                        u16::try_from(v(1)).unwrap(),
                        v(2),
                        v(3),
                    )
                }));
            }
            (after_request, grants, cursors, snapshot_of(&bus))
        };
        let a = run(&batches);
        let b = run(&permuted);
        let c = run(&reversed);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&a, &c);

        // And the grants are the model's.
        let mut model = Model::new(masters, 2);
        let mut want = Vec::new();
        for batch in &batches {
            for &(m, r, txn) in batch {
                model.enqueue(usize::from(r), usize::from(m), txn);
            }
            want.extend(model.transfer());
            for r in 0..2 {
                model.complete(r);
            }
        }
        loop {
            let g = model.transfer();
            if g.is_empty() {
                break;
            }
            want.extend(g);
            for r in 0..2 {
                model.complete(r);
            }
        }
        let got: Vec<(usize, usize, u64)> = a
            .1
            .iter()
            .map(|g| (usize::from(g.0), usize::from(g.1), g.2))
            .collect();
        prop_assert_eq!(got, want);
    }
}

// ---------------------------------------------------------------------------------------
// Runtime: timing, dispatch order, checkpoints
// ---------------------------------------------------------------------------------------

const SRAM_BASE: u64 = 0x9000_0000;
const TICKS_PER_CYCLE: u64 = 1000;

/// Keeps every view of one component taken after an event it handled.
struct Views(ComponentId, Rc<RefCell<Vec<(EventKey, StateView)>>>);

impl Observer for Views {
    fn on_after_dispatch(&mut self, ev: &EventView<'_>, world: &WorldView<'_>) -> Control {
        if ev.target == self.0 {
            let view = world.inspect(self.0).unwrap();
            self.1.borrow_mut().push((ev.key, view));
        }
        Control::Continue
    }
}

struct Ids {
    cpu: ComponentId,
    dma: ComponentId,
    bus: ComponentId,
    ram: ComponentId,
}

type Requests = Vec<(u64, MemMsg)>;

fn ram_of(clock: ClockDomainId, size: u64, hash: u8) -> Ram {
    Ram::new(
        RamConfig {
            size,
            latency: LinkLatency::Cycles {
                domain: clock,
                k: 0,
            },
        },
        &RamImage {
            image_hash: [hash; 32],
            segments: vec![],
        },
    )
    .unwrap()
}

fn add_clock(t: &mut TopologyBuilder) -> ClockDomainId {
    t.add_clock(
        Frequency::from_hz(1_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    )
    .unwrap()
}

/// Two scripted masters (`cpu` = 0, `dma0` = 1) → the bus → `ram` and `sram`, every link
/// `Cycles { 1 }`, RAMs responding `Cycles { 0 }`. With `swap`, the DMA script is declared
/// first, so its requests are dispatched first within a phase.
fn build(cpu: &[(u64, MemMsg)], dma: &[(u64, MemMsg)], swap: bool) -> (Runtime, Ids) {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = add_clock(&mut t);
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let script = |requests: &[(u64, MemMsg)]| {
        Box::new(Script {
            clock,
            requests: requests.to_vec(),
        })
    };
    let (cpu_id, dma_id) = if swap {
        let d = t.add_component("soc.dma", script(dma));
        let c = t.add_component("soc.cpu", script(cpu));
        (c, d)
    } else {
        let c = t.add_component("soc.cpu", script(cpu));
        let d = t.add_component("soc.dma", script(dma));
        (c, d)
    };
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: vec!["cpu", "dma0"],
        regions: vec![
            region("ram", RAM_BASE, RAM_SIZE),
            region("sram", SRAM_BASE, RAM_SIZE),
        ],
        clock,
    })
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = t.add_component("soc.ram", Box::new(ram_of(clock, RAM_SIZE, 1)));
    let sram = t.add_component("soc.sram", Box::new(ram_of(clock, RAM_SIZE, 2)));
    t.connect((cpu_id, "mem"), (bus, "cpu"), Some(cycles(1)));
    t.connect((dma_id, "mem"), (bus, "dma0"), Some(cycles(1)));
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
    t.connect((bus, "sram"), (sram, "mem"), Some(cycles(1)));
    let rt = t.elaborate(SessionConfig::default()).unwrap();
    (
        rt,
        Ids {
            cpu: cpu_id,
            dma: dma_id,
            bus,
            ram,
        },
    )
}

/// Both masters contend for `ram` (the same txns on purpose), use `sram` concurrently,
/// and the DMA also sends an unmapped read.
fn contention() -> (Requests, Requests) {
    let cpu = vec![
        (0, read(1, RAM_BASE, 4)),
        (0, write(2, RAM_BASE + 8, &[1, 2, 3, 4])),
        (1, read(3, SRAM_BASE, 8)),
        (6, read(4, RAM_BASE + 8, 4)),
    ];
    let dma = vec![
        (0, write(1, RAM_BASE + 16, &[9; 16])),
        (0, read(2, RAM_BASE + 16, 16)),
        (1, write(3, SRAM_BASE + 4, &[5; 4])),
        (6, read(4, RAM_BASE, 4)),
        (7, read(5, 0x10, 4)),
    ];
    (cpu, dma)
}

/// The grant records of a trace, with the tick of the event that made each.
/// A traced record: the tick and phase of its event, and its fields.
type TracedAt = (u64, Phase, Vec<(&'static str, Value)>);

fn traced_grants(trace: &Trace, bus: ComponentId) -> Vec<TracedAt> {
    trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == bus)
        .filter(|r| r.kind == GRANT_KIND)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0, key.phase, r.fields.clone())
        })
        .collect()
}

fn master_of(fields: &[(&'static str, Value)]) -> u64 {
    match fields[1].1 {
        Value::U64(m) => m,
        _ => unreachable!(),
    }
}

fn region_of(fields: &[(&'static str, Value)]) -> u64 {
    match fields[0].1 {
        Value::U64(r) => r,
        _ => unreachable!(),
    }
}

#[test]
fn contention_follows_the_frozen_timing() {
    let (cpu, dma) = contention();
    let (mut rt, ids) = build(&cpu, &dma, false);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    let trace = rt.take_trace().unwrap();
    let grants = traced_grants(&trace, ids.bus);

    // RAM: requests reach the bus at cycle 1 and 7; a RAM round trip frees the region
    // in COMPLETE two cycles after its grant, and the next grant is one cycle later.
    let ram: Vec<(u64, u64)> = grants
        .iter()
        .filter(|g| region_of(&g.2) == 0)
        .map(|g| (g.0 / TICKS_PER_CYCLE, master_of(&g.2)))
        .collect();
    assert_eq!(ram, [(1, 0), (4, 1), (7, 0), (10, 1), (13, 0), (16, 1)]);
    // SRAM, concurrently with RAM: both masters' requests arrive at cycle 2.
    let sram: Vec<(u64, u64)> = grants
        .iter()
        .filter(|g| region_of(&g.2) == 1)
        .map(|g| (g.0 / TICKS_PER_CYCLE, master_of(&g.2)))
        .collect();
    assert_eq!(sram, [(2, 0), (5, 1)]);
    assert!(grants.iter().all(|g| g.1 == Phase::Transfer));
    assert!(grants.iter().all(|g| g.0 % TICKS_PER_CYCLE == 0));

    // Each RAM completion is in COMPLETE at grant + 2 cycles; the following grant is in
    // the next cycle's TRANSFER, never the same tick.
    let completions: Vec<u64> = events
        .iter()
        .filter(|e| e.target == ids.bus && e.source == ids.ram)
        .map(|e| {
            assert_eq!(e.key.phase, Phase::Complete);
            e.key.tick.0 / TICKS_PER_CYCLE
        })
        .collect();
    assert_eq!(completions, [3, 6, 9, 12, 15, 18]);

    // Every master gets every response, with its own txns, in COMPLETE.
    let responses = |id| -> Vec<(u64, TxnId)> {
        events
            .iter()
            .filter(|e| e.target == id)
            .map(|e| match &e.delivery {
                Delivered::Message {
                    msg: Message::MemV1(m),
                    ..
                } => {
                    assert_eq!(e.key.phase, Phase::Complete);
                    (e.key.tick.0 / TICKS_PER_CYCLE, txn_of(m))
                }
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(
        responses(ids.cpu),
        [(4, TxnId(1)), (5, TxnId(3)), (10, TxnId(2)), (16, TxnId(4))]
    );
    assert_eq!(
        responses(ids.dma),
        [
            (7, TxnId(1)),
            (8, TxnId(3)),
            (9, TxnId(5)),
            (13, TxnId(2)),
            (19, TxnId(4))
        ]
    );
    let faults: Vec<_> = trace
        .records
        .iter()
        .filter(|r| r.component == ids.bus && r.kind == FAULT_KIND)
        .map(|r| r.fields.clone())
        .collect();
    assert_eq!(faults, [fault_record(5, 0x10, 4, DMA).1]);
}

/// An uncontended request (its region idle) is forwarded in the same tick's `Transfer`,
/// as `AddressBus` forwards: a lone master sees identical response times through either
/// bus.
#[test]
fn uncontended_requests_take_exactly_as_long_as_through_the_address_bus() {
    let requests: Requests = vec![
        (0, read(1, RAM_BASE, 4)),
        (5, write(2, RAM_BASE + 4, &[1])),
        // After the write's response: the region is idle again.
        (9, read(3, RAM_BASE + 4, 1)),
        (20, read(4, RAM_BASE + RAM_SIZE, 1)),
    ];
    let run = |multi: bool| {
        let mut t = TopologyBuilder::new(SimulationClock::default());
        let clock = add_clock(&mut t);
        let cycles = |k| LinkLatency::Cycles { domain: clock, k };
        let script = t.add_component(
            "soc.cpu",
            Box::new(Script {
                clock,
                requests: requests.clone(),
            }),
        );
        let regions = vec![region("ram", RAM_BASE, RAM_SIZE)];
        let bus: Box<dyn Component> = if multi {
            Box::new(
                MultiMasterBus::new(MultiMasterBusConfig {
                    masters: vec!["cpu"],
                    regions,
                    clock,
                })
                .unwrap(),
            )
        } else {
            Box::new(AddressBus::new(regions).unwrap())
        };
        let bus = t.add_component("soc.bus", bus);
        let ram = t.add_component("soc.ram", Box::new(ram_of(clock, RAM_SIZE, 1)));
        t.connect((script, "mem"), (bus, "cpu"), Some(cycles(1)));
        t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
        let mut rt = t.elaborate(SessionConfig::default()).unwrap();
        rt.init().unwrap();
        let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
        let at = |id| -> Vec<(u64, Phase, Delivered)> {
            events
                .iter()
                .filter(|e| e.target == id)
                .map(|e| (e.key.tick.0, e.key.phase, e.delivery.clone()))
                .collect()
        };
        (
            at(script),
            at(ram).len(),
            at(ram).iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(),
        )
    };
    let (via_address, ram_count, ram_times) = run(false);
    let (via_multi, multi_ram_count, multi_ram_times) = run(true);
    assert_eq!(via_multi, via_address);
    assert_eq!((multi_ram_count, multi_ram_times), (ram_count, ram_times));
    assert_eq!(via_multi.len(), 4);
}

/// Declaring the DMA script first dispatches its same-phase requests first; grants,
/// cursors, responses, and the final state do not change.
#[test]
fn the_dispatch_order_of_masters_changes_no_observable_outcome() {
    let (cpu, dma) = contention();
    let mut outcomes = Vec::new();
    for swap in [false, true] {
        let (mut rt, ids) = build(&cpu, &dma, swap);
        let views = Rc::new(RefCell::new(Vec::new()));
        rt.add_observer(Box::new(Views(ids.bus, Rc::clone(&views))));
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
        let trace = rt.take_trace().unwrap();
        let first_at_bus = events
            .iter()
            .find(|e| e.target == ids.bus)
            .map(|e| e.source)
            .unwrap();
        let first_master = if first_at_bus == ids.cpu { 0 } else { 1 };
        // The bus's state after every TRANSFER and COMPLETE event: the cursor evolution
        // and queue contents at each arbitration and completion.
        let settled: Vec<(u64, Phase, StateView)> = views
            .borrow()
            .iter()
            .filter(|(k, _)| matches!(k.phase, Phase::Transfer | Phase::Complete))
            .map(|(k, v)| (k.tick.0, k.phase, v.clone()))
            .collect();
        let grants = traced_grants(&trace, ids.bus);
        // Component ids follow declaration order, so name masters by index.
        let mut responses: Vec<(u64, u16, Delivered)> = events
            .iter()
            .filter(|e| e.target == ids.cpu || e.target == ids.dma)
            .map(|e| {
                let master = if e.target == ids.cpu { CPU } else { DMA };
                (e.key.tick.0, master, e.delivery.clone())
            })
            .collect();
        responses.sort_by_key(|r| (r.0, r.1));
        let last = views.borrow().last().unwrap().1.clone();
        outcomes.push((first_master, grants, settled, responses, last));
    }
    let (a, b) = (&outcomes[0], &outcomes[1]);
    assert_ne!(a.0, b.0, "the two runs dispatch opposite masters first");
    assert_eq!(a.1, b.1, "grants");
    assert_eq!(a.2, b.2, "cursor and queue evolution");
    assert_eq!(a.3, b.3, "responses");
    assert_eq!(a.4, b.4, "final bus state");
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let (cpu, dma) = contention();
    let reference = {
        let (mut rt, _) = build(&cpu, &dma, false);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (events, rt.state_digest().unwrap(), rt.execution_digest())
    };
    for k in 0..=reference.0.len() {
        let (mut rt, _) = build(&cpu, &dma, false);
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let (mut fresh, _) = build(&cpu, &dma, false);
        fresh.restore(&bytes).unwrap();
        assert_eq!(
            fresh.snapshot().unwrap(),
            bytes,
            "checkpoint after {k} events"
        );
        let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.1, reference.2),
            "checkpoint after {k} events"
        );
    }
}

#[test]
fn queued_requests_are_not_duplicated_or_lost_across_a_restore() {
    // Right after both masters' first requests are enqueued, before the TRANSFER wake:
    // the snapshot holds them only in the FIFOs, the wakes only in the runtime queue.
    let (cpu, dma) = contention();
    let (mut rt, ids) = build(&cpu, &dma, false);
    rt.init().unwrap();
    loop {
        let ev = rt.step().unwrap().unwrap();
        if ev.target == ids.bus && ev.key.phase == Phase::Request && ev.source == ids.dma {
            break;
        }
    }
    let bytes = rt.snapshot().unwrap();
    let (mut fresh, ids2) = build(&cpu, &dma, false);
    fresh.restore(&bytes).unwrap();
    let rest: Vec<Dispatched> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
    assert_eq!(fresh.fault(), None);
    // No RAM request has been forwarded yet, so every one of the six arrives after the
    // restore, exactly once, with distinct downstream txns.
    let to_ram: Vec<TxnId> = rest
        .iter()
        .filter(|e| e.target == ids2.ram)
        .map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::MemV1(m),
                ..
            } => txn_of(m),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(to_ram.len(), 6);
    let mut unique = to_ram.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 6);
}
