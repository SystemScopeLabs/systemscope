//! Snapshot and restore across the full topology (`docs/m0-design.md` §7, §9.1).
//!
//! Checkpoints are chosen by what is in flight, read back from the reference trace: queued
//! requests at the bus, both initiators waiting, remapped transactions, requests at the
//! memory, and responses between the bus and their initiator. A resumed run must match
//! the uninterrupted one in every digest, the trace bytes, and the final state, which
//! means arbitration and routing continued exactly.

mod common;

use common::{Flow, flows};
use systemscope_contracts::canonical::{CanonicalEvent, Decoder};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::snapshot::RestoreError;
use systemscope_reference::{BUS, DMA, ReferenceConfig, build, run};
use systemscope_runtime::runtime::{Dispatched, Runtime, RuntimeError};
use systemscope_runtime::trace::Trace;

const CONFIG: ReferenceConfig = ReferenceConfig {
    seed: 0xDEAD_BEEF,
    cpu_ops: 300,
    dma_ops: 300,
};

fn fresh() -> Runtime {
    build(CONFIG)
}

fn run_to_end(rt: &mut Runtime) -> Vec<Dispatched> {
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

/// Everything a finished run is compared on.
#[derive(Debug, PartialEq, Eq)]
struct End {
    snapshot: Vec<u8>,
    state: [u8; 32],
    execution: [u8; 32],
    trace_bytes: Vec<u8>,
    trace: [u8; 32],
}

fn end(mut rt: Runtime) -> End {
    let trace = rt.take_trace().unwrap();
    End {
        snapshot: rt.snapshot().unwrap(),
        state: rt.state_digest().unwrap(),
        execution: rt.execution_digest(),
        trace_bytes: trace.canonical_bytes(),
        trace: trace.digest(),
    }
}

/// A traced run stopped after `k` events: its snapshot and trace prefix.
fn checkpoint(k: usize) -> (Vec<u8>, Trace) {
    let mut rt = fresh();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    (rt.snapshot().unwrap(), rt.take_trace().unwrap())
}

/// A property of the in-flight state.
type Condition = fn(&Flow) -> bool;

/// Checkpoint indices, each found in the reference run's in-flight state.
fn checkpoints(flow: &[Flow]) -> Vec<(&'static str, usize)> {
    let n = flow.len() - 1;
    let conditions: [(&'static str, Condition); 5] = [
        ("a request waits for arbitration", |f| f.queued != [0, 0]),
        ("both initiators wait at the bus", |f| {
            f.queued[0] > 0 && f.queued[1] > 0
        }),
        ("several transactions are remapped", |f| f.mapped >= 4),
        ("the memory owes a response", |f| f.at_memory > 0),
        ("a routed response has not reached its initiator", |f| {
            f.returning > 0
        }),
    ];
    let mut out = vec![
        ("right after init", 0),
        ("after the first event", 1),
        ("halfway", n / 2),
        ("before the last event", n - 1),
        ("after the last event", n),
    ];
    for (what, holds) in conditions {
        // The first time it holds, and the first time after the run is under way.
        for from in [1, n / 3] {
            let k = (from..n)
                .find(|&k| holds(&flow[k]))
                .unwrap_or_else(|| panic!("never: {what}"));
            out.push((what, k));
        }
    }
    let mut x: u64 = 0x5EED_0007;
    for _ in 0..8 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(("random", (z % (n as u64 + 1)) as usize));
    }
    out
}

#[test]
fn resumed_runs_continue_arbitration_and_routing_exactly() {
    let mut rt = fresh();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_to_end(&mut rt);
    let expected = end(rt);
    let reference = {
        let mut rt = fresh();
        rt.start_trace().unwrap();
        rt.init().unwrap();
        run(&mut rt).unwrap();
        rt.take_trace().unwrap()
    };
    let flow = flows(&reference.records);
    assert_eq!(flow.len(), events.len() + 1);
    assert_eq!(*flow.last().unwrap(), Flow::default());

    for (what, k) in checkpoints(&flow) {
        let (snapshot, prefix) = checkpoint(k);
        // The old session is gone; only its bytes and its trace prefix are left.
        let mut rt = fresh();
        rt.restore(&snapshot).unwrap();
        assert_eq!(rt.snapshot().unwrap(), snapshot, "{what} ({k})");
        rt.resume_trace(prefix).unwrap();
        let rest = run_to_end(&mut rt);
        assert_eq!(rest, events[k..], "{what} ({k})");
        assert_eq!(end(rt), expected, "{what} ({k})");
    }
}

#[test]
fn restoring_and_resnapshotting_is_the_identity_at_every_event() {
    let mut rt = build(ReferenceConfig {
        cpu_ops: 60,
        dma_ops: 60,
        ..CONFIG
    });
    rt.init().unwrap();
    loop {
        let bytes = rt.snapshot().unwrap();
        let mut again = build(ReferenceConfig {
            cpu_ops: 60,
            dma_ops: 60,
            ..CONFIG
        });
        again.restore(&bytes).unwrap();
        assert_eq!(again.snapshot().unwrap(), bytes);
        if rt.step().unwrap().is_none() {
            break;
        }
    }
}

/// Where each component's bytes start in a snapshot.
fn component_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut d = Decoder::new(bytes);
    let at = |d: &Decoder<'_>| bytes.len() - d.remaining();
    d.raw(12).unwrap();
    for _ in 0..3 {
        d.u64().unwrap();
    }
    d.str().unwrap();
    for _ in 0..d.len().unwrap() {
        d.raw(4 + 3 * 8 + 1).unwrap();
    }
    d.raw(32).unwrap();
    if d.u8().unwrap() == 1 {
        EventKey::decode(&mut d).unwrap();
    }
    d.raw(2 * 8 + 32).unwrap();
    for _ in 0..d.len().unwrap() {
        CanonicalEvent::decode(&mut d).unwrap();
    }
    for _ in 0..d.len().unwrap() {
        d.raw(32).unwrap();
    }
    let mut out = Vec::new();
    for _ in 0..d.len().unwrap() {
        d.raw(8).unwrap();
        let len = d.u32().unwrap() as usize;
        out.push(at(&d));
        d.raw(len).unwrap();
    }
    out
}

fn invalid(bytes: &[u8]) -> &'static str {
    let mut rt = fresh();
    match rt.restore(bytes) {
        Err(RuntimeError::Restore(RestoreError::InvalidState(what))) => what,
        other => panic!("{other:?}"),
    }
}

#[test]
fn bus_and_dma_restore_reject_impossible_states() {
    let flow = {
        let mut rt = fresh();
        rt.start_trace().unwrap();
        rt.init().unwrap();
        run(&mut rt).unwrap();
        flows(&rt.take_trace().unwrap().records)
    };
    let k = (1..flow.len()).find(|&k| flow[k].mapped >= 2).unwrap();
    let (snapshot, _) = checkpoint(k);
    let offsets = component_offsets(&snapshot);
    let bus = offsets[BUS.0 as usize];
    let dma = offsets[DMA.0 as usize];

    // Bus schema 1: clock u32, next downstream u64, priority u16, ...
    let mut priority = snapshot.clone();
    priority[bus + 12] = 2;
    assert_eq!(invalid(&priority), "toy bus: priority out of range");
    let mut counter = snapshot.clone();
    counter[bus + 4..bus + 12].fill(0);
    assert_eq!(
        invalid(&counter),
        "toy bus: routes out of order or not yet allocated"
    );
    // Dma schema 1: configuration (4 + 8 + 4 + 8 + 8 + 8 + 4 + 4 bytes), issued,
    // completed, next txn, burst slot u32, ...
    let slot = dma + 48 + 3 * 8;
    let mut burst = snapshot.clone();
    burst[slot..slot + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(invalid(&burst), "toy dma: burst slot out of range");
    let mut config = snapshot.clone();
    config[dma + 4] ^= 1;
    assert_eq!(
        invalid(&config),
        "toy dma: snapshot was taken with a different configuration"
    );
}
