//! Sv32 PTE and `satp` encoding for the page tables the kernel writes
//! (`docs/m3-design.md` §5.2, §6.4).
//!
//! This is the writing side only: the kernel builds entries, and the CPU's walker, which
//! this crate never uses or depends on, reads them. The bit positions are the privileged
//! specification's ("Sv32: Page-Based 32-bit Virtual-Memory Systems").

use systemscope_elf::Perms;

use crate::image::perm_bits;

/// Valid.
pub const V: u32 = 1 << 0;
/// Readable.
pub const R: u32 = 1 << 1;
/// Writable.
pub const W: u32 = 1 << 2;
/// Executable.
pub const X: u32 = 1 << 3;
/// User-accessible.
pub const U: u32 = 1 << 4;
/// Global.
pub const G: u32 = 1 << 5;
/// Accessed.
pub const A: u32 = 1 << 6;
/// Dirty.
pub const D: u32 = 1 << 7;
/// `satp.MODE` for Sv32, with ASID 0.
pub const SATP_SV32: u32 = 1 << 31;

/// A PTE pointing at physical page `ppn` with `flags`.
fn entry(ppn: u32, flags: u32) -> u32 {
    (ppn & 0x3F_FFFF) << 10 | flags
}

/// A user 4 KiB leaf (§6.4): `U`, `A`, `D` exactly when writable, and `perms`.
pub fn user_leaf(ppn: u32, perms: Perms) -> u32 {
    let dirty = if perms.write { D } else { 0 };
    entry(ppn, V | u32::from(perm_bits(perms)) | U | A | dirty)
}

/// A level-1 entry pointing at a level-0 table: only `V`.
pub fn pointer(ppn: u32) -> u32 {
    entry(ppn, V)
}

/// The kernel megapage leaf at physical page `ppn`: `R W X`, `G`, `A`, `D`, `U = 0`.
pub fn kernel_megapage(ppn: u32) -> u32 {
    entry(ppn, V | R | W | X | G | A | D)
}

/// The MMIO megapage leaf at physical page `ppn`: `R W`, `G`, `A`, `D`, `U = 0`.
pub fn mmio_megapage(ppn: u32) -> u32 {
    entry(ppn, V | R | W | G | A | D)
}

/// The `satp` value for a root table at `root`: Sv32, ASID 0.
pub fn satp(root: u32) -> u32 {
    SATP_SV32 | (root & 0x3F_FFFF)
}
