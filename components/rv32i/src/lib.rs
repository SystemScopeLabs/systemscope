//! The RV32I CPU for SystemScope M1 (`docs/m1-design.md` §5).
//!
//! This crate currently holds the simulation-independent parts: the instruction
//! representation ([`Instr`], [`Reg`]), the decoder ([`decode()`]), the immediate
//! extractors ([`immediate`]), and the register file ([`RegisterFile`]).

pub mod decode;
pub mod immediate;
pub mod instr;
pub mod regfile;

pub use decode::{Illegal, decode};
pub use instr::{BranchOp, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp};
pub use regfile::RegisterFile;
