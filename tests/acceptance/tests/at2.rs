//! AT-2: snapshot/restore equivalence on the full `m0-reference` (`docs/m0-design.md` §9).
//!
//! Each checkpoint run is snapshotted, encoded, and dropped; a freshly elaborated runtime
//! decodes and restores the bytes, resumes the trace prefix, and runs to the end. Every
//! digest, the canonical trace bytes, and the final snapshot must equal the uninterrupted
//! run's.

mod common;

use systemscope_acceptance::checkpoint::{
    checkpoint_and_resume, checkpoints, ensure_same_end, reference, stop_after,
};
use systemscope_acceptance::golden::Golden;
use systemscope_acceptance::layout::{Layout, get_u64, put_u32, put_u64};
use systemscope_acceptance::{FIXED_SEEDS, seed_from_env};
use systemscope_contracts::canonical::DecodeError;
use systemscope_contracts::snapshot::{RestoreError, SessionField};
use systemscope_reference::{BUS, DMA, ReferenceConfig, build};
use systemscope_runtime::runtime::RuntimeError;
use systemscope_runtime::trace::ResumeError;

/// `tests/golden/m0-reference.mid.snap`, byte for byte.
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m0-reference.mid.snap");

/// Steps 1 to 3 for `seed`, and the reference run against the golden digests if given.
fn at2(seed: u64, golden: Option<&Golden>) {
    let reference = reference(seed);
    if let Some(golden) = golden {
        golden
            .check(seed, &reference.end.digests())
            .unwrap_or_else(|e| panic!("reference run: {e}"));
    }
    for (what, k) in checkpoints(&reference.keys, seed) {
        let end = checkpoint_and_resume(seed, k, |_| {})
            .unwrap_or_else(|e| panic!("seed {seed:#x}, {what} (after {k} events): {e}"));
        ensure_same_end(&reference.end, &end)
            .unwrap_or_else(|e| panic!("seed {seed:#x}, {what} (after {k} events): {e}"));
    }
}

#[test]
fn seed_0_resumes_exactly_from_every_checkpoint() {
    at2(FIXED_SEEDS[0], Some(&common::golden()));
}

#[test]
fn seed_1_resumes_exactly_from_every_checkpoint() {
    at2(FIXED_SEEDS[1], Some(&common::golden()));
}

#[test]
fn seed_deadbeef_resumes_exactly_from_every_checkpoint() {
    at2(FIXED_SEEDS[2], Some(&common::golden()));
}

/// Nightly: a random seed, compared only with its own uninterrupted run.
#[test]
#[ignore = "nightly: needs M0_SEED"]
fn nightly_seed_resumes_exactly_from_every_checkpoint() {
    at2(seed_from_env(), None);
}

#[test]
fn the_checkpoints_cover_every_required_position() {
    let keys = reference(FIXED_SEEDS[0]).keys;
    let points = checkpoints(&keys, FIXED_SEEDS[0]);
    let n = keys.len();
    let names: Vec<&str> = points.iter().map(|(what, _)| *what).collect();
    assert_eq!(
        names[..7],
        [
            "right after init",
            "after the first event",
            "at a tick boundary",
            "between Complete and Commit of one tick",
            "mid-phase with events left in it",
            "halfway",
            "before the last event",
        ]
    );
    assert_eq!(names[7..], ["seeded random"; 8]);
    let k = |i: usize| points[i].1;
    assert_eq!((k(0), k(1), k(5), k(6)), (0, 1, n / 2, n - 1));
    assert!(keys[k(2) - 1].tick < keys[k(2)].tick);
    let (before, after) = (keys[k(3) - 1], keys[k(3)]);
    assert_eq!(before.tick, after.tick);
    assert_eq!(
        (before.phase, after.phase),
        (
            systemscope_contracts::event::Phase::Complete,
            systemscope_contracts::event::Phase::Commit
        )
    );
    let slot = |i: usize| (keys[i].tick, keys[i].phase);
    assert!(slot(k(4) - 1) == slot(k(4)) && slot(k(4)) == slot(k(4) + 1));
    // The random indices differ from each other and from seed to seed.
    let random: Vec<usize> = points[7..].iter().map(|p| p.1).collect();
    assert!(random.iter().all(|&r| r <= n));
    assert!(random.windows(2).all(|w| w[0] != w[1]));
    let other: Vec<usize> = checkpoints(&keys, FIXED_SEEDS[1])[7..]
        .iter()
        .map(|p| p.1)
        .collect();
    assert_ne!(random, other);
}

/// The resumed run starts from the bytes, not from anything the old runtime left: a
/// doctored DMA checksum shows up in the final state.
#[test]
fn a_resumed_run_starts_from_the_snapshot_bytes() {
    let seed = FIXED_SEEDS[2];
    let reference = reference(seed);
    let k = reference.keys.len() / 2;
    let end = checkpoint_and_resume(seed, k, |bytes| {
        let layout = Layout::parse(bytes).unwrap();
        let checksum = layout.components[DMA.0 as usize].state.end - 8;
        let value = get_u64(bytes, checksum);
        put_u64(bytes, checksum, value ^ 1);
    })
    .expect("a different checksum is still a valid state");
    let err = ensure_same_end(&reference.end, &end).unwrap_err();
    assert!(err.contains("StateDigest"), "{err}");
}

#[test]
fn portable_snapshot_resumes_to_the_golden_digests() {
    let golden = common::golden();
    let resumed = golden
        .check_portable(MID_SNAPSHOT)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resumed.trace, None);
}

/// The portability check fails on changed bytes, and on a snapshot that restores but ends
/// elsewhere.
#[test]
fn portable_snapshot_mismatches_are_reported() {
    let golden = common::golden();
    let mut bytes = MID_SNAPSHOT.to_vec();
    let layout = Layout::parse(&bytes).unwrap();
    let checksum = layout.components[DMA.0 as usize].state.end - 8;
    bytes[checksum] ^= 1;
    let err = golden.check_portable(&bytes).unwrap_err();
    assert!(err.contains("bytes differ"), "{err}");

    let mut rehashed = golden.clone();
    rehashed.mid.blake3 = *blake3::hash(&bytes).as_bytes();
    let err = rehashed.check_portable(&bytes).unwrap_err();
    assert!(err.contains("StateDigest"), "{err}");

    let mut wrong = golden.clone();
    wrong.seeds[2].1.execution[0] ^= 1;
    let err = wrong.check_portable(MID_SNAPSHOT).unwrap_err();
    assert!(err.contains("ExecutionDigest"), "{err}");
}

fn restore_error(seed: u64, bytes: &[u8]) -> RestoreError {
    let mut rt = build(ReferenceConfig::full(seed));
    match rt.restore(bytes) {
        Err(RuntimeError::Restore(e)) => e,
        other => panic!("expected a restore error, got {other:?}"),
    }
}

fn doctored(edit: impl FnOnce(&mut Vec<u8>, &Layout)) -> Vec<u8> {
    let mut bytes = MID_SNAPSHOT.to_vec();
    let layout = Layout::parse(&bytes).unwrap();
    edit(&mut bytes, &layout);
    bytes
}

#[test]
fn schema_topology_and_session_mismatches_are_rejected() {
    let seed = common::golden().mid.seed;
    let bus = doctored(|b, l| {
        let at = l.components[BUS.0 as usize].schema;
        let found = u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        put_u32(b, at, found + 1);
    });
    assert!(matches!(
        restore_error(seed, &bus),
        RestoreError::SchemaVersion { component, expected, found }
            if component == BUS && found == expected + 1
    ));
    let topology = doctored(|b, l| b[l.topology_hash] ^= 1);
    assert_eq!(
        restore_error(seed, &topology),
        RestoreError::TopologyMismatch
    );

    assert_eq!(
        restore_error(seed ^ 1, MID_SNAPSHOT),
        RestoreError::SessionMismatch(SessionField::Seed)
    );
    let fields: [(SessionField, Vec<u8>); 4] = [
        (
            SessionField::TicksPerSecond,
            doctored(|b, l| put_u64(b, l.ticks_per_second, 1_000_000_000_000_000)),
        ),
        (
            SessionField::MaxEventsPerPhase,
            doctored(|b, l| put_u64(b, l.max_events_per_phase, 7)),
        ),
        (
            SessionField::ContractsVersion,
            doctored(|b, l| b[l.contracts_version + 4] ^= 1),
        ),
        (
            SessionField::ClockDomains,
            doctored(|b, l| {
                let num = get_u64(b, l.first_domain_num);
                put_u64(b, l.first_domain_num, num + 1);
            }),
        ),
    ];
    for (field, bytes) in fields {
        assert_eq!(
            restore_error(seed, &bytes),
            RestoreError::SessionMismatch(field)
        );
    }
}

#[test]
fn malformed_snapshots_are_rejected() {
    let seed = common::golden().mid.seed;
    let snap = MID_SNAPSHOT;
    for len in 0..snap.len() {
        let err = restore_error(seed, &snap[..len]);
        assert!(
            matches!(err, RestoreError::Decode(_) | RestoreError::BadMagic),
            "truncated to {len}: {err:?}"
        );
    }
    let mut long = snap.to_vec();
    long.push(0);
    assert!(matches!(
        restore_error(seed, &long),
        RestoreError::Decode(_)
    ));
    assert_eq!(
        restore_error(seed, &doctored(|b, _| b[0] ^= 1)),
        RestoreError::BadMagic
    );
    assert_eq!(
        restore_error(seed, &doctored(|b, l| put_u32(b, l.format_version, 2))),
        RestoreError::FormatVersion(2)
    );
    assert_eq!(
        restore_error(seed, &doctored(|b, l| b[l.last_dispatched] = 7)),
        RestoreError::Decode(DecodeError::InvalidTag {
            what: "option",
            tag: 7
        })
    );
}

#[test]
fn impossible_scheduler_and_rng_states_are_rejected() {
    let seed = common::golden().mid.seed;
    let cases: [(&str, Vec<u8>); 5] = [
        (
            "a queued sequence is not below next_sequence",
            doctored(|b, l| put_u64(b, l.next_sequence, 0)),
        ),
        (
            "a queued event is not after the last dispatched event",
            // The last dispatched key's tick, right after its option tag.
            doctored(|b, l| put_u64(b, l.last_dispatched + 1, u64::MAX)),
        ),
        (
            "dispatched_in_phase is inconsistent",
            doctored(|b, l| put_u64(b, l.dispatched_in_phase, 0)),
        ),
        (
            "two queued events share a sequence",
            doctored(|b, l| {
                // Each queued event starts with its key: tick u64, phase u8, sequence u64.
                let sequence = get_u64(b, l.queue[0] + 9);
                put_u64(b, l.queue[1] + 9, sequence);
            }),
        ),
        (
            "all-zero RNG state",
            doctored(|b, l| b[l.rng[DMA.0 as usize]..][..32].fill(0)),
        ),
    ];
    for (what, bytes) in cases {
        assert_eq!(
            restore_error(seed, &bytes),
            RestoreError::InvalidState(what)
        );
    }
}

#[test]
fn resume_trace_rejects_every_prefix_but_its_own() {
    let seed = FIXED_SEEDS[0];
    let k = 5_000;
    let resume = |snapshot: &[u8], prefix| {
        let mut rt = build(ReferenceConfig::full(seed));
        rt.restore(snapshot).unwrap();
        rt.resume_trace(prefix)
    };
    let own = stop_after(seed, k);
    let other = stop_after(seed ^ 1, k);
    assert_eq!(
        resume(&own.snapshot, other.prefix),
        Err(ResumeError::HeaderMismatch)
    );
    let mut short = own.prefix.clone();
    short.records.truncate(short.records.len() - 20);
    assert!(resume(&own.snapshot, short).is_err());
    let longer = stop_after(seed, k + 1).prefix;
    assert!(resume(&own.snapshot, longer).is_err());
    let mut rt = build(ReferenceConfig::full(seed));
    rt.restore(&own.snapshot).unwrap();
    rt.step().unwrap();
    assert_eq!(
        rt.resume_trace(own.prefix.clone()),
        Err(ResumeError::NotFreshlyRestored)
    );
    assert_eq!(resume(&own.snapshot, own.prefix), Ok(()));
}
