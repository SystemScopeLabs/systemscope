//! Trace recording in the runtime (`docs/m0-design.md` §8.1): record placement, the
//! header, dispatch-record fields, lifecycle rules, and exact JSONL integers.

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase, ScheduleWhen};
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::time::{Duration, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{
    CONTRACTS_VERSION, DISPATCH_KIND, TraceAt, TraceHeader, TraceOrigin, TraceRecord, Value,
};
use systemscope_runtime::export::to_jsonl;
use systemscope_runtime::runtime::{Lifecycle, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;

/// Sends one read at init; traces at init and on the response.
struct Requester;

impl Component for Requester {
    fn type_name(&self) -> &'static str {
        "test.requester"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        ctx.trace("req.init", vec![("n", Value::U64(1))]);
        let req = MemMsg::ReadReq {
            txn: TxnId(9),
            addr: 0x40,
            len: 2,
        };
        ctx.send(PortId(0), req.into(), ScheduleWhen::Now, Phase::Request)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if let Delivered::Wake { .. } = ev {
            return Ok(());
        }
        ctx.trace("req.done", Vec::new());
        ctx.wake_self(ScheduleWhen::Now, Phase::Commit, 5)
    }
}

/// Answers reads with two bytes after 10 ns.
struct Responder;

impl Component for Responder {
    fn type_name(&self) -> &'static str {
        "test.responder"
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
        ctx.trace("resp.got", vec![("first", Value::Bool(true))]);
        let Delivered::Message { port, .. } = ev else {
            return Ok(());
        };
        let resp = MemMsg::ReadResp {
            txn: TxnId(9),
            data: vec![0xDE, 0xAD],
        };
        let after = ScheduleWhen::After(Duration::from_ns(10));
        ctx.send(*port, resp.into(), after, Phase::Complete)
    }
}

fn build() -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let r = t.add_component("req", Box::new(Requester));
    let s = t.add_component("resp", Box::new(Responder));
    t.connect(
        (r, "mem"),
        (s, "mem"),
        Some(LinkLatency::After(Duration::from_ns(1))),
    );
    let config = SessionConfig {
        seed: 77,
        ..SessionConfig::default()
    };
    t.elaborate(config).unwrap()
}

fn traced_run() -> Trace {
    let mut rt = build();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    rt.take_trace().unwrap()
}

fn key(tick: u64, phase: Phase, sequence: u64) -> TraceAt {
    TraceAt::Event(EventKey {
        tick: Tick(tick),
        phase,
        sequence,
    })
}

fn component(
    at: TraceAt,
    id: u32,
    kind: &'static str,
    fields: Vec<(&'static str, Value)>,
) -> TraceRecord {
    TraceRecord {
        at,
        origin: TraceOrigin::Component,
        component: ComponentId(id),
        kind,
        fields,
    }
}

fn dispatch(at: TraceAt, target: u32, fields: Vec<(&'static str, Value)>) -> TraceRecord {
    TraceRecord {
        at,
        origin: TraceOrigin::Runtime,
        component: ComponentId(target),
        kind: DISPATCH_KIND,
        fields,
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}

#[test]
fn records_are_placed_and_filled_as_specified() {
    let trace = traced_run();
    let req_at = key(1_000, Phase::Request, 0);
    let resp_at = key(12_000, Phase::Complete, 1);
    let wake_at = key(12_000, Phase::Commit, 2);
    let expected = vec![
        // Init records carry no event key.
        component(TraceAt::Init, 0, "req.init", vec![("n", Value::U64(1))]),
        // Each dispatch record precedes the records its handler emits, and names the target.
        dispatch(
            req_at,
            1,
            vec![
                ("source", Value::U64(0)),
                ("port", Value::U64(0)),
                ("protocol", s("mem")),
                ("version", Value::U64(0)),
                ("msg", s("ReadReq")),
                ("txn", Value::U64(9)),
                ("addr", Value::U64(0x40)),
                ("len", Value::U64(2)),
            ],
        ),
        component(req_at, 1, "resp.got", vec![("first", Value::Bool(true))]),
        dispatch(
            resp_at,
            0,
            vec![
                ("source", Value::U64(1)),
                ("port", Value::U64(0)),
                ("protocol", s("mem")),
                ("version", Value::U64(0)),
                ("msg", s("ReadResp")),
                ("txn", Value::U64(9)),
                ("data", Value::Bytes(vec![0xDE, 0xAD])),
            ],
        ),
        component(resp_at, 0, "req.done", Vec::new()),
        dispatch(
            wake_at,
            0,
            vec![("source", Value::U64(0)), ("token", Value::U64(5))],
        ),
    ];
    assert_eq!(trace.records, expected);
}

#[test]
fn header_describes_the_session() {
    let trace = traced_run();
    let TraceHeader {
        ticks_per_second,
        seed,
        contracts_version,
        clock_domains,
        components,
        links,
    } = trace.header;
    assert_eq!(ticks_per_second, 1_000_000_000_000);
    assert_eq!(seed, 77);
    assert_eq!(contracts_version, CONTRACTS_VERSION);
    assert!(clock_domains.is_empty());
    let names: Vec<_> = components
        .iter()
        .map(|c| (c.path.as_str(), c.type_name, c.ports.len()))
        .collect();
    assert_eq!(
        names,
        [("req", "test.requester", 1), ("resp", "test.responder", 1)]
    );
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].a, (ComponentId(0), PortId(0)));
    assert_eq!(links[0].b, (ComponentId(1), PortId(0)));
    assert_eq!(
        links[0].latency,
        Some(LinkLatency::After(Duration::from_ns(1)))
    );
}

#[test]
fn identical_runs_have_identical_digests() {
    let a = traced_run();
    let b = traced_run();
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.digest(), *blake3::hash(&a.canonical_bytes()).as_bytes());
    // Any record change moves the digest.
    let mut c = a.clone();
    c.records.pop();
    assert_ne!(a.digest(), c.digest());
}

#[test]
fn tracing_starts_only_before_init() {
    let mut rt = build();
    assert!(rt.take_trace().is_none());
    rt.init().unwrap();
    assert_eq!(
        rt.start_trace(),
        Err(RuntimeError::InvalidState(Lifecycle::Ready))
    );
    assert!(rt.take_trace().is_none());
}

#[test]
fn jsonl_keeps_64_bit_integers_exact() {
    let mut trace = traced_run();
    trace.records.push(TraceRecord {
        at: key(u64::MAX, Phase::Observe, u64::MAX),
        origin: TraceOrigin::Component,
        component: ComponentId(u32::MAX),
        kind: "big",
        fields: vec![("u", Value::U64(u64::MAX)), ("i", Value::I64(i64::MIN))],
    });
    let jsonl = to_jsonl(&trace);
    let last: serde_json::Value = serde_json::from_str(jsonl.lines().last().unwrap()).unwrap();
    assert_eq!(last["at"]["tick"].as_u64(), Some(u64::MAX));
    assert_eq!(last["at"]["sequence"].as_u64(), Some(u64::MAX));
    assert_eq!(last["fields"][0][1]["u64"].as_u64(), Some(u64::MAX));
    assert_eq!(last["fields"][1][1]["i64"].as_i64(), Some(i64::MIN));
    // Written as integer literals, not floats or strings.
    assert!(jsonl.contains("\"tick\":18446744073709551615,"));
}
