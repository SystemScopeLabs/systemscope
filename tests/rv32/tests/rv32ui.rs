//! M1-A2: the 40 selected `rv32ui` tests pass on SystemScope (`docs/m1-design.md` §10.2).

use std::cell::Cell;
use std::rc::Rc;

use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_rv32::manifest::Manifest;
use systemscope_rv32::runner::{
    End, PASS_CAUSE, judge, run, run_fixture, run_suite, run_suite_with,
};
use systemscope_rv32::{Rv32iProfile, SELECTED, hex, workspace_root};

#[test]
fn simple_passes() {
    let root = workspace_root();
    let manifest = Manifest::read(&root).unwrap();
    let simple = manifest
        .selected
        .iter()
        .find(|f| f.name == "simple")
        .unwrap();
    let result = run_fixture(&root, simple).unwrap();
    println!("{}", result.line());
    assert_eq!(result.verdict, Ok(()), "{}", result.line());
}

/// selected == executed == passed == 40, failed == 0. Prints one line per test with the
/// run's digests; they are not golden (that is M1.8), only reported.
#[test]
fn the_selected_rv32ui_suite_passes() {
    let report = run_suite(&workspace_root()).unwrap();
    for r in &report.results {
        let o = &r.outcome;
        println!("{}", r.line());
        println!(
            "       image_hash {} state {} execution {} trace {}",
            hex(&r.image_hash),
            o.state.map_or("-".to_owned(), |d| hex(&d)),
            hex(&o.execution),
            o.trace.map_or("-".to_owned(), |d| hex(&d)),
        );
    }
    println!(
        "selected {}, executed {}, passed {}, failed {}",
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    report.accept(SELECTED.len()).unwrap();
    for r in &report.results {
        assert!(
            matches!(&r.outcome.end, End::Trap { cause, .. } if cause == PASS_CAUSE),
            "{}",
            r.line()
        );
        assert!(r.outcome.state.is_some() && r.outcome.trace.is_some());
    }
}

/// The M2 CPU profile passes the same 40 tests (`docs/m2-design.md` §15.4).
#[test]
fn the_selected_rv32ui_suite_passes_with_the_m2_cpu_profile() {
    let report = run_suite_with(&workspace_root(), Rv32iProfile::M2).unwrap();
    for r in &report.results {
        println!("{}", r.line());
    }
    report.accept(SELECTED.len()).unwrap();
    for r in &report.results {
        assert!(
            matches!(&r.outcome.end, End::Trap { cause, .. } if cause == PASS_CAUSE),
            "{}",
            r.line()
        );
    }
}

/// The environment's failure path, end to end: with every `add` in the `add` image turned
/// into `sub`, upstream test 2 (0 + 0) still passes and test 3 (1 + 1) fails, so the run
/// ends in `RVTEST_FAIL` with `gp = a0 = (3 << 1) | 1` and the runner reports a failure.
#[test]
fn a_broken_instruction_fails_at_its_upstream_test_number() {
    let root = workspace_root();
    let manifest = Manifest::read(&root).unwrap();
    let add = manifest.selected.iter().find(|f| f.name == "add").unwrap();
    let mut image = add.read(&root).unwrap();
    let mut patched = 0;
    for segment in &mut image.segments {
        for word in segment.bytes.as_chunks_mut::<4>().0 {
            let w = u32::from_le_bytes(*word);
            // OP, funct3 0, funct7 0: ADD. Setting funct7 bit 5 makes it SUB.
            if w & 0xfe00_707f == 0x0000_0033 {
                *word = (w | 0x4000_0000).to_le_bytes();
                patched += 1;
            }
        }
    }
    assert!(
        patched > 30,
        "the add test's body is made of adds: {patched}"
    );
    let outcome = run(&image, false, Vec::new());
    assert!(
        matches!(&outcome.end, End::Trap { cause, .. } if cause == PASS_CAUSE),
        "RVTEST_FAIL also ends with ECALL: {outcome:?}"
    );
    assert_eq!((outcome.gp, outcome.a0), (7, 7), "{outcome:?}");
    let err = judge(&outcome).unwrap_err();
    assert!(err.contains("test number 3"), "{err}");
}

/// Counts events and never stops the run.
struct Counter(Rc<Cell<u64>>);

impl Observer for Counter {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, _: &WorldView<'_>) -> Control {
        self.0.set(self.0.get() + 1);
        Control::Continue
    }
}

/// M1-A7 in miniature: tracing and an extra observer change neither the verdict nor the
/// event count, `StateDigest`, or `ExecutionDigest`, and a run is reproducible.
#[test]
fn observation_does_not_change_any_result() {
    let root = workspace_root();
    let manifest = Manifest::read(&root).unwrap();
    for fixture in &manifest.selected {
        let image = fixture.read(&root).unwrap();
        let traced = run(&image, true, Vec::new());
        let count = Rc::new(Cell::new(0));
        let observed = run(&image, false, vec![Box::new(Counter(Rc::clone(&count)))]);
        let again = run(&image, true, Vec::new());
        assert_eq!(
            traced, again,
            "{}: a traced run is reproducible",
            fixture.name
        );
        assert_eq!(observed.trace, None);
        assert_eq!(
            observed,
            systemscope_rv32::runner::Outcome {
                trace: None,
                ..traced.clone()
            },
            "{}: tracing and observing change nothing else",
            fixture.name
        );
        assert_eq!(count.get(), observed.events, "{}", fixture.name);
        assert_eq!(judge(&traced), judge(&observed));
    }
}
