//! M3.7 snapshot/restore on `m3-reference` (`docs/m3-design.md` §9.2, §17.4).
//!
//! Every boundary of the reference scenario, from before the first event to after the
//! last, is read by the independent reader and platform validator
//! ([`systemscope_acceptance::m3::portable`]), classified, restored into a freshly built
//! platform, re-encoded, and continued from there, so every event is dispatched by a
//! platform restored just before it. The §9.2 stress points are located and named, and
//! several instances of each are resumed exactly: traced, with the trace prefix, judged
//! by §12.3, without a reissue, and compared in full. `cargo xtask m3-golden
//! every-event` resumes every checkpoint on its own; these tests resume windows of it.
//! Then malformed and doctored snapshots are refused without a panic, and a refused
//! restore leaves no usable platform.

use std::collections::BTreeSet;

use systemscope_acceptance::m3::Fixture;
use systemscope_acceptance::m3::checkpoint::{
    self, Coverage, During, Reference, Stress, every_event, resume_exact, resume_untraced,
    snapshot_after,
};
use systemscope_acceptance::m3::golden::Golden;
use systemscope_acceptance::m3::portable::{self, Platform, Purpose};
use systemscope_runtime::runtime::{Lifecycle, RuntimeError};
use systemscope_rv32::m3::Scenario;
use systemscope_rv32::workspace_root;

/// `tests/golden/m3-reference.mid.snap`, byte for byte.
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m3-reference.mid.snap");
const GOLDEN_JSON: &str = include_str!("../../golden/m3-reference.json");

fn golden() -> Golden {
    let golden = Golden::parse(GOLDEN_JSON)
        .unwrap_or_else(|e| panic!("tests/golden/m3-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn fixture(scenario: Scenario) -> Fixture {
    Fixture::read(&workspace_root(), scenario).unwrap_or_else(|e| panic!("{e}"))
}

fn reference_of(fixture: &Fixture) -> Reference {
    checkpoint::reference(fixture).unwrap_or_else(|e| panic!("{e}"))
}

fn survey_of(fixture: &Fixture) -> Coverage {
    checkpoint::survey(fixture).unwrap_or_else(|e| panic!("{e}"))
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

// ---------------------------------------------------------------------------------------
// Every event.

/// §9.2 and §17.4: every boundary passes the reader and the validator and restores in
/// place, every event is dispatched by a just-restored platform, and the run ends in the
/// reference's state. Every stress point occurs.
#[test]
fn every_boundary_is_portable_and_resumes_in_place() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let coverage = checkpoint::chained(&fixture, &reference).unwrap_or_else(|e| panic!("{e}"));
    let n = reference.events();
    assert_eq!(coverage.checked, n + 1, "every boundary, 0 through {n}");
    for s in Stress::ALL {
        let at = coverage.stress.get(&s).map_or(&[][..], Vec::as_slice);
        println!(
            "{:70} {:6} boundaries, first {:?}",
            s.name(),
            at.len(),
            at.first()
        );
        assert!(!at.is_empty(), "no boundary is at {}", s.name());
    }
    println!(
        "{} place combinations, media outstanding at {} boundaries, at most {} events \
         queued, snapshots up to {} bytes",
        coverage.places.len(),
        coverage.media,
        coverage.max_queued,
        coverage.max_bytes
    );
    // Every master's transaction is seen at every place it can be.
    for (m, name) in ["cpu", "dma", "kernel"].into_iter().enumerate() {
        let places: BTreeSet<_> = coverage
            .places
            .keys()
            .flatten()
            .filter(|p| p.0 == m)
            .map(|p| p.1)
            .collect();
        println!("{name}: {places:?}");
        assert!(places.len() >= 5, "{name} is seen at only {places:?}");
    }
    assert_eq!(
        coverage.mid_checkpoint().unwrap() as u64,
        golden().mid.after_events
    );
    // Exceptions and first dispatches are counted exactly: 23 delegated exceptions and
    // one first U fetch per process.
    let count = |s| coverage.stress.get(&s).map_or(0, Vec::len);
    assert_eq!(count(Stress::ExceptionEntry), 23);
    assert_eq!(
        count(Stress::SretBeforeFirstFetch),
        Scenario::Reference.programs().len()
    );
}

/// The validator also holds on every boundary of the other scenario.
#[test]
fn every_nofault_boundary_is_portable() {
    let fixture = fixture(Scenario::NoFault);
    let coverage = survey_of(&fixture);
    assert_eq!(coverage.checked, 141_129);
    let count = |s| coverage.stress.get(&s).map_or(0, Vec::len);
    assert_eq!(count(Stress::ExceptionEntry), 21);
    assert!(count(Stress::MidWalk(Purpose::Store, 0)) > 0);
}

/// Every stress point's first, middle, and last boundary resume exactly.
#[test]
fn stress_points_resume_exactly() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let coverage = survey_of(&fixture);
    let points = coverage.exact_points().unwrap_or_else(|e| panic!("{e}"));
    let mut failures = Vec::new();
    for &(s, k) in &points {
        let snapshot = snapshot_after(&fixture, k).unwrap();
        if let Err(e) = resume_exact(&fixture, &reference, k, snapshot) {
            failures.push(format!("{} at {k}: {e}", s.name()));
        }
    }
    println!("{} exact resumes at the stress points", points.len());
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(points.len() >= 3 * Stress::ALL.len() - 2);
}

/// Checkpoints spread over the whole run resume exactly, from before the first event to
/// before the last. After the last event a resumed run dispatches nothing, so the runner
/// has no views to judge; [`snapshots_are_canonical_and_deterministic`] resumes that
/// checkpoint untraced.
#[test]
fn spaced_checkpoints_resume_exactly() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let n = reference.events();
    let mut ks: Vec<usize> = (0..n).step_by(n / 40).collect();
    ks.push(n - 1);
    ks.dedup();
    for &k in &ks {
        let snapshot = snapshot_after(&fixture, k).unwrap();
        resume_exact(&fixture, &reference, k, snapshot).unwrap_or_else(|e| panic!("{e}"));
    }
    println!("{} spaced exact resumes", ks.len());
}

/// Windows of the every-event sweep: every checkpoint in them restored into its own
/// platform and run to the end. `cargo xtask m3-golden every-event` covers them all.
#[test]
fn windows_of_every_event_resume() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let n = reference.events();
    let mid = golden().mid.after_events as usize;
    for range in [0..400, mid - 300..mid + 300, n - 400..n + 1] {
        let sweep = every_event(&fixture, &reference, range.clone(), threads());
        assert_eq!(sweep.checked, range.len());
        assert!(
            sweep.failures.is_empty(),
            "{range:?}: {:?}",
            &sweep.failures[..sweep.failures.len().min(5)]
        );
    }
}

/// The committed portable snapshot restores and resumes, traced from a recorded prefix,
/// to the reference's exact end.
#[test]
fn the_portable_snapshot_resumes_exactly() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let golden = golden();
    golden
        .check_portable(&fixture, MID_SNAPSHOT)
        .unwrap_or_else(|e| panic!("{e}"));
    let k = golden.mid.after_events as usize;
    resume_exact(&fixture, &reference, k, MID_SNAPSHOT.to_vec()).unwrap_or_else(|e| panic!("{e}"));
    let (p, acc) = portable::read(MID_SNAPSHOT).unwrap();
    let points = checkpoint::Tracker::default().classify(&p, &acc);
    assert!(
        points.contains(&Stress::HeldAccess(During::Syscall)),
        "{points:?}"
    );
    assert!(points.contains(&Stress::PartialWrite), "{points:?}");
    assert!(acc.held.is_some() && acc.kernel.is_some() && acc.cpu.is_some());
}

// ---------------------------------------------------------------------------------------
// Canonical bytes.

/// Two independent runs have the same bytes at the same checkpoint; restoring and
/// re-encoding is the identity; two platforms restored from the same bytes end alike.
#[test]
fn snapshots_are_canonical_and_deterministic() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let n = reference.events();
    for k in [0, 1, 777, n / 3, golden().mid.after_events as usize, n] {
        let a = snapshot_after(&fixture, k).unwrap();
        let b = snapshot_after(&fixture, k).unwrap();
        assert!(a == b, "two runs differ at checkpoint {k}");
        let mut rt = fixture.platform();
        rt.restore(&a).unwrap();
        assert!(
            rt.snapshot().unwrap() == a,
            "checkpoint {k} re-encodes differently"
        );
        resume_untraced(&fixture, &reference, k, &a).unwrap_or_else(|e| panic!("{e}"));
        resume_untraced(&fixture, &reference, k, &b).unwrap_or_else(|e| panic!("{e}"));
    }
}

// ---------------------------------------------------------------------------------------
// Malformed and doctored snapshots.

/// How a doctored snapshot is caught.
#[derive(Debug, PartialEq, Eq)]
enum Caught {
    /// The runtime's restore refuses it, and the validator does too.
    Both,
    /// Only the validator: the runtime restores it, and the resumed run diverges.
    Validator,
    /// Only the resumed run: a counter the state cannot pin, which then allocates a
    /// `TxnId` the reference run did not.
    Resume,
}

/// Restores `bytes` into a fresh platform. A refusal must leave the session faulted: no
/// step, no second restore, so no partially restored platform ever runs.
fn restores(fixture: &Fixture, bytes: &[u8]) -> bool {
    let mut rt = fixture.platform();
    match rt.restore(bytes) {
        Ok(()) => true,
        Err(_) => {
            assert_eq!(rt.lifecycle(), Lifecycle::Faulted);
            assert!(
                rt.step().is_err(),
                "a refused restore left a runnable session"
            );
            assert!(matches!(
                rt.restore(MID_SNAPSHOT),
                Err(RuntimeError::InvalidState(Lifecycle::Faulted))
            ));
            false
        }
    }
}

fn caught(
    fixture: &Fixture,
    reference: &Reference,
    k: usize,
    bytes: &[u8],
) -> Result<Caught, String> {
    let validator = portable::read(bytes).is_err();
    let runtime = !restores(fixture, bytes);
    match (runtime, validator) {
        (true, true) => Ok(Caught::Both),
        (true, false) => Err("the runtime refuses what the validator accepts".to_owned()),
        (false, true) => {
            if resume_untraced(fixture, reference, k, bytes).is_ok() {
                return Err("the validator refuses what resumes exactly".to_owned());
            }
            Ok(Caught::Validator)
        }
        (false, false) => match resume_untraced(fixture, reference, k, bytes) {
            Ok(()) => Err("accepted everywhere and resumes exactly".to_owned()),
            Err(_) => Ok(Caught::Resume),
        },
    }
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

fn add(bytes: &mut [u8], at: usize, delta: u8) {
    bytes[at] = bytes[at].wrapping_add(delta);
}

/// The §17.4 corruption matrix: each doctoring at a named field of a snapshot at a point
/// where that field matters is caught, by the runtime and the validator both where the
/// component codecs see it, by the validator where only the platform does, and by the
/// resumed run where only a free-running counter changed.
#[test]
fn doctored_snapshots_are_caught() {
    let fixture = fixture(Scenario::Reference);
    let reference = reference_of(&fixture);
    let coverage = survey_of(&fixture);
    let first = |s: Stress| coverage.stress[&s][0];
    let mid = golden().mid.after_events as usize;
    let walk1 = first(Stress::MidWalk(Purpose::Store, 1));
    let walk0 = first(Stress::MidWalk(Purpose::Load, 0));
    let beat = first(Stress::BeatBehindPoll);
    let bases: Vec<(usize, Vec<u8>, Platform)> = [mid, walk1, walk0, beat]
        .into_iter()
        .map(|k| {
            let bytes = snapshot_after(&fixture, k).unwrap();
            let p = Platform::decode(&bytes).unwrap();
            (k, bytes, p)
        })
        .collect();
    let base = |k: usize| bases.iter().find(|b| b.0 == k).unwrap();
    type Doctor = Box<dyn Fn(&mut Vec<u8>, &Platform)>;
    let at = |p: &Platform, name: &str| p.offset(name).unwrap_or_else(|| panic!("no field {name}"));
    let cases: Vec<(&str, usize, Caught, Doctor)> = vec![
        (
            "magic",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "runtime.magic"), 1)),
        ),
        (
            "format version",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "runtime.format_version"), 1)),
        ),
        (
            "component count",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "runtime.component_count"), 1)),
        ),
        (
            "missing component",
            mid,
            Caught::Both,
            Box::new(move |b, p| {
                add(b, at(p, "runtime.component_count"), 0xFF);
                b.truncate(at(p, "runtime.component7.id"));
            }),
        ),
        (
            "duplicate component",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "runtime.component7.id"), &6u32.to_le_bytes())),
        ),
        (
            "unknown component",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "runtime.component7.id"), &8u32.to_le_bytes())),
        ),
        (
            "CPU schema 2",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "runtime.component0.schema"), &2u32.to_le_bytes())),
        ),
        (
            "unknown schema",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "runtime.component7.schema"), &2u32.to_le_bytes())),
        ),
        (
            "component length",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "runtime.component7.len"), 1)),
        ),
        (
            "trailing byte",
            mid,
            Caught::Both,
            Box::new(|b, _| b.push(0)),
        ),
        (
            "truncated",
            mid,
            Caught::Both,
            Box::new(|b, _| {
                b.pop();
            }),
        ),
        (
            "queue length",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "runtime.queue"), 1)),
        ),
        (
            "CPU privilege U",
            mid,
            Caught::Validator,
            Box::new(move |b, p| put(b, at(p, "cpu.priv"), &[0])),
        ),
        (
            "CPU privilege reserved",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "cpu.priv"), &[2])),
        ),
        (
            "CPU state tag",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "cpu.state"), &[9])),
        ),
        (
            "CPU TxnId counter",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "cpu.next_txn"), 1)),
        ),
        (
            "walk level 1 -> 0",
            walk1,
            Caught::Validator,
            Box::new(move |b, p| put(b, at(p, "cpu.walk.level"), &[0])),
        ),
        (
            "walk level 0 -> 1",
            walk0,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "cpu.walk.level"), &[1])),
        ),
        (
            "walk level 2",
            walk0,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "cpu.walk.level"), &[2])),
        ),
        (
            "bus kgate master",
            mid,
            Caught::Validator,
            Box::new(move |b, p| add(b, at(p, "bus.kgate.active") + 1, 2)),
        ),
        (
            "bus kgate direction",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "bus.kgate.active") + 19, 1)),
        ),
        (
            "bus downstream",
            mid,
            Caught::Validator,
            Box::new(move |b, p| add(b, at(p, "bus.kgate.active") + 11, 1)),
        ),
        (
            "DMA beat",
            beat,
            Caught::Validator,
            Box::new(move |b, p| add(b, at(p, "blk.beat"), 1)),
        ),
        (
            "DMA block",
            beat,
            Caught::Validator,
            Box::new(move |b, p| add(b, at(p, "blk.block"), 1)),
        ),
        (
            "DMA engine idle",
            beat,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "blk.engine"), &[0])),
        ),
        (
            "DMA TxnId counter",
            mid,
            Caught::Resume,
            Box::new(move |b, p| add(b, at(p, "blk.dma_txn"), 1)),
        ),
        (
            "kgate held txn",
            mid,
            Caught::Validator,
            Box::new(move |b, p| add(b, at(p, "kernel.held") + 1, 1)),
        ),
        (
            "kernel stage",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "kernel.stage"), &[12])),
        ),
        (
            "kernel Wait as Issue",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "kernel.state"), &[1])),
        ),
        (
            "kernel TxnId counter",
            walk0,
            Caught::Resume,
            Box::new(move |b, p| add(b, at(p, "kernel.next_txn"), 1)),
        ),
        (
            "kernel life",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "kernel.life"), &[2])),
        ),
        (
            "current PID",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "kernel.current") + 1, 1)),
        ),
        (
            "PCB state",
            mid,
            Caught::Both,
            Box::new(move |b, p| put(b, at(p, "kernel.pcb0.state"), &[0])),
        ),
        (
            "frame bitmap",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "kernel.bitmap") + 4 + 8, 0x80)),
        ),
        (
            "syscall progress",
            mid,
            Caught::Both,
            Box::new(move |b, p| add(b, at(p, "kernel.output.done"), 1)),
        ),
        (
            "UART output",
            mid,
            Caught::Resume,
            Box::new(move |b, p| {
                let at = at(p, "uart.output") + 4;
                b[at] ^= 0x20;
            }),
        ),
    ];
    let mut wrong = Vec::new();
    for (name, k, expected, doctor) in &cases {
        let (_, original, p) = base(*k);
        let mut bytes = original.clone();
        doctor(&mut bytes, p);
        assert!(bytes != *original, "{name}: the doctoring changed nothing");
        match caught(&fixture, &reference, *k, &bytes) {
            Ok(how) => {
                println!("{name:24} at {k:6}: {how:?}");
                if how != *expected {
                    wrong.push(format!("{name}: {how:?}, expected {expected:?}"));
                }
            }
            Err(e) => wrong.push(format!("{name}: {e}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Truncations and single-byte flips all over the portable snapshot never panic, and
/// every truncation is refused by both.
#[test]
fn malformed_snapshots_never_panic() {
    let fixture = fixture(Scenario::Reference);
    let p = Platform::decode(MID_SNAPSHOT).unwrap();
    let mut cuts: BTreeSet<usize> = (0..MID_SNAPSHOT.len()).step_by(1009).collect();
    for e in &p.entries {
        for at in [e.state.start, e.state.end] {
            cuts.extend(
                [at - 1, at, at + 1]
                    .into_iter()
                    .filter(|&c| c < MID_SNAPSHOT.len()),
            );
        }
    }
    for &cut in &cuts {
        let bytes = &MID_SNAPSHOT[..cut];
        assert!(portable::read(bytes).is_err(), "a cut at {cut} passes");
        assert!(!restores(&fixture, bytes), "a cut at {cut} restores");
    }
    let (mut refused, mut accepted) = (0, 0);
    for i in 0..3000usize {
        let at = (i * 7919 + 13) % MID_SNAPSHOT.len();
        let mut bytes = MID_SNAPSHOT.to_vec();
        bytes[at] ^= 1 << (i % 8);
        let validator = portable::read(&bytes).is_ok();
        let runtime = restores(&fixture, &bytes);
        assert!(!runtime || validator || portable::read(&bytes).is_err());
        if validator {
            accepted += 1
        } else {
            refused += 1
        }
    }
    println!(
        "{} cuts refused; flips: {refused} refused, {accepted} accepted",
        cuts.len()
    );
}
