//! The M2 privileged subset: Zicsr decoding, the eight machine CSRs, and `MRET`
//! (`docs/m2-design.md` §4, §5.3).
//!
//! SystemScope M2 implements a documented subset of machine-level privileged architecture
//! required for machine-external interrupts. It does not implement full Sm.
//!
//! Everything here is pure. The CPU decodes these words only in its `M2` profile
//! ([`Rv32iProfile`](crate::cpu::Rv32iProfile)); the M1 [`decode()`](crate::decode()) keeps
//! every one of them illegal.

use crate::instr::Reg;

/// `mstatus`.
pub const MSTATUS: u16 = 0x300;
/// `mie`.
pub const MIE: u16 = 0x304;
/// `mtvec`.
pub const MTVEC: u16 = 0x305;
/// `mscratch`.
pub const MSCRATCH: u16 = 0x340;
/// `mepc`.
pub const MEPC: u16 = 0x341;
/// `mcause`.
pub const MCAUSE: u16 = 0x342;
/// `mtval`.
pub const MTVAL: u16 = 0x343;
/// `mip`.
pub const MIP: u16 = 0x344;

/// The whitelist (§4.3), in inspect order. Every other CSR number is unsupported (§4.5).
pub const SUPPORTED: [u16; 8] = [MSTATUS, MIE, MIP, MTVEC, MSCRATCH, MEPC, MCAUSE, MTVAL];

/// The `MRET` word.
pub const MRET: u32 = 0x3020_0073;

/// `mstatus.MIE`.
pub const MSTATUS_MIE: u32 = 1 << 3;
/// `mstatus.MPIE`.
pub const MSTATUS_MPIE: u32 = 1 << 7;
/// `mstatus.MPP`, hardwired to `0b11`.
pub const MSTATUS_MPP: u32 = 0b11 << 11;
/// `mie.MEIE` and `mip.MEIP`.
pub const MEI_BIT: u32 = 1 << 11;

/// Whether `csr` is one of the eight whitelisted CSRs.
pub fn is_supported(csr: u16) -> bool {
    SUPPORTED.contains(&csr)
}

/// A CSR instruction's operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CsrOp {
    /// `CSRRW`, `CSRRWI`: the CSR becomes the operand.
    Write,
    /// `CSRRS`, `CSRRSI`: the operand's bits are set.
    Set,
    /// `CSRRC`, `CSRRCI`: the operand's bits are cleared.
    Clear,
}

impl CsrOp {
    /// The value to store, before the CSR's own write rule, for `old` and `operand`.
    pub fn apply(self, old: u32, operand: u32) -> u32 {
        match self {
            CsrOp::Write => operand,
            CsrOp::Set => old | operand,
            CsrOp::Clear => old & !operand,
        }
    }
}

/// Where a CSR instruction's operand comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CsrSource {
    /// A register, read before the instruction.
    Reg(Reg),
    /// The 5-bit `uimm`, zero-extended.
    Imm(u8),
}

/// A privileged instruction of the M2 profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrivInstr {
    /// One of the six Zicsr instructions, on any CSR number: legality is decided by
    /// [`is_supported`] (§4.5).
    Csr {
        /// The operation.
        op: CsrOp,
        /// The destination register.
        rd: Reg,
        /// The operand.
        src: CsrSource,
        /// The 12-bit CSR number.
        csr: u16,
    },
    /// `MRET`.
    Mret,
}

impl PrivInstr {
    /// Whether a CSR instruction writes its CSR: `CSRRW` and `CSRRWI` always do;
    /// `CSRRS`/`CSRRC` not with `rs1 = x0`, `CSRRSI`/`CSRRCI` not with `uimm = 0` (§4.2).
    /// `MRET` is not a CSR instruction.
    pub fn writes(&self) -> bool {
        match *self {
            PrivInstr::Csr { op, src, .. } => {
                op == CsrOp::Write
                    || match src {
                        CsrSource::Reg(rs1) => rs1 != Reg::ZERO,
                        CsrSource::Imm(uimm) => uimm != 0,
                    }
            }
            PrivInstr::Mret => false,
        }
    }
}

/// Decodes the words the M2 profile adds to RV32I: the six Zicsr forms (`SYSTEM` with
/// `funct3` 1, 2, 3, 5, 6, or 7), with any field values, and `MRET`. Every other word is
/// `None`, including every other `SYSTEM` encoding (§4.2).
pub fn decode_privileged(word: u32) -> Option<PrivInstr> {
    if word == MRET {
        return Some(PrivInstr::Mret);
    }
    if word & 0x7f != 0b111_0011 {
        return None;
    }
    let funct3 = (word >> 12) & 0x7;
    let op = match funct3 & 0b11 {
        1 => CsrOp::Write,
        2 => CsrOp::Set,
        3 => CsrOp::Clear,
        _ => return None,
    };
    let src = if funct3 & 0b100 == 0 {
        CsrSource::Reg(Reg::field(word, 15))
    } else {
        CsrSource::Imm(((word >> 15) & 0x1f) as u8)
    };
    Some(PrivInstr::Csr {
        op,
        rd: Reg::field(word, 7),
        src,
        csr: (word >> 20) as u16,
    })
}

/// The machine CSR state of the M2 profile (§4.3): only the implemented bits are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CsrFile {
    /// `mstatus.MIE`.
    pub mie: bool,
    /// `mstatus.MPIE`.
    pub mpie: bool,
    /// `mie.MEIE`.
    pub meie: bool,
    /// `mtvec`, bits `[1:0]` always 0.
    pub mtvec: u32,
    /// `mscratch`.
    pub mscratch: u32,
    /// `mepc`, bits `[1:0]` always 0.
    pub mepc: u32,
    /// `mcause`.
    pub mcause: u32,
    /// `mtval`.
    pub mtval: u32,
    /// The `irq` input level, which `mip.MEIP` reads.
    pub irq_level: bool,
}

impl Default for CsrFile {
    fn default() -> CsrFile {
        CsrFile::new()
    }
}

impl CsrFile {
    /// The reset state: `mstatus` reads `0x1800`, every other CSR 0, the `irq` input low.
    pub const fn new() -> CsrFile {
        CsrFile {
            mie: false,
            mpie: false,
            meie: false,
            mtvec: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            irq_level: false,
        }
    }

    /// `mstatus` as read: MIE, MPIE, and MPP = `0b11`.
    pub fn mstatus(&self) -> u32 {
        u32::from(self.mie) << 3 | u32::from(self.mpie) << 7 | MSTATUS_MPP
    }

    /// Reads `csr`; `None` if it is unsupported. No supported CSR has a read side effect.
    pub fn read(&self, csr: u16) -> Option<u32> {
        Some(match csr {
            MSTATUS => self.mstatus(),
            MIE => u32::from(self.meie) << 11,
            MIP => u32::from(self.irq_level) << 11,
            MTVEC => self.mtvec,
            MSCRATCH => self.mscratch,
            MEPC => self.mepc,
            MCAUSE => self.mcause,
            MTVAL => self.mtval,
            _ => return None,
        })
    }

    /// Writes `value` to `csr` under its write rule (§4.3): unimplemented bits are
    /// dropped, and a write to `mip` is ignored. Returns `None`, changing nothing, if
    /// `csr` is unsupported.
    pub fn write(&mut self, csr: u16, value: u32) -> Option<()> {
        match csr {
            MSTATUS => {
                self.mie = value & MSTATUS_MIE != 0;
                self.mpie = value & MSTATUS_MPIE != 0;
            }
            MIE => self.meie = value & MEI_BIT != 0,
            MIP => {}
            MTVEC => self.mtvec = value & !0b11,
            MSCRATCH => self.mscratch = value,
            MEPC => self.mepc = value & !0b11,
            MCAUSE => self.mcause = value,
            MTVAL => self.mtval = value,
            _ => return None,
        }
        Some(())
    }

    /// `MRET`'s CSR effects (§5.3): MIE ← MPIE, MPIE ← 1, MPP stays `0b11`. Returns the
    /// new `pc`, `mepc`.
    pub fn mret(&mut self) -> u32 {
        self.mie = self.mpie;
        self.mpie = true;
        self.mepc
    }
}
