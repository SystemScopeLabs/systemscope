//! Pure instruction semantics (`docs/m1-design.md` §5.3).
//!
//! Execution computes what an instruction would do if it retired, as a [`PendingEffect`],
//! or the trap it raises instead, as a [`PendingTrap`]. It reads nothing but its arguments
//! and changes no state: the CPU applies the effect in `Commit`, and only if the
//! instruction retires.
//!
//! Each instruction family has its own function: [`execute_alu`], [`execute_control`], and
//! [`execute_system`] for `FENCE`, `ECALL`, and `EBREAK`.
//! Loads and stores need a memory response in between, so their semantics are split in
//! two halves in [`memory`](crate::memory).

use crate::instr::{BranchOp, ImmOp, Instr, Reg, RegOp, ShiftOp};

/// A register write an instruction makes when it retires.
///
/// `rd` may be `x0`: execution keeps the instruction's destination as written, and the
/// register file discards the write when it is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegWrite {
    /// The destination register.
    pub rd: Reg,
    /// The value written.
    pub value: u32,
}

/// The architectural effect of a retiring instruction, applied in `Commit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PendingEffect {
    /// The register write, if the instruction writes one.
    pub reg_write: Option<RegWrite>,
    /// The `pc` after the instruction.
    pub next_pc: u32,
}

/// A trap an instruction raises instead of retiring (`docs/m1-design.md` §6).
///
/// It carries no `pc`: the trapping instruction's address is the `pc` execution was
/// called with, and the CPU adds it when it records the trap in `Commit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PendingTrap {
    /// Why the instruction trapped.
    pub cause: TrapCause,
    /// The trap value, as §6 defines it for `cause`.
    pub tval: u32,
}

/// The cause of a trap.
///
/// Every cause of `docs/m1-design.md` §6. Pure execution raises most of them; the CPU
/// raises `InstructionAccessFault` on a faulting fetch and `IllegalInstruction` when
/// decoding fails. The `M3` profile adds the mode-dependent `ECALL` causes and the page
/// faults (`docs/m3-design.md` §5.3); M1 and M2 never raise them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrapCause {
    /// A taken branch, `JAL`, or `JALR` whose target is not 4-byte aligned. `tval` is the
    /// target.
    InstructionAddressMisaligned,
    /// A halfword or word load whose effective address is not naturally aligned. Raised
    /// before any request is sent. `tval` is the effective address.
    LoadAddressMisaligned,
    /// A load the memory system answered with `AccessFault`. `tval` is the effective
    /// address.
    LoadAccessFault,
    /// A halfword or word store whose effective address is not naturally aligned. Raised
    /// before any request is sent, so memory is never written. `tval` is the effective
    /// address.
    StoreAddressMisaligned,
    /// A store the memory system answered with `AccessFault`. `tval` is the effective
    /// address.
    StoreAccessFault,
    /// A fetch the memory system answered with `AccessFault`. `tval` is the `pc` of the
    /// instruction that could not be fetched.
    InstructionAccessFault,
    /// A word that does not decode to an RV32I instruction (§5.2). `tval` is the word.
    IllegalInstruction,
    /// `EBREAK`. `tval` is its `pc`.
    Breakpoint,
    /// `ECALL`, the normal way an M1 program ends. `tval` is 0. In the `M3` profile it is
    /// `ECALL` from M-mode, and M3 traces name it `EnvironmentCallFromM`
    /// (`docs/m3-design.md` §5.3).
    EnvironmentCall,
    /// `ECALL` from U-mode (`M3` only). `tval` is 0.
    EnvironmentCallFromU,
    /// `ECALL` from S-mode (`M3` only). `tval` is 0.
    EnvironmentCallFromS,
    /// A fetch the Sv32 walk refuses (`M3`). Defined at M3.2, raised from M3.3. `tval` is
    /// the virtual address.
    InstructionPageFault,
    /// A load the Sv32 walk refuses (`M3`). Defined at M3.2, raised from M3.3. `tval` is
    /// the virtual address.
    LoadPageFault,
    /// A store the Sv32 walk refuses (`M3`). Defined at M3.2, raised from M3.3. `tval` is
    /// the virtual address.
    StorePageFault,
}

impl TrapCause {
    /// The cause's name, as the `rv32.trap` trace record shows it.
    pub const fn name(self) -> &'static str {
        match self {
            TrapCause::InstructionAddressMisaligned => "InstructionAddressMisaligned",
            TrapCause::LoadAddressMisaligned => "LoadAddressMisaligned",
            TrapCause::LoadAccessFault => "LoadAccessFault",
            TrapCause::StoreAddressMisaligned => "StoreAddressMisaligned",
            TrapCause::StoreAccessFault => "StoreAccessFault",
            TrapCause::InstructionAccessFault => "InstructionAccessFault",
            TrapCause::IllegalInstruction => "IllegalInstruction",
            TrapCause::Breakpoint => "Breakpoint",
            TrapCause::EnvironmentCall => "EnvironmentCall",
            TrapCause::EnvironmentCallFromU => "EnvironmentCallFromU",
            TrapCause::EnvironmentCallFromS => "EnvironmentCallFromS",
            TrapCause::InstructionPageFault => "InstructionPageFault",
            TrapCause::LoadPageFault => "LoadPageFault",
            TrapCause::StorePageFault => "StorePageFault",
        }
    }

    /// The cause's name in the `M3` profile's traces and inspect: [`TrapCause::name`],
    /// except that `EnvironmentCall` is `EnvironmentCallFromM` (`docs/m3-design.md` §5.3,
    /// §5.6). M1 and M2 keep `EnvironmentCall`.
    pub const fn m3_name(self) -> &'static str {
        match self {
            TrapCause::EnvironmentCall => "EnvironmentCallFromM",
            other => other.name(),
        }
    }

    /// The architectural exception code, the value `mcause` or `scause` would hold and the
    /// cause's `medeleg` bit (`docs/m3-design.md` §5.3). It is not the snapshot's code for
    /// the cause, which has its own numbering (§5.5).
    pub const fn code(self) -> u32 {
        match self {
            TrapCause::InstructionAddressMisaligned => 0,
            TrapCause::InstructionAccessFault => 1,
            TrapCause::IllegalInstruction => 2,
            TrapCause::Breakpoint => 3,
            TrapCause::LoadAddressMisaligned => 4,
            TrapCause::LoadAccessFault => 5,
            TrapCause::StoreAddressMisaligned => 6,
            TrapCause::StoreAccessFault => 7,
            TrapCause::EnvironmentCallFromU => 8,
            TrapCause::EnvironmentCallFromS => 9,
            TrapCause::EnvironmentCall => 11,
            TrapCause::InstructionPageFault => 12,
            TrapCause::LoadPageFault => 13,
            TrapCause::StorePageFault => 15,
        }
    }
}

/// The result of executing an instruction that may trap.
///
/// Exactly one of the two applies: a trapping instruction does not retire, so none of its
/// effect is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecOutcome {
    /// The instruction retires with this effect.
    Effect(PendingEffect),
    /// The instruction traps: registers, `pc`, and `instret` stay unchanged.
    Trap(PendingTrap),
}

/// The instruction is not an ALU instruction: a branch, jump, load, store, `FENCE`,
/// `ECALL`, or `EBREAK`. [`execute_alu`] does not execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NotAlu;

/// Executes an ALU instruction (`LUI`, `AUIPC`, and every `OP-IMM` and `OP` instruction)
/// at `pc`, with `rs1` and `rs2` the values of its source registers.
///
/// Source values an instruction does not use are ignored. The effect always writes `rd`,
/// even `x0`, and sets `next_pc` to `pc + 4`. Arithmetic wraps modulo 2^32, and every
/// shift uses the low 5 bits of its amount.
pub fn execute_alu(instr: &Instr, pc: u32, rs1: u32, rs2: u32) -> Result<PendingEffect, NotAlu> {
    let (rd, value) = match *instr {
        Instr::Lui { rd, imm } => (rd, imm),
        Instr::Auipc { rd, imm } => (rd, pc.wrapping_add(imm)),
        Instr::OpImm { op, rd, imm, .. } => (rd, op_imm(op, rs1, imm)),
        Instr::ShiftImm { op, rd, shamt, .. } => (rd, shift(op, rs1, shamt)),
        Instr::Op { op, rd, .. } => (rd, op_reg(op, rs1, rs2)),
        Instr::Jal { .. }
        | Instr::Jalr { .. }
        | Instr::Branch { .. }
        | Instr::Load { .. }
        | Instr::Store { .. }
        | Instr::Fence
        | Instr::Ecall
        | Instr::Ebreak => return Err(NotAlu),
    };
    Ok(PendingEffect {
        reg_write: Some(RegWrite { rd, value }),
        next_pc: pc.wrapping_add(4),
    })
}

/// The instruction is not a control-transfer instruction: anything but a branch, `JAL`,
/// or `JALR`. [`execute_control`] does not execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NotControl;

/// Executes a control-transfer instruction (a conditional branch, `JAL`, or `JALR`) at
/// `pc`, with `rs1` and `rs2` the values of its source registers before the instruction.
///
/// Source values an instruction does not use are ignored, and all address arithmetic
/// wraps modulo 2^32.
///
/// - A branch that is not taken retires with `next_pc = pc + 4`. Its target is never
///   computed, so it cannot trap.
/// - A taken branch jumps to `pc + offset`, `JAL` to `pc + offset`, and `JALR` to
///   `(rs1 + offset) & !1`: bit 0 is cleared before alignment is checked. `JAL` and `JALR`
///   write `pc + 4` to `rd`, even `x0`.
/// - A target that is not 4-byte aligned raises
///   [`TrapCause::InstructionAddressMisaligned`] with the target as `tval`, and the
///   instruction writes no register.
///
/// An aligned target is never a trap here, mapped or not: fetching from it is the next
/// instruction's business.
pub fn execute_control(
    instr: &Instr,
    pc: u32,
    rs1: u32,
    rs2: u32,
) -> Result<ExecOutcome, NotControl> {
    let link = |rd| {
        Some(RegWrite {
            rd,
            value: pc.wrapping_add(4),
        })
    };
    let (reg_write, target) = match *instr {
        Instr::Branch { op, offset, .. } => {
            if !taken(op, rs1, rs2) {
                return Ok(ExecOutcome::Effect(PendingEffect {
                    reg_write: None,
                    next_pc: pc.wrapping_add(4),
                }));
            }
            (None, pc.wrapping_add(offset as u32))
        }
        Instr::Jal { rd, offset } => (link(rd), pc.wrapping_add(offset as u32)),
        Instr::Jalr { rd, offset, .. } => (link(rd), rs1.wrapping_add(offset as u32) & !1),
        Instr::Lui { .. }
        | Instr::Auipc { .. }
        | Instr::Load { .. }
        | Instr::Store { .. }
        | Instr::OpImm { .. }
        | Instr::ShiftImm { .. }
        | Instr::Op { .. }
        | Instr::Fence
        | Instr::Ecall
        | Instr::Ebreak => return Err(NotControl),
    };
    Ok(if target & 0x3 != 0 {
        ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::InstructionAddressMisaligned,
            tval: target,
        })
    } else {
        ExecOutcome::Effect(PendingEffect {
            reg_write,
            next_pc: target,
        })
    })
}

/// Whether a branch is taken.
fn taken(op: BranchOp, a: u32, b: u32) -> bool {
    match op {
        BranchOp::Eq => a == b,
        BranchOp::Ne => a != b,
        BranchOp::Lt => (a as i32) < (b as i32),
        BranchOp::Ge => (a as i32) >= (b as i32),
        BranchOp::Ltu => a < b,
        BranchOp::Geu => a >= b,
    }
}

/// A register-immediate operation. The immediate is used as its sign-extended 32-bit
/// pattern, so `SLTIU` compares unsigned against that pattern.
fn op_imm(op: ImmOp, a: u32, imm: i32) -> u32 {
    let b = imm as u32;
    match op {
        ImmOp::Addi => a.wrapping_add(b),
        ImmOp::Slti => u32::from((a as i32) < imm),
        ImmOp::Sltiu => u32::from(a < b),
        ImmOp::Xori => a ^ b,
        ImmOp::Ori => a | b,
        ImmOp::Andi => a & b,
    }
}

fn op_reg(op: RegOp, a: u32, b: u32) -> u32 {
    match op {
        RegOp::Add => a.wrapping_add(b),
        RegOp::Sub => a.wrapping_sub(b),
        RegOp::Sll => shift(ShiftOp::Sll, a, b),
        RegOp::Slt => u32::from((a as i32) < (b as i32)),
        RegOp::Sltu => u32::from(a < b),
        RegOp::Xor => a ^ b,
        RegOp::Srl => shift(ShiftOp::Srl, a, b),
        RegOp::Sra => shift(ShiftOp::Sra, a, b),
        RegOp::Or => a | b,
        RegOp::And => a & b,
    }
}

/// Shifts `value` by the low 5 bits of `amount`.
fn shift(op: ShiftOp, value: u32, amount: u32) -> u32 {
    let amount = amount & 0x1f;
    match op {
        ShiftOp::Sll => value << amount,
        ShiftOp::Srl => value >> amount,
        ShiftOp::Sra => ((value as i32) >> amount) as u32,
    }
}

/// The instruction is not `FENCE`, `ECALL`, or `EBREAK`. [`execute_system`] does not
/// execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NotSystem;

/// Executes `FENCE`, `ECALL`, or `EBREAK` at `pc`.
///
/// - `FENCE` retires as a no-op with `next_pc = pc + 4` and no register write. In M1 every
///   access completes before the next fetch, so every ordering a fence could ask for
///   already holds (`docs/m1-design.md` §5.2).
/// - `ECALL` traps with [`TrapCause::EnvironmentCall`] and `tval` 0.
/// - `EBREAK` traps with [`TrapCause::Breakpoint`] and `tval = pc`.
pub fn execute_system(instr: &Instr, pc: u32) -> Result<ExecOutcome, NotSystem> {
    match *instr {
        Instr::Fence => Ok(ExecOutcome::Effect(PendingEffect {
            reg_write: None,
            next_pc: pc.wrapping_add(4),
        })),
        Instr::Ecall => Ok(ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::EnvironmentCall,
            tval: 0,
        })),
        Instr::Ebreak => Ok(ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::Breakpoint,
            tval: pc,
        })),
        Instr::Lui { .. }
        | Instr::Auipc { .. }
        | Instr::Jal { .. }
        | Instr::Jalr { .. }
        | Instr::Branch { .. }
        | Instr::Load { .. }
        | Instr::Store { .. }
        | Instr::OpImm { .. }
        | Instr::ShiftImm { .. }
        | Instr::Op { .. } => Err(NotSystem),
    }
}
