//! M1-A8: the committed programs reproduce the golden file `tests/golden/m1-reference.json`
//! (`docs/m1-design.md` §10.1).
//!
//! Every committed program is rerun and compared with the golden file field by field, in
//! this process and in separate `m1-run` processes. Both CI operating systems run these
//! tests against the same committed file, and the cross-OS job also exchanges each
//! machine's own result (`cargo xtask m1-golden emit` and `check`).

use std::path::Path;
use std::process::{Command, Stdio};

use systemscope_acceptance::FIXED_SEEDS;
use systemscope_acceptance::digests::End;
use systemscope_acceptance::layout::{Layout, put_u64};
use systemscope_acceptance::m1::golden::{Golden, describe_changes};
use systemscope_acceptance::m1::{self, LONG, Program, REFERENCE, Record};
use systemscope_rv32::hello::EXPECTED_OUTPUT;
use systemscope_rv32::workspace_root;

const GOLDEN_JSON: &str = include_str!("../../golden/m1-reference.json");
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m1-reference.mid.snap");
const M1_RUN: &str = env!("CARGO_BIN_EXE_m1-run");

fn golden() -> Golden {
    let golden = Golden::parse(GOLDEN_JSON)
        .unwrap_or_else(|e| panic!("tests/golden/m1-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn program(name: &str) -> Program {
    m1::program(&workspace_root(), name).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_committed_program_matches_the_golden() {
    let golden = golden();
    let (fresh, snapshot) = Golden::generate(&workspace_root()).unwrap_or_else(|e| panic!("{e}"));
    for record in &fresh.programs {
        golden.check(record).unwrap_or_else(|e| panic!("{e}"));
    }
    assert_eq!(fresh.programs.len(), 41, "hello and the 40 rv32ui tests");
    assert_eq!(
        describe_changes(Some(&golden), &fresh),
        Vec::<String>::new()
    );
    assert!(
        fresh.render() == GOLDEN_JSON,
        "the golden file renders differently"
    );
    assert!(snapshot == MID_SNAPSHOT, "the portable snapshot differs");
}

/// The golden file keeps hello's exact output, not only its hash.
#[test]
fn hello_prints_exactly_and_the_golden_says_so() {
    let golden = golden();
    let record = golden.record(REFERENCE).unwrap();
    assert_eq!(record.uart.as_deref(), Some(&EXPECTED_OUTPUT[..]));
    assert_eq!((record.cause.as_str(), record.tval), ("EnvironmentCall", 0));
    assert_eq!(
        (record.registers[3], record.registers[10]),
        (1, 0),
        "gp, a0"
    );
    assert_eq!(record.instret, 5 + 4 * 20 + 2);
    let run = Record::run(&program(REFERENCE)).unwrap();
    assert_eq!(run.uart.as_deref(), Some(&EXPECTED_OUTPUT[..]));
    assert!(golden.programs[1..].iter().all(|r| r.uart.is_none()));
}

/// Two `m1-run` processes, started at once, print the committed golden file.
#[test]
fn the_runs_reproduce_in_separate_processes() {
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(Path::new(M1_RUN))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("cannot start {M1_RUN}: {e}"))
        })
        .collect();
    let mut pids = Vec::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "m1-run failed: {}", out.status);
        let text = String::from_utf8(out.stdout).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        let pid: u32 = first
            .strip_prefix("{\"pid\":")
            .and_then(|p| p.strip_suffix('}'))
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {first:?}"));
        assert!(rest == GOLDEN_JSON, "process {pid} printed another result");
        pids.push(pid);
    }
    assert_ne!(pids[0], pids[1]);
    assert!(!pids.contains(&std::process::id()));
}

/// Fresh platforms reproduce the same record, time after time.
#[test]
fn fresh_platforms_reproduce_every_field() {
    let golden = golden();
    for name in [REFERENCE, LONG] {
        let program = program(name);
        let first = Record::run(&program).unwrap();
        for _ in 0..2 {
            assert_eq!(Record::run(&program).unwrap(), first, "{name}");
        }
        golden.check(&first).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// `m1-reference` draws no randomness (§9): with M0's other fixed seeds, the events,
/// `ExecutionDigest`, trace records, and every component's final state are seed 0's.
/// Only the seed recorded in the session information and the RNG streams derived from
/// it, which nothing draws from, differ, so `StateDigest` and `TraceDigest` differ with
/// them and nothing else.
#[test]
fn the_seed_reaches_only_the_session_information() {
    for name in [REFERENCE, LONG] {
        let program = program(name);
        let run = |seed: u64| {
            let mut rt = program.platform_with_seed(seed);
            rt.start_trace().unwrap();
            rt.init().unwrap();
            let mut events = 0;
            while rt.step().unwrap().is_some() {
                events += 1;
            }
            End::finish(&mut rt, events)
        };
        let base = run(FIXED_SEEDS[0]);
        for seed in &FIXED_SEEDS[1..] {
            let other = run(*seed);
            assert_eq!(other.events, base.events, "{name} {seed:#x}");
            assert_eq!(other.execution, base.execution, "{name} {seed:#x}");
            let (a, b) = (base.trace.as_ref().unwrap(), other.trace.as_ref().unwrap());
            assert_eq!(a.records, b.records, "{name} {seed:#x}");
            assert_ne!(a.header.seed, b.header.seed);
            // Every component's RNG stream is derived from the seed and never drawn from.
            let layout = Layout::parse(&other.snapshot).unwrap();
            assert_eq!(
                layout.rng_states,
                undrawn(&program, *seed),
                "{name} {seed:#x}"
            );
            // With seed 0's session and streams, the snapshot is seed 0's byte for byte.
            let base_layout = Layout::parse(&base.snapshot).unwrap();
            let mut reseeded = other.snapshot.clone();
            put_u64(&mut reseeded, layout.seed, FIXED_SEEDS[0]);
            for (&at, &from) in layout.rng.iter().zip(&base_layout.rng) {
                reseeded[at..at + 32].copy_from_slice(&base.snapshot[from..from + 32]);
            }
            assert!(
                reseeded == base.snapshot,
                "{name} {seed:#x}: state beyond the seed"
            );
        }
    }
}

/// The RNG states of a platform with `seed` that has not run.
fn undrawn(program: &Program, seed: u64) -> Vec<[u64; 4]> {
    let mut rt = program.platform_with_seed(seed);
    rt.init().unwrap();
    Layout::parse(&rt.snapshot().unwrap()).unwrap().rng_states
}

/// Nothing host-specific reaches the golden file: no path, carriage return, or
/// uppercase hex, and it is its own canonical rendering.
#[test]
fn the_golden_file_is_host_independent() {
    assert_eq!(golden().render(), GOLDEN_JSON);
    assert!(!GOLDEN_JSON.contains('\r'));
    let root = workspace_root().display().to_string();
    for needle in [root.as_str(), ":\\", "/home/", "/Users/", "target", "tmp"] {
        assert!(!GOLDEN_JSON.contains(needle), "{needle:?}");
    }
}

/// `cargo xtask m1-golden verify` and `check` notice drift in any field, and pass on the
/// committed files.
#[test]
fn golden_verification_catches_drift() {
    let root = workspace_root();
    assert_eq!(Golden::verify(&root, GOLDEN_JSON, MID_SNAPSHOT), Ok(()));

    let hello_uart = "48656c6c6f2c2053797374656d53636f7065210a";
    let jello_uart = "4a656c6c6f2c2053797374656d53636f7065210a";
    let rehash = |text: &str| {
        text.replacen(
            &systemscope_rv32::hex(blake3::hash(EXPECTED_OUTPUT).as_bytes()),
            &systemscope_rv32::hex(blake3::hash(b"Jello, SystemScope!\n").as_bytes()),
            1,
        )
    };
    let execution = &systemscope_rv32::hex(&golden().programs[0].execution);
    let mut flipped = execution.clone();
    flipped.replace_range(..1, if execution.starts_with('0') { "1" } else { "0" });
    let cases: [(&str, String); 6] = [
        ("UART byte", GOLDEN_JSON.replacen(hello_uart, jello_uart, 1)),
        (
            "UART byte, rehashed",
            rehash(&GOLDEN_JSON.replacen(hello_uart, jello_uart, 1)),
        ),
        (
            "event count",
            GOLDEN_JSON.replacen("\"events\": 728", "\"events\": 729", 1),
        ),
        (
            "digest nibble",
            GOLDEN_JSON.replacen(execution, &flipped, 1),
        ),
        (
            "image hash",
            GOLDEN_JSON.replacen("\"image_hash\": \"dc71", "\"image_hash\": \"ec71", 1),
        ),
        ("formatting", GOLDEN_JSON.replacen("\n  ", "\n   ", 1)),
    ];
    for (what, text) in cases {
        assert_ne!(text, GOLDEN_JSON, "{what}: the doctoring changed nothing");
        let errors = Golden::verify(&root, &text, MID_SNAPSHOT).unwrap_err();
        println!("{what}: {errors:?}");
        let foreign =
            Golden::check_foreign(&root, (GOLDEN_JSON, MID_SNAPSHOT), (&text, MID_SNAPSHOT));
        assert!(foreign.is_err(), "{what}: check accepted it");
    }
    let mut snapshot = MID_SNAPSHOT.to_vec();
    let last = snapshot.len() - 1;
    snapshot[last] ^= 1;
    let bytes_differ = |errors: Vec<String>| {
        assert!(
            errors
                .iter()
                .any(|e| e.contains("different portable snapshot bytes")),
            "{errors:?}"
        );
    };
    bytes_differ(Golden::verify(&root, GOLDEN_JSON, &snapshot).unwrap_err());
    bytes_differ(
        Golden::check_foreign(&root, (GOLDEN_JSON, MID_SNAPSHOT), (GOLDEN_JSON, &snapshot))
            .unwrap_err(),
    );
    assert_eq!(
        Golden::check_foreign(
            &root,
            (GOLDEN_JSON, MID_SNAPSHOT),
            (GOLDEN_JSON, MID_SNAPSHOT)
        ),
        Ok(())
    );
}
