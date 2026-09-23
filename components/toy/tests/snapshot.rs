//! Snapshot, restore, and trace resume on a real toy run (`docs/m0-design.md` §7, §8.1, §9
//! AT-2).
//!
//! The reference run is uninterrupted. Every checkpoint run stops after `k` events, keeps
//! the snapshot bytes and the trace prefix, drops the runtime, and continues in a freshly
//! elaborated one. The continuation must end in the same `StateDigest`, `ExecutionDigest`,
//! and `TraceDigest` as the reference.

use std::num::NonZeroU64;

use systemscope_contracts::canonical::{CanonicalEvent, DecodeError, Decoder};
use systemscope_contracts::component::{Component, Delivered, InitContext, PortSpec, SimContext};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::EventKey;
use systemscope_contracts::event::Phase;
use systemscope_contracts::snapshot::{RestoreError, SessionField, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::CONTRACTS_VERSION;
use systemscope_runtime::runtime::{Dispatched, Lifecycle, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::scheduler::SchedulerConfig;
use systemscope_runtime::snapshot::SNAPSHOT_MAGIC;
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::{ResumeError, Trace};
use systemscope_toy::{ToyCpu, ToyCpuConfig, ToyMemory, ToyMemoryConfig};

const SEED: u64 = 0x5EED;

/// How a session differs from the reference one.
#[derive(Clone, Copy, Default)]
struct Variant {
    seed: Option<u64>,
    ticks_per_second: Option<u64>,
    max_events_per_phase: Option<u64>,
    hz: Option<u64>,
    /// Renames the last memory, changing only the topology.
    rename: bool,
    /// Wraps the first memory so it reports another snapshot schema.
    schema_bump: bool,
    /// Wraps the first memory so it leaves part of its snapshot unread.
    padded: bool,
    /// Link latency in cycles.
    link_cycles: Option<u64>,
}

/// Wraps a memory and reports a different snapshot schema; otherwise identical.
struct Bumped(ToyMemory);

impl Component for Bumped {
    fn type_name(&self) -> &'static str {
        self.0.type_name()
    }
    fn ports(&self) -> Vec<PortSpec> {
        self.0.ports()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        self.0.init(ctx)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.0.handle_event(ev, ctx)
    }
    fn snapshot_schema_version(&self) -> u32 {
        self.0.snapshot_schema_version() + 1
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.0.snapshot(w);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, schema: u32) -> Result<(), RestoreError> {
        self.0.restore(r, schema - 1)
    }
}

/// Wraps a memory, writes one byte after its state, and never reads it back.
struct Padded(ToyMemory);

impl Component for Padded {
    fn type_name(&self) -> &'static str {
        self.0.type_name()
    }
    fn ports(&self) -> Vec<PortSpec> {
        self.0.ports()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        self.0.init(ctx)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.0.handle_event(ev, ctx)
    }
    fn snapshot_schema_version(&self) -> u32 {
        self.0.snapshot_schema_version()
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.0.snapshot(w);
        w.u8(0);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, schema: u32) -> Result<(), RestoreError> {
        self.0.restore(r, schema)
    }
}

/// Three CPU/memory pairs on one shared 3 GHz clock, so events from different pairs share
/// `(tick, phase)` and checkpoints can fall inside a phase.
fn build(v: Variant) -> Runtime {
    let clock = SimulationClock::new(
        v.ticks_per_second
            .unwrap_or(SimulationClock::DEFAULT_TICKS_PER_SECOND),
    )
    .unwrap();
    let mut t = TopologyBuilder::new(clock);
    let clock = t
        .add_clock(
            Frequency::from_hz(v.hz.unwrap_or(3_000_000_000)).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    for pair in 0..3u64 {
        let cpu = ToyCpu::new(ToyCpuConfig {
            clock,
            ops: 80 + 20 * pair,
            max_outstanding: 4,
            max_think_cycles: NonZeroU64::new(3).unwrap(),
            access_len: 8,
            slots: 16,
            write_percent: 30 + 10 * pair,
        });
        let memory = ToyMemory::new(ToyMemoryConfig {
            size: 16 * 8,
            read_latency: Duration::from_ns(10),
            write_latency: Duration::from_ns(6),
        });
        let c = t.add_component(format!("soc.cpu{pair}"), Box::new(cpu));
        let mem_path = if v.rename && pair == 2 {
            "soc.memX".to_owned()
        } else {
            format!("soc.mem{pair}")
        };
        let memory: Box<dyn Component> = match (pair, v.schema_bump, v.padded) {
            (0, true, _) => Box::new(Bumped(memory)),
            (0, _, true) => Box::new(Padded(memory)),
            _ => Box::new(memory),
        };
        let m = t.add_component(mem_path, memory);
        let link = LinkLatency::Cycles {
            domain: clock,
            k: v.link_cycles.unwrap_or(2),
        };
        t.connect((c, "mem"), (m, "mem"), Some(link));
    }
    let config = SessionConfig {
        seed: v.seed.unwrap_or(SEED),
        scheduler: SchedulerConfig {
            max_events_per_phase: v
                .max_events_per_phase
                .unwrap_or(SchedulerConfig::default().max_events_per_phase),
        },
    };
    t.elaborate(config).unwrap()
}

fn reference() -> Runtime {
    build(Variant::default())
}

fn run_to_end(rt: &mut Runtime) -> Vec<Dispatched> {
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

/// The end state of a run.
#[derive(Debug, PartialEq, Eq)]
struct End {
    state: [u8; 32],
    execution: [u8; 32],
    trace_bytes: Vec<u8>,
    trace: [u8; 32],
}

fn end(mut rt: Runtime) -> End {
    let trace = rt.take_trace().unwrap();
    End {
        state: rt.state_digest().unwrap(),
        execution: rt.execution_digest(),
        trace_bytes: trace.canonical_bytes(),
        trace: trace.digest(),
    }
}

/// A traced session stopped after `k` events: its snapshot and trace prefix.
fn checkpoint(k: usize) -> (Vec<u8>, Trace, [u8; 32]) {
    let mut rt = reference();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    let snapshot = rt.snapshot().unwrap();
    let digest = rt.execution_digest();
    (snapshot, rt.take_trace().unwrap(), digest)
}

/// A fresh session restored from `snapshot`, trace not yet resumed.
fn restored(snapshot: &[u8]) -> Runtime {
    let mut rt = reference();
    rt.restore(snapshot).unwrap();
    rt
}

#[test]
fn topology_hash_is_structural_and_order_sensitive() {
    assert_eq!(reference().topology_hash(), reference().topology_hash());
    // Session settings are not topology.
    let other_session = Variant {
        seed: Some(1),
        hz: Some(2_000_000_000),
        max_events_per_phase: Some(10),
        ..Variant::default()
    };
    assert_eq!(
        build(other_session).topology_hash(),
        reference().topology_hash()
    );
    let renamed = Variant {
        rename: true,
        ..Variant::default()
    };
    assert_ne!(build(renamed).topology_hash(), reference().topology_hash());
    let slower = Variant {
        link_cycles: Some(3),
        ..Variant::default()
    };
    assert_ne!(build(slower).topology_hash(), reference().topology_hash());

    // The same two components declared in the other order.
    let pair = |mem_first: bool| {
        let mut t = TopologyBuilder::new(SimulationClock::default());
        let clock = t
            .add_clock(
                Frequency::from_hz(1_000_000_000).unwrap(),
                Tick::ZERO,
                Rounding::Floor,
            )
            .unwrap();
        let memory = || {
            ToyMemory::new(ToyMemoryConfig {
                size: 8,
                read_latency: Duration::from_ns(1),
                write_latency: Duration::from_ns(1),
            })
        };
        let cpu = || {
            ToyCpu::new(ToyCpuConfig {
                clock,
                ops: 1,
                max_outstanding: 1,
                max_think_cycles: NonZeroU64::new(1).unwrap(),
                access_len: 8,
                slots: 1,
                write_percent: 0,
            })
        };
        let (c, m) = if mem_first {
            let m = t.add_component("m", Box::new(memory()));
            (t.add_component("c", Box::new(cpu())), m)
        } else {
            let c = t.add_component("c", Box::new(cpu()));
            (c, t.add_component("m", Box::new(memory())))
        };
        t.connect((c, "mem"), (m, "mem"), None);
        t.elaborate(SessionConfig::default())
            .unwrap()
            .topology_hash()
    };
    assert_ne!(pair(false), pair(true));
}

#[test]
fn execution_digest_is_deterministic_and_seed_dependent() {
    let digest = |seed| {
        let mut rt = build(Variant {
            seed: Some(seed),
            ..Variant::default()
        });
        assert_eq!(rt.execution_digest(), [0; 32]);
        rt.init().unwrap();
        // Init dispatches nothing, so the digest is still the initial one.
        assert_eq!(rt.execution_digest(), [0; 32]);
        run_to_end(&mut rt);
        rt.execution_digest()
    };
    assert_eq!(digest(SEED), digest(SEED));
    assert_ne!(digest(SEED), digest(SEED + 1));
}

#[test]
fn execution_digest_chains_every_canonical_event() {
    let mut rt = reference();
    rt.init().unwrap();
    let events = run_to_end(&mut rt);
    let expected = events.into_iter().fold([0; 32], |digest, ev| {
        let canonical = CanonicalEvent {
            key: ev.key,
            source: ev.source,
            target: ev.target,
            delivery: ev.delivery,
        };
        let mut h = blake3::Hasher::new();
        h.update(&digest);
        h.update(&canonical.to_bytes());
        *h.finalize().as_bytes()
    });
    assert_eq!(rt.execution_digest(), expected);
}

#[test]
fn tracing_does_not_change_snapshot_bytes() {
    let mut plain = reference();
    let mut traced = reference();
    traced.start_trace().unwrap();
    plain.init().unwrap();
    traced.init().unwrap();
    for _ in 0..300 {
        plain.step().unwrap().unwrap();
        traced.step().unwrap().unwrap();
        assert_eq!(plain.snapshot().unwrap(), traced.snapshot().unwrap());
    }
}

#[test]
fn restoring_and_resnapshotting_is_the_identity() {
    // Round-trip law: encode(restore(decode(encode(s)))) == encode(s), for every event
    // boundary of a run.
    let mut rt = reference();
    rt.init().unwrap();
    loop {
        let bytes = rt.snapshot().unwrap();
        let again = restored(&bytes);
        assert_eq!(again.snapshot().unwrap(), bytes);
        assert_eq!(again.state_digest().unwrap(), rt.state_digest().unwrap());
        assert_eq!(again.execution_digest(), rt.execution_digest());
        if rt.step().unwrap().is_none() {
            break;
        }
    }
}

/// Checkpoint indices: the number of events dispatched before the snapshot.
fn checkpoints(events: &[Dispatched]) -> Vec<(&'static str, usize)> {
    let n = events.len();
    let find = |what: &'static str, f: &dyn Fn(usize) -> bool| {
        let k = (1..n)
            .find(|&k| f(k))
            .unwrap_or_else(|| panic!("no {what}"));
        (what, k)
    };
    let same_phase =
        |a: &Dispatched, b: &Dispatched| (a.key.tick, a.key.phase) == (b.key.tick, b.key.phase);
    let mut out = vec![
        ("right after init", 0),
        ("after the first event", 1),
        find("tick boundary", &|k| {
            events[k - 1].key.tick != events[k].key.tick
        }),
        find("between COMPLETE and COMMIT", &|k| {
            events[k - 1].key.tick == events[k].key.tick
                && events[k - 1].key.phase == Phase::Complete
                && events[k].key.phase == Phase::Commit
        }),
        find("mid-phase with two or more left", &|k| {
            k + 1 < n
                && same_phase(&events[k - 1], &events[k])
                && same_phase(&events[k], &events[k + 1])
        }),
        ("halfway", n / 2),
        ("before the last event", n - 1),
        ("after the last event", n),
    ];
    // Eight more from a fixed SplitMix64 stream.
    let mut x: u64 = 0xA7_2000;
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
fn at2_resumed_runs_match_the_uninterrupted_run() {
    let mut rt = reference();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_to_end(&mut rt);
    assert!(events.len() > 1000, "{}", events.len());
    let expected = end(rt);

    for (what, k) in checkpoints(&events) {
        let (snapshot, prefix, digest) = checkpoint(k);
        // The session is dropped; only the bytes and the prefix survive.
        let mut rt = restored(&snapshot);
        assert_eq!(rt.lifecycle(), Lifecycle::Ready);
        assert_eq!(rt.snapshot().unwrap(), snapshot, "{what} ({k})");
        assert_eq!(rt.execution_digest(), digest, "{what} ({k})");
        rt.resume_trace(prefix).unwrap();
        let rest = run_to_end(&mut rt);
        assert_eq!(rest, events[k..], "{what} ({k})");
        assert_eq!(end(rt), expected, "{what} ({k})");
    }
}

fn restore_error(v: Variant, snapshot: &[u8]) -> RestoreError {
    let mut rt = build(v);
    let Err(RuntimeError::Restore(e)) = rt.restore(snapshot) else {
        panic!("restore accepted a mismatched snapshot");
    };
    // A failed restore faults the session.
    assert_eq!(rt.lifecycle(), Lifecycle::Faulted);
    assert!(rt.step().is_err());
    e
}

#[test]
fn restore_rejects_other_sessions_topologies_and_schemas() {
    let (snapshot, _, _) = checkpoint(500);
    let session = |v, field| {
        assert_eq!(
            restore_error(v, &snapshot),
            RestoreError::SessionMismatch(field)
        );
    };
    let v = Variant::default();
    session(
        Variant {
            seed: Some(SEED + 1),
            ..v
        },
        SessionField::Seed,
    );
    session(
        Variant {
            ticks_per_second: Some(2_000_000_000_000),
            ..v
        },
        SessionField::TicksPerSecond,
    );
    session(
        Variant {
            max_events_per_phase: Some(999),
            ..v
        },
        SessionField::MaxEventsPerPhase,
    );
    session(
        Variant {
            hz: Some(2_000_000_000),
            ..v
        },
        SessionField::ClockDomains,
    );
    assert_eq!(
        restore_error(Variant { rename: true, ..v }, &snapshot),
        RestoreError::TopologyMismatch
    );
    let found = systemscope_toy::memory::SNAPSHOT_SCHEMA;
    let RestoreError::SchemaVersion {
        component,
        expected,
        found: f,
    } = restore_error(
        Variant {
            schema_bump: true,
            ..v
        },
        &snapshot,
    )
    else {
        panic!("expected a schema error");
    };
    assert_eq!((component.0, expected, f), (1, found + 1, found));

    // The contracts version string starts after magic, format, and three u64 settings,
    // and its u32 length.
    let at = 8 + 4 + 3 * 8 + 4;
    assert_eq!(
        &snapshot[at..at + CONTRACTS_VERSION.len()],
        CONTRACTS_VERSION.as_bytes()
    );
    let mut patched = snapshot.clone();
    patched[at] ^= 1;
    assert_eq!(
        restore_error(v, &patched),
        RestoreError::SessionMismatch(SessionField::ContractsVersion)
    );
}

#[test]
fn restore_rejects_malformed_bytes() {
    let (snapshot, _, _) = checkpoint(200);
    let v = Variant::default();
    assert_eq!(
        restore_error(v, &snapshot[..8]),
        RestoreError::Decode(systemscope_contracts::canonical::DecodeError::Truncated)
    );
    assert_eq!(
        restore_error(v, &snapshot[..snapshot.len() - 1]),
        RestoreError::Decode(systemscope_contracts::canonical::DecodeError::Truncated)
    );
    let mut long = snapshot.clone();
    long.push(0);
    assert_eq!(
        restore_error(v, &long),
        RestoreError::Decode(systemscope_contracts::canonical::DecodeError::TrailingBytes)
    );
    let mut magic = snapshot.clone();
    magic[..8].copy_from_slice(b"NOTSNAP!");
    assert_ne!(magic[..8], SNAPSHOT_MAGIC);
    assert_eq!(restore_error(v, &magic), RestoreError::BadMagic);
    let mut format = snapshot.clone();
    format[8] = 9;
    assert_eq!(restore_error(v, &format), RestoreError::FormatVersion(9));
}

#[test]
fn restore_is_refused_outside_a_fresh_untraced_session() {
    let (snapshot, _, _) = checkpoint(100);
    // After start_trace, a restored session must resume a prefix instead.
    let mut rt = reference();
    rt.start_trace().unwrap();
    assert_eq!(rt.restore(&snapshot), Err(RuntimeError::TraceNeedsPrefix));
    // Only an elaborated session restores.
    let mut rt = reference();
    rt.init().unwrap();
    assert_eq!(
        rt.restore(&snapshot),
        Err(RuntimeError::InvalidState(Lifecycle::Ready))
    );
    let mut rt = restored(&snapshot);
    assert_eq!(
        rt.restore(&snapshot),
        Err(RuntimeError::InvalidState(Lifecycle::Ready))
    );
    // Snapshots are taken only when ready.
    assert_eq!(
        reference().snapshot(),
        Err(RuntimeError::InvalidState(Lifecycle::Elaborated))
    );
}

#[test]
fn resume_trace_accepts_only_the_matching_prefix_once() {
    let k = 400;
    let (snapshot, prefix, _) = checkpoint(k);

    // Not restored, or restored and already stepped.
    let mut rt = reference();
    rt.init().unwrap();
    assert_eq!(
        rt.resume_trace(prefix.clone()),
        Err(ResumeError::NotFreshlyRestored)
    );
    let mut rt = restored(&snapshot);
    rt.step().unwrap();
    assert_eq!(
        rt.resume_trace(prefix.clone()),
        Err(ResumeError::NotFreshlyRestored)
    );
    // Twice.
    let mut rt = restored(&snapshot);
    rt.resume_trace(prefix.clone()).unwrap();
    assert_eq!(
        rt.resume_trace(prefix.clone()),
        Err(ResumeError::NotFreshlyRestored)
    );

    // A header from another session.
    let mut other = prefix.clone();
    other.header.seed ^= 1;
    assert_eq!(
        restored(&snapshot).resume_trace(other),
        Err(ResumeError::HeaderMismatch)
    );

    // The prefix of another run with the same header: shorter, longer, and from a
    // different history.
    let (_, shorter, _) = checkpoint(k - 1);
    let (_, longer, _) = checkpoint(k + 1);
    for wrong in [shorter, longer] {
        assert_eq!(
            restored(&snapshot).resume_trace(wrong),
            Err(ResumeError::HistoryMismatch)
        );
    }
    // Dropping the last dispatch record leaves its handler's records orphaned or the
    // history short; either way it is refused.
    // The same last key over a different history: one earlier payload byte changed.
    let mut forged = prefix.clone();
    let data = forged
        .records
        .iter_mut()
        .find_map(|r| match r.fields.last_mut() {
            Some(("data", systemscope_contracts::trace::Value::Bytes(d))) if !d.is_empty() => {
                Some(d)
            }
            _ => None,
        })
        .unwrap();
    data[0] ^= 1;
    assert_eq!(
        restored(&snapshot).resume_trace(forged),
        Err(ResumeError::HistoryMismatch)
    );

    let mut cut = prefix.clone();
    let last = cut
        .records
        .iter()
        .rposition(|r| r.kind == systemscope_contracts::trace::DISPATCH_KIND)
        .unwrap();
    cut.records.remove(last);
    assert!(restored(&snapshot).resume_trace(cut).is_err());

    // The empty prefix matches only a snapshot taken right after init.
    let (fresh, empty, _) = checkpoint(0);
    let init_only = Trace {
        header: empty.header.clone(),
        records: Vec::new(),
    };
    assert_eq!(
        restored(&snapshot).resume_trace(init_only.clone()),
        Err(ResumeError::HistoryMismatch)
    );
    restored(&fresh).resume_trace(empty).unwrap();
}

/// Byte offsets of the variable-position fields in a snapshot.
struct Layout {
    digest: usize,
    queue: usize,
    rngs: usize,
}

fn layout(bytes: &[u8]) -> Layout {
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
    d.u64().unwrap();
    d.u64().unwrap();
    let digest = at(&d);
    d.raw(32).unwrap();
    let n = d.len().unwrap();
    assert!(n > 0);
    let queue = at(&d);
    for _ in 0..n {
        CanonicalEvent::decode(&mut d).unwrap();
    }
    d.len().unwrap();
    Layout {
        digest,
        queue,
        rngs: at(&d),
    }
}

#[test]
fn restore_rejects_impossible_states() {
    let v = Variant::default();
    let invalid = |bytes: &[u8]| match restore_error(v, bytes) {
        RestoreError::InvalidState(what) => what,
        e => panic!("{e:?}"),
    };
    // Right after init, nothing was dispatched and every queued event is a CPU's own wake.
    let (fresh, _, _) = checkpoint(0);
    let at = layout(&fresh);
    let mut digest = fresh.clone();
    digest[at.digest] = 1;
    assert_eq!(
        invalid(&digest),
        "execution digest without a dispatched event"
    );
    let mut rng = fresh.clone();
    rng[at.rngs..at.rngs + 32].fill(0);
    assert_eq!(invalid(&rng), "all-zero RNG state");
    // canonical(ev): key (17 bytes), then source and target.
    let mut target = fresh.clone();
    target[at.queue + 21] = 99;
    assert_eq!(invalid(&target), "queued event for an unknown component");
    let mut source = fresh.clone();
    source[at.queue + 17] ^= 1;
    assert_eq!(invalid(&source), "queued wake for another component");

    // A component that leaves part of its bytes unread fails the restore.
    let padded = Variant { padded: true, ..v };
    let mut rt = build(padded);
    rt.init().unwrap();
    let bytes = rt.snapshot().unwrap();
    assert_eq!(
        restore_error(padded, &bytes),
        RestoreError::Decode(DecodeError::TrailingBytes)
    );
}
