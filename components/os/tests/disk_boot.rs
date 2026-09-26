//! Booting from disk driven through `MockCtx` (`docs/m3-design.md` §8.1–§8.3, §17.3): the
//! kernel reads the `SSX0` table at LBA 0 and each executable it names into staging
//! through an independent block controller model, polling `STATUS` and writing `ACK`;
//! validates the table and the headers with the pure `systemscope-elf` rules, re-reading
//! the headers when the program-header table is past the ELF header; creates each process
//! from staging; and fails a table or an entry exactly as §8.1 and §8.2 say. The
//! snapshot: its disk stages, restore at every access boundary of boot without a reissue,
//! and the restore rejections a disk boot adds.

mod common;

use std::collections::BTreeMap;

use common::disk::{self, Boot, MockBlk};
use common::layout::*;
use common::procs::config;
use common::procs::ksnap::{self, Snap, Stage};
use common::procs::*;
use common::{restore_into, snapshot_of};
use systemscope_contracts::component::Component;
use systemscope_elf::BLOCK_SIZE;
use systemscope_os::boot::{ACK, BLOCK_COUNT, COMMAND, LBA, MEM_ADDR, STATUS};
use systemscope_os::kernel::SHUTDOWN_KIND;
use systemscope_os::process::ProcState;
use systemscope_os::procop::{BOOT_KIND, CREATE_KIND, SWITCH_KIND};
use systemscope_os::{DiskBoot, KernelConfig, ModeledKernel, PlanError, UserLayout};

const BLOCKS: u32 = 64;
const POOL_PPN: u32 = (POOL / 4096) as u32;

fn boot_config() -> DiskBoot {
    DiskBoot {
        layout: UserLayout::M3,
        capacity_blocks: BLOCKS,
    }
}

/// A program with a text page and a data page with `bss` more bytes.
fn prog(tag: u32, bss: u32) -> Vec<u8> {
    two_segment(&[0x0000_0013, 0x0010_0073, tag], b"DATAdata", bss)
}

/// A program with `n` one-page segments, so its headers are `52 + 32 n` bytes: the
/// program-header table is past the 52-byte ELF header, and the kernel re-reads it.
fn many(n: u32) -> Vec<u8> {
    let segs: Vec<Seg> = (0..n)
        .map(|i| {
            let flags = if i == 0 { PF_R | PF_X } else { PF_R };
            Seg::new(0x0001_0000 + 0x1000 * i, flags, words(&[0x13, i]), 8)
        })
        .collect();
    elf32(0x0001_0000, &segs)
}

fn harness(disk: Vec<u8>, pool: u64) -> Harness {
    Harness::with_disk(config_with_pool(pool), boot_config(), MockBlk::new(disk))
}

fn blk(h: &Harness) -> &MockBlk {
    h.blk.as_ref().unwrap()
}

fn creates(h: &Harness) -> Vec<(u64, u64, u64, String)> {
    h.traced(CREATE_KIND)
        .iter()
        .map(|t| (tu(t, "pid"), tu(t, "entry"), tu(t, "root"), ts(t, "error")))
        .collect()
}

/// The oracle's creates as the kernel traces them, without the entry.
fn oracle_creates(b: &Boot) -> Vec<(u64, u64, String)> {
    match b {
        Boot::Booted { creates, .. } => creates
            .iter()
            .enumerate()
            .map(|(i, c)| match c {
                Ok(root) => (i as u64 + 1, root / 4096, String::new()),
                Err(e) => (i as u64 + 1, 0, e.clone()),
            })
            .collect(),
        Boot::Fail(_) => Vec::new(),
    }
}

fn check_against_oracle(h: &Harness, expected: &Boot) {
    let got: Vec<(u64, u64, String)> = creates(h)
        .into_iter()
        .map(|(pid, _, root, e)| (pid, root, e))
        .collect();
    assert_eq!(got, oracle_creates(expected));
    match expected {
        Boot::Fail(detail) => {
            assert!(h.traced(BOOT_KIND).is_empty());
            let s = h.traced(SHUTDOWN_KIND);
            assert_eq!(s.len(), 1);
            assert_eq!(tu(s[0], "reason"), 1);
            assert!(ts(s[0], "detail").starts_with(detail.as_str()), "{s:?}");
            assert_eq!(blk(h).transfers.len(), 1);
            assert_eq!(h.frame(0x90), 1, "action Shutdown");
            assert_eq!(h.frame(0x94), 1, "reason 1");
        }
        Boot::Booted {
            entries,
            transfers,
            first,
            ..
        } => {
            let b = h.traced(BOOT_KIND);
            assert_eq!(b.len(), 1);
            assert_eq!(tu(b[0], "entries"), *entries as u64);
            assert_eq!(&blk(h).transfers, transfers);
            match first {
                Some(pid) => {
                    let s = h.traced(SWITCH_KIND);
                    assert_eq!((tu(s[0], "from"), tu(s[0], "to")), (0, u64::from(*pid)));
                    assert_eq!(h.frame(0x90), 0, "action Resume");
                }
                None => {
                    let s = h.traced(SHUTDOWN_KIND);
                    assert_eq!(s.len(), 1);
                    assert_eq!(tu(s[0], "reason"), 1);
                    assert_eq!(ts(s[0], "detail"), "the run queue is empty");
                }
            }
        }
    }
    assert!(!blk(h).in_flight(), "every transfer was acknowledged");
}

#[test]
fn a_disk_kernel_validates_its_boot_configuration() {
    let small_staging = KernelConfig {
        staging: systemscope_os::Window {
            base: STAGING,
            size: 256,
        },
        ..config()
    };
    let unaligned = KernelConfig {
        staging: systemscope_os::Window {
            base: STAGING + 8,
            size: STAGING_SIZE - 16,
        },
        ..config()
    };
    let short_blk = KernelConfig {
        blk: systemscope_os::Window {
            base: BLK_BASE,
            size: 0x18,
        },
        ..config()
    };
    let none = DiskBoot {
        capacity_blocks: 0,
        ..boot_config()
    };
    let err = |c: KernelConfig, b: DiskBoot| ModeledKernel::with_disk(c, b).err();
    assert_eq!(
        err(config(), none),
        Some(PlanError::Disk("the disk has no block"))
    );
    assert_eq!(
        err(small_staging, boot_config()),
        Some(PlanError::Disk("staging cannot hold block 0"))
    );
    assert_eq!(
        err(unaligned, boot_config()),
        Some(PlanError::Disk("staging is not 16-byte aligned"))
    );
    assert_eq!(
        err(short_blk, boot_config()),
        Some(PlanError::Disk(
            "the block controller window does not hold its registers"
        ))
    );
    assert!(err(config(), boot_config()).is_none());
}

#[test]
fn boot_reads_the_table_and_every_entry_through_the_controller_by_polling() {
    let files = vec![prog(1, 0), prog(2, 5000), prog(3, 0)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d.clone(), 3072);
    h.blk.as_mut().unwrap().delay = 3;
    h.boot();
    let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
    check_against_oracle(&h, &expected);
    // Each transfer: LBA, MEM_ADDR, BLOCK_COUNT, COMMAND; 4 STATUS reads; ACK.
    let regs: Vec<(bool, u64)> = h
        .seen
        .iter()
        .filter(|s| s.1 >= BLK_BASE && s.1 < BLK_BASE + BLK_SIZE)
        .map(|s| (s.0, s.1 - BLK_BASE))
        .collect();
    let one: Vec<(bool, u64)> = [
        (true, LBA),
        (true, MEM_ADDR),
        (true, BLOCK_COUNT),
        (true, COMMAND),
    ]
    .into_iter()
    .chain([(false, STATUS); 4])
    .chain([(true, ACK)])
    .collect();
    assert_eq!(regs, one.repeat(4));
    assert_eq!(blk(&h).polls, 16);
    // The processes are Ready but for PID 1, and the pool holds their frames.
    let pcbs = h.k.processes().unwrap().pcbs();
    assert_eq!(pcbs.len(), 3);
    assert_eq!(pcbs[0].state, ProcState::Running);
    // Every staging read is block 0, then 52 bytes and the whole header table of each
    // file, then its segment bytes.
    let staged: Vec<(u64, usize)> = h
        .seen
        .iter()
        .filter(|s| !s.0 && s.1 >= STAGING && s.1 < STAGING + STAGING_SIZE)
        .map(|s| (s.1, s.2.len()))
        .collect();
    let total = |from: usize, to: usize| -> usize {
        staged
            .iter()
            .skip(from)
            .take(to - from)
            .map(|s| s.1)
            .sum::<usize>()
    };
    assert_eq!(staged[0].0, STAGING);
    assert_eq!(total(0, 32), BLOCK_SIZE, "block 0 in 16-byte pieces");
    assert_eq!(staged[32].0, STAGING, "the first file's ELF header");
    assert_eq!(total(32, 36), 52);
    assert_eq!(
        staged[36].0, STAGING,
        "then the headers again, to their end"
    );
    assert_eq!(total(36, 44), 52 + 2 * 32);
}

#[test]
fn the_header_prefix_is_re_read_to_its_needed_length() {
    for n in [1, 8] {
        let f = many(n);
        let d = disk::disk(std::slice::from_ref(&f), BLOCKS);
        let mut h = harness(d.clone(), 3072);
        h.boot();
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
        check_against_oracle(&h, &expected);
        let c = creates(&h);
        assert_eq!(c[0].3, "", "created");
        // After block 0: 52 header bytes, then 52 + 32 n from the staging base.
        let reads: Vec<(u64, usize)> = h
            .seen
            .iter()
            .filter(|s| !s.0 && s.1 >= STAGING && s.1 < STAGING + STAGING_SIZE)
            .map(|s| (s.1, s.2.len()))
            .skip(32)
            .collect();
        let mut at = 0;
        for want in [52usize, 52 + 32 * n as usize] {
            assert_eq!(reads[at].0, STAGING);
            let mut got = 0;
            while got < want {
                got += reads[at].1;
                at += 1;
            }
            assert_eq!(got, want);
        }
    }
}

#[test]
fn a_file_shorter_than_an_elf_header_is_read_whole_and_rejected() {
    let files = vec![b"\x7fELF\x01\x01\x01".to_vec(), prog(2, 0)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d.clone(), 3072);
    h.boot();
    let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
    check_against_oracle(&h, &expected);
    let c = creates(&h);
    assert_eq!((c[0].0, c[0].1), (1, 0));
    assert!(!c[0].3.is_empty());
    assert_eq!(c[1].3, "", "boot continues with the next entry");
}

#[test]
fn an_invalid_table_shuts_down_with_reason_1_naming_the_check() {
    let files = vec![prog(1, 0)];
    let good = disk::disk(&files, BLOCKS);
    let mut cases = Vec::new();
    let mut bad_magic = good.clone();
    bad_magic[0] ^= 1;
    cases.push(bad_magic);
    let mut reserved = good.clone();
    reserved[0x0C] = 1;
    cases.push(reserved);
    let mut past_end = good.clone();
    past_end[0x10..0x14].copy_from_slice(&BLOCKS.to_le_bytes());
    cases.push(past_end);
    let mut lba0 = good.clone();
    lba0[0x10..0x14].copy_from_slice(&0u32.to_le_bytes());
    cases.push(lba0);
    let mut zero_count = good.clone();
    zero_count[8..12].copy_from_slice(&0u32.to_le_bytes());
    cases.push(zero_count);
    for d in cases {
        let mut h = harness(d.clone(), 3072);
        h.boot();
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
        assert!(matches!(expected, Boot::Fail(_)));
        check_against_oracle(&h, &expected);
        assert!(h.k.processes().unwrap().pcbs().is_empty());
        assert_eq!(h.k.processes().unwrap().frames().free_count(), 3072);
    }
}

#[test]
fn a_table_larger_than_staging_is_invalid() {
    let files = vec![vec![0x5A; 2000]];
    let d = disk::disk(&files, BLOCKS);
    let config = KernelConfig {
        staging: systemscope_os::Window {
            base: STAGING,
            size: 1024,
        },
        ..config()
    };
    let mut h = Harness::with_disk(config, boot_config(), MockBlk::new(d.clone()));
    h.boot();
    let expected = disk::expect(&d, BLOCKS, 1024, 3072, &BTreeMap::new());
    assert!(matches!(&expected, Boot::Fail(e) if e.starts_with("invalid executable table")));
    check_against_oracle(&h, &expected);
}

#[test]
fn a_controller_error_on_the_table_shuts_down_and_on_an_entry_fails_only_it() {
    let files = vec![prog(1, 0), prog(2, 0), prog(3, 0)];
    let d = disk::disk(&files, BLOCKS);
    for errors in [
        BTreeMap::from([(0, 3u8)]),
        BTreeMap::from([(2, 5u8)]),
        BTreeMap::from([(1, 7u8), (3, 1u8)]),
        BTreeMap::from([(1, 2u8), (2, 2u8), (3, 2u8)]),
    ] {
        let mut h = harness(d.clone(), 3072);
        h.blk.as_mut().unwrap().errors = errors.clone();
        h.blk.as_mut().unwrap().delay = 1;
        h.boot();
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &errors);
        check_against_oracle(&h, &expected);
        // A failed transfer's bytes are never read from staging after its ACK.
        let failed_entries = errors.keys().filter(|&&t| t > 0).count();
        let staged_headers = h.seen.iter().filter(|s| !s.0 && s.1 == STAGING).count();
        if !errors.contains_key(&0) {
            assert_eq!(staged_headers, 1 + 2 * (3 - failed_entries), "{errors:?}");
        }
    }
}

#[test]
fn an_entry_in_a_megapage_slot_or_beyond_the_pool_fails_its_creation_only() {
    let mmio = elf32(
        0x1000_0000,
        &[Seg::new(0x1000_0000, PF_R | PF_X, words(&[0x13]), 4)],
    );
    let files = vec![mmio, prog(2, 0), prog(3, 64 * 4096)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d.clone(), 20);
    h.boot();
    let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 20, &BTreeMap::new());
    check_against_oracle(&h, &expected);
    let c = creates(&h);
    assert_eq!(c[0].3, "a segment is in a megapage slot");
    assert_eq!(c[0].1, 0x1000_0000, "the entry of a parsed image");
    assert_eq!(c[1].3, "");
    assert_eq!(c[2].3, "frame pool exhausted");
}

#[test]
fn a_disk_whose_every_entry_fails_shuts_down_with_reason_1() {
    let files = vec![vec![0; 100], vec![1; 100]];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d.clone(), 3072);
    h.boot();
    let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
    assert!(matches!(&expected, Boot::Booted { first: None, .. }));
    check_against_oracle(&h, &expected);
}

#[test]
fn eight_entries_become_pids_1_to_8_in_table_order() {
    let files: Vec<Vec<u8>> = (0..8).map(|i| prog(i, 0)).collect();
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d.clone(), 3072);
    h.boot();
    let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
    check_against_oracle(&h, &expected);
    let pids: Vec<u64> = creates(&h).iter().map(|c| c.0).collect();
    assert_eq!(pids, (1..=8).collect::<Vec<_>>());
}

#[test]
fn the_loaded_image_is_mapped_from_the_disk_bytes() {
    let files = vec![prog(0xABCD_0123, 5000)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d, 3072);
    h.boot();
    let root = (creates(&h)[0].2) as u32;
    let text = walk(&h.mem, root, 0x0001_0008, Kind::Fetch).unwrap();
    assert_eq!(h.mem.word(text), 0xABCD_0123);
    let data = walk(&h.mem, root, 0x0001_1000, Kind::Load).unwrap();
    assert_eq!(h.mem.read(data, 8), b"DATAdata");
    let bss = walk(&h.mem, root, 0x0001_2000, Kind::Store).unwrap();
    assert_eq!(h.mem.word(bss), 0);
    assert_eq!(root, POOL_PPN, "the lowest free frame is the root");
}

/// The kernel snapshot's prefix length (configuration and disk boot) for a pool of
/// `frames` frames.
fn disk_prefix(fresh: &[u8], frames: usize) -> usize {
    ksnap::prefix_len(fresh, frames) - 4
}

/// Runs boot access by access, and at every boundary (each request issued and not yet
/// answered, each response delivered and the next request not yet issued) replaces the
/// kernel with a fresh one restored from its snapshot. The restored kernel must re-encode
/// identically, never reissue a request, and the run must see the uninterrupted run's
/// accesses, memory, and traces. Returns the disk stages the boundaries were in.
fn restore_everywhere(files: &[Vec<u8>], errors: BTreeMap<usize, u8>, delay: u32) {
    let d = disk::disk(files, BLOCKS);
    let make = || {
        let mut h = harness(d.clone(), 3072);
        let b = h.blk.as_mut().unwrap();
        b.errors = errors.clone();
        b.delay = delay;
        h
    };
    let prefix = disk_prefix(&snapshot_of(&make().k), 3072);
    let mut reference = make();
    reference.boot();
    let mut h = make();
    let mut stages = Vec::new();
    let mut swap = |h: &mut Harness| {
        let before = snapshot_of(&h.k);
        let mut fresh = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
        restore_into(&mut fresh, &before).unwrap();
        assert_eq!(
            snapshot_of(&fresh),
            before,
            "restore re-encodes identically"
        );
        if let Some(op) = Snap::decode_disk(&before, prefix).unwrap().op {
            stages.push(format!("{:?}", std::mem::discriminant(&op.stage)));
        }
        h.k = fresh;
        assert!(h.ctx.take_sent().is_empty(), "a restore sends nothing");
        assert!(h.ctx.take_woke().is_empty(), "a restore schedules nothing");
    };
    h.enter(TRAP_FRAME).unwrap();
    swap(&mut h);
    for i in 0.. {
        assert!(i < MAX_OP_ACCESSES, "boot did not end");
        h.issue().unwrap();
        swap(&mut h);
        if h.respond(false).unwrap() {
            break;
        }
        swap(&mut h);
    }
    assert_eq!(h.seen, reference.seen);
    assert_eq!(h.mem, reference.mem);
    let traced =
        |h: &Harness| -> Vec<String> { h.ctx.traced.iter().map(|t| format!("{t:?}")).collect() };
    assert_eq!(traced(&h), traced(&reference));
    assert_eq!(snapshot_of(&h.k), snapshot_of(&reference.k));
    stages.sort();
    stages.dedup();
    // Command, Poll, Ack, Staged, Load, and the creation's and dispatch's own stages.
    assert!(stages.len() >= 6, "{stages:?}");
}

#[test]
fn boot_restores_at_every_access_without_a_reissue() {
    restore_everywhere(&[prog(1, 0), many(8)], BTreeMap::new(), 2);
}

#[test]
fn boot_restores_at_every_access_through_controller_errors() {
    restore_everywhere(
        &[prog(1, 0), prog(2, 0), b"short".to_vec()],
        BTreeMap::from([(2, 4)]),
        1,
    );
}

/// A kernel snapshot taken inside boot at the first `Staged` stage of an entry, decoded.
fn snapshot_at(files: &[Vec<u8>], want: impl Fn(&Stage) -> bool) -> (Vec<u8>, Snap, usize) {
    let d = disk::disk(files, BLOCKS);
    let mut h = harness(d, 3072);
    let prefix = disk_prefix(&snapshot_of(&h.k), 3072);
    h.enter(TRAP_FRAME).unwrap();
    for i in 0.. {
        assert!(i < MAX_OP_ACCESSES, "the stage never came");
        h.issue().unwrap();
        let bytes = snapshot_of(&h.k);
        let snap = Snap::decode_disk(&bytes, prefix).unwrap();
        if snap.op.as_ref().is_some_and(|op| want(&op.stage)) {
            return (bytes, snap, prefix);
        }
        assert!(!h.respond(false).unwrap(), "the stage never came");
    }
    unreachable!()
}

fn rejects(snap: &Snap) -> bool {
    let mut k = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
    restore_into(&mut k, &snap.encode()).is_err()
}

#[test]
fn a_restore_rejects_a_disk_stage_no_boot_could_be_in() {
    let files = vec![prog(1, 0), prog(2, 0)];
    // A table read: block 0 is 512 bytes, from index 0 with no entries yet.
    let (_, snap, _) = snapshot_at(
        &files,
        |s| matches!(s, Stage::Staged { cursor, .. } if cursor.0.is_none()),
    );
    let Some(ksnap::Op {
        stage: Stage::Staged { cursor, len },
        ..
    }) = snap.op.clone()
    else {
        unreachable!()
    };
    assert_eq!(len, 512);
    let with = |stage: Stage| {
        let mut s = snap.clone();
        s.op.as_mut().unwrap().stage = stage;
        s
    };
    assert!(!rejects(&snap));
    assert!(rejects(&with(Stage::Staged {
        cursor: cursor.clone(),
        len: 52
    })));
    assert!(rejects(&with(Stage::Staged {
        cursor: (None, 1),
        len
    })));
    // An entry's header read: between the first read and the file length.
    let (_, snap, _) = snapshot_at(&files, |s| {
        matches!(
            s,
            Stage::Staged {
                cursor: (Some(_), 1),
                ..
            }
        )
    });
    let Some(ksnap::Op {
        stage: Stage::Staged { cursor, len },
        ..
    }) = snap.op.clone()
    else {
        unreachable!()
    };
    let with = |stage: Stage| {
        let mut s = snap.clone();
        s.op.as_mut().unwrap().stage = stage;
        s
    };
    assert!(!rejects(&snap));
    let file_len = cursor.0.as_ref().unwrap()[1].1;
    assert!(rejects(&with(Stage::Staged {
        cursor: cursor.clone(),
        len: 51
    })));
    assert!(rejects(&with(Stage::Staged {
        cursor: cursor.clone(),
        len: file_len + 1
    })));
    assert!(!rejects(&with(Stage::Staged {
        cursor: cursor.clone(),
        len: file_len
    })));
    // Boot runs no process, and loads each entry after the processes before it.
    let mut running = snap.clone();
    running.pcbs[0].state = (1, vec![]);
    running.pcbs[0].context = None;
    running.queue.clear();
    running.current = Some(1);
    assert!(rejects(&running), "a boot stage with a process running");
    assert!(
        rejects(&with(Stage::Staged {
            cursor: (cursor.0.clone(), 0),
            len
        })),
        "a process already at the entry being loaded"
    );
    // A cursor past its table, or a table the kernel's entry count disagrees with.
    assert!(rejects(&with(Stage::Staged {
        cursor: (cursor.0.clone(), 2),
        len
    })));
    let mut other = snap.clone();
    other.entries = Some(3);
    assert!(rejects(&other));
    // A table that is not one block 0 could hold: overlapping entries.
    let mut t = cursor.0.clone().unwrap();
    t[1].0 = t[0].0;
    assert!(rejects(&with(Stage::Staged {
        cursor: (Some(t), 1),
        len
    })));
    // Before the table is read, the kernel knows no entry count.
    let (_, mut before, prefix) = snapshot_at(&files, |s| {
        matches!(s, Stage::Command { cursor: (None, 0) })
    });
    assert!(!rejects(&before));
    before.entries = Some(1);
    assert!(rejects(&before));
    // Nor does a kernel awaiting boot.
    let fresh =
        snapshot_of(&ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap());
    let mut awaiting = Snap::decode_disk(&fresh, prefix).unwrap();
    assert!(!rejects(&awaiting));
    awaiting.entries = Some(1);
    assert!(rejects(&awaiting), "a table count before boot");
}

#[test]
fn a_restore_rejects_a_load_whose_headers_or_frames_are_not_a_creation() {
    let files = vec![prog(1, 5000)];
    let (_, snap, _) = snapshot_at(&files, |s| matches!(s, Stage::Load { .. }));
    let Some(ksnap::Op {
        stage: Stage::Load {
            cursor,
            headers,
            frames,
        },
        ..
    }) = snap.op.clone()
    else {
        unreachable!()
    };
    let with = |headers: Vec<u8>, frames: Vec<u32>| {
        let mut s = snap.clone();
        s.op.as_mut().unwrap().stage = Stage::Load {
            cursor: cursor.clone(),
            headers,
            frames,
        };
        s
    };
    assert!(!rejects(&with(headers.clone(), frames.clone())));
    let mut bad = headers.clone();
    bad[0] ^= 1;
    assert!(
        rejects(&with(bad, frames.clone())),
        "headers that do not parse"
    );
    assert!(
        rejects(&with(headers[..headers.len() - 1].to_vec(), frames.clone())),
        "headers shorter than their table"
    );
    let mut fewer = frames.clone();
    fewer.pop();
    assert!(rejects(&with(headers.clone(), fewer)), "too few frames");
    let mut descending = frames.clone();
    descending.swap(0, 1);
    assert!(rejects(&with(headers.clone(), descending)));
    let mut repeated = frames.clone();
    repeated[1] = repeated[0];
    assert!(rejects(&with(headers.clone(), repeated)), "a frame twice");
    let mut outside = frames.clone();
    *outside.last_mut().unwrap() = POOL_PPN + 3072;
    assert!(rejects(&with(headers, outside)), "a frame outside the pool");
}

#[test]
fn a_restore_rejects_a_disk_snapshot_into_a_kernel_with_a_plan_and_back() {
    let files = vec![prog(1, 0)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d, 3072);
    h.boot();
    let bytes = snapshot_of(&h.k);
    let mut plan = ModeledKernel::with_processes(config_with_pool(3072), plan_of(&files)).unwrap();
    assert!(restore_into(&mut plan, &bytes).is_err());
    let other_capacity = DiskBoot {
        capacity_blocks: BLOCKS + 1,
        ..boot_config()
    };
    let mut k = ModeledKernel::with_disk(config_with_pool(3072), other_capacity).unwrap();
    assert!(restore_into(&mut k, &bytes).is_err());
    let mut same = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
    restore_into(&mut same, &bytes).unwrap();
    assert_eq!(snapshot_of(&same), bytes);
    let plan_bytes = {
        let mut ph = Harness::new(config_with_pool(3072), &files, plan_of(&files));
        ph.boot();
        snapshot_of(&ph.k)
    };
    let mut disk_kernel = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
    assert!(restore_into(&mut disk_kernel, &plan_bytes).is_err());
}

#[test]
fn a_disk_kernel_snapshot_encodes_its_boot_after_the_configuration() {
    // §17.3: the marker 0xD5, then USER_BASE, STACK_TOP, STACK_PAGES, and the capacity.
    let b = boot_config();
    let mut want = vec![0xD5];
    for w in [
        b.layout.user_base,
        b.layout.stack_top,
        b.layout.stack_pages,
        b.capacity_blocks,
    ] {
        want.extend(w.to_le_bytes());
    }
    let has = |bytes: &[u8]| bytes.windows(want.len()).any(|w| w == want);
    let disk = snapshot_of(&ModeledKernel::with_disk(config_with_pool(3072), b).unwrap());
    assert!(has(&disk));
    let files = vec![prog(1, 0)];
    let plan = snapshot_of(
        &ModeledKernel::with_processes(config_with_pool(3072), plan_of(&files)).unwrap(),
    );
    assert!(!has(&plan));
}

#[test]
fn a_restored_disk_kernel_checks_its_live_processes_against_their_regions() {
    let files = vec![prog(1, 5000), prog(2, 0)];
    let d = disk::disk(&files, BLOCKS);
    let mut h = harness(d, 3072);
    h.boot();
    let bytes = snapshot_of(&h.k);
    let prefix = disk_prefix(
        &snapshot_of(&ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap()),
        3072,
    );
    let snap = Snap::decode_disk(&bytes, prefix).unwrap();
    assert_eq!(snap.entries, Some(2));
    assert!(!rejects(&snap));
    // A table frame that is not the one the regions need, or a region frame swapped.
    let mut t = snap.clone();
    t.pcbs[1].tables[0] += 1;
    assert!(rejects(&t));
    let mut r = snap.clone();
    let frames = &mut r.pcbs[0].regions[1].2;
    frames.swap(0, 1);
    assert!(rejects(&r));
    // The same frames as level-0 tables of the other slots.
    let mut swapped = snap.clone();
    let tables = &mut swapped.pcbs[1].tables;
    assert!(tables.len() >= 3, "the root and a table per slot");
    tables.swap(1, 2);
    assert!(rejects(&swapped));
    // A page in two segments.
    let mut shared = snap.clone();
    let (va, _, frames) = shared.pcbs[0].regions[0].clone();
    shared.pcbs[0].regions[1].0 = va + 4096 * (frames.len() as u32 - 1);
    assert!(rejects(&shared), "a page in two segments");
    // Permissions no segment can have: data writable but not readable. (A disk kernel
    // keeps no image after boot, so a region's valid permissions are checked for what
    // they can be; which of them a page has is in its PTE in RAM, §6.8.)
    let mut p = snap.clone();
    p.pcbs[0].regions[1].1 &= !0b0010;
    assert!(rejects(&p), "W without R");
    // The component still works after all the rejected restores.
    let mut k = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
    restore_into(&mut k, &bytes).unwrap();
    let _ = k.snapshot_schema_version();
}
