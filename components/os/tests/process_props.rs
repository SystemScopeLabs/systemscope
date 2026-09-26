//! Properties of the process model (`docs/m3-design.md` §6.4, §6.6, §6.8) over random
//! programs, pools, and trap sequences, checked against an independent oracle of the
//! scheduler and the frame allocator written from the design text: PIDs in image order,
//! lowest-free-first all-or-nothing reservation, a FIFO queue, the ecall switch and the
//! fault kill of §17 M3.4b.

mod common;

use std::collections::VecDeque;

use common::layout::*;
use common::procs::ksnap::Snap;
use common::procs::*;
use common::{MockCtx, restore_into, snapshot_of};
use proptest::prelude::*;
use systemscope_contracts::component::Component;
use systemscope_contracts::event::Phase;
use systemscope_contracts::snapshot::SnapshotReader;
use systemscope_os::frames::Frames;
use systemscope_os::kernel::{ISSUE, SNAPSHOT_SCHEMA};
use systemscope_os::process::ProcState;
use systemscope_os::{ModeledKernel, UserLayout};

const POOL_PPN: u32 = (POOL / 4096) as u32;
const FAULT_CAUSES: [u32; 11] = [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 15];

/// A random two-segment program: 1–4 text words, up to 40 data bytes, up to 6000 bytes
/// of `.bss`.
fn program() -> impl Strategy<Value = Vec<u8>> {
    (
        proptest::collection::vec(any::<u32>(), 1..=4),
        proptest::collection::vec(any::<u8>(), 0..=40),
        0u32..6000,
    )
        .prop_map(|(t, d, b)| two_segment(&t, &d, b))
}

fn programs(n: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<Vec<u8>>> {
    proptest::collection::vec(program(), n)
}

/// A trap of the running process: an ecall (the M3.4b switch) or a fault.
#[derive(Clone, Copy, Debug)]
enum Trap {
    Yield,
    Fault(u32),
}

fn traps() -> impl Strategy<Value = Vec<Trap>> {
    proptest::collection::vec(
        prop_oneof![
            3 => Just(Trap::Yield),
            1 => proptest::sample::select(&FAULT_CAUSES[..]).prop_map(Trap::Fault),
        ],
        0..12,
    )
}

/// The oracle: the scheduler state after boot and each trap, from the design text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Oracle {
    /// `(pid, state)`: 'R'eady, 'U' running, 'F'aulted.
    states: Vec<(u32, char)>,
    queue: VecDeque<u32>,
    current: Option<u32>,
    /// Pool owners, index = pool frame.
    owners: Vec<u32>,
    down: bool,
}

impl Oracle {
    fn boot(files: &[Vec<u8>], pool: usize) -> Oracle {
        let plan = plan_of(files);
        let owners = model::boot_owners(&plan.images, &UserLayout::M3, pool);
        let mut states = Vec::new();
        let mut queue = VecDeque::new();
        for pid in 1..=files.len() as u32 {
            if owners.contains(&pid) {
                states.push((pid, 'R'));
                queue.push_back(pid);
            }
        }
        let mut o = Oracle {
            states,
            queue,
            current: None,
            owners,
            down: false,
        };
        o.next();
        o
    }

    fn set(&mut self, pid: u32, s: char) {
        self.states.iter_mut().find(|p| p.0 == pid).unwrap().1 = s;
    }

    fn next(&mut self) {
        match self.queue.pop_front() {
            Some(pid) => {
                self.set(pid, 'U');
                self.current = Some(pid);
            }
            None => self.down = true,
        }
    }

    fn trap(&mut self, t: Trap) {
        let pid = self.current.take().unwrap();
        match t {
            Trap::Yield => {
                self.set(pid, 'R');
                self.queue.push_back(pid);
            }
            Trap::Fault(_) => {
                self.set(pid, 'F');
                for o in self.owners.iter_mut().filter(|o| **o == pid) {
                    *o = 0;
                }
            }
        }
        self.next();
    }
}

/// The kernel's scheduler state in the oracle's terms.
fn observed(h: &Harness) -> Oracle {
    let p = h.k.processes().unwrap();
    let states = p
        .pcbs()
        .iter()
        .map(|pcb| {
            let c = match pcb.state {
                ProcState::Ready => 'R',
                ProcState::Running => 'U',
                ProcState::Faulted { .. } => 'F',
                ProcState::Exited { .. } => 'E',
            };
            (pcb.pid, c)
        })
        .collect();
    let owners = (0..p.frames().count())
        .map(|i| p.frames().owner(POOL_PPN + i as u32).unwrap())
        .collect();
    let down =
        h.k.inspect().get("life") == Some(&systemscope_contracts::trace::Value::Str("down".into()));
    Oracle {
        states,
        queue: p.queue().clone(),
        current: p.current(),
        owners,
        down,
    }
}

fn apply(h: &mut Harness, t: Trap, n: u32) {
    let (cause, stval) = match t {
        Trap::Yield => (8, 0),
        Trap::Fault(c) => (c, 0x1000 * n),
    };
    assert_eq!(
        h.trap(cause, 0x0001_0000 + 4 * n, stval, 0, n),
        Stop::Released
    );
}

/// Runs boot and `traps` (stopping at shutdown), checking `each` after every operation.
fn run(
    files: &[Vec<u8>],
    pool: usize,
    traps: &[Trap],
    mut each: impl FnMut(&Harness, &Oracle),
) -> Harness {
    let mut h = Harness::new(config_with_pool(pool as u64), files, plan_of(files));
    h.boot();
    let mut o = Oracle::boot(files, pool);
    each(&h, &o);
    for (n, &t) in traps.iter().enumerate() {
        if o.down {
            break;
        }
        apply(&mut h, t, n as u32);
        o.trap(t);
        each(&h, &o);
    }
    h
}

fn own_restore(k: &mut ModeledKernel, bytes: &[u8]) -> Result<bool, ()> {
    let mut r = SnapshotReader::new(bytes);
    k.restore(&mut r, SNAPSHOT_SCHEMA).map_err(|_| ())?;
    Ok(r.finish().is_ok())
}

fn fresh(files: &[Vec<u8>], pool: usize) -> ModeledKernel {
    ModeledKernel::with_processes(config_with_pool(pool as u64), plan_of(files)).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// 1. Arbitrary bytes, and bytes after a valid process-mode prefix, never panic a
    /// restore, and a rejection changes nothing.
    #[test]
    fn arbitrary_snapshot_bytes_never_panic(
        tail in proptest::collection::vec(any::<u8>(), 0..400),
        with_prefix in any::<bool>(),
    ) {
        let files = [two_segment(&[0x13], b"x", 0)];
        let mut k = fresh(&files, 32);
        let before = snapshot_of(&k);
        let mut bytes = if with_prefix {
            before[..ksnap::prefix_len(&before, 32)].to_vec()
        } else {
            Vec::new()
        };
        bytes.extend(tail);
        if own_restore(&mut k, &bytes).is_err() {
            prop_assert_eq!(snapshot_of(&k), before);
        }
    }

    /// 2. Every table a run reaches round-trips: restore into a fresh kernel snapshots
    /// to the same bytes, and the independent codec re-encodes them exactly.
    #[test]
    fn valid_tables_round_trip(files in programs(1..=3), traps in traps()) {
        let pool = 64;
        let prefix = ksnap::prefix_len(&snapshot_of(&fresh(&files, pool)), pool);
        let mut snaps = Vec::new();
        run(&files, pool, &traps, |h, _| snaps.push(snapshot_of(&h.k)));
        for s in snaps {
            let mut k = fresh(&files, pool);
            restore_into(&mut k, &s).unwrap();
            prop_assert_eq!(&snapshot_of(&k), &s);
            prop_assert_eq!(Snap::decode(&s, prefix).unwrap().encode(), s);
        }
    }

    /// 3. The allocator never hands a frame out twice and matches a lowest-free-first,
    /// all-or-nothing model over random reserve and release sequences.
    #[test]
    fn no_double_allocation(
        size in 1u64..48,
        ops in proptest::collection::vec((1u32..6, 0usize..12, any::<bool>()), 0..40),
    ) {
        let mut f = Frames::new(POOL, size * 4096);
        let mut model = vec![0u32; size as usize];
        for (pid, n, release) in ops {
            if release {
                let mine: Vec<u32> = (0..size as u32)
                    .filter(|&i| model[i as usize] == pid)
                    .map(|i| POOL_PPN + i)
                    .collect();
                // A release with one frame it does not own frees nothing at all.
                if let Some(other) = (0..size as u32).find(|&i| model[i as usize] != pid) {
                    let before = f.clone();
                    let mut bad = mine.clone();
                    bad.push(POOL_PPN + other);
                    prop_assert!(f.release(pid, &bad).is_err());
                    prop_assert_eq!(&f, &before);
                }
                prop_assert!(f.release(pid, &mine).is_ok());
                for o in model.iter_mut().filter(|o| **o == pid) {
                    *o = 0;
                }
                // Releasing a frame it does not own fails and changes nothing.
                if let Some(other) = (0..size as u32).find(|&i| model[i as usize] != 0) {
                    let before = f.clone();
                    prop_assert!(f.release(pid, &[POOL_PPN + other]).is_err());
                    prop_assert_eq!(&f, &before);
                }
            } else {
                let free: Vec<usize> = (0..size as usize).filter(|&i| model[i] == 0).take(n).collect();
                let got = f.reserve(pid, n);
                if free.len() == n {
                    let want: Vec<u32> = free.iter().map(|&i| POOL_PPN + i as u32).collect();
                    prop_assert_eq!(got, Some(want));
                    for i in free {
                        model[i] = pid;
                    }
                } else {
                    prop_assert_eq!(got, None);
                }
            }
            for i in 0..size as u32 {
                prop_assert_eq!(f.owner(POOL_PPN + i), Some(model[i as usize]));
            }
        }
    }

    /// 4. No frame outside the pool is ever allocated or written: not the firmware, the
    /// trap frame's page, staging, MMIO, or the kernel gate. The kernel writes only pool
    /// frames and the trap frame, and reads only staging and the trap frame.
    #[test]
    fn reserved_frames_are_never_allocated(files in programs(1..=3), pool in 8usize..40, traps in traps()) {
        let h = run(&files, pool, &traps, |h, _| {
            let p = h.k.processes().unwrap();
            for pcb in p.pcbs() {
                for ppn in pcb.frames() {
                    assert!((POOL_PPN..POOL_PPN + pool as u32).contains(&ppn));
                }
            }
        });
        let pool_end = POOL + 4096 * pool as u64;
        let frame = u64::from(TRAP_FRAME)..u64::from(TRAP_FRAME) + 0x98;
        for (w, addr, bytes) in &h.seen {
            let end = addr + bytes.len() as u64;
            let in_pool = *addr >= POOL && end <= pool_end;
            let in_frame = frame.contains(addr) && end <= frame.end;
            let in_staging = *addr >= STAGING && end <= STAGING + STAGING_SIZE;
            if *w {
                prop_assert!(in_pool || in_frame, "write {:#x}", addr);
            } else {
                prop_assert!(in_staging || in_frame, "read {:#x}", addr);
            }
            prop_assert!(!(UART_BASE..KGATE_BASE + KGATE_SIZE).contains(addr));
        }
    }

    /// 5. The run queue is FIFO: after boot and every trap it is exactly the oracle's.
    #[test]
    fn the_queue_is_fifo(files in programs(1..=4), traps in traps()) {
        run(&files, 64, &traps, |h, o| assert_eq!(observed(h).queue, o.queue));
    }

    /// 6. No PID is ever queued twice, and only Ready processes are queued.
    #[test]
    fn queue_entries_are_unique(files in programs(1..=4), traps in traps()) {
        run(&files, 64, &traps, |h, _| {
            let p = h.k.processes().unwrap();
            let q: Vec<u32> = p.queue().iter().copied().collect();
            let mut d = q.clone();
            d.sort();
            d.dedup();
            assert_eq!(d.len(), q.len());
            for pid in q {
                assert_eq!(p.pcb(pid).unwrap().state, ProcState::Ready);
            }
        });
    }

    /// 7. At most one process runs, and it is exactly the current PID, which holds no
    /// saved context; every Ready process holds one.
    #[test]
    fn current_is_the_one_running_process(files in programs(1..=4), traps in traps()) {
        run(&files, 64, &traps, |h, o| {
            let p = h.k.processes().unwrap();
            let running: Vec<u32> = p.pcbs().iter().filter(|x| x.state == ProcState::Running).map(|x| x.pid).collect();
            assert!(running.len() <= 1);
            assert_eq!(running.first().copied(), p.current());
            assert_eq!(p.current(), o.current);
            for pcb in p.pcbs() {
                assert_eq!(pcb.context.is_some(), pcb.state == ProcState::Ready);
            }
        });
    }

    /// 8. Random creation (boot with random sizes and pools), switch, and fault sequences
    /// keep frame ownership exactly the oracle's: a fault frees exactly the faulted
    /// process's frames, and a shutdown with nothing left has every frame free.
    #[test]
    fn ownership_survives_create_switch_fault(files in programs(1..=4), pool in 8usize..48, traps in traps()) {
        run(&files, pool, &traps, |h, o| {
            let seen = observed(h);
            assert_eq!(&seen, o);
            let p = h.k.processes().unwrap();
            for pcb in p.pcbs() {
                let mut mine = pcb.frames();
                mine.sort();
                assert_eq!(mine, p.frames().owned_by(pcb.pid));
            }
            if o.down {
                assert!(seen.owners.iter().all(|&x| x == 0));
            }
        });
    }

    /// 9. Two processes with data at the same virtual addresses see only their own
    /// bytes through their own roots, in disjoint frames, by an independent walk.
    #[test]
    fn same_va_is_isolated(
        a in proptest::collection::vec(any::<u8>(), 1..=40),
        b in proptest::collection::vec(any::<u8>(), 1..=40),
        ta in any::<u32>(),
        tb in any::<u32>(),
    ) {
        let files = [two_segment(&[ta], &a, 16), two_segment(&[tb], &b, 16)];
        let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
        h.boot();
        let p = h.k.processes().unwrap();
        let (ra, rb) = (p.pcb(1).unwrap().root, p.pcb(2).unwrap().root);
        let da = walk(&h.mem, ra, 0x0001_1000, Kind::Load).unwrap();
        let db = walk(&h.mem, rb, 0x0001_1000, Kind::Load).unwrap();
        prop_assert_ne!(da >> 12, db >> 12);
        prop_assert_eq!(h.mem.read(da, a.len()), a.clone());
        prop_assert_eq!(h.mem.read(db, b.len()), b.clone());
        prop_assert_eq!(h.mem.word(walk(&h.mem, ra, 0x0001_0000, Kind::Fetch).unwrap()), ta);
        prop_assert_eq!(h.mem.word(walk(&h.mem, rb, 0x0001_0000, Kind::Fetch).unwrap()), tb);
        let fa = p.pcb(1).unwrap().frames();
        prop_assert!(p.pcb(2).unwrap().frames().iter().all(|f| !fa.contains(f)));
        // A's pages are not reachable from B's root: every B mapping lands in B's frames.
        for va in [0x0001_0000u32, 0x0001_1000, 0x7FFF_E000] {
            let pb = walk(&h.mem, rb, va, Kind::Load).unwrap();
            prop_assert!(!fa.contains(&((pb >> 12) as u32)));
        }
    }

    /// 10. A restore of a one-byte mutation of a reached snapshot either restores bytes
    /// that snapshot back exactly, or fails and leaves the kernel as it was.
    #[test]
    fn a_failed_restore_is_atomic(
        files in programs(1..=2),
        traps in traps(),
        at in any::<prop::sample::Index>(),
        xor in 1u8..=255,
        into_used in any::<bool>(),
    ) {
        let pool = 48;
        let h = run(&files, pool, &traps, |_, _| {});
        let mut bytes = snapshot_of(&h.k);
        let i = at.index(bytes.len());
        bytes[i] ^= xor;
        let mut k = if into_used {
            let mut u = Harness::new(config_with_pool(pool as u64), &files, plan_of(&files));
            u.boot();
            u.k
        } else {
            fresh(&files, pool)
        };
        let before = snapshot_of(&k);
        match own_restore(&mut k, &bytes) {
            Ok(true) => prop_assert_eq!(snapshot_of(&k), bytes),
            Ok(false) => {}
            Err(()) => prop_assert_eq!(snapshot_of(&k), before),
        }
    }

    /// 11. At any `Wait` of a run, the snapshot restored into a fresh kernel takes a wake
    /// as a fault and sends nothing, and answering the outstanding request continues the
    /// run exactly: the restored kernel never reissues, reallocates, or requeues.
    #[test]
    fn a_restored_wait_never_reissues(files in programs(1..=2), traps in traps(), pick in any::<prop::sample::Index>()) {
        let pool = 48;
        let clean = run(&files, pool, &traps, |_, _| {});
        let n = clean.seen.len();
        let cut = pick.index(n);
        // Replay to the cut'th request, snapshot in Wait, and finish in a fresh kernel.
        let mut h = Harness::new(config_with_pool(pool as u64), &files, plan_of(&files));
        let mut count = 0usize;
        let mut ops: Vec<Option<Trap>> = vec![None];
        ops.extend(traps.iter().copied().map(Some));
        let mut o = Oracle::boot(&files, pool);
        let mut first = true;
        'outer: for (idx, t) in ops.into_iter().enumerate() {
            if let Some(t) = t {
                if o.down {
                    break;
                }
                let n = idx as u32 - 1;
                let (cause, stval) = match t { Trap::Yield => (8, 0), Trap::Fault(c) => (c, 0x1000 * n) };
                let f = u64::from(TRAP_FRAME);
                for i in 1..=31u32 {
                    h.mem.set_word(f + 4 * u64::from(i - 1), 0x100 * i + n);
                }
                h.mem.set_word(f + 0x7C, 0x0001_0000 + 4 * n);
                h.mem.set_word(f + 0x80, 0);
                h.mem.set_word(f + 0x84, cause);
                h.mem.set_word(f + 0x88, stval);
                o.trap(t);
            } else if !first {
                continue;
            }
            first = false;
            h.enter(TRAP_FRAME).unwrap();
            loop {
                h.issue().unwrap();
                if count == cut {
                    let bytes = snapshot_of(&h.k);
                    let mut probe = fresh(&files, pool);
                    restore_into(&mut probe, &bytes).unwrap();
                    let mut ctx = MockCtx::new();
                    prop_assert!(ctx.wake(&mut probe, ISSUE, Phase::Request).is_err());
                    prop_assert!(ctx.sent.is_empty());
                    let mut again = fresh(&files, pool);
                    restore_into(&mut again, &bytes).unwrap();
                    h.k = again;
                }
                count += 1;
                if h.respond(false).unwrap() {
                    continue 'outer;
                }
            }
        }
        prop_assert_eq!(&h.seen, &clean.seen);
        prop_assert_eq!(snapshot_of(&h.k), snapshot_of(&clean.k));
        prop_assert_eq!(&h.mem, &clean.mem);
    }

    /// 12. Creation, switching, and faulting are deterministic: two runs of the same
    /// inputs make the same accesses, traces, memory, and snapshots.
    #[test]
    fn creation_is_deterministic(files in programs(1..=3), pool in 8usize..40, traps in traps()) {
        let mut s1 = Vec::new();
        let mut s2 = Vec::new();
        let a = run(&files, pool, &traps, |h, _| s1.push(snapshot_of(&h.k)));
        let b = run(&files, pool, &traps, |h, _| s2.push(snapshot_of(&h.k)));
        prop_assert_eq!(s1, s2);
        prop_assert_eq!(&a.seen, &b.seen);
        prop_assert_eq!(&a.mem, &b.mem);
        prop_assert_eq!(format!("{:?}", a.ctx.traced), format!("{:?}", b.ctx.traced));
    }
}

#[test]
fn the_oracle_agrees_on_a_known_case() {
    // A sanity anchor for the oracle itself: 3 images, a fault of the first, two yields.
    let files = [
        two_segment(&[0x13], b"a", 0),
        two_segment(&[0x13], b"b", 0),
        two_segment(&[0x13], b"c", 0),
    ];
    let mut o = Oracle::boot(&files, 64);
    assert_eq!(
        (o.current, o.queue.clone()),
        (Some(1), VecDeque::from([2, 3]))
    );
    o.trap(Trap::Fault(13));
    o.trap(Trap::Yield);
    assert_eq!((o.current, o.queue.clone()), (Some(3), VecDeque::from([2])));
    assert_eq!(o.owners.iter().filter(|&&x| x == 1).count(), 0);
}
