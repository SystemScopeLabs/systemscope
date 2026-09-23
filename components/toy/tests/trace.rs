//! Tracing a real toy run: observation invariance and exporter consistency
//! (`docs/m0-design.md` §8.1).

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use serde_json::Value as Json;
use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{DISPATCH_KIND, TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_runtime::export::{perfetto_ts, to_jsonl, to_perfetto};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_toy::{ToyCpu, ToyCpuConfig, ToyMemory, ToyMemoryConfig};

fn build(seed: u64) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(3_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cpu = ToyCpu::new(ToyCpuConfig {
        clock,
        ops: 400,
        max_outstanding: 4,
        max_think_cycles: NonZeroU64::new(8).unwrap(),
        access_len: 8,
        slots: 32,
        write_percent: 40,
    });
    let memory = ToyMemory::new(ToyMemoryConfig {
        size: 32 * 8,
        read_latency: Duration::from_ns(50),
        write_latency: Duration::from_ns(30),
    });
    let c = t.add_component("soc.cpu0", Box::new(cpu));
    let m = t.add_component("soc.mem", Box::new(memory));
    let link = LinkLatency::Cycles {
        domain: clock,
        k: 2,
    };
    t.connect((c, "mem"), (m, "mem"), Some(link));
    let config = SessionConfig {
        seed,
        ..SessionConfig::default()
    };
    t.elaborate(config).unwrap()
}

/// Runs to completion, returning every dispatch and the trace if one was recorded.
fn run(seed: u64, traced: bool) -> (Vec<Dispatched>, Option<Trace>) {
    let mut rt = build(seed);
    if traced {
        rt.start_trace().unwrap();
    }
    rt.init().unwrap();
    let dispatched = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.pending(), 0);
    (dispatched, rt.take_trace())
}

#[test]
fn tracing_does_not_change_execution() {
    for seed in [0, 1, 0xDEAD_BEEF] {
        let (plain, none) = run(seed, false);
        let (traced, trace) = run(seed, true);
        assert!(none.is_none());
        // Same events, keys (so sequence numbers), sources, targets, and payloads.
        assert_eq!(plain, traced, "seed {seed}");
        let trace = trace.unwrap();
        let dispatches = trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Runtime)
            .count();
        assert_eq!(dispatches, plain.len());
        assert!(trace.records.len() > dispatches, "components traced too");
    }
}

#[test]
fn digest_is_stable_and_seed_sensitive() {
    let digest = |seed| run(seed, true).1.unwrap().digest();
    assert_eq!(digest(3), digest(3));
    assert_ne!(digest(3), digest(4));
}

#[test]
fn component_records_carry_the_running_event() {
    let (_, trace) = run(1, true);
    let trace = trace.unwrap();
    let mut current = None;
    for r in &trace.records {
        match r.origin {
            TraceOrigin::Runtime => current = Some(r.at),
            TraceOrigin::Component => match r.at {
                // ToyCpu schedules its first issue in init but traces nothing there.
                TraceAt::Init => panic!("unexpected init record {r:?}"),
                at => assert_eq!(Some(at), current, "record outside its event: {r:?}"),
            },
        }
    }
}

/// A record rebuilt from JSON in an owned form, for comparison with the original.
#[derive(Debug, PartialEq)]
struct Owned {
    at: Option<(u64, String, u64)>,
    origin: String,
    component: u64,
    kind: String,
    fields: Vec<(String, String, Json)>,
}

fn owned(r: &TraceRecord) -> Owned {
    Owned {
        at: match r.at {
            TraceAt::Init => None,
            TraceAt::Event(k) => Some((k.tick.0, k.phase.to_string(), k.sequence)),
        },
        origin: match r.origin {
            TraceOrigin::Component => "component",
            TraceOrigin::Runtime => "runtime",
        }
        .to_owned(),
        component: u64::from(r.component.0),
        kind: r.kind.to_owned(),
        fields: r
            .fields
            .iter()
            .map(|(n, v)| {
                let (tag, json) = match v {
                    Value::U64(x) => ("u64", Json::from(*x)),
                    Value::I64(x) => ("i64", Json::from(*x)),
                    Value::Bool(x) => ("bool", Json::from(*x)),
                    Value::Str(x) => ("str", Json::from(x.clone())),
                    Value::Bytes(x) => (
                        "bytes",
                        Json::from(x.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                    ),
                };
                ((*n).to_owned(), tag.to_owned(), json)
            })
            .collect(),
    }
}

fn fields_from_json(fields: &Json) -> Vec<(String, String, Json)> {
    fields
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            let name = pair[0].as_str().unwrap().to_owned();
            let (tag, value) = pair[1].as_object().unwrap().iter().next().unwrap();
            (name, tag.clone(), value.clone())
        })
        .collect()
}

#[test]
fn jsonl_represents_every_record_exactly() {
    let trace = run(5, true).1.unwrap();
    let jsonl = to_jsonl(&trace);
    let mut lines = jsonl.lines();
    let header: Json = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["type"], "header");
    assert_eq!(header["ticks_per_second"].as_u64(), Some(1_000_000_000_000));
    assert_eq!(header["seed"].as_u64(), Some(5));
    assert_eq!(header["components"][1]["path"], "soc.mem");
    assert_eq!(
        header["links"][0]["latency"]["cycles"]["k"].as_u64(),
        Some(2)
    );

    let rebuilt: Vec<Owned> = lines
        .enumerate()
        .map(|(i, line)| {
            let j: Json = serde_json::from_str(line).unwrap();
            assert_eq!(j["type"], "record");
            assert_eq!(j["index"].as_u64(), Some(i as u64));
            Owned {
                at: match &j["at"] {
                    Json::String(s) if s == "init" => None,
                    at => Some((
                        at["tick"].as_u64().unwrap(),
                        at["phase"].as_str().unwrap().to_owned(),
                        at["sequence"].as_u64().unwrap(),
                    )),
                },
                origin: j["origin"].as_str().unwrap().to_owned(),
                component: j["component"].as_u64().unwrap(),
                kind: j["kind"].as_str().unwrap().to_owned(),
                fields: fields_from_json(&j["fields"]),
            }
        })
        .collect();
    let expected: Vec<Owned> = trace.records.iter().map(owned).collect();
    assert_eq!(rebuilt, expected);
}

#[test]
fn perfetto_represents_the_same_records_and_transactions() {
    let trace = run(5, true).1.unwrap();
    let tps = trace.header.ticks_per_second;
    let text = to_perfetto(&trace);
    let doc: Json = serde_json::from_str(&text).unwrap();
    let events = doc["traceEvents"].as_array().unwrap();

    // Timestamps are written verbatim from the exact integer conversion, one event per line.
    let instant_lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("{\"ph\":\"i\""))
        .collect();
    assert_eq!(instant_lines.len(), trace.records.len());
    for (line, r) in instant_lines.iter().zip(&trace.records) {
        let tick = match r.at {
            TraceAt::Init => 0,
            TraceAt::Event(k) => k.tick.0,
        };
        let ts = format!("\"ts\":{},", perfetto_ts(tick, tps));
        assert!(line.contains(&ts), "{line} lacks {ts}");
    }

    // One named track per component.
    let threads: Vec<(u64, &str)> = events
        .iter()
        .filter(|e| e["ph"] == "M" && e["name"] == "thread_name")
        .map(|e| {
            (
                e["tid"].as_u64().unwrap(),
                e["args"]["name"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(threads, [(0, "soc.cpu0"), (1, "soc.mem")]);

    // Every record is one instant event, in order, with the same fields.
    let instants: Vec<&Json> = events.iter().filter(|e| e["ph"] == "i").collect();
    assert_eq!(instants.len(), trace.records.len());
    for (i, (e, r)) in instants.iter().zip(&trace.records).enumerate() {
        assert_eq!(e["args"]["record"].as_u64(), Some(i as u64));
        assert_eq!(e["name"].as_str(), Some(r.kind));
        assert_eq!(e["tid"].as_u64(), Some(u64::from(r.component.0)));
        let tick = match r.at {
            TraceAt::Init => 0,
            TraceAt::Event(k) => k.tick.0,
        };
        // The display timestamp is derived from the exact tick.
        let ts = serde_json::to_string(&e["ts"]).unwrap();
        let expected: f64 = perfetto_ts(tick, tps).parse().unwrap();
        assert_eq!(e["ts"].as_f64(), Some(expected), "ts {ts}");
        assert_eq!(fields_from_json(&e["args"]["fields"]), owned(r).fields);
    }

    // Every mem transaction is exactly one begin/end pair on the initiator's track.
    let requests = trace
        .records
        .iter()
        .filter(|r| r.kind == DISPATCH_KIND)
        .filter(|r| {
            r.fields.iter().any(|(n, v)| {
                *n == "msg" && matches!(v, Value::Str(m) if m == "ReadReq" || m == "WriteReq")
            })
        })
        .count();
    let mut slices: BTreeMap<String, Vec<(&str, f64, u64)>> = BTreeMap::new();
    for e in events.iter().filter(|e| e["ph"] == "b" || e["ph"] == "e") {
        slices
            .entry(e["id"].as_str().unwrap().to_owned())
            .or_default()
            .push((
                e["ph"].as_str().unwrap(),
                e["ts"].as_f64().unwrap(),
                e["tid"].as_u64().unwrap(),
            ));
    }
    assert_eq!(slices.len(), requests);
    assert_eq!(requests, 400);
    for (id, pair) in &slices {
        assert_eq!(pair.len(), 2, "slice {id}");
        let (b, e) = (pair[0], pair[1]);
        assert_eq!((b.0, e.0), ("b", "e"), "slice {id}");
        assert!(b.1 < e.1, "slice {id} must end after it begins");
        assert_eq!((b.2, e.2), (0, 0), "slices live on the CPU track");
    }
}
