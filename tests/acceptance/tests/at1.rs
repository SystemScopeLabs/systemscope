//! AT-1: reproducibility on the full `m0-reference` (`docs/m0-design.md` §9).
//!
//! Step 1 runs each seed in two separate `m0-run` processes. Step 2 compares the fixed
//! seeds with the committed golden digests, which both CI operating systems must match.
//! Step 3 checks that the seed is used.

mod common;

use std::path::Path;

use systemscope_acceptance::FIXED_SEEDS;
use systemscope_acceptance::digests::full_run;
use systemscope_acceptance::process::{ensure_reproduced, run_in_processes};
use systemscope_acceptance::seed_from_env;

const M0_RUN: &str = env!("CARGO_BIN_EXE_m0-run");

/// Steps 1 and 2 for a fixed seed.
fn fixed_seed(seed: u64) {
    let runs = run_in_processes(Path::new(M0_RUN), seed, 2).unwrap_or_else(|e| panic!("{e}"));
    ensure_reproduced(seed, &runs).unwrap_or_else(|e| panic!("{e}"));
    common::golden()
        .check(seed, &runs[0].digests)
        .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn seed_0_reproduces_in_separate_processes_and_matches_the_golden() {
    fixed_seed(FIXED_SEEDS[0]);
}

#[test]
fn seed_1_reproduces_in_separate_processes_and_matches_the_golden() {
    fixed_seed(FIXED_SEEDS[1]);
}

#[test]
fn seed_deadbeef_reproduces_in_separate_processes_and_matches_the_golden() {
    fixed_seed(FIXED_SEEDS[2]);
}

#[test]
fn different_seeds_produce_different_execution_digests() {
    let mut seen = Vec::new();
    for seed in FIXED_SEEDS.into_iter().chain([2, u64::MAX]) {
        let execution = full_run(seed, false).execution;
        assert!(
            !seen.contains(&execution),
            "seed {seed:#x} repeats another seed's ExecutionDigest"
        );
        seen.push(execution);
    }
    let golden = common::golden();
    for (i, (a, da)) in golden.seeds.iter().enumerate() {
        for (b, db) in &golden.seeds[..i] {
            assert_ne!(da.execution, db.execution, "golden seeds {a:#x} and {b:#x}");
        }
    }
}

/// Nightly: step 1 only, for a random seed that has no golden digests.
#[test]
#[ignore = "nightly: needs M0_SEED"]
fn nightly_seed_reproduces_in_separate_processes() {
    let seed = seed_from_env();
    let runs = run_in_processes(Path::new(M0_RUN), seed, 2)
        .unwrap_or_else(|e| panic!("M0_SEED={seed:#x}: {e}"));
    ensure_reproduced(seed, &runs).unwrap_or_else(|e| panic!("M0_SEED={seed:#x}: {e}"));
}
