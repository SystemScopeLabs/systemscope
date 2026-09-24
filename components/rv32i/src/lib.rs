//! The RV32I CPU for SystemScope M1 (`docs/m1-design.md` §5).
//!
//! [`Rv32iCpu`] is the component: it fetches through `mem.v1`, runs each instruction
//! through the pure layers below, and commits or traps (see [`cpu`]). The pure layers are
//! simulation-independent: the instruction representation ([`Instr`], [`Reg`]), the decoder ([`decode()`]), the immediate
//! extractors ([`immediate`]), the register file ([`RegisterFile`]), and pure execution:
//! [`execute_alu`] for ALU instructions, [`execute_control`] for branches and jumps,
//! [`execute_system`] for `FENCE`, `ECALL`, and `EBREAK`, and
//! [`prepare_memory`] with its completions ([`complete_memory`], [`complete_load`],
//! [`complete_store`]) for loads and stores, producing a [`PendingEffect`] or, for a trap,
//! a [`PendingTrap`].
//!
//! The `M2` profile ([`Rv32iProfile`]) adds the privileged subset of `docs/m2-design.md`
//! §4 in [`csr`]: the Zicsr instructions on eight machine CSRs ([`CsrFile`]) and `MRET`.

pub mod cpu;
pub mod csr;
pub mod decode;
pub mod execute;
pub mod immediate;
pub mod instr;
pub mod memory;
pub mod regfile;

pub use cpu::{CpuConfigError, Halt, Rv32iConfig, Rv32iCpu, Rv32iProfile, RvTrap};
pub use csr::{CsrFile, CsrOp, CsrSource, PrivInstr, decode_privileged};
pub use decode::{Illegal, decode};
pub use execute::{
    ExecOutcome, NotAlu, NotControl, NotSystem, PendingEffect, PendingTrap, RegWrite, TrapCause,
    execute_alu, execute_control, execute_system,
};
pub use instr::{BranchOp, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp};
pub use memory::{
    LoadExtension, LoadPlan, MemWidth, MemoryCompletionError, MemoryPlan, MemoryPrep, NotMemory,
    StorePlan, complete_load, complete_memory, complete_store, prepare_memory,
};
pub use regfile::RegisterFile;
