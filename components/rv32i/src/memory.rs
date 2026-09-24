//! Pure load and store semantics (`docs/m1-design.md` §5.3, §6).
//!
//! A load or store needs a memory response between deciding what to access and knowing
//! what the instruction does, so its semantics come in two halves, both pure:
//!
//! ```text
//! Instr + pc + rs1 + rs2 ──prepare_memory──▶ MemoryPrep::Request(MemoryPlan)   send it
//!                                         └─▶ MemoryPrep::Trap(PendingTrap)    no request
//!
//! MemoryPlan + response ───complete_*──────▶ Ok(ExecOutcome::Effect)           retire
//!                                         ├─▶ Ok(ExecOutcome::Trap)            access fault
//!                                         └─▶ Err(MemoryCompletionError)       model bug
//! ```
//!
//! Neither half touches the register file, the `pc`, or the runtime. The CPU sends the
//! planned request, feeds the response back, and applies the outcome in `Commit`.
//!
//! # Effective addresses and alignment
//!
//! The effective address is `rs1 + offset` modulo 2^32. Wrapping is not a fault: whether
//! anything is mapped there is for the bus to say. M1 supports no misaligned access, so a
//! halfword access must be 2-byte aligned and a word access 4-byte aligned, and a
//! misaligned one traps in [`prepare_memory`], before any request exists. Byte accesses
//! are always aligned.
//!
//! # Architectural faults and model bugs
//!
//! A response of [`ReadOutcome::Fault`] or [`WriteOutcome::Fault`] is an architectural
//! event: the instruction traps with [`TrapCause::LoadAccessFault`] or
//! [`TrapCause::StoreAccessFault`], and the trap value is the effective address. A
//! response that does not fit the request, such as `Data` of the wrong length or a write
//! response to a load, can only come from a broken component. It is a
//! [`MemoryCompletionError`], never a trap, and the CPU turns it into a session fault.
//!
//! # Byte order
//!
//! Memory is little-endian: a load assembles its bytes lowest address first, and a store
//! sends the low bytes of `rs2` lowest byte first. The explicit `from_le_bytes` and
//! `to_le_bytes` conversions make the result independent of the host.

use std::fmt;

use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, WriteOutcome};

use crate::execute::{ExecOutcome, PendingEffect, PendingTrap, RegWrite, TrapCause};
use crate::instr::{Instr, LoadOp, Reg, StoreOp};

/// How many bytes a load or store accesses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemWidth {
    /// One byte: `LB`, `LBU`, `SB`.
    Byte,
    /// Two bytes: `LH`, `LHU`, `SH`.
    Half,
    /// Four bytes: `LW`, `SW`.
    Word,
}

impl MemWidth {
    /// The number of bytes: 1, 2, or 4. Also the required alignment.
    pub const fn bytes(self) -> u32 {
        match self {
            MemWidth::Byte => 1,
            MemWidth::Half => 2,
            MemWidth::Word => 4,
        }
    }
}

/// How a load widens its bytes to 32 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoadExtension {
    /// Copy the top bit of the loaded value into the upper bits: `LB`, `LH`, and `LW`.
    /// A word fills the register, so `LW` has nothing to extend.
    Signed,
    /// Fill the upper bits with zeros: `LBU` and `LHU`.
    Unsigned,
}

/// A load waiting for its read response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LoadPlan {
    /// The destination, which may be `x0`: the load is still performed, since it can
    /// fault, and the register file discards the write.
    pub rd: Reg,
    /// The effective address, aligned to `width`. Also the trap value of an access fault.
    pub addr: u32,
    /// How many bytes to read.
    pub width: MemWidth,
    /// How to widen them.
    pub extension: LoadExtension,
    /// The `pc` after the load, `pc + 4`, if it retires.
    pub next_pc: u32,
}

/// A store waiting for its write response.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StorePlan {
    /// The effective address, aligned to the width. Also the trap value of an access
    /// fault.
    pub addr: u32,
    /// The bytes to write, in address order: the low 1, 2, or 4 bytes of `rs2`,
    /// little-endian.
    pub data: Vec<u8>,
    /// The `pc` after the store, `pc + 4`, if it retires.
    pub next_pc: u32,
}

/// The memory request an aligned load or store needs before it can complete.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MemoryPlan {
    /// Read `width.bytes()` bytes at `addr`.
    Load(LoadPlan),
    /// Write `data` at `addr`.
    Store(StorePlan),
}

/// What a load or store does before memory is involved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MemoryPrep {
    /// The access is aligned: send this request, then complete the instruction with the
    /// response.
    Request(MemoryPlan),
    /// The access is misaligned and traps now. No request is sent, and registers, `pc`,
    /// and `instret` stay unchanged.
    Trap(PendingTrap),
}

/// The instruction is not a load or a store. [`prepare_memory`] does not execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NotMemory;

/// Prepares a load or store at `pc`, with `rs1` and `rs2` the values of its source
/// registers. Source values an instruction does not use are ignored.
///
/// The effective address is `rs1 + offset`, wrapping modulo 2^32. If it is aligned to the
/// access width, the result is the request to send, with `next_pc = pc + 4` kept for
/// completion. Otherwise the instruction traps with
/// [`TrapCause::LoadAddressMisaligned`] or [`TrapCause::StoreAddressMisaligned`] and the
/// effective address as `tval`, and no request is made.
pub fn prepare_memory(instr: &Instr, pc: u32, rs1: u32, rs2: u32) -> Result<MemoryPrep, NotMemory> {
    let next_pc = pc.wrapping_add(4);
    let (addr, width, misaligned, plan) = match *instr {
        Instr::Load { op, rd, offset, .. } => {
            let addr = rs1.wrapping_add(offset as u32);
            let (width, extension) = match op {
                LoadOp::B => (MemWidth::Byte, LoadExtension::Signed),
                LoadOp::Bu => (MemWidth::Byte, LoadExtension::Unsigned),
                LoadOp::H => (MemWidth::Half, LoadExtension::Signed),
                LoadOp::Hu => (MemWidth::Half, LoadExtension::Unsigned),
                LoadOp::W => (MemWidth::Word, LoadExtension::Signed),
            };
            let plan = MemoryPlan::Load(LoadPlan {
                rd,
                addr,
                width,
                extension,
                next_pc,
            });
            (addr, width, TrapCause::LoadAddressMisaligned, plan)
        }
        Instr::Store { op, offset, .. } => {
            let addr = rs1.wrapping_add(offset as u32);
            let bytes = rs2.to_le_bytes();
            let width = match op {
                StoreOp::B => MemWidth::Byte,
                StoreOp::H => MemWidth::Half,
                StoreOp::W => MemWidth::Word,
            };
            let plan = MemoryPlan::Store(StorePlan {
                addr,
                data: bytes[..width.bytes() as usize].to_vec(),
                next_pc,
            });
            (addr, width, TrapCause::StoreAddressMisaligned, plan)
        }
        Instr::Lui { .. }
        | Instr::Auipc { .. }
        | Instr::Jal { .. }
        | Instr::Jalr { .. }
        | Instr::Branch { .. }
        | Instr::OpImm { .. }
        | Instr::ShiftImm { .. }
        | Instr::Op { .. }
        | Instr::Fence
        | Instr::Ecall
        | Instr::Ebreak => return Err(NotMemory),
    };
    Ok(if addr & (width.bytes() - 1) != 0 {
        MemoryPrep::Trap(PendingTrap {
            cause: misaligned,
            tval: addr,
        })
    } else {
        MemoryPrep::Request(plan)
    })
}

/// A memory response that does not fit the request it answers. This is a component or
/// protocol bug, never an architectural event: the CPU raises it as a session fault, not
/// as a trap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryCompletionError {
    /// A read returned `Data` of a length other than the requested width.
    DataLength {
        /// The width requested: 1, 2, or 4.
        expected: u32,
        /// The number of bytes returned.
        actual: usize,
    },
    /// The response is not the response kind the plan waits for: a write response to a
    /// load, a read response to a store, or a request.
    ResponseKind,
}

impl fmt::Display for MemoryCompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryCompletionError::DataLength { expected, actual } => {
                write!(f, "load of {expected} bytes got {actual} bytes of data")
            }
            MemoryCompletionError::ResponseKind => {
                f.write_str("memory response of the wrong kind for the request")
            }
        }
    }
}

impl std::error::Error for MemoryCompletionError {}

/// Completes a load with its read outcome.
///
/// - `Data` of exactly `width.bytes()` bytes retires the load: the bytes are assembled
///   little-endian, widened as `extension` says, and written to `rd` (even `x0`), with
///   `next_pc` from the plan.
/// - `Fault { AccessFault }` traps with [`TrapCause::LoadAccessFault`] and the effective
///   address as `tval`.
/// - `Data` of any other length is a [`MemoryCompletionError::DataLength`].
pub fn complete_load(
    plan: &LoadPlan,
    outcome: &ReadOutcome,
) -> Result<ExecOutcome, MemoryCompletionError> {
    let data = match outcome {
        ReadOutcome::Data { data } => data,
        ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        } => {
            return Ok(ExecOutcome::Trap(PendingTrap {
                cause: TrapCause::LoadAccessFault,
                tval: plan.addr,
            }));
        }
    };
    let value = match (plan.width, plan.extension, data.as_slice()) {
        (MemWidth::Byte, LoadExtension::Signed, &[b]) => i32::from(b as i8) as u32,
        (MemWidth::Byte, LoadExtension::Unsigned, &[b]) => u32::from(b),
        (MemWidth::Half, LoadExtension::Signed, &[b0, b1]) => {
            i32::from(i16::from_le_bytes([b0, b1])) as u32
        }
        (MemWidth::Half, LoadExtension::Unsigned, &[b0, b1]) => {
            u32::from(u16::from_le_bytes([b0, b1]))
        }
        (MemWidth::Word, _, &[b0, b1, b2, b3]) => u32::from_le_bytes([b0, b1, b2, b3]),
        _ => {
            return Err(MemoryCompletionError::DataLength {
                expected: plan.width.bytes(),
                actual: data.len(),
            });
        }
    };
    Ok(ExecOutcome::Effect(PendingEffect {
        reg_write: Some(RegWrite { rd: plan.rd, value }),
        next_pc: plan.next_pc,
    }))
}

/// Completes a store with its write outcome.
///
/// - `Done` retires the store: no register write, `next_pc` from the plan. The target
///   made the bytes visible when it accepted the request; the instruction itself commits
///   only now.
/// - `Fault { AccessFault }` traps with [`TrapCause::StoreAccessFault`] and the effective
///   address as `tval`. The `mem.v1` contract guarantees the target changed nothing.
pub fn complete_store(plan: &StorePlan, outcome: &WriteOutcome) -> ExecOutcome {
    match outcome {
        WriteOutcome::Done => ExecOutcome::Effect(PendingEffect {
            reg_write: None,
            next_pc: plan.next_pc,
        }),
        WriteOutcome::Fault {
            fault: MemFault::AccessFault,
        } => ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::StoreAccessFault,
            tval: plan.addr,
        }),
    }
}

/// Completes a planned load or store with the `mem.v1` message that answers it.
///
/// A `ReadResp` completes a load as [`complete_load`] does, and a `WriteResp` a store as
/// [`complete_store`] does. Any other pairing is a
/// [`MemoryCompletionError::ResponseKind`]. Matching the response's `TxnId` to the
/// request is the caller's job.
pub fn complete_memory(
    plan: &MemoryPlan,
    response: &MemMsg,
) -> Result<ExecOutcome, MemoryCompletionError> {
    match (plan, response) {
        (MemoryPlan::Load(plan), MemMsg::ReadResp { outcome, .. }) => complete_load(plan, outcome),
        (MemoryPlan::Store(plan), MemMsg::WriteResp { outcome, .. }) => {
            Ok(complete_store(plan, outcome))
        }
        (
            MemoryPlan::Load(_) | MemoryPlan::Store(_),
            MemMsg::ReadReq { .. }
            | MemMsg::WriteReq { .. }
            | MemMsg::ReadResp { .. }
            | MemMsg::WriteResp { .. },
        ) => Err(MemoryCompletionError::ResponseKind),
    }
}
