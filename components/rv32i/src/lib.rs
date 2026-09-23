//! The RV32I CPU for SystemScope M1 (`docs/m1-design.md` §5).
//!
//! This crate currently holds the simulation-independent parts: the instruction
//! representation ([`Instr`], [`Reg`]), the decoder ([`decode()`]), the immediate
//! extractors ([`immediate`]), the register file ([`RegisterFile`]), and pure ALU
//! execution ([`execute_alu`], producing a [`PendingEffect`]).

pub mod decode;
pub mod execute;
pub mod immediate;
pub mod instr;
pub mod regfile;

pub use decode::{Illegal, decode};
pub use execute::{NotAlu, PendingEffect, RegWrite, execute_alu};
pub use instr::{BranchOp, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp};
pub use regfile::RegisterFile;
