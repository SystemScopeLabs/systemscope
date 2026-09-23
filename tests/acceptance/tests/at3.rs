//! AT-3: observation invariance on the full `m0-reference` (`docs/m0-design.md` §9).
//!
//! O1 to O5 must end exactly like O0 in every digest, the RNG states, the scheduler, and
//! every component's state, however often they pause, step, or inspect. O1 and O5 must
//! record the same trace and export the same files.

mod common;

use systemscope_acceptance::observation::{
    Config, Observed, PROBE_INTERVAL, ensure_invariant, ensure_same_trace, observe,
};
use systemscope_acceptance::{FIXED_SEEDS, seed_from_env};
use systemscope_contracts::time::Tick;

fn at3(seed: u64) -> Vec<Observed> {
    let o0 = observe(seed, Config::O0, Tick::ZERO);
    let events = o0.digests.events;
    let points = o0.last_tick.0 / PROBE_INTERVAL + 1;
    let mut all = vec![];
    for config in Config::ALL.into_iter().skip(1) {
        let o = observe(seed, config, o0.last_tick);
        ensure_invariant(&o0, &o).unwrap_or_else(|e| panic!("seed {seed:#x}: {e}"));
        let (observes, inspected) = o.observes;
        match config {
            Config::O2 => {
                assert!(o.pauses > 0, "O2 never paused");
                assert_eq!(o.resumes, o.pauses, "every pause returns to the driver");
            }
            Config::O3 => assert_eq!(o.steps, events, "one event per step"),
            Config::O4 => {
                assert_eq!(observes, points);
                assert!(inspected >= 4 * points, "every component inspected");
            }
            Config::O5 => {
                assert_eq!(o.steps, events, "one event per step");
                assert_eq!(observes, points);
                assert!(o.pauses > 0 && o.resumes == 0, "steps ignore pauses");
            }
            _ => {}
        }
        all.push(o);
    }
    let o1 = &all[0];
    let o5 = &all[4];
    ensure_same_trace(o1, o5).unwrap_or_else(|e| panic!("seed {seed:#x}: {e}"));
    assert_eq!(
        all[1].pauses, o5.pauses,
        "O2 and O5 pause on the same events"
    );
    all.insert(0, o0);
    all
}

#[test]
fn seed_0_is_the_same_under_every_observation() {
    let seed = FIXED_SEEDS[0];
    let all = at3(seed);
    // The recorded trace is the golden one.
    common::golden()
        .check(seed, &all[1].digests)
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn seed_deadbeef_is_the_same_under_every_observation() {
    let seed = FIXED_SEEDS[2];
    let all = at3(seed);
    common::golden()
        .check(seed, &all[5].digests)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// Nightly: a random seed, compared only across configurations.
#[test]
#[ignore = "nightly: needs M0_SEED"]
fn nightly_seed_is_the_same_under_every_observation() {
    at3(seed_from_env());
}

/// The invariance check notices a run that differs from O0 in any way it looks at.
#[test]
fn invariance_violations_are_reported() {
    let seed = FIXED_SEEDS[1];
    let o0 = observe(seed, Config::O0, Tick::ZERO);
    let same = observe(seed, Config::O0, Tick::ZERO);
    assert_eq!(ensure_invariant(&o0, &same), Ok(()));

    let other_seed = observe(seed ^ 1, Config::O0, Tick::ZERO);
    assert!(ensure_invariant(&o0, &other_seed).is_err());

    let moved = Observed {
        sequence_around_points: (5, 6),
        ..observe(seed, Config::O0, Tick::ZERO)
    };
    let err = ensure_invariant(&o0, &moved).unwrap_err();
    assert!(err.contains("next_sequence"), "{err}");

    let unfired = Observed {
        points_left: 1,
        ..observe(seed, Config::O0, Tick::ZERO)
    };
    assert!(ensure_invariant(&o0, &unfired).is_err());

    assert!(ensure_same_trace(&o0, &same).is_err(), "no trace recorded");
}
