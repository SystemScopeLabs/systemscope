//! Property tests of booting from disk (`docs/m3-design.md` §8.1–§8.3, §17.3). The
//! oracle is `common::disk::expect`: the §8 rules over the pure `systemscope-elf`
//! validators and the independent frame model, never the kernel's boot state machine. The
//! controller is `common::disk::MockBlk`, written from m2-design §9.

mod common;

use std::collections::BTreeMap;

use common::disk::{self, Boot, MockBlk};
use common::layout::*;
use common::procs::*;
use common::{restore_into, snapshot_of};
use proptest::prelude::*;
use systemscope_elf::BLOCK_SIZE;
use systemscope_os::kernel::SHUTDOWN_KIND;
use systemscope_os::procop::{BOOT_KIND, CREATE_KIND};
use systemscope_os::{DiskBoot, ModeledKernel, UserLayout};

const BLOCKS: u32 = 96;

fn boot_config() -> DiskBoot {
    DiskBoot {
        layout: UserLayout::M3,
        capacity_blocks: BLOCKS,
    }
}

/// A random executable: 1 to 4 page-disjoint segments from `USER_BASE`, the first
/// executable, each with random bytes and `.bss`; sometimes a permission §8.3 rejects.
fn program() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::collection::vec(
            (
                0u32..3,
                prop::collection::vec(any::<u8>(), 0..600),
                0u32..9000,
                1u32..8,
            ),
            1..=4,
        ),
        0u32..16,
    )
        .prop_map(|(segs, invalid)| {
            let mut out = Vec::new();
            let mut va = 0x0001_0000u32;
            for (i, (gap, data, bss, flags)) in segs.into_iter().enumerate() {
                va += gap * 0x1000;
                let flags = if i == 0 {
                    PF_R | PF_X
                } else if invalid == 0 {
                    flags
                } else {
                    flags | PF_R
                };
                let memsz = (data.len() as u32 + bss).max(1);
                out.push(Seg::new(va, flags, data, memsz));
                va = (va + memsz).next_multiple_of(0x1000);
            }
            elf32(0x0001_0000, &out)
        })
}

/// Random files: mostly programs, sometimes arbitrary bytes.
fn file() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => program(),
        1 => prop::collection::vec(any::<u8>(), 1..200),
        1 => program().prop_flat_map(|p| {
            let len = p.len();
            (Just(p), 1..len)
        })
        .prop_map(|(p, cut)| p[..cut].to_vec()),
    ]
}

fn files() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(file(), 1..=8).prop_filter("fits the disk", |fs| {
        // Block 0 and every file's blocks.
        fs.iter()
            .map(|f| f.len().div_ceil(BLOCK_SIZE))
            .sum::<usize>()
            < BLOCKS as usize
    })
}

struct Run {
    h: Harness,
}

fn run(d: &[u8], pool: u64, errors: &BTreeMap<usize, u8>, delay: u32) -> Run {
    let mut blk = MockBlk::new(d.to_vec());
    blk.errors = errors.clone();
    blk.delay = delay;
    let mut h = Harness::with_disk(config_with_pool(pool), boot_config(), blk);
    h.boot();
    Run { h }
}

/// The kernel's creates as `(pid, root, error)`, the root 0 on an error.
fn creates(h: &Harness) -> Vec<(u64, u64, String)> {
    h.traced(CREATE_KIND)
        .iter()
        .map(|t| (tu(t, "pid"), tu(t, "root"), ts(t, "error")))
        .collect()
}

fn agree(r: &Run, expected: &Boot) -> Result<(), TestCaseError> {
    let h = &r.h;
    let blk = h.blk.as_ref().unwrap();
    prop_assert!(!blk.in_flight());
    match expected {
        Boot::Fail(detail) => {
            prop_assert!(h.traced(BOOT_KIND).is_empty());
            prop_assert!(creates(h).is_empty());
            let s = h.traced(SHUTDOWN_KIND);
            prop_assert_eq!(s.len(), 1);
            prop_assert_eq!(tu(s[0], "reason"), 1);
            prop_assert!(ts(s[0], "detail").starts_with(detail.as_str()));
            prop_assert_eq!(blk.transfers.len(), 1);
        }
        Boot::Booted {
            entries,
            creates: want,
            transfers,
            first,
        } => {
            prop_assert_eq!(tu(h.traced(BOOT_KIND)[0], "entries"), *entries as u64);
            prop_assert_eq!(&blk.transfers, transfers);
            let want: Vec<(u64, u64, String)> = want
                .iter()
                .enumerate()
                .map(|(i, c)| match c {
                    Ok(root) => (i as u64 + 1, root / 4096, String::new()),
                    Err(e) => (i as u64 + 1, 0, e.clone()),
                })
                .collect();
            prop_assert_eq!(creates(h), want);
            let procs = h.k.processes().unwrap();
            prop_assert_eq!(
                procs.pcbs().len(),
                transfers.len() - 1 - creates(h).iter().filter(|c| !c.2.is_empty()).count()
            );
            prop_assert_eq!(procs.current(), *first);
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any disk of packed files boots exactly as the oracle says.
    #[test]
    fn a_packed_disk_boots_as_the_oracle_says(fs in files()) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &BTreeMap::new(), 0);
        agree(&r, &disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new()))?;
    }

    /// An arbitrary or mutated block 0 is accepted or refused exactly as §8.1 says.
    #[test]
    fn an_arbitrary_table_is_refused_or_booted_as_the_oracle_says(
        fs in files(),
        flips in prop::collection::vec((0usize..BLOCK_SIZE, any::<u8>()), 0..4),
        raw in prop::option::weighted(0.2, prop::collection::vec(any::<u8>(), BLOCK_SIZE)),
    ) {
        let mut d = disk::disk(&fs, BLOCKS);
        match raw {
            Some(bytes) => d[..BLOCK_SIZE].copy_from_slice(&bytes),
            None => for (at, x) in flips {
                d[at] ^= x;
            },
        }
        let r = run(&d, 3072, &BTreeMap::new(), 0);
        agree(&r, &disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new()))?;
    }

    /// Controller errors on any transfers fail the table or exactly their entries.
    #[test]
    fn controller_errors_fail_the_table_or_their_entries(
        fs in files(),
        errors in prop::collection::btree_map(0usize..9, 1u8..8, 0..4),
        delay in 0u32..4,
    ) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &errors, delay);
        agree(&r, &disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &errors))?;
    }

    /// A small pool fails exactly the creations the frame model cannot place, and the
    /// frames it does place are exactly the created processes' needs.
    #[test]
    fn a_small_pool_fails_exactly_the_creations_that_do_not_fit(
        fs in files(),
        pool in 4u64..48,
    ) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, pool, &BTreeMap::new(), 0);
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, pool as usize, &BTreeMap::new());
        agree(&r, &expected)?;
        let procs = r.h.k.processes().unwrap();
        let used: usize = procs.pcbs().iter().map(|p| procs.frames().owned_by(p.pid).len()).sum();
        prop_assert_eq!(procs.frames().free_count(), pool as usize - used);
    }

    /// The controller's latency changes only how many times `STATUS` is read (one read
    /// more per transfer per busy read): the transfers, memory, and processes are the same.
    #[test]
    fn the_poll_count_is_the_only_effect_of_the_controller_latency(
        fs in files(),
        delay in 1u32..6,
    ) {
        let d = disk::disk(&fs, BLOCKS);
        let fast = run(&d, 3072, &BTreeMap::new(), 0);
        let slow = run(&d, 3072, &BTreeMap::new(), delay);
        let (fb, sb) = (fast.h.blk.as_ref().unwrap(), slow.h.blk.as_ref().unwrap());
        prop_assert_eq!(&fb.transfers, &sb.transfers);
        prop_assert_eq!(fb.polls, fb.transfers.len());
        prop_assert_eq!(sb.polls, sb.transfers.len() * (1 + delay as usize));
        prop_assert_eq!(&fast.h.mem, &slow.h.mem);
        prop_assert_eq!(creates(&fast.h), creates(&slow.h));
        // The kernel's transaction counter counts the polls; the processes do not.
        prop_assert_eq!(
            format!("{:?}", fast.h.k.processes()),
            format!("{:?}", slow.h.k.processes())
        );
    }

    /// Every transfer lands in staging, and the kernel reads staging only inside the
    /// file most recently transferred without an error (block 0 while the table is read).
    #[test]
    fn staging_reads_stay_inside_the_current_file(fs in files()) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &BTreeMap::new(), 0);
        let mut limit = 0u64;
        let mut transfers = r.h.blk.as_ref().unwrap().transfers.iter();
        let mut lens: Vec<u64> = vec![BLOCK_SIZE as u64];
        lens.extend(fs.iter().map(|f| f.len() as u64));
        let mut next_len = lens.into_iter();
        for (write, addr, bytes) in &r.h.seen {
            if *write && *addr == BLK_BASE {
                let t = transfers.next().unwrap();
                prop_assert_eq!(u64::from(t.2), STAGING);
                limit = next_len.next().unwrap();
            }
            let in_staging = *addr >= STAGING && *addr < STAGING + STAGING_SIZE;
            if in_staging {
                prop_assert!(!write, "the kernel never writes staging");
                prop_assert!(addr + bytes.len() as u64 <= STAGING + limit);
            }
        }
    }

    /// A restore at any boundary of boot continues exactly as the uninterrupted run.
    #[test]
    fn boot_restored_at_any_boundary_ends_the_same(
        fs in files(),
        errors in prop::collection::btree_map(1usize..9, 1u8..8, 0..2),
        at in any::<prop::sample::Index>(),
    ) {
        let d = disk::disk(&fs, BLOCKS);
        let reference = run(&d, 3072, &errors, 1);
        let boundaries = 2 * reference.h.seen.len();
        let cut = at.index(boundaries);
        let mut blk = MockBlk::new(d.clone());
        blk.errors = errors.clone();
        blk.delay = 1;
        let mut h = Harness::with_disk(config_with_pool(3072), boot_config(), blk);
        h.enter(TRAP_FRAME).unwrap();
        let mut n = 0;
        loop {
            prop_assert!(n < 2 * MAX_OP_ACCESSES, "boot did not end");
            h.issue().unwrap();
            if n == cut {
                let bytes = snapshot_of(&h.k);
                let mut k = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
                restore_into(&mut k, &bytes).unwrap();
                h.k = k;
            }
            n += 1;
            let done = h.respond(false).unwrap();
            if n == cut {
                let bytes = snapshot_of(&h.k);
                let mut k = ModeledKernel::with_disk(config_with_pool(3072), boot_config()).unwrap();
                restore_into(&mut k, &bytes).unwrap();
                h.k = k;
            }
            n += 1;
            if done {
                break;
            }
        }
        prop_assert_eq!(&h.seen, &reference.h.seen);
        prop_assert_eq!(&h.mem, &reference.h.mem);
        prop_assert_eq!(snapshot_of(&h.k), snapshot_of(&reference.h.k));
    }

    /// A created process's pages hold exactly its file's bytes, read back through an
    /// independent Sv32 walk of the tables the kernel wrote.
    #[test]
    fn created_processes_map_their_file_bytes(fs in files()) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &BTreeMap::new(), 0);
        for (pid, root, error) in creates(&r.h) {
            if !error.is_empty() {
                continue;
            }
            let f = &fs[pid as usize - 1];
            let image = systemscope_elf::parse_user_elf32(f, f.len() as u32, UserLayout::M3.image_range()).unwrap();
            for s in &image.segments {
                for p in &s.pages {
                    let kind = if p.perms.execute { Kind::Fetch } else { Kind::Load };
                    let pa = walk(&r.h.mem, root as u32, p.va, kind).unwrap();
                    let page = r.h.mem.read(pa, 4096);
                    let mut want = vec![0u8; 4096];
                    if let Some(c) = p.copy {
                        let (fo, po, len) = (c.file_offset as usize, c.page_offset as usize, c.len as usize);
                        want[po..po + len].copy_from_slice(&f[fo..fo + len]);
                    }
                    prop_assert_eq!(page, want, "pid {} va {:#x}", pid, p.va);
                }
            }
        }
    }
}

/// The frames the frame model gives entry `i`'s process, from the oracle's creations:
/// `needed` frames from its root, since boot never frees a frame.
fn oracle_frames(fs: &[Vec<u8>], expected: &Boot) -> Vec<(u32, Vec<u32>)> {
    let Boot::Booted { creates, .. } = expected else {
        return Vec::new();
    };
    let layout = UserLayout::M3;
    creates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let root = c.as_ref().ok()?;
            let f = &fs[i];
            let image =
                systemscope_elf::parse_user_elf32(f, f.len() as u32, layout.image_range()).unwrap();
            let boot = systemscope_os::BootImage {
                staged: STAGING as u32,
                file_len: f.len() as u32,
                image,
            };
            let first = (root / 4096) as u32;
            let n = common::procs::model::needed(&boot, &layout) as u32;
            Some((i as u32 + 1, (first..first + n).collect()))
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Boot touches only what the kernel is granted: the trap frame, staging (read
    /// only), the frame pool, and the controller's registers; never the UART or `kgate`.
    #[test]
    fn boot_accesses_only_its_grants(
        fs in files(),
        errors in prop::collection::btree_map(0usize..9, 1u8..8, 0..2),
    ) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &errors, 1);
        let frame = u64::from(TRAP_FRAME);
        for (write, addr, bytes) in &r.h.seen {
            let end = addr + bytes.len() as u64;
            let inside = |base: u64, size: u64| *addr >= base && end <= base + size;
            let ok = inside(frame, 0x98)
                || (!write && inside(STAGING, STAGING_SIZE))
                || inside(POOL, POOL_SIZE)
                || inside(BLK_BASE, BLK_SIZE);
            prop_assert!(ok, "{} {addr:#x}+{}", if *write { "write" } else { "read" }, bytes.len());
        }
    }

    /// After boot the trap frame is the first process's initial context (§6.6): `sepc`
    /// at its entry, `satp` Sv32 on its root, `sp` at `STACK_TOP`, every other register,
    /// `gp` included, zero, and action Resume; or, with no process, action Shutdown with
    /// reason 1.
    #[test]
    fn the_frame_after_boot_is_the_first_context_or_shutdown(fs in files()) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, 3072, &BTreeMap::new(), 0);
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, 3072, &BTreeMap::new());
        let h = &r.h;
        match expected {
            Boot::Booted { first: Some(pid), ref creates, .. } => {
                let root = *creates[pid as usize - 1].as_ref().unwrap();
                prop_assert_eq!(h.frame(0x90), 0, "action Resume");
                prop_assert_eq!(h.frame(0x7C), 0x0001_0000, "sepc");
                prop_assert_eq!(h.frame(0x8C), 0x8000_0000 | (root / 4096) as u32, "satp");
                for x in 1..32u64 {
                    let want = if x == 2 { UserLayout::M3.stack_top } else { 0 };
                    prop_assert_eq!(h.frame((x - 1) * 4), want, "x{}", x);
                }
            }
            _ => {
                prop_assert_eq!(h.frame(0x90), 1, "action Shutdown");
                prop_assert_eq!(h.frame(0x94), 1, "reason 1");
            }
        }
    }

    /// Each created process owns exactly the frames the frame model reserves for it,
    /// lowest free first, and nothing else is taken.
    #[test]
    fn every_process_owns_exactly_its_modeled_frames(fs in files(), pool in 8u64..160) {
        let d = disk::disk(&fs, BLOCKS);
        let r = run(&d, pool, &BTreeMap::new(), 0);
        let expected = disk::expect(&d, BLOCKS, STAGING_SIZE as u32, pool as usize, &BTreeMap::new());
        let procs = r.h.k.processes().unwrap();
        let base = (POOL / 4096) as u32;
        let mut used = 0;
        for (pid, frames) in oracle_frames(&fs, &expected) {
            prop_assert_eq!(procs.frames().owned_by(pid), frames.clone(), "pid {}", pid);
            prop_assert!(frames.iter().all(|&f| f >= base && f < base + pool as u32));
            used += frames.len();
        }
        prop_assert_eq!(procs.frames().free_count(), pool as usize - used);
    }

    /// A disk of any capacity is booted against that capacity: the table is refused
    /// exactly when §8.1 refuses it for that many blocks, and nothing is read past it.
    #[test]
    fn the_table_is_checked_against_the_disk_capacity(fs in files(), cut in 1u32..=BLOCKS) {
        let d = disk::disk(&fs, BLOCKS);
        let small = d[..cut as usize * BLOCK_SIZE].to_vec();
        let boot = DiskBoot { layout: UserLayout::M3, capacity_blocks: cut };
        let mut h = Harness::with_disk(config_with_pool(3072), boot, MockBlk::new(small.clone()));
        h.boot();
        let expected = disk::expect(&small, cut, STAGING_SIZE as u32, 3072, &BTreeMap::new());
        agree(&Run { h }, &expected)?;
    }
}
