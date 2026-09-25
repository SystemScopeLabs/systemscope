//! M2.9: `block_irq.elf` reproduces the golden file `tests/golden/m2-reference.json`
//! (`docs/m2-design.md` §13.2, §15.1), in the M1 golden format.
//!
//! The program is rerun and compared with the golden file field by field, in this
//! process and in separate `m2-run` processes. Both CI operating systems run these tests
//! against the same committed file, and the cross-OS job also exchanges each machine's
//! own result (`cargo xtask m2-golden emit` and `check`).

use std::path::Path;
use std::process::{Command, Stdio};

use systemscope_acceptance::m2::golden::{Golden, describe_changes};
use systemscope_acceptance::m2::{Fixture, MEI_CAUSE, Record};
use systemscope_contracts::trace::Value;
use systemscope_rv32::block_irq::{self, EXPECTED_ENTRIES, EXPECTED_OUTPUT};
use systemscope_rv32::m2ref::{BLK, IRQC};
use systemscope_rv32::runner::{PASS_A0, PASS_CAUSE, PASS_GP, Start};
use systemscope_rv32::{hex, workspace_root};

const GOLDEN_JSON: &str = include_str!("../../golden/m2-reference.json");
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m2-reference.mid.snap");
const M2_RUN: &str = env!("CARGO_BIN_EXE_m2-run");

fn golden() -> Golden {
    let golden = Golden::parse(GOLDEN_JSON)
        .unwrap_or_else(|e| panic!("tests/golden/m2-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn block_irq_matches_the_golden() {
    let golden = golden();
    let (fresh, snapshot) = Golden::generate(&workspace_root()).unwrap_or_else(|e| panic!("{e}"));
    golden
        .check(&fresh.record)
        .unwrap_or_else(|e| panic!("{e}"));
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

/// The golden file holds the frozen M2.8 baseline, not only hashes of it.
#[test]
fn the_golden_is_the_frozen_baseline() {
    let record = golden().record;
    assert_eq!((record.cause.as_str(), record.tval), (PASS_CAUSE, 0));
    assert_eq!(
        (record.registers[3], record.registers[10]),
        (PASS_GP, PASS_A0),
        "gp, a0"
    );
    assert_eq!(record.uart, EXPECTED_OUTPUT);
    assert_eq!(record.interrupts, vec![MEI_CAUSE; 3]);
    assert_eq!(record.entries, EXPECTED_ENTRIES);
    let ops: Vec<(&str, u64)> = record
        .disk_ops
        .iter()
        .map(|(op, lba)| (op.as_str(), *lba))
        .collect();
    assert_eq!(ops, [("read", 0), ("write", 1), ("read", 1)]);
    assert_eq!(record.dma_beats, 3 * 32);
    assert_eq!((record.instret, record.events), (2401, 22000));
    let digest = |d: &[u8; 32]| {
        let h = hex(d);
        format!("{}…{}", &h[..8], &h[60..])
    };
    assert_eq!(digest(&record.state), "cd04fb9b…5dd3");
    assert_eq!(digest(&record.execution), "f9b6214d…ce0d");
    assert_eq!(digest(&record.trace), "4534ee4c…90c3");
    let blocks: Vec<(u64, [u8; 32])> = [(0, block_irq::pattern()), (1, block_irq::transformed())]
        .into_iter()
        .map(|(lba, b)| (lba, *blake3::hash(&b).as_bytes()))
        .collect();
    assert_eq!(
        record.disk_blocks, blocks,
        "exact media: LBA 0 kept, LBA 1 written"
    );

    // What the golden record does not hold: the controller ends with ERROR 0, never set
    // REJECTED, and the line is deasserted end to end.
    let run = fixture().run(Start::Init { traced: false }, Vec::new());
    block_irq::judge(&run).unwrap_or_else(|e| panic!("{e}"));
    assert!(!run.rejected, "STATUS.REJECTED was set");
    let views = &run.finished.finished.views;
    let blk = &views[BLK.0 as usize];
    assert_eq!(blk.get("error"), Some(&Value::U64(0)));
    assert_eq!(blk.get("rejected"), Some(&Value::Bool(false)));
    assert_eq!(blk.get("irq"), Some(&Value::Bool(false)));
    assert_eq!(views[IRQC.0 as usize].get("out"), Some(&Value::Bool(false)));
    let state = run.state.as_ref().unwrap();
    assert_eq!(state.entries, 3, "the handler's counter");
}

/// Two `m2-run` processes, started at once, print the committed golden file.
#[test]
fn the_run_reproduces_in_separate_processes() {
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(Path::new(M2_RUN))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("cannot start {M2_RUN}: {e}"))
        })
        .collect();
    let mut pids = Vec::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "m2-run failed: {}", out.status);
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

/// Fresh platforms reproduce the same record and snapshot, time after time.
#[test]
fn fresh_platforms_reproduce_every_field() {
    let golden = golden();
    let fixture = fixture();
    let first = Record::run(&fixture).unwrap();
    for _ in 0..2 {
        assert_eq!(Record::run(&fixture).unwrap(), first);
    }
    golden.check(&first).unwrap_or_else(|e| panic!("{e}"));
    let (a, snap_a) = Golden::generate(&workspace_root()).unwrap();
    let (b, snap_b) = Golden::generate(&workspace_root()).unwrap();
    assert!(
        a.render() == b.render() && snap_a == snap_b,
        "generation 1 and 2 differ"
    );
}

/// Nothing host-specific reaches the golden file: no path, carriage return, timestamp,
/// or uppercase hex, and it is its own canonical rendering.
#[test]
fn the_golden_file_is_host_independent() {
    assert_eq!(golden().render(), GOLDEN_JSON);
    assert!(!GOLDEN_JSON.contains('\r'));
    let root = workspace_root().display().to_string();
    for needle in [
        root.as_str(),
        ":\\",
        "/home/",
        "/Users/",
        "target",
        "tmp",
        "time",
        "date",
    ] {
        assert!(!GOLDEN_JSON.contains(needle), "{needle:?}");
    }
    let hex_only = GOLDEN_JSON
        .split('"')
        .filter(|s| s.len() == 64 || s.starts_with("0x"))
        .all(|s| !s.chars().any(|c| c.is_ascii_uppercase()));
    assert!(hex_only, "uppercase hex");
}

/// `cargo xtask m2-golden verify` and `check` notice drift in any field, and pass on the
/// committed files.
#[test]
fn golden_verification_catches_drift() {
    let root = workspace_root();
    assert_eq!(Golden::verify(&root, GOLDEN_JSON, MID_SNAPSHOT), Ok(()));

    let pass = hex(EXPECTED_OUTPUT);
    let fail = hex(block_irq::FAIL_OUTPUT);
    let rehash = |text: &str| {
        text.replacen(
            &hex(blake3::hash(EXPECTED_OUTPUT).as_bytes()),
            &hex(blake3::hash(block_irq::FAIL_OUTPUT).as_bytes()),
            1,
        )
    };
    let execution = &hex(&golden().record.execution);
    let mut flipped = execution.clone();
    flipped.replace_range(..1, if execution.starts_with('0') { "1" } else { "0" });
    let cases: [(&str, String); 8] = [
        ("UART bytes", GOLDEN_JSON.replacen(&pass, &fail, 1)),
        (
            "UART bytes, rehashed",
            rehash(&GOLDEN_JSON.replacen(&pass, &fail, 1)),
        ),
        (
            "event count",
            GOLDEN_JSON.replacen("\"events\": 22000", "\"events\": 22001", 1),
        ),
        (
            "digest nibble",
            GOLDEN_JSON.replacen(execution, &flipped, 1),
        ),
        (
            "interrupt count",
            GOLDEN_JSON.replacen("\"handler_entries\": 3", "\"handler_entries\": 2", 1),
        ),
        (
            "disk op",
            GOLDEN_JSON.replacen(
                "{ \"op\": \"write\", \"lba\": 1 }",
                "{ \"op\": \"write\", \"lba\": 2 }",
                1,
            ),
        ),
        (
            "mid checkpoint",
            GOLDEN_JSON.replacen("\"after_events\": 13462", "\"after_events\": 13463", 1),
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
