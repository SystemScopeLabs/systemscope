//! Directed ToyBus tests: arbitration, remapping, and routing (`docs/m0-design.md` §9.1).
//!
//! Scripted initiators send fixed requests at fixed times, so every grant is predictable.
//! Everything runs on one 1 GHz bus clock with one-cycle links: a request sent at `t`
//! reaches the bus on the first bus edge after `t`, in `Request`.

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{DISPATCH_KIND, TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_runtime::runtime::{Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_toy::{ToyBus, ToyBusConfig, ToyMemory, ToyMemoryConfig};

const A: ComponentId = ComponentId(0);
const B: ComponentId = ComponentId(1);
const BUS: ComponentId = ComponentId(2);
const MEM: ComponentId = ComponentId(3);

/// Sends each request at its time in picoseconds, from the phase given, and ignores
/// responses. A deferred script first wakes itself once more in the same phase, so its
/// sends get later sequence numbers than every other send at that tick.
struct Script {
    steps: Vec<(u64, Phase, MemMsg)>,
    deferred: bool,
}

fn script(steps: Vec<(u64, Phase, MemMsg)>) -> Script {
    Script {
        steps,
        deferred: false,
    }
}

/// Wake tokens at or above this send step `token - DEFERRED`.
const DEFERRED: u64 = 1 << 32;

impl Component for Script {
    fn type_name(&self) -> &'static str {
        "test.script"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for (token, (ps, phase, _)) in (0u64..).zip(&self.steps) {
            ctx.wake_self(ScheduleWhen::After(Duration::from_ps(*ps)), *phase, token)?;
        }
        Ok(())
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if let Delivered::Wake { token } = ev {
            if self.deferred && *token < DEFERRED {
                let phase = self.steps[*token as usize].1;
                return ctx.wake_self(ScheduleWhen::Now, phase, token + DEFERRED);
            }
            let (_, phase, msg) = self.steps[(token % DEFERRED) as usize].clone();
            ctx.send(PortId(0), msg.into(), ScheduleWhen::Now, phase)?;
        }
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// A target that answers every request with `answer(txn)` after 5 ns.
struct Liar(fn(TxnId) -> MemMsg);

impl Component for Liar {
    fn type_name(&self) -> &'static str {
        "test.liar"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Target,
        }]
    }
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            msg: Message::Mem(MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. }),
            ..
        } = ev
        else {
            return Ok(());
        };
        let after = ScheduleWhen::After(Duration::from_ns(5));
        ctx.send(PortId(0), (self.0)(*txn).into(), after, Phase::Complete)
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

fn read(txn: u64, addr: u64) -> MemMsg {
    MemMsg::ReadReq {
        txn: TxnId(txn),
        addr,
        len: 8,
    }
}

fn write(txn: u64, addr: u64, byte: u8) -> MemMsg {
    MemMsg::WriteReq {
        txn: TxnId(txn),
        addr,
        data: vec![byte; 8],
    }
}

fn at(ns: u64, msg: MemMsg) -> (u64, Phase, MemMsg) {
    (ns * 1_000, Phase::Request, msg)
}

fn elaborate(a: Script, b: Script, target: Box<dyn Component>) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let a = t.add_component("a", Box::new(a));
    let b = t.add_component("b", Box::new(b));
    let bus = t.add_component("bus", Box::new(ToyBus::new(ToyBusConfig { clock })));
    let mem = t.add_component("mem", target);
    let link = Some(LinkLatency::Cycles {
        domain: clock,
        k: 1,
    });
    t.connect((a, "mem"), (bus, "cpu"), link);
    t.connect((b, "mem"), (bus, "dma"), link);
    t.connect((bus, "mem"), (mem, "mem"), link);
    t.elaborate(SessionConfig::default()).unwrap()
}

fn build_with(a: Script, b: Script, target: Box<dyn Component>) -> Runtime {
    let mut rt = elaborate(a, b, target);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    rt
}

fn memory() -> Box<dyn Component> {
    Box::new(ToyMemory::new(ToyMemoryConfig {
        size: 1024,
        read_latency: Duration::from_ns(50),
        write_latency: Duration::from_ns(30),
    }))
}

fn build(a: Script, b: Script) -> Runtime {
    build_with(a, b, memory())
}

fn run(a: Script, b: Script) -> Trace {
    let mut rt = build(a, b);
    while rt.step().unwrap().is_some() {}
    rt.take_trace().unwrap()
}

fn u64_field(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

/// Each grant as `(tick, phase, port, upstream txn, downstream txn, contended)`.
fn grants(trace: &Trace) -> Vec<(u64, Phase, u64, u64, u64, bool)> {
    trace
        .records
        .iter()
        .filter(|r| r.kind == "toy.bus.grant")
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("grant outside an event")
            };
            let contended = match r.fields[3] {
                ("contended", Value::Bool(c)) => c,
                _ => panic!("contended"),
            };
            (
                key.tick.0,
                key.phase,
                u64_field(r, "port"),
                u64_field(r, "txn"),
                u64_field(r, "downstream"),
                contended,
            )
        })
        .collect()
}

/// Messages delivered to `target`, as `(tick, phase, msg name, txn, fields after txn)`.
fn delivered(trace: &Trace, target: ComponentId) -> Vec<(u64, Phase, String, u64, Vec<Value>)> {
    trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND)
        .filter(|r| r.component == target && r.fields.len() > 2)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("dispatch outside an event")
            };
            let Value::Str(msg) = &r.fields[4].1 else {
                panic!("msg")
            };
            let rest = r.fields[6..].iter().map(|(_, v)| v.clone()).collect();
            (
                key.tick.0,
                key.phase,
                msg.clone(),
                u64_field(r, "txn"),
                rest,
            )
        })
        .collect()
}

#[test]
fn a_lone_request_is_granted_on_the_edge_it_arrives() {
    let trace = run(script(vec![at(0, read(0, 0))]), script(Vec::new()));
    // Sent at 0, on the bus at the 1 ns edge in REQUEST, granted there in TRANSFER.
    assert_eq!(grants(&trace), [(1_000, Phase::Transfer, 0, 0, 0, false)]);
    // The memory sees it one bus cycle later, still in TRANSFER.
    let at_mem = delivered(&trace, MEM);
    assert_eq!(at_mem[0].0, 2_000);
    assert_eq!(at_mem[0].1, Phase::Transfer);
}

#[test]
fn contention_alternates_starting_with_the_cpu_port_one_grant_per_edge() {
    let three = |base, deferred| Script {
        steps: (0..3).map(|i| at(0, read(i, base + 8 * i))).collect(),
        deferred,
    };
    // Port 0's sends are deferred, so port 1's requests reach the bus first within the
    // 1 ns edge's REQUEST phase. Port 0 still wins the first round.
    let trace = run(three(0, true), three(64, false));
    let arrivals: Vec<_> = trace
        .records
        .iter()
        .filter(|r| r.kind == DISPATCH_KIND && r.component == BUS)
        .filter(|r| r.fields.len() > 2)
        .map(|r| u64_field(r, "source"))
        .take(6)
        .collect();
    assert_eq!(arrivals, [1, 1, 1, 0, 0, 0]);
    assert_eq!(
        grants(&trace),
        [
            (1_000, Phase::Transfer, 0, 0, 0, true),
            (2_000, Phase::Transfer, 1, 0, 1, true),
            (3_000, Phase::Transfer, 0, 1, 2, true),
            (4_000, Phase::Transfer, 1, 1, 3, true),
            (5_000, Phase::Transfer, 0, 2, 4, true),
            (6_000, Phase::Transfer, 1, 2, 5, false),
        ]
    );
}

#[test]
fn the_pointer_moves_after_uncontended_grants_too() {
    // Port 0 wins alone at 1 ns, so port 1 has priority when both contend at 2 ns.
    let a = script(vec![at(0, read(0, 0)), at(1, read(1, 8))]);
    let b = script(vec![at(1, read(0, 64))]);
    let g = grants(&run(a, b));
    assert_eq!(
        g.iter().map(|g| (g.0, g.2, g.5)).collect::<Vec<_>>(),
        [(1_000, 0, false), (2_000, 1, true), (3_000, 0, false)]
    );
    // Symmetrically, port 1 alone first hands priority back to port 0.
    let a = script(vec![at(1, read(0, 0))]);
    let b = script(vec![at(0, read(0, 64)), at(1, read(1, 72))]);
    let g = grants(&run(a, b));
    assert_eq!(
        g.iter().map(|g| (g.0, g.2, g.5)).collect::<Vec<_>>(),
        [(1_000, 1, false), (2_000, 0, true), (3_000, 1, false)]
    );
}

#[test]
fn a_queue_is_served_in_arrival_order() {
    let a = script(vec![
        at(0, read(5, 0)),
        at(0, read(3, 8)),
        at(0, read(9, 16)),
    ]);
    let g = grants(&run(a, script(Vec::new())));
    assert_eq!(g.iter().map(|g| g.3).collect::<Vec<_>>(), [5, 3, 9]);
}

/// Both initiators use TxnId 7 at the same time; A then reads what B wrote.
fn same_txn_on_both_ports() -> (Script, Script) {
    let a = script(vec![at(0, read(7, 64)), at(100, read(8, 64))]);
    let b = script(vec![at(0, write(7, 64, 0xAB))]);
    (a, b)
}

#[test]
fn equal_upstream_txns_are_remapped_and_routed_back_to_their_initiators() {
    let (a, b) = same_txn_on_both_ports();
    let trace = run(a, b);
    let at_mem: Vec<_> = delivered(&trace, MEM)
        .into_iter()
        .map(|d| (d.2, d.3))
        .collect();
    assert_eq!(
        at_mem,
        [
            ("ReadReq".to_owned(), 0),
            ("WriteReq".to_owned(), 1),
            ("ReadReq".to_owned(), 2),
        ]
    );
    let to_a: Vec<_> = delivered(&trace, A)
        .into_iter()
        .map(|d| (d.1, d.2, d.3, d.4))
        .collect();
    assert_eq!(
        to_a,
        [
            (
                Phase::Complete,
                "ReadResp".to_owned(),
                7,
                vec![Value::Bytes(vec![0; 8])]
            ),
            (
                Phase::Complete,
                "ReadResp".to_owned(),
                8,
                vec![Value::Bytes(vec![0xAB; 8])]
            ),
        ]
    );
    let to_b: Vec<_> = delivered(&trace, B)
        .into_iter()
        .map(|d| (d.2, d.3))
        .collect();
    assert_eq!(to_b, [("WriteResp".to_owned(), 7)]);
    let routes: Vec<_> = trace
        .records
        .iter()
        .filter(|r| r.kind == "toy.bus.route")
        .map(|r| {
            (
                u64_field(r, "downstream"),
                u64_field(r, "port"),
                u64_field(r, "txn"),
            )
        })
        .collect();
    // The write is faster, so its response comes back first.
    assert_eq!(routes, [(1, 1, 7), (0, 0, 7), (2, 0, 8)]);
}

#[test]
fn a_restored_bus_keeps_allocating_after_its_newest_downstream_txn() {
    let (a, b) = same_txn_on_both_ports();
    let reference = run(a, b);
    // Stop right after downstream 1 (the write) was routed back while downstream 0 (the
    // read) is still at the memory: the live routes then only reach downstream 0, and the
    // next grant must still take downstream 2.
    let routed = reference
        .records
        .iter()
        .position(|r| r.kind == "toy.bus.route" && u64_field(r, "downstream") == 1)
        .unwrap();
    let k = reference.records[..routed]
        .iter()
        .filter(|r| r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND)
        .count();
    let (a, b) = same_txn_on_both_ports();
    let mut rt = build(a, b);
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    let (snapshot, prefix) = (rt.snapshot().unwrap(), rt.take_trace().unwrap());
    drop(rt);

    let (a, b) = same_txn_on_both_ports();
    let mut rt = elaborate(a, b, memory());
    rt.restore(&snapshot).unwrap();
    rt.resume_trace(prefix).unwrap();
    while rt.step().unwrap().is_some() {}
    let resumed = rt.take_trace().unwrap();
    assert_eq!(grants(&resumed).last().unwrap().4, 2);
    assert_eq!(resumed.canonical_bytes(), reference.canonical_bytes());
}

fn fault(rt: &mut Runtime) -> RuntimeError {
    loop {
        match rt.step() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("ran to completion"),
            Err(e) => return e,
        }
    }
}

#[test]
fn a_request_after_request_phase_faults() {
    let late = script(vec![(0, Phase::Transfer, read(0, 0))]);
    let mut rt = build(late, script(Vec::new()));
    assert_eq!(
        fault(&mut rt),
        RuntimeError::Faulted(SimError::ComponentFault(
            "toy bus: request arrived after REQUEST"
        ))
    );
}

#[test]
fn unknown_or_mismatched_responses_fault() {
    type Answer = fn(TxnId) -> MemMsg;
    let cases: [(Answer, &str); 2] = [
        (
            |txn| MemMsg::WriteResp {
                txn: TxnId(txn.0 + 1),
            },
            "toy bus: response for unknown txn",
        ),
        (
            |txn| MemMsg::WriteResp { txn },
            "toy bus: response kind mismatch",
        ),
    ];
    for (answer, error) in cases {
        let a = script(vec![at(0, read(0, 0))]);
        let mut rt = build_with(a, script(Vec::new()), Box::new(Liar(answer)));
        assert_eq!(
            fault(&mut rt),
            RuntimeError::Faulted(SimError::ComponentFault(error))
        );
    }
}
