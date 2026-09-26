//! The process plan: the user layout and the boot images the kernel creates processes
//! from (`docs/m3-design.md` §6.2, §6.4, §8.3, §17 M3.4b).
//!
//! A [`BootImage`] is a user executable that is already in the staging area and already
//! validated: `staged` and `file_len` locate its bytes, and `image` is the mapping plan
//! `systemscope_elf::parse_user_elf32` made from its headers. The kernel never parses the
//! ELF again. It only copies the [`FileCopy`](systemscope_elf::FileCopy) ranges the plan
//! names from staging into frames, through its own bus accesses (§8.3). How the bytes
//! reached staging (the executable table and the block controller, §8.1, §8.2) is a later
//! step; M3.4b starts from staged bytes.
//!
//! The plan is fixed at construction and is part of the snapshot, like the configuration.
//! [`ProcessPlan::validate`] checks it once: everything a well-formed `UserImage` already
//! guarantees is checked again, so the kernel never trusts a hand-built plan, and the
//! layout rules the address-space builder relies on are checked here.

use std::fmt;
use std::ops::Range;

use systemscope_contracts::snapshot::SnapshotWriter;
use systemscope_elf::{Perms, UserImage};

use crate::config::{KernelConfig, KernelConfigError};
use crate::core::PAGE;

/// The largest number of processes, and of boot images (§6.2).
pub const MAX_PROCESSES: usize = 8;
/// The size of a Sv32 megapage.
pub const MEGAPAGE: u64 = 4 << 20;
/// The MMIO megapage's base, identity-mapped in every address space (§6.4).
pub const MMIO_MEGAPAGE: u64 = 0x1000_0000;
/// The first physical address Sv32 cannot reach (a PPN has 22 bits).
pub const PHYS_LIMIT: u64 = 1 << 34;
/// The first byte of the plan's snapshot encoding. No M3.4a state tag has this value, so
/// a kernel without processes rejects a snapshot with them, and the reverse.
pub(crate) const PLAN_MARKER: u8 = 0xA5;

/// The user layout (§6.2, §6.4): where user segments may lie and where the stack is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UserLayout {
    /// The lowest user virtual address, `USER_BASE`.
    pub user_base: u32,
    /// The top of the stack, `STACK_TOP`: the page at it stays unmapped.
    pub stack_top: u32,
    /// The number of stack pages, `STACK_PAGES`.
    pub stack_pages: u32,
}

impl UserLayout {
    /// The M3 user layout (§11.2): `USER_BASE = 0x0001_0000`, `STACK_TOP = 0x7FFF_F000`,
    /// `STACK_PAGES = 4`.
    pub const M3: UserLayout = UserLayout {
        user_base: 0x0001_0000,
        stack_top: 0x7FFF_F000,
        stack_pages: 4,
    };

    /// The lowest stack address, `STACK_TOP − STACK_PAGES · 4096`, or `None` if it would
    /// be negative.
    pub fn stack_base(&self) -> Option<u32> {
        self.stack_pages
            .checked_mul(PAGE as u32)
            .and_then(|size| self.stack_top.checked_sub(size))
    }

    /// The range segments may occupy, `[USER_BASE, stack base)`: the `user_range` to
    /// validate user images with (§8.3).
    pub fn image_range(&self) -> Range<u32> {
        self.user_base..self.stack_base().unwrap_or(0)
    }

    /// The stack pages' virtual addresses, ascending.
    pub fn stack_vas(&self) -> impl Iterator<Item = u32> + '_ {
        let base = self.stack_base().unwrap_or(0);
        (0..self.stack_pages).map(move |i| base + i * PAGE as u32)
    }
}

/// A user executable in staging and its validated mapping plan.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BootImage {
    /// The physical address of the file's first byte, inside staging.
    pub staged: u32,
    /// The file's length in bytes.
    pub file_len: u32,
    /// The mapping plan `parse_user_elf32` made from the file's headers.
    pub image: UserImage,
}

/// What a kernel with processes is configured with, besides its [`KernelConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessPlan {
    /// The user layout.
    pub layout: UserLayout,
    /// The boot images, in the order boot creates them: image `i` becomes PID `i + 1`
    /// (§6.4).
    pub images: Vec<BootImage>,
}

/// Why a process plan cannot form a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The configuration breaks a rule of [`KernelConfig::validate`].
    Config(KernelConfigError),
    /// The configuration breaks a rule that only processes need; the text names it.
    Layout(&'static str),
    /// There is no boot image, or more than [`MAX_PROCESSES`].
    ImageCount(usize),
    /// Boot image `index` breaks the named rule.
    Image {
        /// The image's index.
        index: usize,
        /// The rule.
        rule: &'static str,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Config(e) => write!(f, "{e}"),
            PlanError::Layout(rule) => write!(f, "layout: {rule}"),
            PlanError::ImageCount(n) => {
                write!(f, "{n} boot images, not 1 to {MAX_PROCESSES}")
            }
            PlanError::Image { index, rule } => write!(f, "boot image {index}: {rule}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Whether `perms` is a permission set a user leaf may have: readable or executable, and
/// never writable without readable (§8.3).
pub fn valid_perms(perms: Perms) -> bool {
    (perms.read || perms.execute) && (perms.read || !perms.write)
}

/// The kernel megapage's base: the RAM base (§6.4).
pub fn kernel_megapage(config: &KernelConfig) -> u64 {
    config.ram.base
}

impl ProcessPlan {
    /// Checks the configuration and the plan. Beyond [`KernelConfig::validate`]:
    ///
    /// - **Megapages:** the RAM base is 4 MiB aligned and below the Sv32 physical limit;
    ///   the trap frame and staging lie in the kernel megapage, so the trampoline reaches
    ///   them under any `satp`; the frame pool is page-aligned, below the limit, and
    ///   outside the kernel megapage.
    /// - **Layout:** `USER_BASE` and `STACK_TOP` page-aligned, at least one stack page,
    ///   `USER_BASE` below the stack, and no stack page in the slot of either megapage.
    /// - **Images:** 1 to [`MAX_PROCESSES`]; each file non-empty and inside staging; an
    ///   entry 4-byte aligned in the image range; at least one segment; every page
    ///   page-aligned, in the image range, with its segment's valid permissions, in strictly
    ///   ascending order across the image (no shared page); every copy non-empty, inside
    ///   its page, and inside the file.
    ///
    /// A segment in a megapage's slot is not rejected here: `parse_user_elf32` accepts it,
    /// so it is a guest error, and creating that process fails at boot (§6.4).
    pub fn validate(&self, config: &KernelConfig) -> Result<(), PlanError> {
        config.validate().map_err(PlanError::Config)?;
        let layout_err = |rule| Err(PlanError::Layout(rule));
        let kernel = kernel_megapage(config);
        let mega = crate::config::Window {
            base: kernel,
            size: MEGAPAGE,
        };
        if !kernel.is_multiple_of(MEGAPAGE) || kernel + MEGAPAGE > PHYS_LIMIT {
            return layout_err("the RAM base is not a 4 MiB aligned Sv32 address");
        }
        let frame = config.frame();
        if !mega.contains(frame.base, frame.size) {
            return layout_err("the trap frame is outside the kernel megapage");
        }
        if !mega.contains(config.staging.base, config.staging.size) {
            return layout_err("staging is outside the kernel megapage");
        }
        let pool = config.frame_pool;
        if !pool.base.is_multiple_of(PAGE) || !pool.size.is_multiple_of(PAGE) {
            return layout_err("the frame pool is not page-aligned");
        }
        if pool.base + pool.size > PHYS_LIMIT || pool.size / PAGE > u64::from(u32::MAX) {
            return layout_err("the frame pool is not reachable by Sv32");
        }
        if mega.overlaps(pool.base, pool.size) {
            return layout_err("the frame pool overlaps the kernel megapage");
        }
        let l = &self.layout;
        let page = PAGE as u32;
        if !l.user_base.is_multiple_of(page) || !l.stack_top.is_multiple_of(page) {
            return layout_err("USER_BASE or STACK_TOP is not page-aligned");
        }
        if l.stack_pages == 0 {
            return layout_err("there is no stack page");
        }
        let Some(stack_base) = l.stack_base() else {
            return layout_err("the stack runs below address 0");
        };
        if l.user_base >= stack_base {
            return layout_err("USER_BASE is not below the stack");
        }
        let slots = [vpn1(kernel), vpn1(MMIO_MEGAPAGE)];
        if l.stack_vas().any(|va| slots.contains(&vpn1(u64::from(va)))) {
            return layout_err("a stack page is in a megapage slot");
        }
        if self.images.is_empty() || self.images.len() > MAX_PROCESSES {
            return Err(PlanError::ImageCount(self.images.len()));
        }
        for (index, image) in self.images.iter().enumerate() {
            image
                .check(config, l)
                .map_err(|rule| PlanError::Image { index, rule })?;
        }
        Ok(())
    }

    /// The canonical encoding, as the snapshot stores it after the configuration.
    pub(crate) fn encode(&self, w: &mut SnapshotWriter) {
        w.u8(PLAN_MARKER);
        w.u32(self.layout.user_base);
        w.u32(self.layout.stack_top);
        w.u32(self.layout.stack_pages);
        w.len(self.images.len());
        for b in &self.images {
            w.u32(b.staged);
            w.u32(b.file_len);
            w.u32(b.image.entry);
            w.len(b.image.segments.len());
            for s in &b.image.segments {
                w.u16(s.index);
                w.u32(s.vaddr);
                w.u32(s.memsz);
                w.u8(perm_bits(s.perms));
                w.len(s.pages.len());
                for p in &s.pages {
                    w.u32(p.va);
                    w.u8(perm_bits(p.perms));
                    match p.copy {
                        None => w.u8(0),
                        Some(c) => {
                            w.u8(1);
                            w.u32(c.file_offset);
                            w.u32(c.page_offset);
                            w.u32(c.len);
                        }
                    }
                }
            }
        }
    }
}

impl BootImage {
    /// The rules of [`ProcessPlan::validate`] for one image, or the one it breaks.
    fn check(&self, config: &KernelConfig, layout: &UserLayout) -> Result<(), &'static str> {
        let range = layout.image_range();
        let page = PAGE as u32;
        if self.file_len == 0 {
            return Err("the file is empty");
        }
        if !config
            .staging
            .contains(u64::from(self.staged), u64::from(self.file_len))
        {
            return Err("the file is not inside staging");
        }
        let entry = self.image.entry;
        if !entry.is_multiple_of(4) || !range.contains(&entry) {
            return Err("the entry is misaligned or outside the image range");
        }
        if self.image.segments.is_empty() {
            return Err("there is no segment");
        }
        let mut last: Option<u32> = None;
        for seg in &self.image.segments {
            if !valid_perms(seg.perms) || seg.pages.is_empty() {
                return Err("a segment has no pages or invalid permissions");
            }
            for p in &seg.pages {
                if !p.va.is_multiple_of(page)
                    || p.va < range.start
                    || p.va >= range.end
                    || p.perms != seg.perms
                    || last.is_some_and(|l| p.va <= l)
                {
                    return Err("a page is misplaced, out of order, or shared");
                }
                last = Some(p.va);
                if let Some(c) = p.copy {
                    let in_page = c.len != 0 && c.page_offset.checked_add(c.len) <= Some(page);
                    let in_file = c
                        .file_offset
                        .checked_add(c.len)
                        .is_some_and(|end| end <= self.file_len);
                    if !in_page || !in_file {
                        return Err("a copy leaves its page or the file");
                    }
                }
            }
        }
        Ok(())
    }
}

/// `perms` as the PTE's `R`/`W`/`X` bits (bits 1–3).
pub fn perm_bits(perms: Perms) -> u8 {
    u8::from(perms.read) << 1 | u8::from(perms.write) << 2 | u8::from(perms.execute) << 3
}

/// `perms` as `rwx`, with `-` for a missing permission, as the trace spells it.
pub fn perm_str(perms: Perms) -> String {
    [(perms.read, 'r'), (perms.write, 'w'), (perms.execute, 'x')]
        .iter()
        .map(|&(on, c)| if on { c } else { '-' })
        .collect()
}

/// The level-1 index of an address: its 4 MiB slot.
pub fn vpn1(addr: u64) -> u32 {
    ((addr >> 22) & 0x3FF) as u32
}
