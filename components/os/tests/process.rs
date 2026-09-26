//! The process model driven through `MockCtx` (`docs/m3-design.md` §6.4, §6.6, §6.7,
//! §6.8, §8.3, §17 M3.4b): boot from staged, validated user images; frame reservation and
//! OOM; the address spaces the kernel writes, read back with an independent Sv32 walk;
//! FIFO switching and the fault kill through the trap frame; bus faults at every kind of
//! step; and the snapshot: its contents, every restore rejection, and restore at every
//! step boundary without a reissue.

mod common;

use common::layout::*;
use common::procs::config;
use common::procs::ksnap::{self, Snap, Stage};
use common::procs::*;
use common::{restore_into, snapshot_of};
use systemscope_contracts::component::Component;
use systemscope_contracts::event::Phase;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::Value;
use systemscope_os::kernel::{ENTER_KIND, ISSUE, RELEASE_KIND, SHUTDOWN_KIND};
use systemscope_os::process::ProcState;
use systemscope_os::procop::{BOOT_KIND, CREATE_KIND, FAULT_KIND, SEGMENT_KIND, SWITCH_KIND};
use systemscope_os::{ModeledKernel, PlanError, ProcessPlan, UserLayout};

const STACK_TOP: u32 = 0x7FFF_F000;
const POOL_PPN: u32 = (POOL / 4096) as u32;

/// Program A: three text words, 8 data bytes and 5000 bytes of `.bss`, so its data
/// segment spans two pages.
fn prog_a() -> Vec<u8> {
    two_segment(&[0x0000_0013, 0x0010_0073, 0x1234_5678], b"AAAAaaaa", 5000)
}

/// Program B: two text words and 4 data bytes.
fn prog_b() -> Vec<u8> {
    two_segment(&[0x0000_0073, 0x0000_0013], b"BBBB", 12)
}

/// Program C: one text word and 4 data bytes.
fn prog_c() -> Vec<u8> {
    two_segment(&[0x0000_0013], b"CCCC", 0)
}

fn harness(files: &[Vec<u8>]) -> Harness {
    Harness::new(config(), files, plan_of(files))
}

fn procs(h: &Harness) -> &systemscope_os::process::Processes {
    h.k.processes().unwrap()
}

fn root(h: &Harness, pid: u32) -> u32 {
    procs(h).pcb(pid).unwrap().root
}

fn state(h: &Harness, pid: u32) -> ProcState {
    procs(h).pcb(pid).unwrap().state
}

fn queue(h: &Harness) -> Vec<u32> {
    procs(h).queue().iter().copied().collect()
}

fn kinds(h: &Harness) -> Vec<&'static str> {
    h.ctx.traced.iter().map(|t| t.0).collect()
}

/// The frame's context words `x1`..`x31`, `sepc`, `sstatus`.
fn frame_context(h: &Harness) -> Vec<u32> {
    (0..33).map(|i| h.frame(4 * i)).collect()
}

#[test]
fn boot_creates_the_images_in_order_and_dispatches_the_first() {
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    let plan = plan_of(&files);
    h.boot();
    assert_eq!(
        kinds(&h),
        [
            ENTER_KIND,
            BOOT_KIND,
            SEGMENT_KIND,
            SEGMENT_KIND,
            CREATE_KIND,
            SEGMENT_KIND,
            SEGMENT_KIND,
            CREATE_KIND,
            SWITCH_KIND,
            RELEASE_KIND
        ]
    );
    assert_eq!(ts(&h.ctx.traced[0], "op"), "boot");
    assert_eq!(tu(&h.ctx.traced[1], "entries"), 2);
    let creates = h.traced(CREATE_KIND);
    for (i, c) in creates.iter().enumerate() {
        assert_eq!(tu(c, "pid"), i as u64 + 1);
        assert_eq!(tu(c, "entry"), 0x0001_0000);
        assert_eq!(ts(c, "error"), "");
        assert_eq!(tu(c, "root"), u64::from(root(&h, i as u32 + 1)));
    }
    let segs = h.traced(SEGMENT_KIND);
    assert_eq!(
        segs.iter()
            .map(|s| (tu(s, "pid"), tu(s, "va"), tu(s, "memsz"), ts(s, "perms")))
            .collect::<Vec<_>>(),
        [
            (1, 0x10000, 12, "r-x".to_owned()),
            (1, 0x11000, 5008, "rw-".to_owned()),
            (2, 0x10000, 8, "r-x".to_owned()),
            (2, 0x11000, 16, "rw-".to_owned()),
        ]
    );
    let sw = h.traced(SWITCH_KIND);
    assert_eq!((tu(sw[0], "from"), tu(sw[0], "to")), (0, 1));

    // PIDs, states, queue, and frames, against the independent reservation model.
    assert_eq!(state(&h, 1), ProcState::Running);
    assert_eq!(state(&h, 2), ProcState::Ready);
    assert_eq!(procs(&h).current(), Some(1));
    assert_eq!(queue(&h), [2]);
    let layout = UserLayout::M3;
    let owners = model::boot_owners(&plan.images, &layout, 3072);
    for (f, &o) in owners.iter().enumerate() {
        assert_eq!(
            procs(&h).frames().owner(POOL_PPN + f as u32),
            Some(o),
            "frame {f}"
        );
    }
    assert_eq!(model::needed(&plan.images[0], &layout), 1 + 2 + 3 + 4);
    assert_eq!(root(&h, 1), POOL_PPN);
    assert_eq!(root(&h, 2), POOL_PPN + 10);

    // The trap frame holds A's initial context (§6.6).
    let mut want = vec![0u32; 33];
    want[1] = STACK_TOP;
    want[31] = 0x0001_0000;
    assert_eq!(frame_context(&h), want);
    assert_eq!(h.frame(0x8C), 0x8000_0000 | root(&h, 1));
    assert_eq!(h.frame(0x90), 0, "action = Resume");
}

#[test]
fn the_address_spaces_are_what_the_images_say_by_an_independent_walk() {
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    h.boot();
    let mem = &h.mem;
    for (pid, file) in [(1u32, &files[0]), (2, &files[1])] {
        let r = root(&h, pid);
        let img = &plan_of(&files).images[pid as usize - 1];
        // Text: fetchable and readable, not writable; its bytes are the file's.
        let text = walk(mem, r, 0x0001_0000, Kind::Fetch).unwrap();
        assert_eq!(walk(mem, r, 0x0001_0000, Kind::Load), Some(text));
        assert_eq!(walk(mem, r, 0x0001_0000, Kind::Store), None);
        let c = img.image.segments[0].pages[0].copy.unwrap();
        assert_eq!(
            mem.read(text, c.len as usize),
            file[c.file_offset as usize..(c.file_offset + c.len) as usize]
        );
        assert!(
            mem.read(text + u64::from(c.len), 4096 - c.len as usize)
                .iter()
                .all(|&b| b == 0)
        );
        // Data: readable and writable, not fetchable; file bytes then zero .bss.
        let data = walk(mem, r, 0x0001_1000, Kind::Store).unwrap();
        assert_eq!(walk(mem, r, 0x0001_1000, Kind::Fetch), None);
        let dc = img.image.segments[1].pages[0].copy.unwrap();
        assert_eq!(
            mem.read(data, dc.len as usize),
            file[dc.file_offset as usize..(dc.file_offset + dc.len) as usize]
        );
        assert!(
            mem.read(data + u64::from(dc.len), 64)
                .iter()
                .all(|&b| b == 0)
        );
        // The stack: STACK_PAGES read-write pages ending at STACK_TOP, and nothing above.
        for i in 1..=4u32 {
            let va = STACK_TOP - 4096 * i;
            assert!(walk(mem, r, va, Kind::Store).is_some(), "stack page {i}");
            assert_eq!(walk(mem, r, va, Kind::Fetch), None);
        }
        assert_eq!(walk(mem, r, STACK_TOP, Kind::Load), None);
        assert_eq!(walk(mem, r, STACK_TOP - 5 * 4096, Kind::Load), None);
        // The megapages are there, but U = 0 keeps U-mode out (§6.4).
        assert_eq!(l1_entry(mem, r, 0x8000_0000), 0x2000_0000 | 0xEF);
        assert_eq!(l1_entry(mem, r, 0x1000_0000), 0x0400_0000 | 0xE7);
        assert_eq!(walk(mem, r, 0x8001_0000, Kind::Load), None);
        assert_eq!(walk(mem, r, 0x1000_0000, Kind::Store), None);
        // Nothing else is mapped.
        assert_eq!(walk(mem, r, 0x0000_F000, Kind::Load), None);
        assert_eq!(walk(mem, r, 0x0001_3000, Kind::Load), None);
    }
    // A's .bss reaches the second data page.
    let r = root(&h, 1);
    let second = walk(mem, r, 0x0001_2000, Kind::Store).unwrap();
    assert!(mem.read(second, 4096).iter().all(|&b| b == 0));
    // Same VA, different frames: nothing aliases.
    for va in [0x0001_0000, 0x0001_1000, STACK_TOP - 4096] {
        let a = walk(mem, root(&h, 1), va, Kind::Load).unwrap();
        let b = walk(mem, root(&h, 2), va, Kind::Load).unwrap();
        assert_ne!(a >> 12, b >> 12, "{va:#x}");
    }
}

/// The level-0 PTE mapping `va` under `root`, read from memory directly.
fn leaf(mem: &PageMem, root: u32, va: u32) -> u32 {
    let l1 = l1_entry(mem, root, va);
    assert_eq!(l1 & 0xF, 1, "a pointer: V only");
    mem.word(u64::from(l1 >> 10) * 4096 + 4 * u64::from(va >> 12 & 0x3FF))
}

#[test]
fn every_pte_has_exactly_the_frozen_bits() {
    // §6.4, §8.3: user leaves are V | U | A with the segment's R/W/X and D = W; pointers
    // are V only; the megapages are fixed. Nothing else is set.
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    h.boot();
    for pid in [1, 2] {
        let r = root(&h, pid);
        let pcb = procs(&h).pcb(pid).unwrap().clone();
        for region in &pcb.regions {
            let flags = match (region.perms.read, region.perms.write, region.perms.execute) {
                (true, false, true) => 0x01 | 0x02 | 0x08 | 0x10 | 0x40,
                (true, true, false) => 0x01 | 0x02 | 0x04 | 0x10 | 0x40 | 0x80,
                other => panic!("{other:?}"),
            };
            for (i, &frame) in region.frames.iter().enumerate() {
                let va = region.va + 4096 * i as u32;
                assert_eq!(
                    leaf(&h.mem, r, va),
                    frame << 10 | flags,
                    "pid {pid} va {va:#x}"
                );
            }
        }
        // Every other level-1 entry is empty.
        let used = [0u32, 0x80000000 >> 22, 0x1000_0000 >> 22, STACK_TOP >> 22];
        for slot in 0..1024u32 {
            if !used.contains(&slot) {
                assert_eq!(l1_entry(&h.mem, r, slot << 22), 0, "slot {slot}");
            }
        }
    }
}

#[test]
fn creation_accesses_are_whitelisted_ordered_and_zero_every_frame() {
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    h.boot();
    let pool = POOL..POOL + POOL_SIZE;
    let frame = u64::from(TRAP_FRAME)..u64::from(TRAP_FRAME) + 0x98;
    let staging = STAGING..STAGING + STAGING_SIZE;
    for (write, addr, bytes) in &h.seen {
        let end = addr + bytes.len() as u64;
        assert!(bytes.len() <= 16 && addr / 4096 == (end - 1) / 4096);
        if *write {
            assert!(pool.contains(addr) || frame.contains(addr), "{addr:#x}");
        } else {
            assert!(staging.contains(addr), "{addr:#x}");
        }
    }
    // Every frame of both processes is written whole with zeros first.
    for pid in [1, 2] {
        for ppn in procs(&h).pcb(pid).unwrap().frames() {
            let base = u64::from(ppn) * 4096;
            let zeros: Vec<u64> = h
                .seen
                .iter()
                .filter(|(w, a, b)| {
                    *w && (base..base + 4096).contains(a)
                        && b.iter().all(|&x| x == 0)
                        && b.len() == 16
                })
                .map(|s| s.1)
                .collect();
            assert!(zeros.len() >= 256, "frame {ppn:#x}");
        }
    }
    // Frames are written before the PTEs that point at them; tables before their
    // pointers.
    let tables: Vec<u32> = [1, 2]
        .iter()
        .flat_map(|&p| procs(&h).pcb(p).unwrap().tables.clone())
        .collect();
    for (i, (w, addr, bytes)) in h.seen.iter().enumerate() {
        let in_table = tables.contains(&((addr / 4096) as u32));
        if !*w || !in_table || bytes.len() != 4 {
            continue;
        }
        let pte = u32::from_le_bytes(bytes[..].try_into().unwrap());
        if pte & 1 == 0 || pte & 0x20 != 0 {
            continue; // not a PTE write, or a global megapage
        }
        let target = u64::from(pte >> 10) * 4096;
        let last_write = h
            .seen
            .iter()
            .rposition(|(w, a, _)| *w && (target..target + 4096).contains(a))
            .unwrap();
        assert!(last_write < i, "PTE at {addr:#x} written before its target");
    }
}

#[test]
fn an_unaligned_staging_address_copies_exactly_without_crossing_pages() {
    // Staged so that the text's file bytes (at file offset 0x74) start 6 bytes below a
    // staging page end: the 12-byte text copy's source and destination page offsets
    // differ, it splits at the staging page boundary, and the bytes land exactly.
    let files = [prog_a()];
    let plan = ProcessPlan {
        layout: UserLayout::M3,
        images: vec![boot_image(&files[0], STAGING as u32 + 0xF86)],
    };
    let c = plan.images[0].image.segments[0].pages[0].copy.unwrap();
    assert_eq!((plan.images[0].staged + c.file_offset) % 4096, 4096 - 6);
    let mut h = Harness::new(config(), &files, plan.clone());
    h.boot();
    for (_, addr, bytes) in &h.seen {
        let end = addr + bytes.len() as u64;
        assert!(
            bytes.len() <= 16 && addr / 4096 == (end - 1) / 4096,
            "{addr:#x}"
        );
    }
    let r = root(&h, 1);
    for seg in &plan.images[0].image.segments {
        for page in &seg.pages {
            let Some(c) = page.copy else { continue };
            let pa = walk(&h.mem, r, page.va, Kind::Load).unwrap();
            assert_eq!(
                h.mem.read(pa + u64::from(c.page_offset), c.len as usize),
                files[0][c.file_offset as usize..(c.file_offset + c.len) as usize]
            );
        }
    }
}

#[test]
fn switching_is_fifo_through_the_trap_frame_and_contexts_move_not_copy() {
    let files = [prog_a(), prog_b(), prog_c()];
    let mut h = harness(&files);
    h.boot();
    assert_eq!(queue(&h), [2, 3]);
    // A traps with an ecall: its frame becomes its context with sepc + 4.
    assert_eq!(h.trap(8, 0x0001_0004, 0, 0x20, 0xA), Stop::Released);
    let a = procs(&h).pcb(1).unwrap().context.unwrap();
    assert_eq!(a.pc, 0x0001_0008);
    assert_eq!(a.sstatus, 0x20);
    assert_eq!(a.regs[4], 0x500 + 0xA);
    assert_eq!(state(&h, 1), ProcState::Ready);
    assert_eq!(state(&h, 2), ProcState::Running);
    assert!(
        procs(&h).pcb(2).unwrap().context.is_none(),
        "a running process has no saved context"
    );
    assert_eq!(queue(&h), [3, 1]);
    // B's initial context is in the frame, under B's root.
    assert_eq!(h.frame(0x04), STACK_TOP);
    assert_eq!(h.frame(0x00), 0);
    assert_eq!(h.frame(0x7C), 0x0001_0000);
    assert_eq!(h.frame(0x8C), 0x8000_0000 | root(&h, 2));
    // B → C → A.
    assert_eq!(h.trap(8, 0x0001_0000, 0, 0, 0xB), Stop::Released);
    assert_eq!(procs(&h).current(), Some(3));
    assert_eq!(queue(&h), [1, 2]);
    assert_eq!(h.trap(8, 0x0001_0000, 0, 0, 0xC), Stop::Released);
    assert_eq!(procs(&h).current(), Some(1));
    assert_eq!(queue(&h), [2, 3]);
    // A resumes exactly where it was: its saved registers, sepc + 4, sstatus.
    let mut want: Vec<u32> = (1..=31).map(|i| 0x100 * i + 0xA).collect();
    want.push(0x0001_0008);
    want.push(0x20);
    assert_eq!(frame_context(&h), want);
    assert_eq!(h.frame(0x8C), 0x8000_0000 | root(&h, 1));
    assert!(procs(&h).pcb(1).unwrap().context.is_none());
    let switches: Vec<(u64, u64)> = h
        .traced(SWITCH_KIND)
        .iter()
        .map(|t| (tu(t, "from"), tu(t, "to")))
        .collect();
    assert_eq!(switches, [(0, 1), (1, 2), (2, 3), (3, 1)]);
    assert_eq!(h.traced(SHUTDOWN_KIND).len(), 0);
}

#[test]
fn a_lone_process_yields_to_itself() {
    let files = [prog_a()];
    let mut h = harness(&files);
    h.boot();
    assert_eq!(h.trap(8, 0x0001_0004, 0, 0, 1), Stop::Released);
    assert_eq!(procs(&h).current(), Some(1));
    assert_eq!(queue(&h), Vec::<u32>::new());
    assert_eq!(h.frame(0x7C), 0x0001_0008);
    assert_eq!(h.frame(0x10), 0x500 + 1);
}

#[test]
fn a_fault_kills_frees_and_dispatches_the_next_until_the_queue_empties() {
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    h.boot();
    let free_before = procs(&h).frames().free_count();
    assert_eq!(h.trap(13, 0x0001_0004, 0xDEAD_0000, 0, 1), Stop::Released);
    assert_eq!(
        state(&h, 1),
        ProcState::Faulted {
            cause: 13,
            epc: 0x0001_0004,
            tval: 0xDEAD_0000
        }
    );
    let pcb = procs(&h).pcb(1).unwrap();
    assert!(pcb.tables.is_empty() && pcb.regions.is_empty() && pcb.context.is_none());
    assert!(procs(&h).frames().owned_by(1).is_empty());
    assert_eq!(procs(&h).frames().free_count(), free_before + 10);
    let f = h.traced(FAULT_KIND);
    assert_eq!(ts(f[0], "cause"), "LoadPageFault");
    assert_eq!(
        (tu(f[0], "pid"), tu(f[0], "epc"), tu(f[0], "tval")),
        (1, 0x10004, 0xDEAD_0000)
    );
    assert_eq!(procs(&h).current(), Some(2));
    // B faults too: the queue is empty, so the kernel shuts down with reason 1.
    assert_eq!(h.trap(2, 0x0001_0000, 0x73, 0, 2), Stop::Released);
    assert_eq!(ts(h.traced(FAULT_KIND)[1], "cause"), "IllegalInstruction");
    assert_eq!(procs(&h).current(), None);
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 1));
    let s = h.traced(SHUTDOWN_KIND);
    assert_eq!(
        (tu(s[0], "reason"), ts(s[0], "detail")),
        (1, "the run queue is empty".to_owned())
    );
    // Frame accounting: everything is free after the shutdown.
    assert_eq!(procs(&h).frames().free_count(), 3072);
    assert_eq!(
        h.k.inspect().get("life"),
        Some(&Value::Str("down".to_owned()))
    );
    // The hart halts on SRST; another ENTER is a kernel bug.
    assert!(matches!(h.op(TRAP_FRAME, None), Stop::Fault(_)));
}

#[test]
fn a_trap_from_s_mode_shuts_down_without_touching_processes() {
    let files = [prog_a(), prog_b()];
    let mut h = harness(&files);
    h.boot();
    assert_eq!(h.trap(13, 0x8000_2000, 4, 1 << 8, 0), Stop::Released);
    let s = h.traced(SHUTDOWN_KIND);
    assert_eq!(tu(s[0], "reason"), 1);
    assert_eq!(ts(s[0], "detail"), "trap from S-mode: LoadPageFault");
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 1));
    assert_eq!(state(&h, 1), ProcState::Running);
    assert_eq!(queue(&h), [2]);
}

#[test]
fn a_bad_enter_value_shuts_down_before_or_after_boot() {
    let files = [prog_a()];
    let mut h = harness(&files);
    assert_eq!(h.op(0x1234, None), Stop::Released);
    assert_eq!(ts(&h.ctx.traced[0], "op"), "shutdown");
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 1));
    assert!(procs(&h).pcbs().is_empty());
    assert_eq!(procs(&h).frames().free_count(), 3072);
    let mut h = harness(&files);
    h.boot();
    assert_eq!(h.op(0x1234, None), Stop::Released);
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 1));
    // After the shutdown the hart halts; any further ENTER, of any value, is a kernel
    // bug, not a second shutdown.
    let mut again = harness(&files);
    again.boot();
    again.op(0x1234, None);
    assert!(matches!(again.op(0x1234, None), Stop::Fault(_)));
    assert!(matches!(h.op(TRAP_FRAME, None), Stop::Fault(_)));
}

#[test]
fn the_pool_fits_exactly_and_one_frame_less_skips_the_image_cleanly() {
    let files = [prog_a(), prog_b(), prog_c()];
    let plan = plan_of(&files);
    let need: Vec<usize> = plan
        .images
        .iter()
        .map(|b| model::needed(b, &UserLayout::M3))
        .collect();
    assert_eq!(need, [10, 9, 9]);
    // Exactly enough for all three: the last frame is allocated.
    let mut h = Harness::new(config_with_pool(28), &files, plan.clone());
    h.boot();
    assert_eq!(procs(&h).frames().free_count(), 0);
    assert_eq!(procs(&h).pcbs().len(), 3);
    // One frame less: C is skipped with nothing reserved.
    let mut h = Harness::new(config_with_pool(27), &files, plan.clone());
    h.boot();
    let pids: Vec<u32> = procs(&h).pcbs().iter().map(|p| p.pid).collect();
    assert_eq!(pids, [1, 2]);
    assert!(procs(&h).frames().owned_by(3).is_empty());
    assert_eq!(procs(&h).frames().free_count(), 27 - 19);
    let c = h.traced(CREATE_KIND);
    assert_eq!((tu(c[2], "pid"), tu(c[2], "root")), (3, 0));
    assert_eq!(ts(c[2], "error"), "frame pool exhausted");
    assert_eq!(queue(&h), [2]);
    // A middle image that does not fit is skipped and a later, smaller one takes the
    // frames right after the first: B (9), A (10) fails in the 9 left, C (9) fits.
    let middle = [prog_b(), prog_a(), prog_c()];
    let mut h = Harness::new(config_with_pool(18), &middle, plan_of(&middle));
    h.boot();
    let pids: Vec<u32> = procs(&h).pcbs().iter().map(|p| p.pid).collect();
    assert_eq!(pids, [1, 3]);
    assert!(procs(&h).frames().owned_by(2).is_empty());
    assert_eq!(
        procs(&h).frames().owned_by(3),
        (POOL_PPN + 9..POOL_PPN + 18).collect::<Vec<_>>()
    );
    assert_eq!(procs(&h).frames().free_count(), 0);
    assert_eq!(
        ts(h.traced(CREATE_KIND)[1], "error"),
        "frame pool exhausted"
    );
    assert_eq!(
        h.traced(SEGMENT_KIND).len(),
        4,
        "no segment of the skipped image"
    );
    assert_eq!(queue(&h), [3]);
    // No write ever touched a frame outside the created processes'.
    let used: Vec<u32> = [1, 3]
        .iter()
        .flat_map(|&p| procs(&h).pcb(p).unwrap().frames())
        .collect();
    for (w, addr, _) in &h.seen {
        if *w && (POOL..POOL + POOL_SIZE).contains(addr) {
            assert!(used.contains(&((addr / 4096) as u32)));
        }
    }
    // Too small for anything: no process, and the kernel shuts down at boot.
    let mut h = Harness::new(config_with_pool(8), &files, plan);
    h.boot();
    assert!(procs(&h).pcbs().is_empty());
    assert_eq!(h.traced(CREATE_KIND).len(), 3);
    assert_eq!((h.frame(0x90), h.frame(0x94)), (1, 1));
    assert_eq!(procs(&h).frames().free_count(), 8);
}

#[test]
fn an_image_in_a_megapage_slot_fails_creation_and_boot_continues() {
    let bad = elf32(
        0x1000_0000,
        &[Seg::new(0x1000_0000, PF_R | PF_X, words(&[0x13]), 4)],
    );
    let files = [bad, prog_b()];
    let mut h = harness(&files);
    h.boot();
    let c = h.traced(CREATE_KIND);
    assert_eq!(ts(c[0], "error"), "a segment is in a megapage slot");
    assert_eq!(procs(&h).current(), Some(2));
    assert_eq!(
        procs(&h).frames().owned_by(2),
        (POOL_PPN..POOL_PPN + 9).collect::<Vec<_>>()
    );
}

#[test]
fn a_plan_is_validated_at_construction() {
    let files = [prog_a()];
    let ok = plan_of(&files);
    assert!(ModeledKernel::with_processes(config(), ok.clone()).is_ok());
    let reject = |plan: ProcessPlan, config: systemscope_os::KernelConfig| {
        ModeledKernel::with_processes(config, plan).err().unwrap()
    };
    assert_eq!(
        reject(
            ProcessPlan {
                images: vec![],
                ..ok.clone()
            },
            config()
        ),
        PlanError::ImageCount(0)
    );
    let nine = ProcessPlan {
        images: vec![ok.images[0].clone(); 9],
        ..ok.clone()
    };
    assert_eq!(reject(nine, config()), PlanError::ImageCount(9));
    let mut outside = ok.clone();
    outside.images[0].staged = (STAGING + STAGING_SIZE) as u32 - 4;
    assert!(matches!(
        reject(outside, config()),
        PlanError::Image { index: 0, .. }
    ));
    let mut shared = ok.clone();
    let page = shared.images[0].image.segments[0].pages[0];
    shared.images[0].image.segments[1].pages.insert(0, page);
    assert!(matches!(reject(shared, config()), PlanError::Image { .. }));
    let mut long = ok.clone();
    long.images[0].file_len = 8;
    assert!(matches!(reject(long, config()), PlanError::Image { .. }));
    let mut c = config();
    c.frame_pool.base += 0x800;
    assert!(matches!(
        reject(ok.clone(), c),
        PlanError::Config(_) | PlanError::Layout(_)
    ));
    let mut c = config();
    c.frame_pool.base = RAM_BASE + 0x0030_0000;
    c.frame_pool.size = 0x0020_0000;
    assert_eq!(
        reject(ok.clone(), c),
        PlanError::Layout("the frame pool overlaps the kernel megapage")
    );
    let stack_in_mmio = ProcessPlan {
        layout: UserLayout {
            stack_top: 0x1000_2000,
            ..UserLayout::M3
        },
        ..ok.clone()
    };
    assert!(matches!(
        reject(stack_in_mmio, config()),
        PlanError::Layout(_)
    ));
    let no_stack = ProcessPlan {
        layout: UserLayout {
            stack_pages: 0,
            ..UserLayout::M3
        },
        ..ok
    };
    assert!(matches!(reject(no_stack, config()), PlanError::Layout(_)));
}

/// The index in `seen` of the first access matching `pred`.
fn first(seen: &[Seen], pred: impl Fn(&Seen) -> bool) -> usize {
    seen.iter().position(pred).unwrap()
}

#[test]
fn a_bus_fault_on_any_kind_of_kernel_access_faults_the_session_once() {
    let files = [prog_a(), prog_b()];
    let mut clean = harness(&files);
    clean.boot();
    let s = &clean.seen;
    let in_pool = |a: &u64| (POOL..POOL + POOL_SIZE).contains(a);
    let tables: Vec<u32> = [1, 2]
        .iter()
        .flat_map(|&p| procs(&clean).pcb(p).unwrap().tables.clone())
        .collect();
    let zero = first(s, |x| x.0 && in_pool(&x.1));
    let copy_read = first(s, |x| !x.0);
    let copy_write = copy_read + 1;
    let pte = first(s, |x| {
        x.0 && x.2.len() == 4 && tables.contains(&((x.1 / 4096) as u32)) && x.2 != [0; 4]
    });
    let frame_write = first(s, |x| x.1 == u64::from(TRAP_FRAME));
    for (what, at) in [
        ("zero", zero),
        ("copy read", copy_read),
        ("copy write", copy_write),
        ("pte", pte),
        ("trap frame", frame_write),
    ] {
        let mut h = harness(&files);
        let stop = h.op(TRAP_FRAME, Some(at));
        assert_eq!(
            stop,
            Stop::Fault("modeled kernel: its own access faulted"),
            "{what}"
        );
        assert_eq!(
            h.seen.len(),
            at + 1,
            "{what}: nothing after the faulted access"
        );
        assert_eq!(h.ctx.take_sent(), vec![], "{what}: no release, no reissue");
    }
    // A fault on the frame read of a trap.
    let mut h = harness(&files);
    h.boot();
    let before = h.seen.len();
    assert_eq!(
        h.op(TRAP_FRAME, Some(0)),
        Stop::Fault("modeled kernel: its own access faulted")
    );
    assert_eq!(h.seen.len(), before + 1);
}

fn prefix(files: &[Vec<u8>], frames: usize) -> usize {
    let fresh = Harness::new(config_with_pool(frames as u64), files, plan_of(files));
    ksnap::prefix_len(&snapshot_of(&fresh.k), frames)
}

#[test]
fn the_snapshot_holds_kernel_metadata_only() {
    let files = [prog_a(), prog_b()];
    let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
    h.boot();
    h.trap(8, 0x0001_0004, 0, 0, 7);
    let bytes = snapshot_of(&h.k);
    let snap = Snap::decode(&bytes, prefix(&files, 64)).unwrap();
    assert_eq!(snap.encode(), bytes);
    assert_eq!((snap.state, snap.op.clone(), snap.held), (0, None, None));
    assert_eq!(snap.life, 1);
    assert_eq!(snap.queue, [1]);
    assert_eq!(snap.current, Some(2));
    assert_eq!(snap.pcbs.len(), 2);
    assert_eq!(snap.pcbs[0].state.0, 0);
    let ctx = snap.pcbs[0].context.as_ref().unwrap();
    assert_eq!((ctx[31], ctx[4]), (0x0001_0008, 0x507));
    assert_eq!(snap.pcbs[1].state.0, 1);
    assert_eq!(
        snap.pcbs[1].context, None,
        "the running context belongs to the hart"
    );
    assert_eq!(snap.pcbs[1].root, POOL_PPN + 10);
    assert_eq!(snap.pcbs[1].regions.len(), 3);
    assert_eq!(snap.bitmap.len(), 8);
    assert_eq!(snap.bitmap[..2], [0xFF, 0xFF]);
    assert_eq!(snap.bitmap[2], 0b0000_0111);
    // No page of memory, no trap frame, no staged file is in it.
    let after_prefix = &bytes[prefix(&files, 64)..];
    assert!(after_prefix.len() < 1024, "{}", after_prefix.len());
    for file in &files {
        assert!(!after_prefix.windows(8).any(|w| w == &file[..8]));
    }
    // In flight: the creation's PID, frames, and step; the copy's bytes between read and
    // write.
    let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
    h.enter(TRAP_FRAME).unwrap();
    loop {
        h.issue().unwrap();
        if matches!(h.pending, Some(MemMsg::ReadReq { .. })) {
            h.respond(false).unwrap();
            break;
        }
        h.respond(false).unwrap();
    }
    let snap = Snap::decode(&snapshot_of(&h.k), prefix(&files, 64)).unwrap();
    let op = snap.op.unwrap();
    assert_eq!(
        op.stage,
        Stage::Create {
            pid: 1,
            frames: (POOL_PPN..POOL_PPN + 10).collect()
        }
    );
    assert_eq!(op.step, 10 * 256 + 1);
    assert_eq!(op.data.len(), 12);
    assert_eq!(snap.held, Some(HELD.0));
    assert!(snap.pcbs.is_empty());
    assert_eq!(snap.bitmap[..2], [0xFF, 0b11]);
}

/// A named snapshot mutation.
type Mutant = (&'static str, Box<dyn Fn(&mut Snap)>);

/// `bytes` is rejected by a booted kernel, which is left exactly as it was.
fn assert_rejected(bytes: &[u8], why: &str) {
    let files = [prog_a(), prog_b()];
    let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
    h.boot();
    let before = snapshot_of(&h.k);
    assert!(restore_into(&mut h.k, bytes).is_err(), "accepted: {why}");
    assert_eq!(
        snapshot_of(&h.k),
        before,
        "a failed restore changed state: {why}"
    );
}

#[test]
fn restore_rejects_every_impossible_process_table() {
    let files = [prog_a(), prog_b()];
    let pre = prefix(&files, 64);
    let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
    h.boot();
    let good = Snap::decode(&snapshot_of(&h.k), pre).unwrap();
    {
        let mut k = ModeledKernel::with_processes(config_with_pool(64), plan_of(&files)).unwrap();
        restore_into(&mut k, &good.encode()).unwrap();
        assert_eq!(snapshot_of(&k), good.encode());
    }
    let mutants: Vec<Mutant> = vec![
        ("duplicate queue entry", Box::new(|s| s.queue.push(2))),
        ("queued running process", Box::new(|s| s.queue.push(1))),
        (
            "ready process missing from the queue",
            Box::new(|s| s.queue.clear()),
        ),
        (
            "two running",
            Box::new(|s| {
                s.pcbs[1].state = (1, vec![]);
                s.pcbs[1].context = None;
                s.queue.clear();
            }),
        ),
        (
            "current is not the running one",
            Box::new(|s| s.current = Some(2)),
        ),
        ("running and no current", Box::new(|s| s.current = None)),
        (
            "ready without context",
            Box::new(|s| s.pcbs[1].context = None),
        ),
        (
            "running with context",
            Box::new(|s| s.pcbs[0].context = Some(vec![0; 33])),
        ),
        ("allocated frame nobody owns", Box::new(|s| s.bitmap[4] = 1)),
        ("owned frame not allocated", Box::new(|s| s.bitmap[0] &= !1)),
        (
            "frame owned twice",
            Box::new(|s| {
                let f = s.pcbs[0].regions[0].2[0];
                s.pcbs[1].regions[0].2[0] = f;
            }),
        ),
        (
            "tables not the image's",
            Box::new(|s| s.pcbs[1].tables.swap(1, 2)),
        ),
        (
            "root not the first table",
            Box::new(|s| s.pcbs[0].root += 1),
        ),
        (
            "region perms not the image's",
            Box::new(|s| s.pcbs[0].regions[0].1 = 0b0110),
        ),
        (
            "region va not the image's",
            Box::new(|s| s.pcbs[0].regions[1].0 += 4096),
        ),
        ("PID outside the plan", Box::new(|s| s.pcbs[1].pid = 9)),
        ("PIDs out of order", Box::new(|s| s.pcbs.swap(0, 1))),
        (
            "terminal process with frames",
            Box::new(|s| {
                s.pcbs[1].state = (3, vec![13, 0, 0]);
                s.pcbs[1].context = None;
                s.queue.clear();
            }),
        ),
        (
            "idle after boot with nothing running",
            Box::new(|s| {
                s.pcbs[0].state = (0, vec![]);
                s.pcbs[0].context = Some(vec![0; 33]);
                s.queue.insert(0, 1);
                s.current = None;
            }),
        ),
        ("processes before boot", Box::new(|s| s.life = 0)),
        ("bad life tag", Box::new(|s| s.life = 3)),
        ("bitmap bit past the pool", Box::new(|s| s.bitmap.push(0))),
        (
            "operation without held",
            Box::new(|s| {
                s.state = 1;
                s.op = Some(ksnap::Op {
                    stage: Stage::ReadFrame,
                    step: 0,
                    data: vec![],
                });
            }),
        ),
        (
            "frame read not running",
            Box::new(|s| {
                s.state = 1;
                s.held = Some(1);
                s.op = Some(ksnap::Op {
                    stage: Stage::ReadFrame,
                    step: 0,
                    data: vec![],
                });
                s.life = 2;
            }),
        ),
        (
            "frame read with wrong data",
            Box::new(|s| {
                s.state = 1;
                s.held = Some(1);
                s.op = Some(ksnap::Op {
                    stage: Stage::ReadFrame,
                    step: 1,
                    data: vec![0; 3],
                });
            }),
        ),
        (
            "dispatch of another PID",
            Box::new(|s| {
                s.state = 1;
                s.held = Some(1);
                s.op = Some(ksnap::Op {
                    stage: Stage::Dispatch {
                        pid: 2,
                        context: vec![0; 33],
                    },
                    step: 0,
                    data: vec![],
                });
            }),
        ),
        (
            "shutdown not decided",
            Box::new(|s| {
                s.state = 1;
                s.held = Some(1);
                s.op = Some(ksnap::Op {
                    stage: Stage::Shutdown { reason: 1 },
                    step: 0,
                    data: vec![],
                });
            }),
        ),
        (
            "shutdown reason 2",
            Box::new(|s| {
                s.state = 1;
                s.held = Some(1);
                s.life = 2;
                s.op = Some(ksnap::Op {
                    stage: Stage::Shutdown { reason: 2 },
                    step: 0,
                    data: vec![],
                });
            }),
        ),
        (
            "stale wait txn",
            Box::new(|s| {
                s.state = 2;
                s.wait_txn = s.next_txn;
                s.held = Some(1);
                s.op = Some(ksnap::Op {
                    stage: Stage::ReadFrame,
                    step: 0,
                    data: vec![],
                });
            }),
        ),
    ];
    for (why, m) in &mutants {
        let mut s = good.clone();
        m(&mut s);
        assert_rejected(&s.encode(), why);
    }
    // Creation in flight: frames that are not the lowest free, too few, or a PID in use.
    let mut c = Harness::new(config_with_pool(64), &files, plan_of(&files));
    c.enter(TRAP_FRAME).unwrap();
    c.issue().unwrap();
    let creating = Snap::decode(&snapshot_of(&c.k), pre).unwrap();
    {
        let mut k = ModeledKernel::with_processes(config_with_pool(64), plan_of(&files)).unwrap();
        restore_into(&mut k, &creating.encode()).unwrap();
    }
    let shift = |s: &mut Snap| {
        if let Some(ksnap::Op {
            stage: Stage::Create { frames, .. },
            ..
        }) = &mut s.op
        {
            for f in frames.iter_mut() {
                *f += 1;
            }
        }
        s.bitmap[0] = 0xFE;
        s.bitmap[1] = 0b111;
    };
    let fewer = |s: &mut Snap| {
        if let Some(ksnap::Op {
            stage: Stage::Create { frames, .. },
            ..
        }) = &mut s.op
        {
            frames.pop();
        }
        s.bitmap[1] = 0b01;
    };
    let pid_in_use = |s: &mut Snap| {
        if let Some(ksnap::Op {
            stage: Stage::Create { pid, .. },
            ..
        }) = &mut s.op
        {
            *pid = 3;
        }
    };
    let unmarked = |s: &mut Snap| s.bitmap[0] = 0xFE;
    for (why, m) in [
        (
            "creation frames not the lowest free",
            &shift as &dyn Fn(&mut Snap),
        ),
        ("creation with too few frames", &fewer),
        ("creation PID outside the plan", &pid_in_use),
        ("creation frame not marked", &unmarked),
    ] {
        let mut s = creating.clone();
        m(&mut s);
        assert_rejected(&s.encode(), why);
    }
    // The other mode's snapshots.
    let proto = ModeledKernel::new(config_with_pool(64)).unwrap();
    assert_rejected(&snapshot_of(&proto), "prototype snapshot");
    let mut proto = ModeledKernel::new(config_with_pool(64)).unwrap();
    assert!(restore_into(&mut proto, &good.encode()).is_err());
}

#[test]
fn restore_rejects_bitmap_bits_past_a_partial_last_byte() {
    // 60 frames: the bitmap's last byte has four bits past the pool.
    let files = [prog_a()];
    let mut h = Harness::new(config_with_pool(60), &files, plan_of(&files));
    h.boot();
    let good = Snap::decode(&snapshot_of(&h.k), prefix(&files, 60)).unwrap();
    assert_eq!(good.bitmap.len(), 8);
    for bit in 4..8 {
        let mut s = good.clone();
        s.bitmap[7] |= 1 << bit;
        let mut k = ModeledKernel::with_processes(config_with_pool(60), plan_of(&files)).unwrap();
        let before = snapshot_of(&k);
        assert!(restore_into(&mut k, &s.encode()).is_err(), "bit {bit}");
        assert_eq!(snapshot_of(&k), before);
    }
}

/// Every handler boundary of `run` (the boot, then traps), as a snapshot, restores into
/// a fresh kernel that then does exactly what the original did: the same requests with
/// the same txns, the same traces, the same final snapshot. In `Wait`, a wake to the
/// restored kernel faults: it waits, it never reissues.
#[test]
fn every_step_boundary_restores_and_never_reissues() {
    let files = [prog_a(), prog_b()];
    let traps: [(u32, u32); 3] = [(8, 0x0001_0004), (8, 0x0001_0000), (13, 0x0001_0008)];
    let pool = 64;
    let run = |k: Option<&[u8]>, cut: usize| -> (Vec<Vec<u8>>, Vec<MemMsg>, usize) {
        // Returns the snapshots at each boundary, the requests, and boundaries seen.
        let mut h = Harness::new(config_with_pool(pool), &files, plan_of(&files));
        let mut snaps = vec![snapshot_of(&h.k)];
        let mut requests = Vec::new();
        let mut boundary = 0usize;
        let mut restored = k.is_none();
        let ops: Vec<Option<(u32, u32)>> = std::iter::once(None)
            .chain(traps.iter().map(|&t| Some(t)))
            .collect();
        for t in ops {
            if let Some((cause, sepc)) = t {
                let f = u64::from(TRAP_FRAME);
                h.mem.set_word(f + 0x7C, sepc);
                h.mem.set_word(f + 0x84, cause);
                h.mem.set_word(f + 0x80, 0);
            }
            h.enter(TRAP_FRAME).unwrap();
            boundary += 1;
            snaps.push(snapshot_of(&h.k));
            loop {
                if !restored && boundary == cut {
                    restore_into(&mut h.k, k.unwrap()).unwrap();
                    restored = true;
                }
                let req = h.issue().unwrap();
                requests.push(req);
                boundary += 1;
                snaps.push(snapshot_of(&h.k));
                if !restored && boundary == cut {
                    restore_into(&mut h.k, k.unwrap()).unwrap();
                    restored = true;
                    // A restored Wait takes a wake as a fault: nothing is reissued.
                    let mut probe =
                        ModeledKernel::with_processes(config_with_pool(pool), plan_of(&files))
                            .unwrap();
                    restore_into(&mut probe, k.unwrap()).unwrap();
                    let mut ctx = common::MockCtx::new();
                    assert!(ctx.wake(&mut probe, ISSUE, Phase::Request).is_err());
                    assert!(ctx.sent.is_empty());
                }
                let done = h.respond(false).unwrap();
                boundary += 1;
                snaps.push(snapshot_of(&h.k));
                if done {
                    break;
                }
            }
        }
        (snaps, requests, boundary)
    };
    let (snaps, requests, total) = run(None, usize::MAX);
    let last = snaps.last().unwrap().clone();
    // Restore at a spread of boundaries in every stage: all of the non-zero stages, and
    // every 97th boundary of the zeroing.
    let mut cuts: Vec<usize> = (1..total).filter(|c| c % 97 == 0).collect();
    let snap_at = |c: usize| &snaps[c];
    for c in 1..total {
        let s = Snap::decode(snap_at(c), prefix(&files, pool as usize)).unwrap();
        let busy = matches!(&s.op, Some(o) if !matches!(o.stage, Stage::Create { .. }) || !o.data.is_empty() || o.step >= 10 * 256);
        if busy {
            cuts.push(c);
        }
    }
    cuts.sort();
    cuts.dedup();
    assert!(cuts.len() > 100);
    for c in cuts {
        let (snaps2, requests2, _) = run(Some(&snaps[c]), c);
        assert_eq!(requests2, requests, "cut {c}");
        assert_eq!(snaps2.last().unwrap(), &last, "cut {c}");
    }
}

#[test]
fn inspect_shows_the_process_table() {
    let files = [prog_a(), prog_b()];
    let mut h = Harness::new(config_with_pool(64), &files, plan_of(&files));
    let s = |h: &Harness, n: &str| h.k.inspect().get(n).cloned().unwrap();
    assert_eq!(s(&h, "life"), Value::Str("await_boot".into()));
    assert_eq!(s(&h, "free_frames"), Value::U64(64));
    h.boot();
    h.trap(13, 0x10000, 0, 0, 0);
    assert_eq!(s(&h, "running"), Value::Str("2".into()));
    assert_eq!(s(&h, "queue"), Value::Str("".into()));
    assert_eq!(
        s(&h, "processes"),
        Value::Str(format!(
            "1:faulted(0xd,0x10000,0x0):{:#x};2:running:{:#x}",
            POOL_PPN,
            POOL_PPN + 10
        ))
    );
    assert_eq!(s(&h, "free_frames"), Value::U64(64 - 9));
}
