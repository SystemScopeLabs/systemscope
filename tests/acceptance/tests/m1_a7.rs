//! M1-A7: observation invariance on `m1-reference` (`docs/m1-design.md` §10.1), with the
//! M0 configurations O0 to O5 and M0's own checks (`docs/m0-design.md` §9 AT-3).
//!
//! O1 to O5 must end exactly like O0 in every digest, the RNG states, the scheduler, and
//! every component's state, however often they pause, step, or inspect. O1 and O5 must
//! record the same trace and export the same files, and that trace is the golden one.

use systemscope_acceptance::m1::golden::Golden;
use systemscope_acceptance::m1::observation::observe;
use systemscope_acceptance::m1::{self, LONG, Program, REFERENCE};
use systemscope_acceptance::observation::{
    Config, Observed, PROBE_INTERVAL, ensure_invariant, ensure_same_trace,
};
use systemscope_contracts::time::Tick;
use systemscope_rv32::workspace_root;

fn golden() -> Golden {
    let golden = Golden::parse(include_str!("../../golden/m1-reference.json"))
        .unwrap_or_else(|e| panic!("tests/golden/m1-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn program(name: &str) -> Program {
    m1::program(&workspace_root(), name).unwrap_or_else(|e| panic!("{e}"))
}

fn a7(name: &str) -> Vec<Observed> {
    let program = program(name);
    let components = if program.uart { 4 } else { 3 };
    let o0 = observe(&program, Config::O0, Tick::ZERO);
    let events = o0.digests.events;
    let points = o0.last_tick.0 / PROBE_INTERVAL + 1;
    let mut all = vec![];
    for config in Config::ALL.into_iter().skip(1) {
        let o = observe(&program, config, o0.last_tick);
        ensure_invariant(&o0, &o).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (observes, inspected) = o.observes;
        match config {
            Config::O2 => {
                assert!(o.pauses > 0, "O2 never paused");
                assert_eq!(o.resumes, o.pauses, "every pause returns to the driver");
            }
            Config::O3 => assert_eq!(o.steps, events, "one event per step"),
            Config::O4 => {
                assert_eq!(observes, points);
                assert!(
                    inspected >= components * points,
                    "every component inspected"
                );
            }
            Config::O5 => {
                assert_eq!(o.steps, events, "one event per step");
                assert_eq!(observes, points);
                assert!(o.pauses > 0 && o.resumes == 0, "steps ignore pauses");
            }
            _ => {}
        }
        println!(
            "{name} {config:?}: events {}, pauses {}, resumes {}, steps {}, observes {observes}",
            o.digests.events, o.pauses, o.resumes, o.steps
        );
        all.push(o);
    }
    ensure_same_trace(&all[0], &all[4]).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        all[1].pauses, all[4].pauses,
        "O2 and O5 pause on the same events"
    );
    // O0 ends at the golden state; the recorded trace is the golden one.
    let record = golden().record(name).cloned().unwrap();
    assert_eq!(o0.digests.events, record.events);
    assert_eq!(o0.digests.state, record.state);
    assert_eq!(o0.digests.execution, record.execution);
    assert_eq!(all[0].digests.trace, Some(record.trace));
    all.insert(0, o0);
    all
}

#[test]
fn hello_is_the_same_under_every_observation() {
    let all = a7(REFERENCE);
    // O2 pauses once per CPU Commit: each retired instruction and the trapping ECALL.
    let record = golden().record(REFERENCE).cloned().unwrap();
    assert_eq!(all[2].pauses, record.instret + 1);
}

#[test]
fn ld_st_is_the_same_under_every_observation() {
    a7(LONG);
}

/// The invariance check notices a run that differs from O0 in any way it looks at.
#[test]
fn invariance_violations_are_reported() {
    let hello = program(REFERENCE);
    let o0 = observe(&hello, Config::O0, Tick::ZERO);
    let same = observe(&hello, Config::O0, Tick::ZERO);
    assert_eq!(ensure_invariant(&o0, &same), Ok(()));

    let other = observe(&program("simple"), Config::O0, Tick::ZERO);
    assert!(ensure_invariant(&o0, &other).is_err());

    let moved = Observed {
        sequence_around_points: (5, 6),
        ..observe(&hello, Config::O0, Tick::ZERO)
    };
    let err = ensure_invariant(&o0, &moved).unwrap_err();
    assert!(err.contains("next_sequence"), "{err}");

    let mut changed = observe(&hello, Config::O0, Tick::ZERO);
    let last = changed.snapshot.len() - 1;
    changed.snapshot[last] ^= 1;
    assert!(ensure_invariant(&o0, &changed).is_err());

    assert!(ensure_same_trace(&o0, &same).is_err(), "no trace recorded");
}
