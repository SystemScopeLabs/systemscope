//! Text views of a [`Trace`] (`docs/m0-design.md` §8.1).
//!
//! Both exporters are derived from the same logical records and are never digested. JSON
//! is written by hand so output does not depend on a serializer: integers are exact, bytes
//! are lowercase hex, and strings are escaped by one fixed rule.

use std::collections::BTreeMap;
use std::fmt::Write;

use systemscope_contracts::component::Role;
use systemscope_contracts::time::Rounding;
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{
    DISPATCH_KIND, TRACE_FORMAT_VERSION, TraceAt, TraceOrigin, TraceRecord, Value,
};

use crate::trace::Trace;

/// Femtoseconds per microsecond.
const FS_PER_US: u128 = 1_000_000_000;

/// Writes a JSON string literal. Escapes `"`, `\`, and control characters; everything
/// else, including non-ASCII, is written as UTF-8.
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A typed value: `{"u64":1}`, `{"i64":-1}`, `{"bool":true}`, `{"str":"…"}`, `{"bytes":"aa"}`.
fn json_value(out: &mut String, v: &Value) {
    match v {
        Value::U64(x) => {
            let _ = write!(out, "{{\"u64\":{x}}}");
        }
        Value::I64(x) => {
            let _ = write!(out, "{{\"i64\":{x}}}");
        }
        Value::Bool(x) => {
            let _ = write!(out, "{{\"bool\":{x}}}");
        }
        Value::Str(x) => {
            out.push_str("{\"str\":");
            json_str(out, x);
            out.push('}');
        }
        Value::Bytes(x) => {
            let _ = write!(out, "{{\"bytes\":\"{}\"}}", hex(x));
        }
    }
}

/// Fields as an ordered array of `[name, value]` pairs, so order and duplicates survive.
fn json_fields(out: &mut String, fields: &[(&'static str, Value)]) {
    out.push('[');
    for (i, (name, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        json_str(out, name);
        out.push(',');
        json_value(out, value);
        out.push(']');
    }
    out.push(']');
}

fn origin_name(origin: TraceOrigin) -> &'static str {
    match origin {
        TraceOrigin::Component => "component",
        TraceOrigin::Runtime => "runtime",
    }
}

/// JSONL: a header line, then one line per record in emission order.
pub fn to_jsonl(trace: &Trace) -> String {
    let h = &trace.header;
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"type\":\"header\",\"format_version\":{TRACE_FORMAT_VERSION},\
         \"ticks_per_second\":{},\"seed\":{},\"contracts_version\":",
        h.ticks_per_second, h.seed
    );
    json_str(&mut out, &h.contracts_version);
    out.push_str(",\"clock_domains\":[");
    for (i, d) in h.clock_domains.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let rounding = match d.edge_rounding() {
            Rounding::Floor => "floor",
            Rounding::Ceil => "ceil",
        };
        let _ = write!(
            out,
            "{{\"id\":{},\"freq_num\":{},\"freq_den\":{},\"offset\":{},\"rounding\":\"{rounding}\"}}",
            d.id().0,
            d.frequency().num(),
            d.frequency().den(),
            d.offset().0
        );
    }
    out.push_str("],\"components\":[");
    for (i, c) in h.components.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"id\":{i},\"path\":");
        json_str(&mut out, &c.path);
        out.push_str(",\"type\":");
        json_str(&mut out, c.type_name);
        out.push_str(",\"ports\":[");
        for (j, p) in c.ports.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json_str(&mut out, p.name);
            out.push_str(",\"protocol\":");
            json_str(&mut out, p.protocol.name);
            let role = match p.role {
                Role::Initiator => "initiator",
                Role::Target => "target",
            };
            let _ = write!(
                out,
                ",\"version\":{},\"role\":\"{role}\"}}",
                p.protocol.version
            );
        }
        out.push_str("]}");
    }
    out.push_str("],\"links\":[");
    for (i, l) in h.links.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"a\":[{},{}],\"b\":[{},{}],\"latency\":",
            l.a.0.0, l.a.1.0, l.b.0.0, l.b.1.0
        );
        match l.latency {
            None => out.push_str("null"),
            Some(LinkLatency::After(d)) => {
                let _ = write!(out, "{{\"after_fs\":{}}}", d.as_femtoseconds());
            }
            Some(LinkLatency::Cycles { domain, k }) => {
                let _ = write!(out, "{{\"cycles\":{{\"domain\":{},\"k\":{k}}}}}", domain.0);
            }
        }
        out.push('}');
    }
    out.push_str("]}\n");

    for (index, r) in trace.records.iter().enumerate() {
        let _ = write!(out, "{{\"type\":\"record\",\"index\":{index},\"at\":");
        match r.at {
            TraceAt::Init => out.push_str("\"init\""),
            TraceAt::Event(k) => {
                let _ = write!(
                    out,
                    "{{\"tick\":{},\"phase\":\"{}\",\"sequence\":{}}}",
                    k.tick.0, k.phase, k.sequence
                );
            }
        }
        let _ = write!(
            out,
            ",\"origin\":\"{}\",\"component\":{},\"kind\":",
            origin_name(r.origin),
            r.component.0
        );
        json_str(&mut out, r.kind);
        out.push_str(",\"fields\":");
        json_fields(&mut out, &r.fields);
        out.push_str("}\n");
    }
    out
}

/// Converts a tick to microseconds with exactly nine decimal places, for display only:
/// `floor(tick × 10^15 / ticks_per_second)` femtoseconds, split into µs and remainder.
pub fn perfetto_ts(tick: u64, ticks_per_second: u64) -> String {
    let fs = u128::from(tick) * 1_000_000_000_000_000 / u128::from(ticks_per_second);
    format!("{}.{:09}", fs / FS_PER_US, fs % FS_PER_US)
}

fn field<'a>(r: &'a TraceRecord, name: &str) -> Option<&'a Value> {
    r.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
}

/// The Perfetto process and thread id of a component: `ComponentId + 1`, since Perfetto
/// treats id 0 specially.
pub fn perfetto_track(component: u32) -> u64 {
    u64::from(component) + 1
}

/// Chrome JSON Trace Event format for the Perfetto UI.
///
/// Each component is its own process and thread (`pid = tid =` [`perfetto_track`]); every
/// record is an instant event on its component's thread with its fields as `args`; every
/// `mem.v0` transaction is an async slice scoped to the initiator's process and keyed by
/// `(initiator, txn)`, from the request's dispatch to the response's dispatch.
pub fn to_perfetto(trace: &Trace) -> String {
    let tps = trace.header.ticks_per_second;
    let mut events: Vec<String> = Vec::new();

    for (id, c) in (0u32..).zip(&trace.header.components) {
        let track = perfetto_track(id);
        for kind in ["process_name", "thread_name"] {
            let mut e = format!(
                "{{\"ph\":\"M\",\"pid\":{track},\"tid\":{track},\"name\":\"{kind}\",\"args\":{{\"name\":"
            );
            json_str(&mut e, &c.path);
            e.push_str("}}");
            events.push(e);
        }
    }

    // Open slices by (initiator, txn) → request name, so the end matches the begin.
    let mut open: BTreeMap<(u32, u64), &'static str> = BTreeMap::new();
    for (index, r) in trace.records.iter().enumerate() {
        let tick = match r.at {
            TraceAt::Init => 0,
            TraceAt::Event(k) => k.tick.0,
        };
        let ts = perfetto_ts(tick, tps);

        let track = perfetto_track(r.component.0);
        let mut e = format!(
            "{{\"ph\":\"i\",\"s\":\"t\",\"pid\":{track},\"tid\":{track},\"ts\":{ts},\"name\":"
        );
        json_str(&mut e, r.kind);
        let _ = write!(
            e,
            ",\"args\":{{\"record\":{index},\"origin\":\"{}\",\"fields\":",
            origin_name(r.origin)
        );
        json_fields(&mut e, &r.fields);
        e.push_str("}}");
        events.push(e);

        if r.origin != TraceOrigin::Runtime || r.kind != DISPATCH_KIND {
            continue;
        }
        let (Some(Value::Str(msg)), Some(Value::U64(txn))) = (field(r, "msg"), field(r, "txn"))
        else {
            continue;
        };
        let (phase, initiator, name) = match msg.as_str() {
            "ReadReq" | "WriteReq" => {
                let Some(Value::U64(source)) = field(r, "source") else {
                    continue;
                };
                let name = if msg == "ReadReq" {
                    "ReadReq"
                } else {
                    "WriteReq"
                };
                let initiator = *source as u32;
                open.insert((initiator, *txn), name);
                ("b", initiator, name)
            }
            "ReadResp" | "WriteResp" => {
                let initiator = r.component.0;
                let Some(name) = open.remove(&(initiator, *txn)) else {
                    continue;
                };
                ("e", initiator, name)
            }
            _ => continue,
        };
        let track = perfetto_track(initiator);
        events.push(format!(
            "{{\"ph\":\"{phase}\",\"cat\":\"mem\",\"id2\":{{\"local\":\"{initiator}:{txn}\"}},\
             \"pid\":{track},\"tid\":{track},\"ts\":{ts},\"name\":\"{name}\"}}"
        ));
    }

    let mut out = String::from("{\"displayTimeUnit\":\"ns\",\"traceEvents\":[\n");
    out.push_str(&events.join(",\n"));
    out.push_str("\n]}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfetto_ts_is_exact_integer_arithmetic() {
        let ps = 1_000_000_000_000;
        assert_eq!(perfetto_ts(0, ps), "0.000000000");
        assert_eq!(perfetto_ts(1, ps), "0.000001000");
        assert_eq!(perfetto_ts(1_500_000, ps), "1.500000000");
        assert_eq!(perfetto_ts(u64::MAX, ps), "18446744073709.551615000");
        // A resolution that does not divide 10^15 floors to the femtosecond.
        assert_eq!(perfetto_ts(1, 3), "333333.333333333");
    }

    #[test]
    fn slices_are_keyed_by_initiator_and_txn() {
        use systemscope_contracts::component::ComponentId;
        use systemscope_contracts::event::{EventKey, Phase};
        use systemscope_contracts::time::Tick;
        use systemscope_contracts::trace::{ComponentDecl, TraceHeader};

        let decl = |path: &str| ComponentDecl {
            path: path.to_owned(),
            type_name: "t",
            ports: Vec::new(),
        };
        let at = |tick, sequence| {
            TraceAt::Event(EventKey {
                tick: Tick(tick),
                phase: Phase::Request,
                sequence,
            })
        };
        let dispatch = |tick, seq, target: u32, source: u64, msg: &str| TraceRecord {
            at: at(tick, seq),
            origin: TraceOrigin::Runtime,
            component: ComponentId(target),
            kind: DISPATCH_KIND,
            fields: vec![
                ("source", Value::U64(source)),
                ("msg", Value::Str(msg.to_owned())),
                ("txn", Value::U64(1)),
            ],
        };
        // Two initiators both use txn 1 against one target.
        let trace = Trace {
            header: TraceHeader {
                ticks_per_second: 1_000_000_000_000,
                seed: 0,
                contracts_version: "v".into(),
                clock_domains: Vec::new(),
                components: vec![decl("a"), decl("b"), decl("mem")],
                links: Vec::new(),
            },
            records: vec![
                dispatch(1, 0, 2, 0, "ReadReq"),
                dispatch(2, 1, 2, 1, "WriteReq"),
                dispatch(3, 2, 1, 2, "WriteResp"),
                dispatch(4, 3, 0, 2, "ReadResp"),
            ],
        };
        let out = to_perfetto(&trace);
        let slices: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("\"cat\":\"mem\""))
            .collect();
        assert_eq!(slices.len(), 4);
        assert!(slices[0].contains("\"ph\":\"b\",\"cat\":\"mem\",\"id2\":{\"local\":\"0:1\"}"));
        assert!(slices[1].contains("\"ph\":\"b\",\"cat\":\"mem\",\"id2\":{\"local\":\"1:1\"}"));
        assert!(slices[2].contains("\"ph\":\"e\",\"cat\":\"mem\",\"id2\":{\"local\":\"1:1\"}"));
        assert!(slices[2].contains("\"name\":\"WriteReq\""));
        assert!(slices[3].contains("\"ph\":\"e\",\"cat\":\"mem\",\"id2\":{\"local\":\"0:1\"}"));
        assert!(slices[3].contains("\"pid\":1,\"tid\":1,"));
    }

    #[test]
    fn json_strings_escape_by_one_fixed_rule() {
        let mut out = String::new();
        json_str(&mut out, "a\"b\\c\n\u{1}\u{e9}");
        assert_eq!(out, "\"a\\\"b\\\\c\\n\\u0001\u{e9}\"");
    }
}
