//! ToyDma against a ToyMemory: bursts, the region, and the outstanding limit
//! (`docs/m0-design.md` §9.1).

use std::num::NonZeroU64;

use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_toy::{ToyDma, ToyDmaConfig, ToyMemory, ToyMemoryConfig};

const OPS: u64 = 400;
const BASE: u64 = 256;
const LEN: u32 = 16;
const SLOTS: u32 = 32;

fn build(seed: u64, max_outstanding: u32, max_gap: u64, write_latency: Duration) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let io = t
        .add_clock(
            Frequency::new(3_000_000_000, 2).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let dma = t.add_component(
        "dma",
        Box::new(ToyDma::new(ToyDmaConfig {
            clock: io,
            ops: OPS,
            max_outstanding,
            max_burst: NonZeroU64::new(8).unwrap(),
            max_gap_cycles: NonZeroU64::new(max_gap).unwrap(),
            base: BASE,
            access_len: LEN,
            slots: SLOTS,
        })),
    );
    let mem = t.add_component(
        "mem",
        Box::new(ToyMemory::new(ToyMemoryConfig {
            size: BASE as u32 + SLOTS * LEN,
            read_latency: Duration::from_ns(50),
            write_latency,
        })),
    );
    let link = Some(LinkLatency::Cycles { domain: io, k: 1 });
    t.connect((dma, "mem"), (mem, "mem"), link);
    t.elaborate(SessionConfig {
        seed,
        ..SessionConfig::default()
    })
    .unwrap()
}

fn records(mut rt: Runtime) -> Vec<TraceRecord> {
    rt.start_trace().unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    rt.take_trace().unwrap().records
}

fn u64_field(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn of_kind<'a>(records: &'a [TraceRecord], kind: &str) -> Vec<&'a TraceRecord> {
    records.iter().filter(|r| r.kind == kind).collect()
}

#[test]
fn every_write_completes_once_under_ascending_txns_inside_the_region() {
    let records = records(build(1, 8, 16, Duration::from_ns(30)));
    let issued = of_kind(&records, "toy.dma.issue");
    let txns: Vec<u64> = issued.iter().map(|r| u64_field(r, "txn")).collect();
    assert_eq!(txns, (0..OPS).collect::<Vec<_>>());
    for r in &issued {
        let addr = u64_field(r, "addr");
        assert!(
            (BASE..BASE + u64::from(SLOTS * LEN)).contains(&addr),
            "{addr}"
        );
        assert_eq!((addr - BASE) % u64::from(LEN), 0, "{addr}");
    }
    let mut done: Vec<u64> = of_kind(&records, "toy.dma.done")
        .iter()
        .map(|r| u64_field(r, "txn"))
        .collect();
    done.sort_unstable();
    assert_eq!(done, txns);
}

#[test]
fn bursts_write_consecutive_slots_and_wrap_at_the_region_end() {
    let records = records(build(2, 8, 16, Duration::from_ns(30)));
    let slots: Vec<u64> = of_kind(&records, "toy.dma.issue")
        .iter()
        .map(|r| (u64_field(r, "addr") - BASE) / u64::from(LEN))
        .collect();
    let next = |a: u64| (a + 1) % u64::from(SLOTS);
    let consecutive = slots.windows(2).filter(|w| w[1] == next(w[0])).count();
    let wrapped = slots
        .windows(2)
        .filter(|w| w[0] == u64::from(SLOTS) - 1 && w[1] == 0)
        .count();
    // Bursts average 4.5 writes, so about 3.5 of every 4.5 steps continue a burst.
    assert!(
        consecutive * 2 > slots.len(),
        "{consecutive} of {}",
        slots.len()
    );
    assert!(wrapped > 0);
}

#[test]
fn a_slow_memory_holds_the_dma_at_its_outstanding_limit() {
    for limit in [1, 3] {
        let records = records(build(3, limit, 1, Duration::from_ns(500)));
        let mut in_flight = 0i64;
        let mut peak = 0;
        for r in &records {
            match r.kind {
                "toy.dma.issue" => in_flight += 1,
                "toy.dma.done" => in_flight -= 1,
                _ => continue,
            }
            peak = peak.max(in_flight);
        }
        assert_eq!(in_flight, 0);
        assert_eq!(peak, i64::from(limit));
    }
}

#[test]
fn the_seed_decides_the_writes() {
    let run = |seed| {
        let mut rt = build(seed, 8, 16, Duration::from_ns(30));
        rt.init().unwrap();
        while rt.step().unwrap().is_some() {}
        rt.state_digest().unwrap()
    };
    assert_eq!(run(7), run(7));
    assert_ne!(run(7), run(8));
}
