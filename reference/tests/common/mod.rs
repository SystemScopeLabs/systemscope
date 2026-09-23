//! Helpers shared by the reference scenario tests: reading the recorded trace.

#![allow(dead_code, reason = "each test binary uses a different subset")]

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::event::EventKey;
use systemscope_contracts::trace::{DISPATCH_KIND, TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_reference::{BUS, CPU, DMA, MEM};

/// A delivered `mem.v0` message, read back from its `runtime.dispatch` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hop {
    pub key: EventKey,
    pub source: ComponentId,
    pub target: ComponentId,
    pub msg: String,
    pub txn: u64,
    /// The message's fields after `txn`.
    pub payload: Vec<Value>,
}

impl Hop {
    pub fn is_request(&self) -> bool {
        self.msg.ends_with("Req")
    }
}

pub fn u64_field(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

pub fn bool_field(r: &TraceRecord, name: &str) -> bool {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::Bool(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

pub fn is_dispatch(r: &TraceRecord) -> bool {
    r.origin == TraceOrigin::Runtime && r.kind == DISPATCH_KIND
}

/// The message a dispatch record delivered, or `None` for a wake.
pub fn hop(r: &TraceRecord) -> Option<Hop> {
    if !is_dispatch(r) || r.fields.len() <= 2 {
        return None;
    }
    let TraceAt::Event(key) = r.at else {
        panic!("dispatch outside an event");
    };
    let Value::Str(msg) = &r.fields[4].1 else {
        panic!("msg");
    };
    Some(Hop {
        key,
        source: ComponentId(u64_field(r, "source") as u32),
        target: r.component,
        msg: msg.clone(),
        txn: u64_field(r, "txn"),
        payload: r.fields[6..].iter().map(|(_, v)| v.clone()).collect(),
    })
}

pub fn hops(records: &[TraceRecord]) -> Vec<Hop> {
    records.iter().filter_map(hop).collect()
}

/// The initiator behind a bus upstream port.
pub fn initiator(port: u64) -> ComponentId {
    match port {
        0 => CPU,
        1 => DMA,
        _ => panic!("port {port}"),
    }
}

/// Transactions in flight at each event boundary, derived from the trace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flow {
    /// Requests that reached the bus and wait for a grant, per upstream port.
    pub queued: [u64; 2],
    /// Granted requests the memory has not answered yet.
    pub at_memory: u64,
    /// Remapped transactions whose response has not reached its initiator.
    pub mapped: u64,
    /// Responses routed by the bus and not yet delivered upstream.
    pub returning: u64,
}

/// `Flow` before each event, indexed by the number of events dispatched so far, plus one
/// entry after the last event.
pub fn flows(records: &[TraceRecord]) -> Vec<Flow> {
    let mut out = Vec::new();
    let mut f = Flow::default();
    for r in records {
        if is_dispatch(r) {
            out.push(f);
            if let Some(h) = hop(r) {
                match (h.source, h.target) {
                    (s, BUS) if s == CPU => f.queued[0] += 1,
                    (s, BUS) if s == DMA => f.queued[1] += 1,
                    (BUS, MEM) => f.at_memory += 1,
                    (MEM, BUS) => f.at_memory -= 1,
                    (BUS, _) => {
                        f.returning -= 1;
                        f.mapped -= 1;
                    }
                    other => panic!("unexpected hop {other:?}"),
                }
            }
        } else if r.kind == "toy.bus.grant" {
            f.queued[u64_field(r, "port") as usize] -= 1;
            f.mapped += 1;
        } else if r.kind == "toy.bus.route" {
            f.returning += 1;
        }
    }
    out.push(f);
    out
}
