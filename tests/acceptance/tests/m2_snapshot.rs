//! M2.9 snapshot/restore on `m2-reference` (`docs/m2-design.md` §13.1, §13.2).
//!
//! Every checkpoint of `block_irq.elf`, from before the first event to after the last,
//! is snapshotted, restored into a freshly built platform, re-encoded, and run to the end
//! under the watchdog, and compared with the uninterrupted run in full. The §13.1 stress
//! points are located and named. Then the committed portable mid-DMA snapshot restores
//! on its own, the watchdog is shown to stay outside the architectural state, and every
//! configuration mismatch and malformed snapshot is rejected by the existing restore
//! rules, which fault the session.

use std::collections::BTreeSet;

use systemscope_acceptance::layout::{Layout, get_u64, put_u32, put_u64};
use systemscope_acceptance::m2::checkpoint::{
    Owner, Reference, Stress, check, every_event, owners, reference, resume_exact,
};
use systemscope_acceptance::m2::golden::{
    Golden, MID_BEATS, MidRest, mid_after, rest_of_work, snapshot_after,
};
use systemscope_acceptance::m2::{Fixture, MEI_CAUSE, pending};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::snapshot::{RestoreError, SessionField};
use systemscope_platform::dma;
use systemscope_runtime::runtime::{Lifecycle, RuntimeError};
use systemscope_rv32::block_irq::{self, HANDLER};
use systemscope_rv32::m2ref::{
    self, BLK, BUS, CPU, Config, DISK, EVENT_BUDGET, IRQC, MASTER_CPU, MASTER_DMA, RAM, UART,
};
use systemscope_rv32::runner::{self, End, Start};
use systemscope_rv32::workspace_root;

/// `tests/golden/m2-reference.mid.snap`, byte for byte.
const MID_SNAPSHOT: &[u8] = include_bytes!("../../golden/m2-reference.mid.snap");
const GOLDEN_JSON: &str = include_str!("../../golden/m2-reference.json");

fn golden() -> Golden {
    let golden = Golden::parse(GOLDEN_JSON)
        .unwrap_or_else(|e| panic!("tests/golden/m2-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}

fn fixture() -> Fixture {
    Fixture::read(&workspace_root()).unwrap_or_else(|e| panic!("{e}"))
}

fn reference_of(fixture: &Fixture) -> Reference {
    reference(fixture).unwrap_or_else(|e| panic!("{e}"))
}

// ---------------------------------------------------------------------------------------
// Every event.

/// The M2.9 closure: every checkpoint, no sampling. Each one's classes and stress points
/// are counted, and every class and stress point must occur.
#[test]
fn every_event_resumes_to_the_same_end() {
    let fixture = fixture();
    let reference = reference_of(&fixture);
    let n = reference.events();
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get());
    let coverage = every_event(&fixture, &reference, threads);
    println!(
        "{} checkpoints (0..={n}) on {threads} threads",
        coverage.checked
    );
    for (k, why) in coverage.failures.iter().take(20) {
        println!("checkpoint {k}: {why}");
    }
    assert!(
        coverage.failures.is_empty(),
        "{} of {} checkpoints failed",
        coverage.failures.len(),
        coverage.checked
    );
    assert_eq!(coverage.checked, n + 1, "every checkpoint, 0 through {n}");
    for owner in Owner::ALL {
        let count = coverage.owners.get(&owner).copied().unwrap_or(0);
        println!("{owner:?}: {count} checkpoints");
        assert!(count > 0, "no checkpoint holds {owner:?}");
    }
    for stress in Stress::ALL {
        let (count, first) = coverage.stress.get(&stress).copied().unwrap_or((0, 0));
        println!(
            "{}: {count} checkpoints, first after {first} events",
            stress.name()
        );
        assert!(count > 0, "no checkpoint is {}", stress.name());
    }
    println!(
        "both masters queued at once: {} checkpoints; at most {} events queued",
        coverage.both_queued, coverage.max_queued
    );
}

/// Every checkpoint's classes and stress points, from one producer run.
fn classify(
    fixture: &Fixture,
    reference: &Reference,
) -> Vec<(usize, BTreeSet<Owner>, BTreeSet<Stress>)> {
    let mut rt = fixture.platform();
    rt.init().unwrap();
    let mut out = Vec::new();
    for k in 0..=reference.events() {
        let queued = pending(&rt.snapshot().unwrap()).unwrap();
        let o = owners(reference, k, &queued);
        let s = Stress::ALL
            .into_iter()
            .filter(|s| s.holds(reference, k, &o))
            .collect();
        out.push((k, o, s));
        if k < reference.events() {
            rt.step().unwrap();
        }
    }
    out
}

/// The §13.1 stress points, each at its first checkpoint, named, checked for what makes
/// it that point, and resumed.
#[test]
fn the_stress_points_are_where_they_say() {
    let fixture = fixture();
    let reference = reference_of(&fixture);
    let all = classify(&fixture, &reference);
    for stress in Stress::ALL {
        let (k, owners, _) = all
            .iter()
            .find(|(_, _, s)| s.contains(&stress))
            .unwrap_or_else(|| panic!("never: {}", stress.name()));
        let k = *k;
        let b = reference.boundary(k).unwrap();
        println!("{}: after {k} events: {b:?}", stress.name());
        match stress {
            Stress::DmaBeatPending => {
                assert_eq!(b.engine, "wait_beat");
                assert!(
                    owners.contains(&Owner::DmaRamRequest) || owners.contains(&Owner::RamRequest)
                );
            }
            Stress::CpuQueuedBehindDma => {
                assert_eq!(b.ram.0, Some(MASTER_DMA));
                assert!(b.ram.2[MASTER_CPU as usize] > 0);
            }
            Stress::DmaQueuedBehindCpu => {
                assert_eq!(b.ram.0, Some(MASTER_CPU));
                assert!(b.ram.2[MASTER_DMA as usize] > 0);
            }
            Stress::MediaResultPending => assert_eq!(b.engine, "wait_media"),
            Stress::IrqAssertedNotTaken => {
                assert!(b.irqc_out && b.blk_irq, "the line is high end to end");
                assert!(!b.in_handler());
                // The next interrupt is taken later, at a retirement boundary.
                let next = reference
                    .interrupt_events
                    .range(k..)
                    .next()
                    .copied()
                    .unwrap();
                assert!(next >= k, "taken after the checkpoint");
            }
            Stress::HandlerEntry => {
                assert_eq!(
                    (b.pc, b.cpu_state.as_str()),
                    (u64::from(HANDLER), "fetch_issue")
                );
                assert_eq!(b.mcause, MEI_CAUSE);
                assert!(owners.contains(&Owner::MeiEntry));
            }
            Stress::IrqClearedBeforeMret => {
                assert!(b.in_handler() && !b.blk_irq && !b.irqc_out);
                let mret = reference.mret_events.range(k..).next().copied().unwrap();
                assert!(mret >= k, "MRET retires after the checkpoint");
            }
            Stress::WritePartiallyBuffered => {
                assert!(b.command.starts_with("write lba=0x1"));
                assert!(b.beat > 0 && b.beat < u64::from(dma::BEATS_PER_BLOCK));
            }
        }
        let snapshot = snapshot_after(&fixture, k).unwrap();
        check(&fixture, &reference, k, snapshot)
            .unwrap_or_else(|e| panic!("{}: after {k} events: {e}", stress.name()));
    }
}

/// The IRQ-critical checkpoints of each completion: the level on its way to the IRQ
/// controller and to the CPU, the interrupt just taken, the ACK store and the falling
/// level on their way, and MRET about to retire. Each continuation takes exactly the
/// remaining interrupts, so the whole run takes three.
#[test]
fn irq_critical_checkpoints_take_exactly_three_interrupts() {
    let fixture = fixture();
    let reference = reference_of(&fixture);
    let critical = [
        Owner::LevelHighAtSource,
        Owner::LevelHighToCpu,
        Owner::MeiEntry,
        Owner::AckStore,
        Owner::LevelLowAtSource,
        Owner::LevelLowToCpu,
        Owner::MretNext,
    ];
    let mut seen = 0;
    for (k, owners, _) in classify(&fixture, &reference) {
        if !critical.iter().any(|c| owners.contains(c)) {
            continue;
        }
        seen += 1;
        let taken = reference.interrupt_events.range(..k).count();
        let resumed = fixture.run(
            Start::Restore {
                snapshot: snapshot_after(&fixture, k).unwrap(),
                prefix: None,
            },
            Vec::new(),
        );
        block_irq::judge(&resumed).unwrap_or_else(|e| panic!("after {k} events: {e}"));
        let interrupts = resumed
            .finished
            .finished
            .dispatched
            .iter()
            .filter(|ev| {
                ev.source == IRQC
                    && ev.target == CPU
                    && matches!(
                        ev.delivery,
                        systemscope_contracts::component::Delivered::Message {
                            msg: systemscope_contracts::protocol::Message::Irq(
                                systemscope_contracts::protocol::irq_v0::IrqMsg::Level {
                                    asserted: true
                                }
                            ),
                            ..
                        }
                    )
            })
            .count();
        let state = resumed.state.as_ref().unwrap();
        assert_eq!(
            state.entries,
            block_irq::EXPECTED_ENTRIES,
            "after {k} events"
        );
        println!(
            "{owners:?} after {k} events: {taken} taken before, {interrupts} rising levels after"
        );
        assert_eq!(taken + interrupts, 3, "after {k} events");
    }
    assert!(
        seen >= 3 * critical.len() - 4,
        "{seen} IRQ-critical checkpoints"
    );
}

// ---------------------------------------------------------------------------------------
// The watchdog.

/// The event watchdog bounds a run; it is not architectural. The same run with no
/// budget, the M2 budget, and a budget of exactly its event count ends in the same
/// state, digests, and trace; a run stopped by the budget leaves the same snapshot as
/// stepping that far, and that snapshot resumes to the golden end; neither the snapshot
/// nor the golden file holds the budget.
#[test]
fn the_watchdog_is_not_architectural() {
    let fixture = fixture();
    let golden = golden();
    let n = golden.record.events;
    let bounded = |budget: Option<u64>| {
        runner::execute_bounded(
            fixture.platform(),
            Start::Init { traced: true },
            Vec::new(),
            budget,
        )
    };
    let (base, rt) = bounded(None);
    let base_snapshot = rt.snapshot().unwrap();
    for budget in [Some(EVENT_BUDGET), Some(n + 1), Some(n)] {
        let (other, rt) = bounded(budget);
        let mut outcome = other.outcome.clone();
        if budget == Some(n) {
            // A budget of exactly the event count stops before the step that would find
            // the queue empty: the run reports the watchdog, in the same state.
            assert_eq!(
                outcome.end,
                End::Fault(format!("stopped at the budget of {n} events"))
            );
            outcome.end = base.outcome.end.clone();
        }
        assert_eq!(outcome, base.outcome, "{budget:?}");
        assert_eq!(rt.snapshot().unwrap(), base_snapshot, "{budget:?}");
        assert_eq!(
            other.trace.as_ref().unwrap().canonical_bytes(),
            base.trace.as_ref().unwrap().canonical_bytes()
        );
    }
    assert_eq!(base.outcome.state, Some(golden.record.state));

    let stop = 5_000;
    let (stopped, rt) = bounded(Some(stop));
    assert_eq!(
        stopped.outcome.end,
        End::Fault(format!("stopped at the budget of {stop} events"))
    );
    let snapshot = rt.snapshot().unwrap();
    assert_eq!(snapshot, snapshot_after(&fixture, stop as usize).unwrap());
    let reference = reference_of(&fixture);
    resume_exact(&fixture, &reference, stop as usize, snapshot.clone()).unwrap();

    let budget = EVENT_BUDGET.to_le_bytes();
    let layout = Layout::parse(&snapshot).unwrap();
    println!(
        "S5 max_events_per_phase in the snapshot: {}",
        get_u64(&snapshot, layout.max_events_per_phase)
    );
    assert_ne!(
        get_u64(&snapshot, layout.max_events_per_phase),
        EVENT_BUDGET
    );
    for bytes in [&snapshot[..], MID_SNAPSHOT] {
        assert!(
            !bytes.windows(8).any(|w| w == budget),
            "the budget is in a snapshot"
        );
    }
    assert!(!GOLDEN_JSON.contains(&EVENT_BUDGET.to_string()));
    assert!(!GOLDEN_JSON.contains("budget"));
}

// ---------------------------------------------------------------------------------------
// The portable snapshot.

#[test]
fn portable_snapshot_resumes_to_the_golden() {
    golden()
        .check_portable(&fixture(), MID_SNAPSHOT)
        .unwrap_or_else(|e| panic!("{e}"));
}

/// The portable snapshot is mid-DMA: the WRITE to LBA 1 has read 16 of its 32 beats
/// into the controller's buffer, beat 16's request is on its way to the bus, LBA 1 is
/// still zero, the CPU is polling, and one interrupt is still to come per remaining
/// transfer.
#[test]
fn the_portable_snapshot_is_where_it_says() {
    let fixture = fixture();
    let golden = golden();
    let reference = reference_of(&fixture);
    let k = mid_after(&reference.boundaries).unwrap();
    assert_eq!(k as u64, golden.mid.after_events);
    let b = reference.boundary(k).unwrap();
    assert!(b.command.starts_with("write lba=0x1"), "{b:?}");
    assert_eq!((b.engine.as_str(), b.beat), ("wait_beat", MID_BEATS));
    assert!(!b.in_handler() && !b.blk_irq && !b.irqc_out);
    assert_eq!(
        reference.interrupt_events.range(..k).count(),
        1,
        "transfer 1 completed"
    );

    let layout = Layout::parse(MID_SNAPSHOT).unwrap();
    let blk = &layout.components[BLK.0 as usize].state;
    // The tail of the controller's state: the buffer (length-prefixed), the dma and blk
    // txns, and the IRQ level.
    let buffer_end = blk.end - 1 - 8 - 8;
    let buffer_len = MID_SNAPSHOT[buffer_end - 256 - 4..buffer_end - 256]
        .try_into()
        .map(u32::from_le_bytes)
        .unwrap();
    assert_eq!(buffer_len, 16 * MID_BEATS as u32, "16 beats buffered");
    let transformed = block_irq::transformed();
    assert_eq!(
        &MID_SNAPSHOT[buffer_end - 256..buffer_end],
        &transformed[..256]
    );

    let queued = pending(MID_SNAPSHOT).unwrap();
    let classes = owners(&reference, k, &queued);
    println!("queued at the portable point: {queued:?}");
    assert!(classes.contains(&Owner::DmaRamRequest), "{classes:?}");

    let components = m2ref::components(MID_SNAPSHOT).unwrap();
    let disk = &components[DISK.0 as usize].bytes;
    assert_eq!(
        m2ref::disk_block(disk, 1).unwrap(),
        vec![0; 512],
        "LBA 1 not yet written"
    );
    assert_eq!(m2ref::disk_block(disk, 0).unwrap(), block_irq::pattern());
    let rest = fixture.run(
        Start::Restore {
            snapshot: MID_SNAPSHOT.to_vec(),
            prefix: None,
        },
        Vec::new(),
    );
    assert_eq!(
        rest_of_work(&rest.finished.finished.dispatched),
        MidRest::expected()
    );
    println!("m2-reference.mid.snap: {} bytes", MID_SNAPSHOT.len());
    for (path, entry) in m2ref::PATHS.iter().zip(&layout.components) {
        println!("  {path}: {} bytes of state", entry.state.len());
    }
    let pages = m2ref::ram_pages(&components[RAM.0 as usize].bytes).unwrap();
    println!("  RAM pages stored: {}", pages.len());
}

/// The committed snapshot is exactly what this machine takes at the same point, twice,
/// and holds nothing host-specific.
#[test]
fn the_portable_snapshot_is_canonical() {
    let fixture = fixture();
    let k = golden().mid.after_events as usize;
    let a = snapshot_after(&fixture, k).unwrap();
    let b = snapshot_after(&fixture, k).unwrap();
    assert!(a == b && a == MID_SNAPSHOT, "the snapshot bytes differ");
    let root = workspace_root().display().to_string();
    let contains = |needle: &[u8]| MID_SNAPSHOT.windows(needle.len()).any(|w| w == needle);
    for needle in [root.as_bytes(), b"/home/", b"/Users/"] {
        assert!(!contains(needle), "{:?}", String::from_utf8_lossy(needle));
    }
    // A traced run takes the same snapshot at the same point.
    let mut rt = fixture.platform();
    rt.start_trace().unwrap();
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    assert!(rt.snapshot().unwrap() == MID_SNAPSHOT);
}

/// The resumed run starts from the bytes: a doctored byte of the half-buffered WRITE
/// data reaches LBA 1, the program reads it back in transfer 3, finds the mismatch, and
/// fails; the golden check reports it.
#[test]
fn a_resumed_run_starts_from_the_snapshot_bytes() {
    let fixture = fixture();
    let golden = golden();
    let mut bytes = MID_SNAPSHOT.to_vec();
    let layout = Layout::parse(&bytes).unwrap();
    let blk = &layout.components[BLK.0 as usize].state;
    let first_buffered = blk.end - 1 - 8 - 8 - 256;
    bytes[first_buffered] ^= 0x01;
    let mut rehashed = golden.clone();
    rehashed.mid.blake3 = *blake3::hash(&bytes).as_bytes();
    let err = rehashed.check_portable(&fixture, &bytes).unwrap_err();
    println!("{err}");
    assert!(err.contains("RVTEST_FAIL"), "{err}");
    let resumed = fixture.run(
        Start::Restore {
            snapshot: bytes,
            prefix: None,
        },
        Vec::new(),
    );
    assert_eq!(resumed.output.as_deref(), Ok(&block_irq::FAIL_OUTPUT[..]));
    assert!(block_irq::judge(&resumed).is_err());
    let state = resumed.state.as_ref().unwrap();
    let mut expected = block_irq::transformed();
    expected[0] ^= 0x01;
    assert_eq!(
        state.lba1, expected,
        "the doctored byte was written to LBA 1"
    );
}

#[test]
fn portable_snapshot_mismatches_are_reported() {
    let golden = golden();
    let fixture = fixture();
    let mut bytes = MID_SNAPSHOT.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let err = golden.check_portable(&fixture, &bytes).unwrap_err();
    assert!(err.contains("bytes differ"), "{err}");

    let mut wrong = golden.clone();
    wrong.record.execution[0] ^= 1;
    let err = wrong.check_portable(&fixture, MID_SNAPSHOT).unwrap_err();
    assert!(err.contains("ExecutionDigest"), "{err}");

    let mut short = golden.clone();
    short.mid.size -= 1;
    assert!(short.check_portable(&fixture, MID_SNAPSHOT).is_err());

    let mut other = fixture.clone();
    other.disk_blake3[0] ^= 1;
    let err = golden.check_portable(&other, MID_SNAPSHOT).unwrap_err();
    assert!(err.contains("another"), "{err}");
}

// ---------------------------------------------------------------------------------------
// Rejections.

fn restore_error(rt: &mut systemscope_runtime::runtime::Runtime, bytes: &[u8]) -> RestoreError {
    match rt.restore(bytes) {
        Err(RuntimeError::Restore(e)) => e,
        other => panic!("expected a restore error, got {other:?}"),
    }
}

fn frozen_error(fixture: &Fixture, bytes: &[u8]) -> RestoreError {
    restore_error(&mut fixture.platform(), bytes)
}

fn doctored(edit: impl FnOnce(&mut Vec<u8>, &Layout)) -> Vec<u8> {
    let mut bytes = MID_SNAPSHOT.to_vec();
    let layout = Layout::parse(&bytes).unwrap();
    edit(&mut bytes, &layout);
    bytes
}

fn state(l: &Layout, id: ComponentId) -> std::ops::Range<usize> {
    l.components[id.0 as usize].state.clone()
}

/// A failed restore faults the session; the runtime does not roll the components back
/// (`docs/m0-design.md` §7): the session refuses every later restore and step.
#[test]
fn a_failed_restore_faults_the_session() {
    let fixture = fixture();
    let mut rt = fixture.platform();
    let bad = doctored(|b, l| b[state(l, IRQC).end - 1] ^= 1);
    let err = restore_error(&mut rt, &bad);
    println!("{err:?}");
    assert_eq!(rt.lifecycle(), Lifecycle::Faulted);
    assert!(rt.restore(MID_SNAPSHOT).is_err(), "no second restore");
    assert!(rt.step().is_err(), "no step");
    assert!(rt.snapshot().is_err(), "no snapshot");
    // A fresh platform restores the good bytes.
    fixture.platform().restore(MID_SNAPSHOT).unwrap();
}

/// Every configuration the snapshot was taken with must match the platform it restores
/// into. Reported: which rule catches each one.
#[test]
fn configuration_mismatches_are_rejected() {
    let fixture = fixture();
    let frozen = Config::frozen();
    let build = |config: Config| fixture.platform_with(&config).unwrap();
    let cases: Vec<(&str, systemscope_runtime::runtime::Runtime)> = vec![
        (
            "seed",
            build(Config {
                seed: frozen.seed ^ 1,
                ..frozen.clone()
            }),
        ),
        (
            "RAM size",
            build(Config {
                ram_size: frozen.ram_size / 2,
                dma_size: frozen.dma_size / 2,
                ..frozen.clone()
            }),
        ),
        (
            "DMA aperture base",
            build(Config {
                dma_base: frozen.dma_base + 0x1000,
                dma_size: frozen.dma_size - 0x1000,
                ..frozen.clone()
            }),
        ),
        (
            "DMA aperture size",
            build(Config {
                dma_size: frozen.dma_size / 2,
                ..frozen.clone()
            }),
        ),
        (
            "media and controller capacity",
            build(Config {
                controller_blocks: 32,
                media_blocks: 32,
                ..frozen.clone()
            }),
        ),
        (
            "UART base",
            build(Config {
                uart_base: frozen.uart_base + 0x100,
                ..frozen.clone()
            }),
        ),
        (
            "IRQ controller base",
            build(Config {
                irqc_base: frozen.irqc_base + 0x100,
                ..frozen.clone()
            }),
        ),
        (
            "block controller base",
            build(Config {
                blk_base: frozen.blk_base + 0x100,
                ..frozen.clone()
            }),
        ),
        (
            "bad blocks",
            build(Config {
                bad_blocks: BTreeSet::from([7]),
                ..frozen.clone()
            }),
        ),
    ];
    let mut report = Vec::new();
    for (what, mut rt) in cases {
        let err = restore_error(&mut rt, MID_SNAPSHOT);
        println!("{what}: {err:?}");
        report.push((what, err));
    }
    let find = |what: &str| &report.iter().find(|r| r.0 == what).unwrap().1;
    assert_eq!(
        *find("seed"),
        RestoreError::SessionMismatch(SessionField::Seed)
    );
    let invalid = |what: &str, component: &str| {
        let err = find(what);
        let text = format!("{err:?}").to_lowercase();
        assert!(
            matches!(err, RestoreError::InvalidState(_)) && text.contains(component),
            "{what}: {err:?}"
        );
    };
    invalid(
        "DMA aperture base",
        "dma controller: snapshot has a different config",
    );
    invalid(
        "DMA aperture size",
        "dma controller: snapshot has a different config",
    );
    invalid("bad blocks", "disk: snapshot has different bad blocks");
    // The bus holds the address map, so it is the first to refuse a moved region; its
    // component id is below the RAM's and the devices'.
    for what in [
        "RAM size",
        "UART base",
        "IRQ controller base",
        "block controller base",
    ] {
        invalid(
            what,
            "multi-master bus: snapshot was taken with a different configuration",
        );
    }
    // The controller restores before the media.
    invalid(
        "media and controller capacity",
        "dma controller: snapshot has a different config",
    );

    // Another disk image: the media's image_hash check refuses it.
    let mut disk = fixture.disk.clone();
    disk[5] ^= 0x40;
    let mut rt = m2ref::build(&fixture.image, &disk, &frozen).unwrap();
    let err = restore_error(&mut rt, MID_SNAPSHOT);
    println!("another disk image: {err:?}");
    assert!(format!("{err:?}").contains("disk"), "{err:?}");
    // Another ELF: the RAM's image_hash check refuses it.
    let hello = systemscope_rv32::hello::HelloManifest::read(&workspace_root())
        .and_then(|m| m.read_image(&workspace_root()))
        .unwrap();
    let mut rt = m2ref::build(&hello, &fixture.disk, &frozen).unwrap();
    let err = restore_error(&mut rt, MID_SNAPSHOT);
    println!("another ELF: {err:?}");
    assert!(format!("{err:?}").contains("ram"), "{err:?}");
    // The platform refuses to build a media capacity the controller does not share, so
    // a snapshot recording another media capacity is the only way to reach the media's
    // own check.
    let err = frozen_error(
        &fixture,
        &doctored(|b, l| put_u64(b, state(l, DISK).start, 32)),
    );
    println!("media capacity 32 in the snapshot: {err:?}");
    assert_eq!(
        err,
        RestoreError::InvalidState("disk: snapshot has a different capacity")
    );
    // The contracts version is session information.
    let err = frozen_error(&fixture, &doctored(|b, l| b[l.contracts_version + 4] ^= 1));
    assert_eq!(
        err,
        RestoreError::SessionMismatch(SessionField::ContractsVersion)
    );
    // The master order is part of the bus configuration: the encoded ["cpu", "dma0"]
    // swapped in place to ["dma0", "cpu"], same length, is refused by the bus.
    let err = frozen_error(
        &fixture,
        &doctored(|b, l| {
            let bus = state(l, BUS);
            let masters: &[u8] = b"   cpu   dma0";
            let at = bus.start
                + MID_SNAPSHOT[bus.clone()]
                    .windows(masters.len())
                    .position(|w| w == masters)
                    .unwrap();
            b[at..at + masters.len()].copy_from_slice(b"   dma0   cpu");
        }),
    );
    println!("master order swapped: {err:?}");
    assert_eq!(
        err,
        RestoreError::InvalidState(
            "multi-master bus: snapshot was taken with a different configuration"
        )
    );
}

/// Corrupted snapshots are rejected by the runtime's decoder and the components' restore
/// rules, each for the reason printed.
#[test]
fn malformed_snapshots_are_rejected() {
    let fixture = fixture();
    let snap = MID_SNAPSHOT;
    for len in (0..snap.len()).step_by(1) {
        let err = frozen_error(&fixture, &snap[..len]);
        assert!(
            matches!(err, RestoreError::Decode(_) | RestoreError::BadMagic),
            "truncated to {len}: {err:?}"
        );
    }
    let mut long = snap.to_vec();
    long.push(0);
    assert!(matches!(
        frozen_error(&fixture, &long),
        RestoreError::Decode(_)
    ));
    assert_eq!(
        frozen_error(&fixture, &doctored(|b, _| b[0] ^= 1)),
        RestoreError::BadMagic
    );

    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "CPU schema 1 in the M2 profile",
            doctored(|b, l| put_u32(b, l.components[CPU.0 as usize].schema, 1)),
            "SchemaVersion",
        ),
        (
            "component entries out of order",
            doctored(|b, l| {
                put_u32(b, l.components[RAM.0 as usize].id, UART.0);
                put_u32(b, l.components[UART.0 as usize].id, RAM.0);
            }),
            "out of id order",
        ),
        (
            "one component entry too few (the disk's dropped)",
            {
                let l = Layout::parse(snap).unwrap();
                let disk = &l.components[DISK.0 as usize];
                let mut b = snap[..disk.id].to_vec();
                let count_at = l.components[CPU.0 as usize].id - 4;
                put_u32(&mut b, count_at, 6);
                b
            },
            "not one entry per component",
        ),
        (
            "CPU pc misaligned",
            // The configuration (clock, entry, instruction limit) comes first, then pc.
            doctored(|b, l| b[state(l, CPU).start + 16] |= 2),
            "rv32 cpu: misaligned pc",
        ),
        (
            "CPU mstatus.MIE not 0 or 1",
            doctored(|b, l| b[state(l, CPU).end - 24] = 2),
            "rv32 cpu: a CSR flag is not 0 or 1",
        ),
        (
            "CPU irq level not 0 or 1",
            doctored(|b, l| b[state(l, CPU).end - 1] = 2),
            "rv32 cpu: a CSR flag is not 0 or 1",
        ),
        (
            "bus round-robin cursor past the last master",
            doctored(|b, l| {
                let at = bus_ram_cursor(b, l);
                b[at] = 2;
            }),
            "multi-master bus: cursor past the last master",
        ),
        (
            "bus queue claiming a request it does not hold",
            doctored(|b, l| {
                let at = bus_ram_cursor(b, l) + 2;
                put_u32(b, at, 1);
            }),
            "multi-master bus",
        ),
        (
            "DMA beat index past the buffer",
            doctored(|b, l| {
                let at = state(l, BLK).end - 1 - 8 - 8 - 256 - 4 - 1;
                b[at] = 33;
            }),
            "dma controller: engine position not reachable for the command",
        ),
        (
            "DMA buffer shorter than its beat index",
            doctored(|b, l| {
                let at = state(l, BLK).end - 1 - 8 - 8 - 256 - 4 - 1;
                b[at] = MID_BEATS as u8 + 1;
            }),
            "dma controller: engine position not reachable for the command",
        ),
        (
            "DMA outstanding txn not the latest issued",
            doctored(|b, l| {
                let at = state(l, BLK).end - 1 - 8 - 8;
                let txn = get_u64(b, at);
                put_u64(b, at, txn + 1);
            }),
            "dma controller: engine position not reachable for the command",
        ),
        (
            "DMA engine state unknown",
            // WaitBeat's tag and txn, the block index, and the beat index come before the
            // buffer.
            doctored(|b, l| {
                let at = state(l, BLK).end - 1 - 8 - 8 - 256 - 4 - 1 - 4 - 8 - 1;
                assert_eq!(b[at], 3, "WaitBeat");
                b[at] = 9;
            }),
            "dma controller: unknown engine state",
        ),
        (
            "DMA IRQ level without DONE",
            doctored(|b, l| b[state(l, BLK).end - 1] = 1),
            "dma controller: IRQ level is not DONE && IRQ_ENABLE",
        ),
        (
            "IRQ controller out without pending & enable",
            doctored(|b, l| b[state(l, IRQC).end - 1] = 1),
            "irq controller: out is not (pending & enable) != 0",
        ),
        (
            "IRQ controller pending bit past its sources",
            doctored(|b, l| b[state(l, IRQC).end - 9] = 2),
            "irq controller: pending bit outside the sources",
        ),
        (
            "media image hash",
            doctored(|b, l| {
                let disk = state(l, DISK);
                // capacity, latency (1 + 4 + 8), bad-block count, then the hash.
                b[disk.start + 8 + 13 + 4] ^= 1;
            }),
            "disk: snapshot has a different image hash",
        ),
        (
            "media block not canonical: LBA 0 stored as zeros",
            doctored(|b, l| {
                let disk = state(l, DISK);
                let blocks = disk.start + 8 + 13 + 4 + 32 + 4 + 8 + 4;
                b[blocks..blocks + 512].fill(0);
            }),
            "disk: all-zero block",
        ),
    ];
    for (what, bytes, reason) in cases {
        let err = frozen_error(&fixture, &bytes);
        println!("{what}: {err:?}");
        let text = format!("{err:?}").to_lowercase();
        assert!(text.contains(&reason.to_lowercase()), "{what}: {err:?}");
    }
}

/// The offset of the bus's RAM-region cursor in a snapshot: after the configuration
/// (regions, masters, clock), the downstream counter, and the region's active txn.
fn bus_ram_cursor(bytes: &[u8], l: &Layout) -> usize {
    let bus = state(l, BUS);
    let mut at = bus.start;
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let regions = u32_at(at);
    at += 4;
    for _ in 0..regions {
        at += 4 + u32_at(at) + 16;
    }
    let masters = u32_at(at);
    at += 4;
    for _ in 0..masters {
        at += 4 + u32_at(at);
    }
    at += 4 + 8;
    at + if bytes[at] == 1 { 1 + 2 + 8 + 8 + 1 } else { 1 }
}
