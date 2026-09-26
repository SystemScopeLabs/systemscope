//! The address-space builder (`docs/m3-design.md` §6.4, §8.3): which frames a new process
//! needs, what goes in each, and the kernel accesses that put it there.
//!
//! # Frames
//!
//! A process needs one root (level-1) table, one level-0 table for every 4 MiB slot its
//! pages touch, and one frame per page: every segment page and every stack page. The
//! kernel reserves them all at once, lowest free first, before it writes anything
//! ([`frames_needed`]), so pool exhaustion is found before the process has any state and
//! the creation fails with nothing to undo (§6.4). The reserved frames, ascending, are
//! assigned in the order a lazy builder would allocate them: the root; then, for each
//! page in order (segment pages, then stack pages), its level-0 table on first use and
//! then its frame.
//!
//! # Accesses
//!
//! [`Space::access`] lists the creation's kernel accesses by step, all through the bus:
//!
//! 1. **zero** every frame with 16-byte writes, in assignment order;
//! 2. **copy** each page's file bytes from staging: a read of at most 16 bytes that
//!    crosses no page on either side, then a write of the same bytes to the frame;
//! 3. **map**: the level-0 leaves, page by page, then the level-1 entries in ascending
//!    slot order: the pointers to the level-0 tables and the two megapages.
//!
//! Every frame is therefore written before any PTE points at it, and every level-0
//! table is complete before the level-1 entry that points at it (§8.3).

use systemscope_elf::{Perms, UserImage};

use crate::config::KernelConfig;
use crate::core::{Access, MAX_ACCESS, PAGE};
use crate::image::{BootImage, MMIO_MEGAPAGE, UserLayout, kernel_megapage, vpn1};
use crate::pte;

/// The stack's permissions: read and write.
pub const STACK_PERMS: Perms = Perms {
    read: true,
    write: true,
    execute: false,
};

/// A mapped region of a process (§6.4): a loaded segment or the stack.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Region {
    /// The first page's virtual address.
    pub va: u32,
    /// The permissions of every page.
    pub perms: Perms,
    /// The frame of each page, in page order.
    pub frames: Vec<u32>,
}

/// One user page to map: its virtual address, permissions, and file bytes.
struct Page {
    va: u32,
    perms: Perms,
    copy: Option<(u32, u32, u32)>,
}

fn pages(image: &UserImage, layout: &UserLayout) -> Vec<Page> {
    let segments = image.segments.iter().flat_map(|s| {
        s.pages.iter().map(|p| Page {
            va: p.va,
            perms: p.perms,
            copy: p.copy.map(|c| (c.file_offset, c.page_offset, c.len)),
        })
    });
    let stack = layout.stack_vas().map(|va| Page {
        va,
        perms: STACK_PERMS,
        copy: None,
    });
    segments.chain(stack).collect()
}

/// The number of frames a process for `image` needs: the root, a level-0 table per
/// touched slot, and one per segment and stack page.
pub fn frames_needed(image: &UserImage, layout: &UserLayout) -> usize {
    let pages = pages(image, layout);
    let mut slots: Vec<u32> = pages.iter().map(|p| vpn1(u64::from(p.va))).collect();
    slots.sort_unstable();
    slots.dedup();
    1 + slots.len() + pages.len()
}

/// Whether a segment page of `image` lies in the slot of the kernel or the MMIO
/// megapage, which every address space maps (§6.4). Such an image cannot be mapped.
pub fn megapage_conflict(image: &UserImage, config: &KernelConfig) -> bool {
    let slots = [vpn1(kernel_megapage(config)), vpn1(MMIO_MEGAPAGE)];
    image
        .segments
        .iter()
        .flat_map(|s| &s.pages)
        .any(|p| slots.contains(&vpn1(u64::from(p.va))))
}

/// A process's address space: its frames and the creation's accesses.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Space {
    /// The root table's PPN.
    pub root: u32,
    /// The page-table frames: the root, then the level-0 tables in assignment order.
    pub tables: Vec<u32>,
    /// The regions: the segments in image order, then the stack.
    pub regions: Vec<Region>,
    /// Every frame, in assignment order.
    frames: Vec<u32>,
    /// The level-0 table of each touched 4 MiB slot: `(VPN[1], PPN)`.
    slots: Vec<(u32, u32)>,
    /// The copies: `(source, destination, length)`, physical.
    copies: Vec<(u64, u64, u32)>,
    /// The PTE writes: `(address, value)`.
    ptes: Vec<(u64, u32)>,
}

impl Space {
    /// The address space of `boot` built in `frames`, which are exactly
    /// [`frames_needed`] frames in ascending order.
    pub fn build(
        config: &KernelConfig,
        layout: &UserLayout,
        boot: &BootImage,
        frames: &[u32],
    ) -> Space {
        let mut next = frames.iter().copied();
        let mut take = || next.next().expect("frames_needed frames");
        let root = take();
        let mut tables = vec![root];
        let mut slots: Vec<(u32, u32)> = Vec::new();
        let mut order = vec![root];
        let mut leaves = Vec::new();
        let mut copies = Vec::new();
        let mut placed: Vec<(u32, Perms, u32)> = Vec::new();
        for page in pages(&boot.image, layout) {
            let slot = vpn1(u64::from(page.va));
            let table = match slots.iter().find(|s| s.0 == slot) {
                Some(&(_, t)) => t,
                None => {
                    let t = take();
                    slots.push((slot, t));
                    tables.push(t);
                    order.push(t);
                    t
                }
            };
            let frame = take();
            order.push(frame);
            let vpn0 = u64::from(page.va >> 12 & 0x3FF);
            leaves.push((
                u64::from(table) * PAGE + 4 * vpn0,
                pte::user_leaf(frame, page.perms),
            ));
            if let Some((file_offset, page_offset, len)) = page.copy {
                let src = u64::from(boot.staged) + u64::from(file_offset);
                let dst = u64::from(frame) * PAGE + u64::from(page_offset);
                split_copy(src, dst, u64::from(len), &mut copies);
            }
            placed.push((page.va, page.perms, frame));
        }
        let kernel = kernel_megapage(config);
        let mut l1: Vec<(u32, u32)> = slots
            .iter()
            .map(|&(slot, t)| (slot, pte::pointer(t)))
            .collect();
        l1.push((vpn1(kernel), pte::kernel_megapage((kernel / PAGE) as u32)));
        l1.push((
            vpn1(MMIO_MEGAPAGE),
            pte::mmio_megapage((MMIO_MEGAPAGE / PAGE) as u32),
        ));
        l1.sort_unstable_by_key(|e| e.0);
        let mut ptes = leaves;
        ptes.extend(
            l1.iter()
                .map(|&(slot, value)| (u64::from(root) * PAGE + 4 * u64::from(slot), value)),
        );
        Space {
            root,
            tables,
            regions: regions(&boot.image, layout, &placed),
            frames: order,
            slots,
            copies,
            ptes,
        }
    }

    /// The level-0 table that maps `va`'s 4 MiB slot, if the space has one.
    pub fn table_for(&self, va: u32) -> Option<u32> {
        let slot = vpn1(u64::from(va));
        self.slots.iter().find(|s| s.0 == slot).map(|s| s.1)
    }

    /// Every frame of the address space, in assignment order.
    pub fn frames(&self) -> &[u32] {
        &self.frames
    }

    fn zero_steps(&self) -> usize {
        self.frames.len() * (PAGE / MAX_ACCESS) as usize
    }

    /// The number of accesses the creation makes.
    pub fn steps(&self) -> u32 {
        let n = self.zero_steps() + 2 * self.copies.len() + self.ptes.len();
        u32::try_from(n).expect("a process has a bounded number of accesses")
    }

    /// The working data step `step` requires: a copy's bytes between its read and its
    /// write, else none.
    pub fn data_len(&self, step: u32) -> usize {
        let k = (step as usize).wrapping_sub(self.zero_steps());
        match self.copies.get(k / 2) {
            Some(c) if k % 2 == 1 => c.2 as usize,
            _ => 0,
        }
    }

    /// The access due at `step`, with `data` the working data (the bytes a copy read),
    /// or `None` past the last one.
    pub fn access(&self, step: u32, data: &[u8]) -> Option<Access> {
        let step = step as usize;
        let per_frame = (PAGE / MAX_ACCESS) as usize;
        let zero = self.zero_steps();
        if step < zero {
            let frame = u64::from(self.frames[step / per_frame]);
            let addr = frame * PAGE + (step % per_frame) as u64 * MAX_ACCESS;
            return Some(Access::Write {
                addr,
                data: vec![0; MAX_ACCESS as usize],
            });
        }
        let k = step - zero;
        if let Some(&(src, dst, len)) = self.copies.get(k / 2) {
            return Some(if k.is_multiple_of(2) {
                Access::Read { addr: src, len }
            } else {
                Access::Write {
                    addr: dst,
                    data: data.to_vec(),
                }
            });
        }
        let (addr, value) = *self.ptes.get(k - 2 * self.copies.len())?;
        Some(Access::Write {
            addr,
            data: value.to_le_bytes().to_vec(),
        })
    }
}

/// `len` bytes from `src` to `dst` as copies of at most [`MAX_ACCESS`] bytes that cross
/// no page on either side.
fn split_copy(mut src: u64, mut dst: u64, len: u64, out: &mut Vec<(u64, u64, u32)>) {
    let end = src + len;
    while src < end {
        let n = (end - src)
            .min(MAX_ACCESS)
            .min(PAGE - src % PAGE)
            .min(PAGE - dst % PAGE);
        out.push((src, dst, n as u32));
        src += n;
        dst += n;
    }
}

/// The regions of `image` with the frames `placed` gave each page.
fn regions(image: &UserImage, layout: &UserLayout, placed: &[(u32, Perms, u32)]) -> Vec<Region> {
    let mut at = placed.iter();
    let mut out: Vec<Region> = image
        .segments
        .iter()
        .map(|s| Region {
            va: s.pages[0].va,
            perms: s.perms,
            frames: at.by_ref().take(s.pages.len()).map(|p| p.2).collect(),
        })
        .collect();
    out.push(Region {
        va: layout.stack_base().expect("a validated layout"),
        perms: STACK_PERMS,
        frames: at.map(|p| p.2).collect(),
    });
    out
}
