//! Decoded instructions (`docs/m1-design.md` §5.2).
//!
//! An [`Instr`] is always one of the 40 RV32I instructions M1 implements, with its operands
//! already extracted. Illegal encodings have no representation, so execution never looks at
//! `funct3` or `funct7` again. Instructions are grouped by format, with an operation enum
//! where a format has several instructions.

/// An integer register, `x0` to `x31`.
///
/// The index is always below 32: [`Reg::new`] rejects anything else, and the decoder
/// builds registers from 5-bit fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reg(u8);

impl Reg {
    /// `x0`, which always reads 0.
    pub const ZERO: Reg = Reg(0);

    /// Register `x{index}`, or `None` if `index` is 32 or more.
    pub const fn new(index: u8) -> Option<Reg> {
        if index < 32 { Some(Reg(index)) } else { None }
    }

    /// The register's index, in `0..32`.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// The register named by the 5-bit field at `word[shift + 4:shift]`.
    pub(crate) const fn field(word: u32, shift: u32) -> Reg {
        Reg(((word >> shift) & 0x1f) as u8)
    }
}

/// A decoded RV32I instruction.
///
/// Immediates are already sign-extended and scaled, as the [`immediate`](crate::immediate)
/// extractors return them. Values produced by [`decode`](crate::decode()) satisfy the ranges
/// documented on each field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Instr {
    /// `LUI`: `rd = imm`.
    Lui {
        /// Destination.
        rd: Reg,
        /// The U-type immediate: the upper 20 bits, low 12 bits zero.
        imm: u32,
    },
    /// `AUIPC`: `rd = pc + imm`.
    Auipc {
        /// Destination.
        rd: Reg,
        /// The U-type immediate: the upper 20 bits, low 12 bits zero.
        imm: u32,
    },
    /// `JAL`: `rd = pc + 4`, then jump to `pc + offset`.
    Jal {
        /// Destination for the return address.
        rd: Reg,
        /// Even byte offset in `-2^20..2^20`.
        offset: i32,
    },
    /// `JALR`: `rd = pc + 4`, then jump to `(rs1 + offset) & !1`.
    Jalr {
        /// Destination for the return address.
        rd: Reg,
        /// Base address.
        rs1: Reg,
        /// Byte offset in `-2048..2048`.
        offset: i32,
    },
    /// A conditional branch to `pc + offset`.
    Branch {
        /// The comparison.
        op: BranchOp,
        /// Left operand.
        rs1: Reg,
        /// Right operand.
        rs2: Reg,
        /// Even byte offset in `-4096..4096`.
        offset: i32,
    },
    /// A load from `rs1 + offset` into `rd`.
    Load {
        /// Width and extension.
        op: LoadOp,
        /// Destination.
        rd: Reg,
        /// Base address.
        rs1: Reg,
        /// Byte offset in `-2048..2048`.
        offset: i32,
    },
    /// A store of `rs2` to `rs1 + offset`.
    Store {
        /// Width.
        op: StoreOp,
        /// Base address.
        rs1: Reg,
        /// The value stored.
        rs2: Reg,
        /// Byte offset in `-2048..2048`.
        offset: i32,
    },
    /// A register-immediate operation other than a shift: `rd = rs1 op imm`.
    OpImm {
        /// The operation.
        op: ImmOp,
        /// Destination.
        rd: Reg,
        /// Left operand.
        rs1: Reg,
        /// The I-type immediate, in `-2048..2048`.
        imm: i32,
    },
    /// A shift by a constant: `SLLI`, `SRLI`, or `SRAI`.
    ShiftImm {
        /// The shift.
        op: ShiftOp,
        /// Destination.
        rd: Reg,
        /// The value shifted.
        rs1: Reg,
        /// Shift amount in `0..32`.
        shamt: u32,
    },
    /// A register-register operation: `rd = rs1 op rs2`.
    Op {
        /// The operation.
        op: RegOp,
        /// Destination.
        rd: Reg,
        /// Left operand.
        rs1: Reg,
        /// Right operand.
        rs2: Reg,
    },
    /// `FENCE`, in any of its forward-compatible encodings. It has no effect in M1.
    Fence,
    /// `ECALL`.
    Ecall,
    /// `EBREAK`.
    Ebreak,
}

/// The comparison of a conditional branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BranchOp {
    /// `BEQ`: equal.
    Eq,
    /// `BNE`: not equal.
    Ne,
    /// `BLT`: signed less than.
    Lt,
    /// `BGE`: signed greater than or equal.
    Ge,
    /// `BLTU`: unsigned less than.
    Ltu,
    /// `BGEU`: unsigned greater than or equal.
    Geu,
}

/// The width and extension of a load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoadOp {
    /// `LB`: one byte, sign-extended.
    B,
    /// `LH`: two bytes, sign-extended.
    H,
    /// `LW`: four bytes.
    W,
    /// `LBU`: one byte, zero-extended.
    Bu,
    /// `LHU`: two bytes, zero-extended.
    Hu,
}

/// The width of a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StoreOp {
    /// `SB`: the low byte.
    B,
    /// `SH`: the low two bytes.
    H,
    /// `SW`: four bytes.
    W,
}

/// A register-immediate operation other than a shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImmOp {
    /// `ADDI`.
    Addi,
    /// `SLTI`: signed comparison.
    Slti,
    /// `SLTIU`: unsigned comparison with the sign-extended immediate.
    Sltiu,
    /// `XORI`.
    Xori,
    /// `ORI`.
    Ori,
    /// `ANDI`.
    Andi,
}

/// A shift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShiftOp {
    /// Logical left.
    Sll,
    /// Logical right.
    Srl,
    /// Arithmetic right.
    Sra,
}

/// A register-register operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegOp {
    /// `ADD`.
    Add,
    /// `SUB`.
    Sub,
    /// `SLL`.
    Sll,
    /// `SLT`: signed comparison.
    Slt,
    /// `SLTU`: unsigned comparison.
    Sltu,
    /// `XOR`.
    Xor,
    /// `SRL`.
    Srl,
    /// `SRA`.
    Sra,
    /// `OR`.
    Or,
    /// `AND`.
    And,
}
