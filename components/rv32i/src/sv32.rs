//! Sv32 translation (`docs/m3-design.md` §5.2), pure.
//!
//! The walk is the privileged specification's Sv32 algorithm with §5.2's fixed choices,
//! checked against the pinned Spike in `docs/m3-3-spike-appendix.md`:
//!
//! - a PTE is invalid if `V` = 0, or `R` = 0 with `W` = 1;
//! - a pointer (`R` = `W` = `X` = 0) at level 0 faults, and so does a pointer with any of
//!   `D`, `A`, `U` set: the specification reserves them in a non-leaf PTE and raises a page
//!   fault when a reserved bit is set (appendix C.4 item 4);
//! - a level-1 leaf with `PPN[0]` ≠ 0 is a misaligned megapage;
//! - fetch needs `X`, load needs `R` (or `X` with `MXR`), store needs `W`; U-mode needs
//!   `U` = 1, and S-mode faults on a `U` = 1 page for a fetch always and for a load or
//!   store unless `SUM` = 1;
//! - Svade: `A` = 0, or `D` = 0 on a store, is a page fault. Nothing here writes a PTE;
//! - `G` and RSW are ignored.
//!
//! Every fault is a page fault of the access type, except that a PTE read the bus refuses
//! is the access's access fault ([`Fault::Access`]); either way `tval` is the virtual
//! address (§5.3). Alignment is checked before any walk, by the caller.
//!
//! The CPU walks one PTE read at a time over the bus (§5.4): it computes each PTE's
//! address with [`pte_address`], and feeds each PTE it reads to [`step`].
//! [`sv32_translate`] runs the same steps against a synchronous PTE reader, as the
//! reference for the whole walk.

use crate::execute::TrapCause;
use crate::privilege::{Privilege, SATP_MODE, SATP_PPN};

/// `V`.
pub const PTE_V: u32 = 1 << 0;
/// `R`.
pub const PTE_R: u32 = 1 << 1;
/// `W`.
pub const PTE_W: u32 = 1 << 2;
/// `X`.
pub const PTE_X: u32 = 1 << 3;
/// `U`.
pub const PTE_U: u32 = 1 << 4;
/// `G`, ignored.
pub const PTE_G: u32 = 1 << 5;
/// `A`.
pub const PTE_A: u32 = 1 << 6;
/// `D`.
pub const PTE_D: u32 = 1 << 7;

/// The page size, 4 KiB.
pub const PAGE_SIZE: u64 = 4096;

/// A physical address has 34 bits: a 22-bit PPN and a 12-bit offset.
pub const PA_BITS: u32 = 34;

/// What the translated access does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// An instruction fetch.
    Fetch,
    /// A load.
    Load,
    /// A store.
    Store,
}

impl Access {
    /// The page fault of this access type.
    pub const fn page_fault(self) -> TrapCause {
        match self {
            Access::Fetch => TrapCause::InstructionPageFault,
            Access::Load => TrapCause::LoadPageFault,
            Access::Store => TrapCause::StorePageFault,
        }
    }

    /// The access fault of this access type.
    pub const fn access_fault(self) -> TrapCause {
        match self {
            Access::Fetch => TrapCause::InstructionAccessFault,
            Access::Load => TrapCause::LoadAccessFault,
            Access::Store => TrapCause::StoreAccessFault,
        }
    }
}

/// What a walk checks the leaf against, sampled when the walk starts (§5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Context {
    /// The mode the access runs in, S or U.
    pub privilege: Privilege,
    /// `mstatus.SUM`.
    pub sum: bool,
    /// `mstatus.MXR`.
    pub mxr: bool,
    /// The access.
    pub access: Access,
}

/// A walk that did not produce a physical address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fault {
    /// A page fault of the access type.
    Page,
    /// A PTE read the bus refused: the access fault of the access type.
    Access,
}

impl Fault {
    /// The trap cause this fault raises for `access`.
    pub const fn cause(self, access: Access) -> TrapCause {
        match self {
            Fault::Page => access.page_fault(),
            Fault::Access => access.access_fault(),
        }
    }
}

/// What one PTE means for the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// A valid pointer at level 1: read the level-0 PTE in the table with this PPN.
    Next {
        /// The next table's PPN.
        table: u32,
    },
    /// A leaf that allows the access: the translated physical address.
    Leaf {
        /// The physical address, below 2^34.
        pa: u64,
    },
    /// The walk stops with a page fault.
    PageFault,
}

/// `VPN[level]` of `va`: bits 21:12 for level 0, bits 31:22 for level 1.
pub const fn vpn(va: u32, level: u8) -> u32 {
    (va >> (12 + 10 * level as u32)) & 0x3FF
}

/// The address of the level-`level` PTE for `va` in the table with PPN `table`:
/// `table × 4096 + VPN[level] × 4`.
pub const fn pte_address(table: u32, va: u32, level: u8) -> u64 {
    table as u64 * PAGE_SIZE + vpn(va, level) as u64 * 4
}

/// What the level-`level` PTE `pte` means for an access to `va` under `ctx`.
pub fn step(ctx: &Context, va: u32, level: u8, pte: u32) -> Step {
    let has = |bit: u32| pte & bit != 0;
    if !has(PTE_V) || (!has(PTE_R) && has(PTE_W)) {
        return Step::PageFault;
    }
    let ppn = pte >> 10;
    if !has(PTE_R) && !has(PTE_X) {
        // A pointer. D, A, and U are reserved in a non-leaf PTE.
        if level == 0 || has(PTE_D | PTE_A | PTE_U) {
            return Step::PageFault;
        }
        return Step::Next { table: ppn };
    }
    if level == 1 && ppn & 0x3FF != 0 {
        return Step::PageFault;
    }
    let user_ok = match ctx.privilege {
        Privilege::User => has(PTE_U),
        Privilege::Supervisor => !has(PTE_U) || (ctx.sum && ctx.access != Access::Fetch),
        Privilege::Machine => true,
    };
    let allowed = match ctx.access {
        Access::Fetch => has(PTE_X),
        Access::Load => has(PTE_R) || (ctx.mxr && has(PTE_X)),
        Access::Store => has(PTE_W),
    };
    let dirty_ok = ctx.access != Access::Store || has(PTE_D);
    if !user_ok || !allowed || !has(PTE_A) || !dirty_ok {
        return Step::PageFault;
    }
    let pa = if level == 1 {
        u64::from(pte >> 20) << 22 | u64::from(va & 0x003F_FFFF)
    } else {
        u64::from(ppn) << 12 | u64::from(va & 0xFFF)
    };
    Step::Leaf { pa }
}

/// Translates `va` for `access` in `privilege` under `satp`, `SUM`, and `MXR`, reading each
/// PTE with `read_pte`, which returns `None` when the bus refuses the read. An access in M,
/// or under a Bare `satp`, is not translated: the result is `va` itself.
pub fn sv32_translate(
    satp: u32,
    privilege: Privilege,
    sum: bool,
    mxr: bool,
    access: Access,
    va: u32,
    mut read_pte: impl FnMut(u64) -> Option<u32>,
) -> Result<u64, Fault> {
    if privilege == Privilege::Machine || satp & SATP_MODE == 0 {
        return Ok(u64::from(va));
    }
    let ctx = Context {
        privilege,
        sum,
        mxr,
        access,
    };
    let mut table = satp & SATP_PPN;
    let mut level = 1;
    loop {
        let pte = read_pte(pte_address(table, va, level)).ok_or(Fault::Access)?;
        match step(&ctx, va, level, pte) {
            Step::Next { table: next } => {
                table = next;
                level -= 1;
            }
            Step::Leaf { pa } => return Ok(pa),
            Step::PageFault => return Err(Fault::Page),
        }
    }
}
