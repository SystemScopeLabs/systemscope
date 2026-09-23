//! The RV32I decoder (`docs/m1-design.md` §5.2).

use std::fmt;

use crate::immediate::{imm_b, imm_i, imm_j, imm_s, imm_u};
use crate::instr::{BranchOp, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp};

/// A word that is not one of the 40 instructions M1 implements. The CPU raises
/// `IllegalInstruction` with `word` as the trap value (§6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Illegal {
    /// The instruction word as fetched.
    pub word: u32,
}

impl fmt::Display for Illegal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal instruction {:#010x}", self.word)
    }
}

impl std::error::Error for Illegal {}

const LOAD: u32 = 0b000_0011;
const MISC_MEM: u32 = 0b000_1111;
const OP_IMM: u32 = 0b001_0011;
const AUIPC: u32 = 0b001_0111;
const STORE: u32 = 0b010_0011;
const OP: u32 = 0b011_0011;
const LUI: u32 = 0b011_0111;
const BRANCH: u32 = 0b110_0011;
const JALR: u32 = 0b110_0111;
const JAL: u32 = 0b110_1111;
const SYSTEM: u32 = 0b111_0011;

const ECALL: u32 = 0x0000_0073;
const EBREAK: u32 = 0x0010_0073;

/// The `funct7` (for shifts, `imm[11:5]`) of the base operation and of its alternate
/// (`SUB`, `SRA`, `SRAI`).
const BASE: u32 = 0b000_0000;
const ALT: u32 = 0b010_0000;

/// Decodes one instruction word. Total over all `u32` values and never panics: every word
/// is either one of the 40 RV32I instructions or [`Illegal`].
///
/// HINT encodings, such as `ADDI x0, x0, 1`, decode as the ordinary instruction.
pub fn decode(word: u32) -> Result<Instr, Illegal> {
    let illegal = Err(Illegal { word });
    let rd = Reg::field(word, 7);
    let rs1 = Reg::field(word, 15);
    let rs2 = Reg::field(word, 20);
    let funct3 = (word >> 12) & 0x7;
    let funct7 = word >> 25;
    let instr = match word & 0x7f {
        LUI => Instr::Lui {
            rd,
            imm: imm_u(word),
        },
        AUIPC => Instr::Auipc {
            rd,
            imm: imm_u(word),
        },
        JAL => Instr::Jal {
            rd,
            offset: imm_j(word),
        },
        JALR if funct3 == 0 => Instr::Jalr {
            rd,
            rs1,
            offset: imm_i(word),
        },
        BRANCH => {
            let op = match funct3 {
                0b000 => BranchOp::Eq,
                0b001 => BranchOp::Ne,
                0b100 => BranchOp::Lt,
                0b101 => BranchOp::Ge,
                0b110 => BranchOp::Ltu,
                0b111 => BranchOp::Geu,
                _ => return illegal,
            };
            Instr::Branch {
                op,
                rs1,
                rs2,
                offset: imm_b(word),
            }
        }
        LOAD => {
            let op = match funct3 {
                0b000 => LoadOp::B,
                0b001 => LoadOp::H,
                0b010 => LoadOp::W,
                0b100 => LoadOp::Bu,
                0b101 => LoadOp::Hu,
                _ => return illegal,
            };
            Instr::Load {
                op,
                rd,
                rs1,
                offset: imm_i(word),
            }
        }
        STORE => {
            let op = match funct3 {
                0b000 => StoreOp::B,
                0b001 => StoreOp::H,
                0b010 => StoreOp::W,
                _ => return illegal,
            };
            Instr::Store {
                op,
                rs1,
                rs2,
                offset: imm_s(word),
            }
        }
        OP_IMM => {
            let op = match funct3 {
                0b000 => ImmOp::Addi,
                0b010 => ImmOp::Slti,
                0b011 => ImmOp::Sltiu,
                0b100 => ImmOp::Xori,
                0b110 => ImmOp::Ori,
                0b111 => ImmOp::Andi,
                // Shifts: imm[11:5] selects the shift. Every other value is reserved,
                // including shamt[5] = 1.
                _ => {
                    let op = match (funct3, funct7) {
                        (0b001, BASE) => ShiftOp::Sll,
                        (0b101, BASE) => ShiftOp::Srl,
                        (0b101, ALT) => ShiftOp::Sra,
                        _ => return illegal,
                    };
                    return Ok(Instr::ShiftImm {
                        op,
                        rd,
                        rs1,
                        shamt: (word >> 20) & 0x1f,
                    });
                }
            };
            Instr::OpImm {
                op,
                rd,
                rs1,
                imm: imm_i(word),
            }
        }
        OP => {
            let op = match (funct7, funct3) {
                (BASE, 0b000) => RegOp::Add,
                (ALT, 0b000) => RegOp::Sub,
                (BASE, 0b001) => RegOp::Sll,
                (BASE, 0b010) => RegOp::Slt,
                (BASE, 0b011) => RegOp::Sltu,
                (BASE, 0b100) => RegOp::Xor,
                (BASE, 0b101) => RegOp::Srl,
                (ALT, 0b101) => RegOp::Sra,
                (BASE, 0b110) => RegOp::Or,
                (BASE, 0b111) => RegOp::And,
                _ => return illegal,
            };
            Instr::Op { op, rd, rs1, rs2 }
        }
        // Every FENCE configuration, whatever fm, pred, succ, rs1, and rd hold. Other
        // funct3 values, including FENCE.I (001), fall through to illegal.
        MISC_MEM if funct3 == 0 => Instr::Fence,
        SYSTEM if word == ECALL => Instr::Ecall,
        SYSTEM if word == EBREAK => Instr::Ebreak,
        // Unknown opcodes, 16-bit encodings (low bits not 11), and every rejected
        // funct3 of the guarded opcodes above.
        _ => return illegal,
    };
    Ok(instr)
}
