//! The M3 privileged subset: privilege modes, the supervisor CSRs, the CSR access rule,
//! `SRET`, `SFENCE.VMA`, delegated exception entry, and the machine external interrupt
//! with modes (`docs/m3-design.md` §5.1, §5.3).
//!
//! Everything here is pure, and only the `M3` profile
//! ([`Rv32iProfile`](crate::cpu::Rv32iProfile)) uses it. It extends the M2 [`CsrFile`]
//! without changing it: [`M3State`] holds what the `M3` profile adds (the mode, the
//! `mstatus` fields M2 does not have, and the supervisor CSRs), and every operation that
//! needs the machine CSRs takes the M2 file as an argument. The `M2` profile's rules
//! (`MPP` hardwired to `0b11`, [`CsrFile::mret`], [`CsrFile::mei_eligible`]) are untouched.
//!
//! Privilege-valued fields have types that hold only supported modes ([`Privilege`],
//! [`Spp`]), so the reserved encoding 2 cannot be represented (§5.1).
//!
//! From M3.3 both `satp` modes, Bare and Sv32, are supported (§5.1). [`M3State::translates`]
//! says whether an access is translated; [`crate::sv32`] has the walk.

use crate::csr::{self, CsrFile, MCAUSE, MEPC, MIE, MIP, MSCRATCH, MSTATUS, MTVAL, MTVEC};
use crate::execute::TrapCause;

/// `sstatus`.
pub const SSTATUS: u16 = 0x100;
/// `sie`.
pub const SIE: u16 = 0x104;
/// `stvec`.
pub const STVEC: u16 = 0x105;
/// `sscratch`.
pub const SSCRATCH: u16 = 0x140;
/// `sepc`.
pub const SEPC: u16 = 0x141;
/// `scause`.
pub const SCAUSE: u16 = 0x142;
/// `stval`.
pub const STVAL: u16 = 0x143;
/// `sip`.
pub const SIP: u16 = 0x144;
/// `satp`.
pub const SATP: u16 = 0x180;
/// `medeleg`.
pub const MEDELEG: u16 = 0x302;
/// `mideleg`.
pub const MIDELEG: u16 = 0x303;

/// The `M3` whitelist, in inspect order: the eight M2 CSRs ([`csr::SUPPORTED`]), then the
/// ones §5.1 adds. Every other CSR number is unsupported.
pub const SUPPORTED: [u16; 19] = [
    MSTATUS, MIE, MIP, MTVEC, MSCRATCH, MEPC, MCAUSE, MTVAL, SSTATUS, SIE, SIP, STVEC, SSCRATCH,
    SEPC, SCAUSE, STVAL, SATP, MEDELEG, MIDELEG,
];

/// The `SRET` word.
pub const SRET: u32 = 0x1020_0073;

/// `mstatus.SIE`.
pub const MSTATUS_SIE: u32 = 1 << 1;
/// `mstatus.SPIE`.
pub const MSTATUS_SPIE: u32 = 1 << 5;
/// `mstatus.SPP`.
pub const MSTATUS_SPP: u32 = 1 << 8;
/// `mstatus.MPP`'s shift: the field is bits 12:11.
pub const MSTATUS_MPP_SHIFT: u32 = 11;
/// `mstatus.SUM`.
pub const MSTATUS_SUM: u32 = 1 << 18;
/// `mstatus.MXR`.
pub const MSTATUS_MXR: u32 = 1 << 19;
/// The `mstatus` bits `sstatus` shows and writes: `SIE`, `SPIE`, `SPP`, `SUM`, `MXR`.
pub const SSTATUS_MASK: u32 = MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_SUM | MSTATUS_MXR;

/// `medeleg`'s writable bits: causes 0–8, 12, 13, and 15. Bit 9 (`ecall` from S) and bit
/// 11 (`ecall` from M) read 0.
pub const MEDELEG_MASK: u32 = 0xB1FF;

/// `satp.MODE`: bit 31, 0 for Bare and 1 for Sv32.
pub const SATP_MODE: u32 = 1 << 31;
/// `satp.ASID`: bits 30:22, always 0.
pub const SATP_ASID: u32 = 0x1FF << 22;
/// `satp.PPN`: bits 21:0.
pub const SATP_PPN: u32 = 0x003F_FFFF;

/// A privilege mode the `M3` profile supports. The reserved encoding 2 has no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Privilege {
    /// User mode, encoding 0.
    User,
    /// Supervisor mode, encoding 1.
    Supervisor,
    /// Machine mode, encoding 3.
    Machine,
}

impl Privilege {
    /// The architectural encoding: 0, 1, or 3.
    pub const fn bits(self) -> u8 {
        match self {
            Privilege::User => 0,
            Privilege::Supervisor => 1,
            Privilege::Machine => 3,
        }
    }

    /// The mode encoded as `bits`; `None` for the reserved 2 and anything above 3.
    pub const fn from_bits(bits: u8) -> Option<Privilege> {
        match bits {
            0 => Some(Privilege::User),
            1 => Some(Privilege::Supervisor),
            3 => Some(Privilege::Machine),
            _ => None,
        }
    }

    /// `U`, `S`, or `M`, as the `rv32.exception` and `rv32.interrupt` records show it.
    pub const fn name(self) -> &'static str {
        match self {
            Privilege::User => "U",
            Privilege::Supervisor => "S",
            Privilege::Machine => "M",
        }
    }
}

/// `mstatus.SPP`: the mode before the last delegated trap, U or S.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spp {
    /// User mode, encoding 0.
    User,
    /// Supervisor mode, encoding 1.
    Supervisor,
}

impl Spp {
    /// The architectural encoding: 0 or 1.
    pub const fn bits(self) -> u8 {
        match self {
            Spp::User => 0,
            Spp::Supervisor => 1,
        }
    }

    /// The value encoded as `bits`; `None` above 1.
    pub const fn from_bits(bits: u8) -> Option<Spp> {
        match bits {
            0 => Some(Spp::User),
            1 => Some(Spp::Supervisor),
            _ => None,
        }
    }

    /// The mode `SRET` returns to.
    pub const fn privilege(self) -> Privilege {
        match self {
            Spp::User => Privilege::User,
            Spp::Supervisor => Privilege::Supervisor,
        }
    }
}

/// `MPP` as a write legalizes it (§5.1): U, S, and M are stored as written, and the
/// reserved `0b10` is stored as U.
pub const fn legal_mpp(field: u32) -> Privilege {
    match field & 0b11 {
        1 => Privilege::Supervisor,
        3 => Privilege::Machine,
        _ => Privilege::User,
    }
}

/// Whether `csr` is on the `M3` whitelist.
pub fn is_supported(csr: u16) -> bool {
    SUPPORTED.contains(&csr)
}

/// The CSR access rule (§5.1): bits `[9:8]` of the number are the lowest mode that may
/// access it, and bits `[11:10]` = `0b11` make it read-only, so an instruction that writes
/// it is illegal. `writes` is [`PrivInstr::writes`](crate::csr::PrivInstr::writes).
pub fn accessible(csr: u16, privilege: Privilege, writes: bool) -> bool {
    let lowest = ((csr >> 8) & 0b11) as u8;
    let read_only = (csr >> 10) & 0b11 == 0b11;
    privilege.bits() >= lowest && !(writes && read_only)
}

/// `satp` after a write of `value` (§5.1). From M3.3 both `MODE` values, Bare and Sv32, are
/// supported, so a write stores `MODE` and `PPN` as written and `ASID` as 0, and the old
/// value never matters.
pub const fn satp_write(value: u32) -> u32 {
    value & !SATP_ASID
}

/// Whether some sequence of writes from reset can leave `satp` at `value`: `ASID` 0, with
/// either `MODE`. Restore rejects any other value (§5.5).
pub const fn satp_reachable(value: u32) -> bool {
    value & SATP_ASID == 0
}

/// A privileged instruction the `M3` profile adds to the M2 ones
/// ([`decode_privileged`](crate::csr::decode_privileged)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SupervisorInstr {
    /// `SRET`.
    Sret,
    /// `SFENCE.VMA`, with any `rs1` and `rs2`.
    SfenceVma,
}

/// Decodes `SRET` and `SFENCE.VMA` (`funct7` `0001001`, `rd` = 0, `funct3` = 0, `SYSTEM`,
/// any `rs1` and `rs2`). Every other word is `None`; in particular `WFI` stays illegal
/// (§5.1).
pub fn decode_supervisor(word: u32) -> Option<SupervisorInstr> {
    if word == SRET {
        Some(SupervisorInstr::Sret)
    } else if word & 0xFE00_7FFF == 0x1200_0073 {
        Some(SupervisorInstr::SfenceVma)
    } else {
        None
    }
}

/// The state the `M3` profile adds to the M2 [`CsrFile`]: the current mode, the `mstatus`
/// fields M2 does not store, and the supervisor CSRs (§5.1). `MIE` and `MPIE` stay in the
/// M2 file. `sstatus` is a view, and `mideleg`, `sie`, and `sip` are constant 0, so none
/// of them is stored (§5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct M3State {
    /// The current mode.
    pub privilege: Privilege,
    /// `mstatus.SIE`.
    pub sie: bool,
    /// `mstatus.SPIE`.
    pub spie: bool,
    /// `mstatus.SPP`.
    pub spp: Spp,
    /// `mstatus.MPP`.
    pub mpp: Privilege,
    /// `mstatus.SUM`.
    pub sum: bool,
    /// `mstatus.MXR`.
    pub mxr: bool,
    /// `medeleg`, within [`MEDELEG_MASK`].
    pub medeleg: u32,
    /// `stvec`, bits `[1:0]` always 0.
    pub stvec: u32,
    /// `sscratch`.
    pub sscratch: u32,
    /// `sepc`, bits `[1:0]` always 0.
    pub sepc: u32,
    /// `scause`.
    pub scause: u32,
    /// `stval`.
    pub stval: u32,
    /// `satp`, always [`satp_reachable`].
    pub satp: u32,
}

impl Default for M3State {
    fn default() -> M3State {
        M3State::new()
    }
}

impl M3State {
    /// The reset state (§5.1): M-mode, `MPP` = U, and every field 0. With the M2 file at
    /// its reset, `mstatus` reads 0.
    pub const fn new() -> M3State {
        M3State {
            privilege: Privilege::Machine,
            sie: false,
            spie: false,
            spp: Spp::User,
            mpp: Privilege::User,
            sum: false,
            mxr: false,
            medeleg: 0,
            stvec: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
        }
    }

    /// `mstatus` as read in the `M3` profile: `SIE`, `MIE`, `SPIE`, `MPIE`, `SPP`, `MPP`,
    /// `SUM`, and `MXR`; every other bit 0.
    pub fn mstatus(&self, m: &CsrFile) -> u32 {
        self.sstatus()
            | u32::from(m.mie) << 3
            | u32::from(m.mpie) << 7
            | u32::from(self.mpp.bits()) << MSTATUS_MPP_SHIFT
    }

    /// `sstatus`: `SIE`, `SPIE`, `SPP`, `SUM`, and `MXR`; every other bit 0.
    pub fn sstatus(&self) -> u32 {
        u32::from(self.sie) << 1
            | u32::from(self.spie) << 5
            | u32::from(self.spp.bits()) << 8
            | u32::from(self.sum) << 18
            | u32::from(self.mxr) << 19
    }

    /// Reads `csr` against the M2 file `m`; `None` if it is not on the `M3` whitelist. No
    /// CSR has a read side effect. Access rights are the caller's ([`accessible`]).
    pub fn read(&self, m: &CsrFile, csr: u16) -> Option<u32> {
        Some(match csr {
            MSTATUS => self.mstatus(m),
            SSTATUS => self.sstatus(),
            MEDELEG => self.medeleg,
            MIDELEG | SIE | SIP => 0,
            STVEC => self.stvec,
            SSCRATCH => self.sscratch,
            SEPC => self.sepc,
            SCAUSE => self.scause,
            STVAL => self.stval,
            SATP => self.satp,
            MIE | MIP | MTVEC | MSCRATCH | MEPC | MCAUSE | MTVAL => m.read(csr)?,
            _ => return None,
        })
    }

    /// Writes `value` to `csr` under its `M3` rule (§5.1), with the machine CSRs in `m`.
    /// Returns `None`, changing nothing, if `csr` is not on the whitelist.
    pub fn write(&mut self, m: &mut CsrFile, csr: u16, value: u32) -> Option<()> {
        match csr {
            MSTATUS => {
                m.mie = value & csr::MSTATUS_MIE != 0;
                m.mpie = value & csr::MSTATUS_MPIE != 0;
                self.mpp = legal_mpp(value >> MSTATUS_MPP_SHIFT);
                self.write_sstatus(value);
            }
            SSTATUS => self.write_sstatus(value),
            MEDELEG => self.medeleg = value & MEDELEG_MASK,
            MIDELEG | SIE | SIP => {}
            STVEC => self.stvec = value & !0b11,
            SSCRATCH => self.sscratch = value,
            SEPC => self.sepc = value & !0b11,
            SCAUSE => self.scause = value,
            STVAL => self.stval = value,
            SATP => self.satp = satp_write(value),
            MIE | MIP | MTVEC | MSCRATCH | MEPC | MCAUSE | MTVAL => m.write(csr, value)?,
            _ => return None,
        }
        Some(())
    }

    /// The `sstatus` fields of a write.
    fn write_sstatus(&mut self, value: u32) {
        self.sie = value & MSTATUS_SIE != 0;
        self.spie = value & MSTATUS_SPIE != 0;
        self.spp = if value & MSTATUS_SPP != 0 {
            Spp::Supervisor
        } else {
            Spp::User
        };
        self.sum = value & MSTATUS_SUM != 0;
        self.mxr = value & MSTATUS_MXR != 0;
    }

    /// `MRET` in the `M3` profile (§5.1), legal only in M: `priv` ← `MPP`, `MIE` ← `MPIE`,
    /// `MPIE` ← 1, `MPP` ← U. Returns the new `pc`, `mepc`.
    pub fn mret(&mut self, m: &mut CsrFile) -> u32 {
        self.privilege = self.mpp;
        self.mpp = Privilege::User;
        m.mie = m.mpie;
        m.mpie = true;
        m.mepc
    }

    /// `SRET` (§5.1), legal in S and M: `priv` ← `SPP`, `SIE` ← `SPIE`, `SPIE` ← 1,
    /// `SPP` ← U. Returns the new `pc`, `sepc`.
    pub fn sret(&mut self) -> u32 {
        self.privilege = self.spp.privilege();
        self.spp = Spp::User;
        self.sie = self.spie;
        self.spie = true;
        self.sepc
    }

    /// Whether fetches, loads, and stores are translated (§5.2): the mode is S or U and
    /// `satp.MODE` is Sv32. M-mode is always bare.
    pub const fn translates(&self) -> bool {
        !matches!(self.privilege, Privilege::Machine) && self.satp & SATP_MODE != 0
    }

    /// The root page table's PPN, `satp.PPN`.
    pub const fn root(&self) -> u32 {
        self.satp & SATP_PPN
    }

    /// Whether a synchronous exception with `cause` is delegated to S (§5.3): it happened
    /// below M, and its `medeleg` bit is set.
    pub fn delegates(&self, cause: TrapCause) -> bool {
        self.privilege != Privilege::Machine && self.medeleg >> cause.code() & 1 == 1
    }

    /// Delivers a synchronous exception of the instruction at `pc` to S if it is delegated
    /// (§5.3): `sepc` ← `pc`, `scause` ← the cause's code, `stval` ← `tval`, `SPP` ← the
    /// mode it happened in, `SPIE` ← `SIE`, `SIE` ← 0, `priv` ← S. Returns the new `pc`,
    /// `stvec` BASE; or `None`, changing nothing, if the exception is not delegated.
    pub fn take_exception(&mut self, cause: TrapCause, pc: u32, tval: u32) -> Option<u32> {
        if !self.delegates(cause) {
            return None;
        }
        self.spp = match self.privilege {
            Privilege::User => Spp::User,
            Privilege::Supervisor => Spp::Supervisor,
            Privilege::Machine => return None,
        };
        self.sepc = pc;
        self.scause = cause.code();
        self.stval = tval;
        self.spie = self.sie;
        self.sie = false;
        self.privilege = Privilege::Supervisor;
        Some(self.stvec & !0b11)
    }

    /// Whether the machine external interrupt is eligible in the `M3` profile (§5.1):
    /// `MEIP` and `MEIE` are set, and either the hart is below M or `MIE` is set.
    pub fn mei_eligible(&self, m: &CsrFile) -> bool {
        m.irq_level && m.meie && (self.privilege < Privilege::Machine || m.mie)
    }

    /// The machine external interrupt's entry in the `M3` profile at a boundary whose next
    /// PC is `next_pc` (§5.1): `MPP` ← `priv` and `priv` ← M, then the M2 entry
    /// ([`CsrFile::take_mei`]). `mideleg` is 0, so it is always taken in M. Returns the new
    /// `pc`, `mtvec` BASE.
    pub fn take_mei(&mut self, m: &mut CsrFile, next_pc: u32) -> u32 {
        self.mpp = self.privilege;
        self.privilege = Privilege::Machine;
        m.take_mei(next_pc)
    }
}
