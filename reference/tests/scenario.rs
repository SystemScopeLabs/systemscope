//! The full m0-reference topology, run end to end (`docs/m0-design.md` §9.1): it finishes,
//! it really collides, reorders, and arbitrates, remapping keeps the initiators apart, and
//! the trace and Perfetto views show every hop.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use common::{Hop, bool_field, hops, initiator, u64_field};
use serde_json::Value as Json;
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::Phase;
use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_reference::{
    BUS, CPU, CPU_ACCESS, CPU_SLOTS, DMA, DMA_ACCESS, DMA_BASE, DMA_SLOTS, MEM, MEMORY_SIZE,
    ReferenceConfig, build, run,
};
use systemscope_runtime::export::{perfetto_track, to_perfetto};
use systemscope_runtime::runtime::{Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_toy::{
    ToyBus, ToyBusConfig, ToyCpu, ToyCpuConfig, ToyDma, ToyDmaConfig, ToyMemory, ToyMemoryConfig,
};

const OPS: u64 = 2_000;

fn config(seed: u64) -> ReferenceConfig {
    ReferenceConfig {
        seed,
        cpu_ops: OPS,
        dma_ops: OPS,
    }
}

/// Runs to the end with tracing on; returns the finished runtime and its trace.
fn traced(seed: u64) -> (Runtime, Trace) {
    let mut rt = build(config(seed));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    run(&mut rt).unwrap();
    let trace = rt.take_trace().unwrap();
    (rt, trace)
}

fn count(trace: &Trace, kind: &str) -> usize {
    trace.records.iter().filter(|r| r.kind == kind).count()
}

#[test]
fn both_initiators_finish_well_before_t_end() {
    for seed in [0, 1, 0xDEAD_BEEF] {
        let (rt, trace) = traced(seed);
        assert_eq!(rt.pending(), 0, "seed {seed}");
        assert!(rt.now() < Tick(1_000_000_000), "{:?}", rt.now());
        assert_eq!(count(&trace, "toy.cpu.commit") as u64, OPS);
        assert_eq!(count(&trace, "toy.dma.done") as u64, OPS);
        assert_eq!(count(&trace, "toy.bus.grant") as u64, 2 * OPS);
        assert_eq!(count(&trace, "toy.bus.route") as u64, 2 * OPS);
    }
}

#[test]
fn the_run_really_collides_arbitrates_and_reorders() {
    let (_, trace) = traced(0);
    let hops = hops(&trace.records);

    // Requests from both initiators reach the bus at the same tick.
    let arrivals = |from| -> BTreeSet<u64> {
        hops.iter()
            .filter(|h| h.source == from && h.target == BUS)
            .map(|h| h.key.tick.0)
            .collect()
    };
    let same_tick = arrivals(CPU).intersection(&arrivals(DMA)).count();
    assert!(same_tick > 50, "{same_tick}");

    // The bus grants with both queues waiting, both ways round.
    let contended: Vec<u64> = trace
        .records
        .iter()
        .filter(|r| r.kind == "toy.bus.grant" && bool_field(r, "contended"))
        .map(|r| u64_field(r, "port"))
        .collect();
    assert!(contended.iter().filter(|&&p| p == 0).count() > 20);
    assert!(contended.iter().filter(|&&p| p == 1).count() > 20);

    // Responses reach the CPU out of issue order (writes are faster than reads).
    let to_cpu: Vec<u64> = hops
        .iter()
        .filter(|h| h.target == CPU)
        .map(|h| h.txn)
        .collect();
    let reordered = to_cpu.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(reordered > 50, "{reordered}");
}

#[test]
fn cpu_and_dma_have_equal_txns_in_flight_together() {
    let (_, trace) = traced(0);
    let mut in_flight: [BTreeSet<u64>; 2] = Default::default();
    let mut overlaps = 0;
    for r in &trace.records {
        let (side, add) = match r.kind {
            "toy.cpu.issue" => (0, true),
            "toy.cpu.commit" => (0, false),
            "toy.dma.issue" => (1, true),
            "toy.dma.done" => (1, false),
            _ => continue,
        };
        let txn = u64_field(r, "txn");
        if add {
            in_flight[side].insert(txn);
            overlaps += usize::from(in_flight[1 - side].contains(&txn));
        } else {
            assert!(in_flight[side].remove(&txn));
        }
    }
    assert!(overlaps > 20, "{overlaps}");
}

#[test]
fn every_response_returns_to_its_initiator_under_its_own_txn() {
    let (_, trace) = traced(0);
    let hops = hops(&trace.records);
    let upstream_request: BTreeMap<(ComponentId, u64), &Hop> = hops
        .iter()
        .filter(|h| h.target == BUS && h.is_request())
        .map(|h| ((h.source, h.txn), h))
        .collect();
    assert_eq!(
        upstream_request.len() as u64,
        2 * OPS,
        "upstream txns are unique"
    );

    // downstream → (initiator, upstream txn), from the grants.
    let routes: BTreeMap<u64, (ComponentId, u64)> = trace
        .records
        .iter()
        .filter(|r| r.kind == "toy.bus.grant")
        .map(|r| {
            let to = (initiator(u64_field(r, "port")), u64_field(r, "txn"));
            (u64_field(r, "downstream"), to)
        })
        .collect();
    assert_eq!(routes.len() as u64, 2 * OPS);
    assert_eq!(
        routes.keys().copied().collect::<Vec<_>>(),
        (0..2 * OPS).collect::<Vec<_>>()
    );

    let mut answers = BTreeMap::new();
    for h in &hops {
        match (h.source, h.target) {
            // The memory sees the upstream request unchanged except for its txn.
            (BUS, MEM) => {
                let up = upstream_request[&routes[&h.txn]];
                assert_eq!((&h.msg, &h.payload), (&up.msg, &up.payload));
                assert_eq!(h.key.phase, Phase::Transfer);
            }
            (MEM, BUS) => {
                assert!(answers.insert(routes[&h.txn], h).is_none());
            }
            // The initiator gets the memory's answer, under its own txn.
            (BUS, to) => {
                let answer = answers.remove(&(to, h.txn)).expect("routed response");
                assert_eq!((&h.msg, &h.payload), (&answer.msg, &answer.payload));
                assert_eq!(h.key.phase, Phase::Complete);
                let up = upstream_request[&(to, h.txn)];
                assert_eq!(h.msg.replace("Resp", "Req"), up.msg);
            }
            _ => {}
        }
    }
    assert!(answers.is_empty());
}

#[test]
fn each_initiator_stays_in_its_own_region() {
    let (_, trace) = traced(0);
    let mut seen = [0u64; 2];
    for h in hops(&trace.records) {
        if !(h.target == BUS && h.is_request()) {
            continue;
        }
        let Some(Value::U64(addr)) = h.payload.first() else {
            panic!("addr");
        };
        let (start, end, len) = if h.source == CPU {
            seen[0] += 1;
            (0, DMA_BASE, CPU_ACCESS)
        } else {
            assert_eq!(h.source, DMA);
            assert_eq!(h.msg, "WriteReq", "the DMA only writes");
            seen[1] += 1;
            (DMA_BASE, u64::from(MEMORY_SIZE), DMA_ACCESS)
        };
        assert!(start <= *addr && addr + u64::from(len) <= end, "{h:?}");
    }
    assert_eq!(seen, [OPS, OPS]);
}

/// The reference topology, except that the DMA writes into the CPU's region.
fn overlapping(seed: u64) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let mut clock = |num, den| {
        t.add_clock(
            Frequency::new(num, den).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap()
    };
    let (cpu_clock, io_clock, bus_clock) = (
        clock(3_000_000_000, 1),
        clock(3_000_000_000, 2),
        clock(1_000_000_000, 1),
    );
    let cpu = t.add_component(
        "soc.cpu0",
        Box::new(ToyCpu::new(ToyCpuConfig {
            clock: cpu_clock,
            ops: OPS,
            max_outstanding: 4,
            max_think_cycles: NonZeroU64::new(8).unwrap(),
            access_len: CPU_ACCESS,
            slots: CPU_SLOTS,
            write_percent: 40,
        })),
    );
    let dma = t.add_component(
        "soc.dma0",
        Box::new(ToyDma::new(ToyDmaConfig {
            clock: io_clock,
            ops: OPS,
            max_outstanding: 8,
            max_burst: NonZeroU64::new(8).unwrap(),
            max_gap_cycles: NonZeroU64::new(128).unwrap(),
            base: 0,
            access_len: DMA_ACCESS,
            slots: DMA_SLOTS,
        })),
    );
    let bus = t.add_component(
        "soc.bus",
        Box::new(ToyBus::new(ToyBusConfig { clock: bus_clock })),
    );
    let mem = t.add_component(
        "soc.mem",
        Box::new(ToyMemory::new(ToyMemoryConfig {
            size: MEMORY_SIZE,
            read_latency: Duration::from_ns(50),
            write_latency: Duration::from_ns(30),
        })),
    );
    let link = Some(LinkLatency::Cycles {
        domain: bus_clock,
        k: 1,
    });
    t.connect((cpu, "mem"), (bus, "cpu"), link);
    t.connect((dma, "mem"), (bus, "dma"), link);
    t.connect((bus, "mem"), (mem, "mem"), link);
    t.elaborate(SessionConfig {
        seed,
        ..SessionConfig::default()
    })
    .unwrap()
}

#[test]
fn the_cpu_shadow_check_holds_and_would_catch_a_foreign_writer() {
    // With disjoint regions every read matches the shadow copy (a mismatch faults).
    for seed in [0, 1, 0xDEAD_BEEF] {
        let mut rt = build(config(seed));
        rt.init().unwrap();
        run(&mut rt).unwrap();
        assert_eq!(rt.pending(), 0);
    }
    // A DMA writing the CPU's slots breaks the CPU's private view, and the check says so.
    let mut rt = overlapping(0);
    rt.init().unwrap();
    assert_eq!(
        run(&mut rt),
        Err(RuntimeError::Faulted(SimError::ComponentFault(
            "toy cpu: read mismatch"
        )))
    );
}

#[test]
fn runs_are_reproducible_and_seed_dependent() {
    let digests = |seed| {
        let (rt, trace) = traced(seed);
        (
            rt.state_digest().unwrap(),
            rt.execution_digest(),
            trace.digest(),
        )
    };
    let a = digests(1);
    assert_eq!(a, digests(1));
    let seeds = [digests(0).1, a.1, digests(0xDEAD_BEEF).1];
    assert_ne!(seeds[0], seeds[1]);
    assert_ne!(seeds[1], seeds[2]);
    assert_ne!(seeds[0], seeds[2]);
}

#[test]
fn perfetto_shows_the_bus_and_one_slice_per_hop() {
    let (_, trace) = traced(0);
    let json: Json = serde_json::from_str(&to_perfetto(&trace)).unwrap();
    let events = json["traceEvents"].as_array().unwrap();

    let names: BTreeMap<u64, &str> = events
        .iter()
        .filter(|e| e["ph"] == "M" && e["name"] == "process_name")
        .map(|e| {
            (
                e["pid"].as_u64().unwrap(),
                e["args"]["name"].as_str().unwrap(),
            )
        })
        .collect();
    let track = |c: ComponentId| perfetto_track(c.0);
    assert_eq!(
        names,
        BTreeMap::from([
            (track(CPU), "soc.cpu0"),
            (track(DMA), "soc.dma0"),
            (track(BUS), "soc.bus"),
            (track(MEM), "soc.mem"),
        ])
    );

    // Every slice begins and ends once, on one track, and ids never repeat.
    let mut open: BTreeMap<String, (u64, f64)> = BTreeMap::new();
    let mut closed: BTreeMap<String, (u64, f64, f64)> = BTreeMap::new();
    for e in events.iter().filter(|e| e["cat"] == "mem") {
        let id = e["id2"]["local"].as_str().unwrap().to_owned();
        let pid = e["pid"].as_u64().unwrap();
        assert_eq!(e["tid"].as_u64(), Some(pid));
        let ts: f64 = e["ts"].as_f64().unwrap();
        match e["ph"].as_str().unwrap() {
            "b" => assert!(open.insert(id, (pid, ts)).is_none()),
            "e" => {
                let (begin_pid, begin) = open.remove(&id).expect("slice was open");
                assert_eq!(begin_pid, pid);
                assert!(closed.insert(id, (pid, begin, ts)).is_none());
            }
            other => panic!("{other}"),
        }
    }
    assert!(open.is_empty());
    let per_track = |c: ComponentId| closed.values().filter(|s| s.0 == track(c)).count() as u64;
    assert_eq!(
        [
            per_track(CPU),
            per_track(DMA),
            per_track(BUS),
            per_track(MEM)
        ],
        [OPS, OPS, 2 * OPS, 0]
    );
    // Ids name the initiator, so equal txn numbers on the CPU and the DMA stay apart.
    assert!(closed.contains_key(&format!("{}:0", CPU.0)));
    assert!(closed.contains_key(&format!("{}:0", DMA.0)));

    // Each bus hop nests inside the upstream transaction it serves.
    for r in trace.records.iter().filter(|r| r.kind == "toy.bus.grant") {
        let up = format!(
            "{}:{}",
            initiator(u64_field(r, "port")).0,
            u64_field(r, "txn")
        );
        let down = format!("{}:{}", BUS.0, u64_field(r, "downstream"));
        let (_, up_begin, up_end) = closed[&up];
        let (_, down_begin, down_end) = closed[&down];
        assert!(up_begin <= down_begin && down_end <= up_end, "{up} {down}");
    }
}
