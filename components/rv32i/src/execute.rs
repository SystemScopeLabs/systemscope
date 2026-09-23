//! Pure instruction semantics (`docs/m1-design.md` §5.3).
//!
//! Execution computes what an instruction would do if it retired, as a [`PendingEffect`].
//! It reads nothing but its arguments and changes no state: the CPU applies the effect in
//! `Commit`, and only if the instruction retires.

use crate::instr::{ImmOp, Instr, Reg, RegOp, ShiftOp};

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
