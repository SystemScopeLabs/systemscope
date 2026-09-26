//! The syscall ABI (`docs/m3-design.md` §6.5) and the kernel's user-copy walk.
//!
//! # ABI
//!
//! A trap with `scause = 8` (`ecall` from U) is a syscall. The trap frame (§7.2) is the
//! only representation of its input and output: `a7` holds the number and `a0`–`a5` the
//! arguments; the result goes in `a0`, `sepc` advances by 4, and every other register is
//! preserved. The numbers are the Linux RV32 asm-generic ones (§19.1 decision 8):
//!
//! | Nr | Name | Result |
//! |---|---|---|
//! | 64 | `write(fd, buf, count)` | `n = min(count, WRITE_MAX)`, or `-EBADF` / `-EFAULT` |
//! | 93 | `exit(status)` | does not return |
//! | 94 | `exit_group(status)` | does not return (the same as `exit`) |
//! | 124 | `sched_yield()` | 0 |
//! | 172 | `getpid()` | the PID |
//! | any other | | `-ENOSYS` |
//!
//! Nothing here reaches the host: no host syscall, no host PID, no host output.
//!
//! # The user-copy walk
//!
//! The kernel reads a `write` buffer through the caller's page table in RAM with its own
//! Sv32 walk, written independently of the CPU's (§6.5): [`walk_step`] decides one PTE,
//! and the operation reads each PTE with an ordinary kernel bus access. The buffer is
//! checked as a U-mode load with `SUM = MXR = 0` would be: a pointer only at level 1 and
//! with `D`, `A`, and `U` clear, as the CPU requires, and every leaf valid, readable,
//! user-accessible, and accessed. A megapage leaf has `U = 0`, so a buffer in the
//! kernel or MMIO megapage is refused with `-EFAULT` (§6.3) before any physical access.

use crate::core::PAGE;
use crate::pte::{A, D, R, U, V, W, X};

/// `write(fd, buf, count)`.
pub const SYS_WRITE: u32 = 64;
/// `exit(status)`.
pub const SYS_EXIT: u32 = 93;
/// `exit_group(status)`.
pub const SYS_EXIT_GROUP: u32 = 94;
/// `sched_yield()`.
pub const SYS_SCHED_YIELD: u32 = 124;
/// `getpid()`.
pub const SYS_GETPID: u32 = 172;

/// Bad file descriptor.
pub const EBADF: u32 = 9;
/// Bad address.
pub const EFAULT: u32 = 14;
/// Function not implemented.
pub const ENOSYS: u32 = 38;

/// The most bytes one `write` outputs (§6.2).
pub const WRITE_MAX: u32 = 4096;

/// The offset of `a0` (`x10`) in the trap frame.
pub const FRAME_A0: u64 = 0x24;
/// The offset of `a1` (`x11`) in the trap frame.
pub const FRAME_A1: u64 = 0x28;
/// The offset of `a2` (`x12`) in the trap frame.
pub const FRAME_A2: u64 = 0x2C;
/// The offset of `a7` (`x17`) in the trap frame.
pub const FRAME_A7: u64 = 0x40;

/// The index of `a0` (`x10`) in a context's registers.
pub const A0: usize = 9;

/// A decoded syscall number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Syscall {
    /// 64.
    Write,
    /// 93.
    Exit,
    /// 94.
    ExitGroup,
    /// 124.
    SchedYield,
    /// 172.
    GetPid,
    /// Any other number: `-ENOSYS`.
    Unsupported(u32),
}

impl Syscall {
    /// The syscall `a7 = nr` asks for.
    pub fn decode(nr: u32) -> Syscall {
        match nr {
            SYS_WRITE => Syscall::Write,
            SYS_EXIT => Syscall::Exit,
            SYS_EXIT_GROUP => Syscall::ExitGroup,
            SYS_SCHED_YIELD => Syscall::SchedYield,
            SYS_GETPID => Syscall::GetPid,
            other => Syscall::Unsupported(other),
        }
    }
}

/// `-errno` as the `a0` bit pattern.
pub fn neg(errno: u32) -> u32 {
    errno.wrapping_neg()
}

/// A `write` buffer being checked or output: the caller's `sepc`, the buffer's VA, and
/// `n`, the number of bytes to output (1 to [`WRITE_MAX`]; the range does not wrap).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Buffer {
    /// The caller's `sepc`, the `ecall`'s address.
    pub sepc: u32,
    /// The buffer's first VA.
    pub buf: u32,
    /// The bytes to output.
    pub n: u32,
}

impl Buffer {
    /// A buffer of `n` bytes at `buf`, or `None` unless `n` is 1 to [`WRITE_MAX`] and
    /// `[buf, buf + n)` stays below 2³².
    pub fn new(sepc: u32, buf: u32, n: u32) -> Option<Buffer> {
        let fits = (1..=WRITE_MAX).contains(&n) && u64::from(buf) + u64::from(n) <= 1 << 32;
        fits.then_some(Buffer { sepc, buf, n })
    }

    /// The number of pages `[buf, buf + n)` touches: 1 or 2.
    pub fn pages(&self) -> usize {
        let first = u64::from(self.buf) / PAGE;
        let last = (u64::from(self.buf) + u64::from(self.n) - 1) / PAGE;
        (last - first + 1) as usize
    }

    /// The VA of the buffer's page `i`.
    pub fn page_va(&self, i: usize) -> u32 {
        (self.buf & !0xFFF) + (i as u32) * PAGE as u32
    }

    /// The output chunk at `done` bytes in: `(va, len)`, at most 16 bytes that cross no
    /// page, as [`crate::core::chunks`] splits the buffer.
    pub fn chunk(&self, done: u32) -> (u32, u32) {
        let va = self.buf + done;
        let len = (self.n - done).min(16).min(PAGE as u32 - va % PAGE as u32);
        (va, len)
    }

    /// Whether `done` is where a chunk starts: 0, or the end of an earlier chunk.
    pub fn is_chunk_start(&self, done: u32) -> bool {
        let mut at = 0;
        while at < self.n {
            if at == done {
                return true;
            }
            at += self.chunk(at).1;
        }
        false
    }
}

/// One step of the user-copy walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WalkStep {
    /// A pointer: read the level-0 PTE in the table at this PPN.
    Next(u32),
    /// A readable user leaf: the buffer's page is the physical page at this PPN.
    Page(u32),
    /// Not a readable user mapping: `-EFAULT`.
    Fault,
}

/// The physical address of the level-`level` PTE for `va` in the table at `table`.
pub fn pte_address(table: u32, va: u32, level: u8) -> u64 {
    let vpn = if level == 1 {
        va >> 22
    } else {
        va >> 12 & 0x3FF
    };
    u64::from(table) * PAGE + 4 * u64::from(vpn)
}

/// Decides the level-`level` PTE `pte` for a U-mode read of `va` with `SUM = MXR = 0`
/// (the privileged specification's "Virtual Address Translation Process").
pub fn walk_step(pte: u32, level: u8, va: u32) -> WalkStep {
    if pte & V == 0 || (pte & R == 0 && pte & W != 0) {
        return WalkStep::Fault;
    }
    let ppn = pte >> 10;
    if pte & (R | X) == 0 {
        // A pointer. D, A, and U are reserved in a non-leaf PTE; the CPU faults on them,
        // and so does the user copy.
        return if level == 1 && pte & (D | A | U) == 0 {
            WalkStep::Next(ppn)
        } else {
            WalkStep::Fault
        };
    }
    if pte & U == 0 || pte & R == 0 || pte & A == 0 {
        return WalkStep::Fault;
    }
    if level == 1 {
        // A megapage: its PPN[0] must be zero, and the page is PPN[1] with VPN[0].
        if ppn & 0x3FF != 0 {
            return WalkStep::Fault;
        }
        return WalkStep::Page(ppn | (va >> 12 & 0x3FF));
    }
    WalkStep::Page(ppn)
}

/// The whole walk of `va` from the root table at `root`, reading each PTE with
/// `read_pte`: the physical address, or `None` for `-EFAULT`. The operation takes the
/// same steps one bus access at a time; this form exists for tests against the CPU's
/// `sv32_translate`.
pub fn translate(root: u32, va: u32, mut read_pte: impl FnMut(u64) -> u32) -> Option<u64> {
    let mut table = root;
    for level in [1, 0] {
        match walk_step(read_pte(pte_address(table, va, level)), level, va) {
            WalkStep::Next(next) => table = next,
            WalkStep::Page(ppn) => return Some(u64::from(ppn) * PAGE + u64::from(va & 0xFFF)),
            WalkStep::Fault => return None,
        }
    }
    None
}
