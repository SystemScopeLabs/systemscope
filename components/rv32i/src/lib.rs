//! The RV32I CPU for SystemScope M1 (`docs/m1-design.md` §5).
//!
//! This crate currently holds the simulation-independent parts: the instruction
//! representation ([`Instr`], [`Reg`]), the decoder ([`decode()`]), the immediate
//! extractors ([`immediate`]), the register file ([`RegisterFile`]), and pure execution:
//! [`execute_alu`] for ALU instructions, [`execute_control`] for branches and jumps, and
//! [`prepare_memory`] with its completions ([`complete_memory`], [`complete_load`],
//! [`complete_store`]) for loads and stores, producing a [`PendingEffect`] or, for a trap,
//! a [`PendingTrap`].

pub mod decode;
pub mod execute;
pub mod immediate;
pub mod instr;
pub mod memory;
pub mod regfile;

pub use decode::{Illegal, decode};
pub use execute::{
    ExecOutcome, NotAlu, NotControl, PendingEffect, PendingTrap, RegWrite, TrapCause, execute_alu,
    execute_control,
};
pub use instr::{BranchOp, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp};
pub use memory::{
    LoadExtension, LoadPlan, MemWidth, MemoryCompletionError, MemoryPlan, MemoryPrep, NotMemory,
    StorePlan, complete_load, complete_memory, complete_store, prepare_memory,
};
pub use regfile::RegisterFile;
