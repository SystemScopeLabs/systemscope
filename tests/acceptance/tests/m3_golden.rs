//! M3.7: both M3 disks reproduce the golden file `tests/golden/m3-reference.json`
//! (`docs/m3-design.md` §9.2, §17.4), in the M1 and M2 golden format.
//!
//! The scenarios are rerun and compared with the golden file field by field, in this
//! process and in separate `m3-run` processes. Both CI operating systems run these tests
//! against the same committed file, and the cross-OS job also exchanges each machine's
//! own result (`cargo xtask m3-golden emit` and `check`).

use std::path::Path;
use std::process::{Command, Stdio};

use systemscope_acceptance::m3::golden::{
    GOLDEN_PATH, Golden, MID_CHECKPOINT, Record, describe_changes,
};
use systemscope_acceptance::m3::portable::SCHEMAS;
use systemscope_acceptance::m3::{Fixture, checkpoint};
use systemscope_rv32::m3::{HALT_CAUSE, Scenario};
use systemscope_rv32::{hex, workspace_root};

const GOLDEN_JSON: &str = include_str!("../../golden/m3-reference.json");
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m3-reference.mid.snap");
const M3_RUN: &str = env!("CARGO_BIN_EXE_m3-run");

fn golden() -> Golden {
    let golden = Golden::parse(GOLDEN_JSON)
        .unwrap_or_else(|e| panic!("tests/golden/m3-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn fixture(scenario: Scenario) -> Fixture {
    Fixture::read(&workspace_root(), scenario).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn both_scenarios_match_the_golden() {
    let golden = golden();
    let (fresh, snapshot) = Golden::generate(&workspace_root()).unwrap_or_else(|e| panic!("{e}"));
    for record in &fresh.records {
        golden.check(record).unwrap_or_else(|e| panic!("{e}"));
    }
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

/// The golden file holds the frozen M3.6 results, not only hashes of them: the exact
/// output, the shutdown reason, and every metric M3.6 reported.
#[test]
fn the_golden_is_the_frozen_baseline() {
    let golden = golden();
    let digest = |d: &[u8; 32]| {
        let h = hex(d);
        format!("{}…{}", &h[..8], &h[60..])
    };
    let expected: [(Scenario, u32, [u64; 8], [&str; 3]); 2] = [
        (
            Scenario::Reference,
            1,
            [166_897, 2176, 75, 22, 11, 23, 6, 53],
            ["a419f1f0…103a", "0f200e0a…ba18", "b9739324…c148"],
        ),
        (
            Scenario::NoFault,
            0,
            [141_128, 1999, 68, 21, 10, 21, 5, 47],
            ["1066404e…9b9e", "ea7f23b2…dd37", "ff07c683…abe8"],
        ),
    ];
    for (r, (scenario, reason, metrics, digests)) in golden.records.iter().zip(expected) {
        assert_eq!(r.scenario, scenario.name());
        assert_eq!((r.halt.as_str(), r.reason), (HALT_CAUSE, reason));
        assert_eq!(r.uart, scenario.expected_output());
        assert_eq!(
            [
                r.events,
                r.instret,
                r.os_records,
                r.syscalls,
                r.switches,
                r.exceptions,
                r.dma_commands,
                r.uart_bytes
            ],
            metrics,
            "{}: events, instret, os-records, syscalls, switches, exceptions, dma-commands, \
             uart-bytes",
            r.scenario
        );
        assert_eq!(
            [digest(&r.execution), digest(&r.state), digest(&r.trace)],
            digests.map(str::to_owned),
            "{}: ExecutionDigest, StateDigest, TraceDigest",
            r.scenario
        );
        assert_eq!(
            r.final_blake3, r.state,
            "StateDigest is the final snapshot's"
        );
    }
    let mid = &golden.mid;
    assert_eq!(mid.checkpoint, MID_CHECKPOINT);
    assert_eq!(mid.schemas, SCHEMAS);
    assert_eq!(
        (mid.size, mid.blake3),
        (
            MID_SNAPSHOT.len() as u64,
            *blake3::hash(MID_SNAPSHOT).as_bytes()
        )
    );
}

/// The portable snapshot is where the golden file says: the first boundary of the chained
/// every-event classification at stress point 3 in a syscall with bytes at the UART.
#[test]
fn the_portable_snapshot_is_at_its_checkpoint() {
    let golden = golden();
    let fixture = fixture(Scenario::Reference);
    let coverage = checkpoint::survey(&fixture).unwrap_or_else(|e| panic!("{e}"));
    let k = coverage.mid_checkpoint().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(k as u64, golden.mid.after_events);
    let snapshot = checkpoint::snapshot_after(&fixture, k).unwrap();
    assert!(
        snapshot == MID_SNAPSHOT,
        "a replay to the checkpoint differs"
    );
    golden
        .check_portable(&fixture, MID_SNAPSHOT)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// Two `m3-run` processes, started at once, print the committed golden file.
#[test]
fn the_run_reproduces_in_separate_processes() {
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(Path::new(M3_RUN))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("cannot start {M3_RUN}: {e}"))
        })
        .collect();
    let mut pids = Vec::new();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "m3-run failed: {}", out.status);
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

/// Fresh platforms reproduce the same records and snapshot, run after run.
#[test]
fn fresh_platforms_reproduce_every_field() {
    let golden = golden();
    for scenario in Scenario::ALL {
        let fixture = fixture(scenario);
        let record = || {
            let run = fixture
                .judged_run(Vec::new())
                .unwrap_or_else(|e| panic!("{e}"));
            Record::of(&fixture, &run).unwrap_or_else(|e| panic!("{e}"))
        };
        let first = record();
        for _ in 0..2 {
            assert_eq!(record(), first);
        }
        golden.check(&first).unwrap_or_else(|e| panic!("{e}"));
    }
    let (a, snap_a) = Golden::generate(&workspace_root()).unwrap();
    let (b, snap_b) = Golden::generate(&workspace_root()).unwrap();
    assert!(
        a.render() == b.render() && snap_a == snap_b,
        "generation 1 and 2 differ"
    );
}

/// Nothing host-specific reaches the golden files: no path, user, carriage return,
/// timestamp, or uppercase hex, and the JSON is its own canonical rendering. The snapshot
/// holds no host path either.
#[test]
fn the_golden_files_are_host_independent() {
    assert_eq!(golden().render(), GOLDEN_JSON);
    assert!(!GOLDEN_JSON.contains('\r'));
    let root = workspace_root().display().to_string();
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let mut needles = vec![
        root.as_str(),
        ":\\",
        "/home/",
        "/Users/",
        "target",
        "tmp",
        "time",
        "date",
    ];
    if user.len() > 2 {
        needles.push(user.as_str());
    }
    for needle in needles {
        assert!(
            !GOLDEN_JSON.contains(needle),
            "{needle:?} in the golden file"
        );
        let found = MID_SNAPSHOT
            .windows(needle.len())
            .any(|w| w == needle.as_bytes());
        assert!(
            !found || needle.len() < 5,
            "{needle:?} in the portable snapshot"
        );
    }
    let hex_only = GOLDEN_JSON
        .split('"')
        .filter(|s| s.len() == 64 || s.starts_with("0x"))
        .all(|s| !s.chars().any(|c| c.is_ascii_uppercase()));
    assert!(hex_only, "uppercase hex");
}

/// `cargo xtask m3-golden verify` and `check` notice drift in any field, and pass on the
/// committed files.
#[test]
fn golden_verification_catches_drift() {
    let root = workspace_root();
    assert_eq!(Golden::verify(&root, GOLDEN_JSON, MID_SNAPSHOT), Ok(()));
    let golden = golden();
    let r = &golden.records[0];
    let flip = |h: String| {
        let mut f = h.clone();
        f.replace_range(..1, if h.starts_with('0') { "1" } else { "0" });
        (h, f)
    };
    let uart = hex(&r.uart);
    // The last byte, '\n' -> '!', so a comparison of a prefix cannot pass.
    let mut other_uart = uart.clone();
    other_uart.replace_range(uart.len() - 2.., "21");
    // The doctored output with its own BLAKE3, so only the comparison can catch it.
    let rehash = |text: &str| {
        let mut doctored = r.uart.clone();
        *doctored.last_mut().unwrap() = b'!';
        text.replacen(
            &hex(blake3::hash(&r.uart).as_bytes()),
            &hex(blake3::hash(&doctored).as_bytes()),
            1,
        )
    };
    let digest_case = |d: &[u8; 32]| {
        let (a, b) = flip(hex(d));
        GOLDEN_JSON.replacen(&a, &b, 1)
    };
    // Each case names what the errors must say, so every layer of the comparison is
    // shown to catch its own kind of drift, not only the first that happens to fire.
    let renders = "renders the golden file differently";
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "UART bytes",
            GOLDEN_JSON.replacen(&uart, &other_uart, 1),
            "uart_blake3 is not the BLAKE3 of uart",
        ),
        (
            "UART bytes, rehashed",
            rehash(&GOLDEN_JSON.replacen(&uart, &other_uart, 1)),
            "reference: uart ",
        ),
        (
            "shutdown reason",
            GOLDEN_JSON.replacen("\"shutdown_reason\": 1", "\"shutdown_reason\": 0", 1),
            "reference: shutdown_reason ",
        ),
        (
            "event count",
            GOLDEN_JSON.replacen("\"events\": 166897", "\"events\": 166896", 1),
            "reference: events ",
        ),
        (
            "syscalls",
            GOLDEN_JSON.replacen("\"syscalls\": 22", "\"syscalls\": 21", 1),
            "reference: syscalls ",
        ),
        (
            "ExecutionDigest",
            digest_case(&r.execution),
            "reference: ExecutionDigest ",
        ),
        (
            "TraceDigest",
            digest_case(&r.trace),
            "reference: TraceDigest ",
        ),
        (
            "StateDigest",
            digest_case(&r.state),
            "the final snapshot's BLAKE3 is not its StateDigest",
        ),
        (
            "final snapshot",
            digest_case(&r.final_blake3).replacen(&hex(&r.state), &flip(hex(&r.state)).1, 1),
            "reference: final_snapshot_blake3 ",
        ),
        (
            "mid checkpoint",
            GOLDEN_JSON.replacen(
                &format!("\"after_events\": {}", golden.mid.after_events),
                &format!("\"after_events\": {}", golden.mid.after_events + 1),
                1,
            ),
            "~ mid snapshot",
        ),
        (
            "mid checkpoint id",
            GOLDEN_JSON.replacen("some bytes at the UART", "no bytes at the UART", 1),
            "the portable snapshot is \"first boundary",
        ),
        (
            "mid hash",
            digest_case(&golden.mid.blake3),
            "the portable snapshot's bytes differ from the golden",
        ),
        (
            "schemas",
            GOLDEN_JSON.replacen("[3, 1, 1, 1, 1, 1, 1, 1]", "[2, 1, 1, 1, 1, 1, 1, 1]", 1),
            "the portable snapshot's schemas are",
        ),
        (
            "formatting",
            GOLDEN_JSON.replacen("\n  ", "\n   ", 1),
            renders,
        ),
        ("CRLF line ends", GOLDEN_JSON.replace('\n', "\r\n"), renders),
        ("trailing byte", format!("{GOLDEN_JSON} "), renders),
        (
            "prefix only",
            GOLDEN_JSON[..GOLDEN_JSON.find("\"runs\"").unwrap()].to_owned() + "}\n",
            GOLDEN_PATH,
        ),
    ];
    for (i, (what, text, needle)) in cases.into_iter().enumerate() {
        assert_ne!(text, GOLDEN_JSON, "{what}: the doctoring changed nothing");
        let errors = Golden::verify(&root, &text, MID_SNAPSHOT).unwrap_err();
        println!("{what}: {errors:?}");
        assert!(
            errors.iter().any(|e| e.contains(needle)),
            "{what}: no error says {needle:?}: {errors:?}"
        );
        // `check` shares `verify`'s comparison; every third case keeps the test short.
        if i % 3 == 0 {
            let foreign =
                Golden::check_foreign(&root, (GOLDEN_JSON, MID_SNAPSHOT), (&text, MID_SNAPSHOT));
            assert!(foreign.is_err(), "{what}: check accepted it");
        }
    }
    let bytes_differ = |errors: Vec<String>| {
        assert!(
            errors
                .iter()
                .any(|e| e.contains("different portable snapshot bytes")),
            "{errors:?}"
        );
    };
    let mut flipped = MID_SNAPSHOT.to_vec();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    let mut longer = MID_SNAPSHOT.to_vec();
    longer.push(0);
    let shorter = &MID_SNAPSHOT[..MID_SNAPSHOT.len() - 1];
    let fixture = Fixture::read(&root, Scenario::Reference).unwrap();
    for snapshot in [&flipped[..], &longer[..], shorter] {
        let refused = golden.check_portable(&fixture, snapshot).unwrap_err();
        assert!(
            refused.contains("the portable snapshot's bytes differ from the golden"),
            "{refused}"
        );
        bytes_differ(Golden::verify(&root, GOLDEN_JSON, snapshot).unwrap_err());
        bytes_differ(
            Golden::check_foreign(&root, (GOLDEN_JSON, MID_SNAPSHOT), (GOLDEN_JSON, snapshot))
                .unwrap_err(),
        );
    }
    assert_eq!(
        Golden::check_foreign(
            &root,
            (GOLDEN_JSON, MID_SNAPSHOT),
            (GOLDEN_JSON, MID_SNAPSHOT)
        ),
        Ok(())
    );
}
