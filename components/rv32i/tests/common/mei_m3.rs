//! The machine external interrupt in the `M3` profile, written from `docs/m3-design.md`
//! §5.1 alone. It shares no code with the crate or with the M2 oracle ([`super::mei`]):
//! every constant and bit position is spelled out here, and nothing in `systemscope_rv32i`
//! is called.

/// `mstatus.MIE`.
const MSTATUS_MIE: u32 = 1 << 3;
/// `mstatus.MPIE`.
const MSTATUS_MPIE: u32 = 1 << 7;
/// `mstatus.MPP`: bits 12:11.
const MSTATUS_MPP: u32 = 0b11 << 11;
/// `mie.MEIE` and `mip.MEIP`: bit 11.
const MEI: u32 = 1 << 11;
/// Machine mode's encoding.
const M: u8 = 3;

/// The interrupt bit and exception code 11.
pub const MEI_CAUSE: u32 = 0x8000_000b;

/// The oracle's answer at one retirement boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeiM3 {
    /// Whether the interrupt is taken.
    pub taken: bool,
    /// `mtvec & !3` when taken, else `pc`.
    pub new_pc: u32,
    /// M when taken, else `privilege`.
    pub new_priv: u8,
    /// `(mepc, mcause, mtval)` = `(pc, 0x8000_000B, 0)` when taken; `None` = unchanged.
    pub entry: Option<(u32, u32, u32)>,
    /// When taken: MPIE ← MIE, MIE ← 0, MPP ← `privilege`; every other bit kept.
    pub new_mstatus: u32,
}

/// §5.1: the interrupt is eligible when MEIP and MEIE are set and the hart is below M or
/// MIE is set. `pc` is the next PC at the boundary; `privilege` (0, 1, or 3) and the CSRs
/// are the values after the retiring instruction.
pub fn take_mei_m3(pc: u32, privilege: u8, mstatus: u32, mie: u32, mip: u32, mtvec: u32) -> MeiM3 {
    let enabled = privilege < M || mstatus & MSTATUS_MIE != 0;
    if !(mie & MEI != 0 && mip & MEI != 0 && enabled) {
        return MeiM3 {
            taken: false,
            new_pc: pc,
            new_priv: privilege,
            entry: None,
            new_mstatus: mstatus,
        };
    }
    let mut new_mstatus = mstatus & !(MSTATUS_MIE | MSTATUS_MPIE | MSTATUS_MPP);
    if mstatus & MSTATUS_MIE != 0 {
        new_mstatus |= MSTATUS_MPIE;
    }
    new_mstatus |= u32::from(privilege) << 11;
    MeiM3 {
        taken: true,
        new_pc: mtvec & !3,
        new_priv: M,
        entry: Some((pc, MEI_CAUSE, 0)),
        new_mstatus,
    }
}
