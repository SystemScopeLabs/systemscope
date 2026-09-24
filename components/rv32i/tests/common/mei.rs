//! The machine external interrupt, written from `docs/m2-design.md` §5.1, §5.2, and §5.8
//! alone. It shares no code with the crate: every constant and bit position is spelled out
//! here, and nothing in `systemscope_rv32i` is called.

/// `mstatus.MIE`.
const MSTATUS_MIE: u32 = 1 << 3;
/// `mstatus.MPIE`.
const MSTATUS_MPIE: u32 = 1 << 7;
/// `mstatus.MPP`.
const MSTATUS_MPP: u32 = 0b11 << 11;
/// `mie.MEIE` and `mip.MEIP`: bit 11.
const MEI: u32 = 1 << 11;

/// The interrupt bit and exception code 11.
pub const MEI_CAUSE: u32 = 0x8000_000b;

/// The oracle's answer at one boundary (§5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeiResult {
    /// Whether the interrupt is taken.
    pub taken: bool,
    /// `mtvec & !3` when taken, else `pc`.
    pub new_pc: u32,
    /// `Some(pc)` when taken; `None` = unchanged.
    pub mepc: Option<u32>,
    /// `Some(0x8000_000B)` when taken; `None` = unchanged.
    pub mcause: Option<u32>,
    /// `Some(0)` when taken; `None` = unchanged.
    pub mtval: Option<u32>,
    /// MPIE ← MIE, MIE ← 0, MPP = `0b11` when taken, else `mstatus`.
    pub new_mstatus: u32,
}

/// §5.8: `pc` is the next PC at the boundary; `mstatus`, `mie`, `mip`, and `mtvec` are the
/// CSR values after the retiring instruction.
pub fn take_mei(pc: u32, mstatus: u32, mie: u32, mip: u32, mtvec: u32) -> MeiResult {
    let eligible = mstatus & MSTATUS_MIE != 0 && mie & MEI != 0 && mip & MEI != 0;
    if !eligible {
        return MeiResult {
            taken: false,
            new_pc: pc,
            mepc: None,
            mcause: None,
            mtval: None,
            new_mstatus: mstatus,
        };
    }
    let old_mie = mstatus & MSTATUS_MIE != 0;
    let mut new_mstatus = mstatus & !(MSTATUS_MIE | MSTATUS_MPIE);
    if old_mie {
        new_mstatus |= MSTATUS_MPIE;
    }
    new_mstatus |= MSTATUS_MPP;
    MeiResult {
        taken: true,
        new_pc: mtvec & !3,
        mepc: Some(pc),
        mcause: Some(MEI_CAUSE),
        mtval: Some(0),
        new_mstatus,
    }
}

/// What follows a successful retirement (§5.1), in the frozen order: the instruction's
/// effects are already applied and `instret` already counts it; then the instruction limit;
/// then the interrupt; then the next fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// `instret` reached the limit: the CPU halts and takes no interrupt.
    InstructionLimit,
    /// The CPU fetches next at `mei.new_pc`, after the interrupt if `mei.taken`.
    Fetch(MeiResult),
}

/// The boundary after a retirement that left `instret` retired instructions, the next PC
/// `pc`, and the CSRs as given.
pub fn boundary(
    instret: u64,
    max_instructions: u64,
    pc: u32,
    mstatus: u32,
    mie: u32,
    mip: u32,
    mtvec: u32,
) -> Boundary {
    if instret >= max_instructions {
        return Boundary::InstructionLimit;
    }
    Boundary::Fetch(take_mei(pc, mstatus, mie, mip, mtvec))
}
