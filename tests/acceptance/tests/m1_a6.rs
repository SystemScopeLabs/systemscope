//! M1-A6: snapshot/restore on `m1-reference` (`docs/m1-design.md` §10.1).
//!
//! For `hello.elf` and the long `rv32ui` test `ld_st`: every named checkpoint is
//! snapshotted, encoded, and dropped, and a freshly elaborated platform restores the
//! bytes, resumes the trace prefix, and runs to the end. Then the committed portable
//! snapshot restores on its own, and every configuration mismatch and malformed snapshot
//! is rejected by the components' and the runtime's existing restore rules.

use systemscope_acceptance::layout::{Layout, get_u64, put_u64};
use systemscope_acceptance::m1::checkpoint::{
    Reference, checkpoint_and_resume, checkpoints, reference,
};
use systemscope_acceptance::m1::golden::{Golden, MID_BYTES, mid_after, snapshot_after};
use systemscope_acceptance::m1::{self, LONG, Program, REFERENCE, mem};
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::snapshot::{RestoreError, SessionField};
use systemscope_runtime::runtime::RuntimeError;
use systemscope_rv32::runner::{BUS, CPU, RAM, UART};
use systemscope_rv32::workspace_root;

/// `tests/golden/m1-reference.mid.snap`, byte for byte.
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m1-reference.mid.snap");

fn golden() -> Golden {
    let golden = Golden::parse(include_str!("../../golden/m1-reference.json"))
        .unwrap_or_else(|e| panic!("tests/golden/m1-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn program(name: &str) -> Program {
    m1::program(&workspace_root(), name).unwrap_or_else(|e| panic!("{e}"))
}

/// Every named checkpoint of `name`, resumed and compared with the uninterrupted run.
fn a6(name: &str) -> Reference {
    let program = program(name);
    let reference = reference(&program);
    let points = checkpoints(&reference).unwrap_or_else(|e| panic!("{name}: {e}"));
    let n = reference.finished.dispatched.len();
    for (what, k) in points {
        println!("{name}: {what}: after {k} of {n} events");
        checkpoint_and_resume(&program, &reference, k, |_| {})
            .unwrap_or_else(|e| panic!("{name}, {what} (after {k} events): {e}"));
    }
    reference
}

#[test]
fn hello_resumes_exactly_from_every_named_checkpoint() {
    let reference = a6(REFERENCE);
    let record = m1::Record::of(&program(REFERENCE), &reference.finished).unwrap();
    golden().check(&record).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn ld_st_resumes_exactly_from_every_named_checkpoint() {
    let reference = a6(LONG);
    let record = m1::Record::of(&program(LONG), &reference.finished).unwrap();
    golden().check(&record).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn the_named_checkpoints_are_where_they_say() {
    let reference = reference(&program(REFERENCE));
    let points = checkpoints(&reference).unwrap();
    let names: Vec<&str> = points.iter().map(|p| p.0).collect();
    assert_eq!(
        names[..9],
        [
            "before the first fetch",
            "during a fetch",
            "between Complete and Commit",
            "during a load",
            "during a store",
            "right after a UART byte",
            "just before the trap commits",
            "after the last event",
            "seeded random",
        ]
    );
    assert_eq!(names[8..], ["seeded random"; 8]);
    let events = &reference.finished.dispatched;
    let n = events.len();
    let state = |k: usize| reference.cpu_states[k - 1].as_str();
    let k = |i: usize| points[i].1;
    assert_eq!(k(0), 0);
    assert_eq!(state(k(1)), "fetch_wait");
    assert_eq!(state(k(2)), "commit_pending");
    assert_eq!(state(k(3)), "mem_wait");
    assert_eq!(state(k(4)), "mem_wait");
    assert_eq!((k(6), k(7)), (n - 1, n));
    let random: Vec<usize> = points[8..].iter().map(|p| p.1).collect();
    assert!(random.iter().all(|&r| r <= n));
    assert!(random.windows(2).all(|w| w[0] != w[1]));

    // The UART checkpoint is the portable snapshot's point: the UART has accepted its
    // tenth byte, the CPU waits for the store's response, and the only queued event is
    // that WriteResp on its way from the UART to the bus.
    let at = k(5);
    assert_eq!(Some(at), mid_after(events));
    assert_eq!(at as u64, golden().mid.after_events);
    assert_eq!(events[at - 1].target, UART);
    assert!(matches!(
        mem(&events[at - 1]),
        Some(MemMsg::WriteReq { .. })
    ));
    assert_eq!(m1::uart_writes(&events[..at]).len(), MID_BYTES);
    assert_eq!(state(at), "mem_wait");
    assert_eq!((events[at].source, events[at].target), (UART, BUS));
    assert!(matches!(mem(&events[at]), Some(MemMsg::WriteResp { .. })));
    assert_eq!(Layout::parse(MID_SNAPSHOT).unwrap().queue.len(), 1);

    // ld_st has no UART, so no UART checkpoint.
    let long = checkpoints(&reference_of(LONG)).unwrap();
    assert!(long.iter().all(|p| p.0 != "right after a UART byte"));
    assert_eq!(long.len(), 7 + 8, "seven named and eight seeded random");
}

fn reference_of(name: &str) -> Reference {
    reference(&program(name))
}

/// The resumed run starts from the bytes, not from anything the old runtime left: a
/// doctored register shows up in the final state.
#[test]
fn a_resumed_run_starts_from_the_snapshot_bytes() {
    let program = program(REFERENCE);
    let reference = reference(&program);
    let k = golden().mid.after_events as usize;
    let err = checkpoint_and_resume(&program, &reference, k, |bytes| {
        let layout = Layout::parse(bytes).unwrap();
        // The CPU's configuration (16 bytes), pc, then x1..x31: x31, which hello never
        // uses.
        let x31 = layout.components[CPU.0 as usize].state.start + 16 + 4 + 4 * 30;
        bytes[x31] ^= 1;
    })
    .unwrap_err();
    assert!(
        err.contains("StateDigest") && err.contains("registers"),
        "{err}"
    );
    assert!(!err.contains("ExecutionDigest"), "{err}");
}

/// A restore that prints a byte twice is caught, by the checkpoint comparison and by the
/// portability check. At the portable point, `hello` waits for its tenth byte's store
/// with `t1` (x6) still pointing at that byte; one byte back, it stores the byte again
/// after the snapshot and ends with the same registers but 21 bytes of output.
#[test]
fn a_duplicated_uart_byte_is_caught() {
    let program = program(REFERENCE);
    let golden = golden();
    let k = golden.mid.after_events as usize;
    let back_one = |bytes: &mut Vec<u8>| {
        let layout = Layout::parse(bytes).unwrap();
        let t1 = layout.components[CPU.0 as usize].state.start + 16 + 4 + 4 * 5;
        let value = u32::from_le_bytes(bytes[t1..t1 + 4].try_into().unwrap());
        bytes[t1..t1 + 4].copy_from_slice(&(value - 1).to_le_bytes());
    };

    let err = checkpoint_and_resume(&program, &reference(&program), k, back_one).unwrap_err();
    for field in [
        "event count",
        "ExecutionDigest",
        "replayed events",
        "UART output",
    ] {
        assert!(err.contains(field), "{field}: {err}");
    }
    assert!(!err.contains("registers"), "{err}");

    let mut bytes = MID_SNAPSHOT.to_vec();
    back_one(&mut bytes);
    let mut rehashed = golden.clone();
    rehashed.mid.blake3 = *blake3::hash(&bytes).as_bytes();
    let err = rehashed.check_portable(&program, &bytes).unwrap_err();
    for field in [
        "event count",
        "UART output",
        "UART writes after the snapshot",
    ] {
        assert!(err.contains(field), "{field}: {err}");
    }
    assert!(!err.contains("registers"), "{err}");
}

#[test]
fn portable_snapshot_resumes_to_the_golden() {
    golden()
        .check_portable(&program(REFERENCE), MID_SNAPSHOT)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// The committed snapshot is exactly what this machine takes at the same point, twice,
/// and holds nothing host-specific.
#[test]
fn the_portable_snapshot_is_canonical() {
    let program = program(REFERENCE);
    let k = golden().mid.after_events as usize;
    let a = snapshot_after(&program, k).unwrap();
    let b = snapshot_after(&program, k).unwrap();
    assert!(a == b && a == MID_SNAPSHOT, "the snapshot bytes differ");
    let root = workspace_root().display().to_string();
    let contains = |needle: &[u8]| MID_SNAPSHOT.windows(needle.len()).any(|w| w == needle);
    for needle in [root.as_bytes(), b":\\", b"/home/", b"/Users/", b"target"] {
        assert!(!contains(needle), "{:?}", String::from_utf8_lossy(needle));
    }
}

/// The portability check fails on changed bytes, and on a snapshot that restores but
/// ends elsewhere.
#[test]
fn portable_snapshot_mismatches_are_reported() {
    let golden = golden();
    let program = program(REFERENCE);
    let layout = Layout::parse(MID_SNAPSHOT).unwrap();
    let uart = layout.components[UART.0 as usize].state.clone();
    let printed = MID_SNAPSHOT[uart.clone()]
        .windows(MID_BYTES)
        .position(|w| w == b"Hello, Sys")
        .expect("the UART state holds its output")
        + uart.start;
    let mut bytes = MID_SNAPSHOT.to_vec();
    bytes[printed] = b'J';
    let err = golden.check_portable(&program, &bytes).unwrap_err();
    assert!(err.contains("bytes differ"), "{err}");

    let mut rehashed = golden.clone();
    rehashed.mid.blake3 = *blake3::hash(&bytes).as_bytes();
    let err = rehashed.check_portable(&program, &bytes).unwrap_err();
    assert!(
        err.contains("StateDigest") && err.contains("UART output"),
        "{err}"
    );

    let mut wrong = golden.clone();
    wrong.programs[0].execution[0] ^= 1;
    let err = wrong.check_portable(&program, MID_SNAPSHOT).unwrap_err();
    assert!(err.contains("ExecutionDigest"), "{err}");

    let mut short = golden.clone();
    short.mid.size -= 1;
    assert!(short.check_portable(&program, MID_SNAPSHOT).is_err());
    assert!(
        golden
            .check_portable(&self::program(LONG), MID_SNAPSHOT)
            .is_err()
    );
}

fn restore_error(program: &Program, seed: u64, bytes: &[u8]) -> RestoreError {
    let mut rt = program.platform_with_seed(seed);
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

fn start(l: &Layout, id: systemscope_contracts::component::ComponentId) -> usize {
    l.components[id.0 as usize].state.start
}

/// Every configuration the snapshot was taken with must match the platform it restores
/// into, as the existing component and session rules require.
#[test]
fn configuration_mismatches_are_rejected() {
    let hello = program(REFERENCE);
    let seed = golden().mid.seed;

    // Another ELF: same topology, so the RAM's image_hash check refuses it.
    let mut other = program("simple");
    other.uart = true;
    let err = restore_error(&other, seed, MID_SNAPSHOT);
    println!("another ELF: {err:?}");
    assert!(format!("{err:?}").contains("ram"), "{err:?}");
    // The platform without the UART is another topology.
    assert_eq!(
        restore_error(&program("simple"), seed, MID_SNAPSHOT),
        RestoreError::TopologyMismatch
    );
    // Another seed or compatibility id is another session.
    assert_eq!(
        restore_error(&hello, seed ^ 1, MID_SNAPSHOT),
        RestoreError::SessionMismatch(SessionField::Seed)
    );
    assert_eq!(
        restore_error(
            &hello,
            seed,
            &doctored(|b, l| b[l.contracts_version + 4] ^= 1)
        ),
        RestoreError::SessionMismatch(SessionField::ContractsVersion)
    );

    // Component configurations, each as its own restore reads it.
    let cases: [(&str, Vec<u8>, &str); 8] = [
        ("CPU clock", doctored(|b, l| b[start(l, CPU)] ^= 1), "cpu"),
        (
            "CPU entry",
            doctored(|b, l| b[start(l, CPU) + 4] ^= 4),
            "cpu",
        ),
        (
            "CPU max_instructions",
            doctored(|b, l| b[start(l, CPU) + 8] ^= 1),
            "cpu",
        ),
        (
            "RAM size",
            doctored(|b, l| {
                let at = start(l, RAM);
                let size = get_u64(b, at);
                put_u64(b, at, size * 2);
            }),
            "ram",
        ),
        (
            "RAM image_hash",
            doctored(|b, l| b[start(l, RAM) + 8] ^= 1),
            "ram",
        ),
        (
            "RAM latency",
            doctored(|b, l| b[start(l, RAM) + 8 + 32 + 1 + 4] ^= 1),
            "ram",
        ),
        (
            "UART latency",
            doctored(|b, l| b[start(l, UART) + 1 + 4] ^= 1),
            "uart",
        ),
        (
            "bus memory map",
            doctored(|b, l| {
                // The region count (u32), then "ram" (length u32 and 3 bytes), then its base.
                let base = start(l, BUS) + 4 + 4 + 3;
                b[base + 3] ^= 1;
            }),
            "bus",
        ),
    ];
    for (what, bytes, component) in cases {
        let err = restore_error(&hello, seed, &bytes);
        println!("{what}: {err:?}");
        let text = format!("{err:?}").to_lowercase();
        assert!(
            matches!(err, RestoreError::InvalidState(_)) && text.contains(component),
            "{what}: {err:?}"
        );
    }
}

#[test]
fn malformed_snapshots_are_rejected() {
    let hello = program(REFERENCE);
    let seed = golden().mid.seed;
    let snap = MID_SNAPSHOT;
    for len in 0..snap.len() {
        let err = restore_error(&hello, seed, &snap[..len]);
        assert!(
            matches!(err, RestoreError::Decode(_) | RestoreError::BadMagic),
            "truncated to {len}: {err:?}"
        );
    }
    let mut long = snap.to_vec();
    long.push(0);
    assert!(matches!(
        restore_error(&hello, seed, &long),
        RestoreError::Decode(_)
    ));
    assert_eq!(
        restore_error(&hello, seed, &doctored(|b, _| b[0] ^= 1)),
        RestoreError::BadMagic
    );
    // The CPU's pending state at the portable point is MemWait: tag 3, the txn (u64),
    // then the store's instruction word (u32), which ends the CPU's state. Each doctored
    // field describes a state the CPU can never be in.
    let cpu = |l: &Layout| l.components[CPU.0 as usize].state.end;
    let cases: [(&str, Vec<u8>, &str); 3] = [
        (
            "unknown state tag",
            doctored(|b, l| b[cpu(l) - 13] = 9),
            "InvalidTag",
        ),
        (
            "stale txn",
            doctored(|b, l| b[cpu(l) - 12] ^= 1),
            "not the latest issued",
        ),
        (
            "pending instruction that is no load or store",
            doctored(|b, l| b[cpu(l) - 4] = 0x13),
            "not an aligned load or store",
        ),
    ];
    for (what, bytes, reason) in cases {
        let err = restore_error(&hello, seed, &bytes);
        println!("{what}: {err:?}");
        assert!(format!("{err:?}").contains(reason), "{what}: {err:?}");
    }
}
