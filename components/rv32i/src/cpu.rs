//! `Rv32iCpu`: the RV32I CPU component (`docs/m1-design.md` §5.3–§5.7).
//!
//! The CPU orchestrates the pure layers: it fetches through memory, decodes with
//! [`decode()`](crate::decode()), executes with [`execute_alu`], [`execute_control`],
//! [`execute_system`], or [`prepare_memory`] and [`complete_memory`], and applies the result
//! in `Commit`. It implements no instruction semantics of its own.
//!
//! ```text
//! FetchIssue ──Wake(FETCH) @ REQUEST──▶ send ReadReq { pc, 4 } ──▶ FetchWait { txn }
//! FetchWait ──ReadResp @ COMPLETE──▶ decode + execute
//!    ├─ result known ─────────────▶ CommitPending  (Wake(COMMIT) @ COMMIT, same tick)
//!    └─ aligned load or store ────▶ MemIssue       (Wake(MEMORY) @ REQUEST, next cycle)
//! MemIssue ──Wake(MEMORY)──▶ send ReadReq / WriteReq ──▶ MemWait { txn }
//! MemWait ──Read/WriteResp @ COMPLETE──▶ complete_memory ──▶ CommitPending
//! CommitPending ──Wake(COMMIT)──▶ retire: x[rd], pc, instret ──▶ FetchIssue (next cycle)
//!                              ├▶ trap: nothing changes ──▶ Halted
//!                              └▶ retire, then MEI entry (M2) ──▶ FetchIssue at mtvec
//! ```
//!
//! # Ownership
//!
//! The CPU owns `pc`, the registers, `instret`, its next `TxnId`, and its execution state.
//! Every state but `Halted` waits for exactly one event, and that event is the runtime's:
//! a wake the CPU scheduled, or the response to the one request it has outstanding. The
//! snapshot therefore holds the state and never a copy of the event. In particular
//! `MemWait` means "the request is already sent": restoring it waits for the response in
//! the runtime's queue and never sends the request again, so a store cannot be repeated.
//!
//! # Traps and faults
//!
//! An architectural trap is an outcome of the instruction: a fetch `Fault`
//! (`InstructionAccessFault`), an illegal word, a trap from pure execution, or a memory
//! `Fault` converted by [`complete_memory`]. It is recorded in `Commit`, changes no
//! architectural state, and halts the CPU (§6).
//!
//! Anything that breaks the protocol is a model bug and faults the session with
//! [`SimError::ComponentFault`] instead, without retiring or trapping: a response for any
//! `TxnId` but the one outstanding (including stale and duplicate ones), a response while
//! nothing is outstanding, a fetch that returns other than 4 bytes or a write response, a
//! [`MemoryCompletionError`], a request on the initiator port, a response outside
//! `Complete`, or a wake that does not match the state.
//!
//! # Profiles
//!
//! [`Rv32iProfile::M1`] is exactly the M1 CPU. [`Rv32iProfile::M2`] adds the privileged
//! subset of `docs/m2-design.md` §4 ([`crate::csr`]): the six Zicsr instructions on the
//! eight whitelisted CSRs and `MRET`, decoded in `Complete` into a pending operation that
//! reads and writes the CSRs only in `Commit` (§6.3); snapshot schema 2 (§6.4); the CSRs
//! in `inspect` and `csr`/`csr_value` in CSR commits (§6.5); and checked `TxnId`
//! allocation (§6.2). A synchronous trap still halts and writes no CSR (§5.6).
//!
//! [`Rv32iProfile::M3`] adds the privilege and trap boundary of `docs/m3-design.md` §5
//! ([`crate::privilege`]) to everything `M2` has; see below. Every M3 rule is selected by
//! the profile, so the `M1` and `M2` behavior is unchanged.
//!
//! # Privilege and traps (`M3`)
//!
//! The hart has a mode, M, S, or U, reset to M, with `mstatus` = 0 (`MPP` = U). Each
//! fetched word is decoded against the current mode, so the pending outcome already holds
//! the CSR access rule, whether `MRET`, `SRET`, and `SFENCE.VMA` are legal, and the
//! mode-dependent `ECALL` cause (§5.1, §5.3). In `Commit`, a pending exception raised
//! below M whose `medeleg` bit is set is delivered to S: `sepc`, `scause`, `stval`, and
//! `mstatus` are written, the mode becomes S, and the CPU fetches at `stvec`, without
//! retiring or sampling the interrupt; it traces `rv32.exception`. Any other exception
//! halts exactly as in M2 and writes no CSR. The interrupt is eligible below M whatever
//! `MIE` says, and its entry saves the mode in `MPP`. Snapshot schema 3 is schema 2 plus
//! the mode and the new CSRs (§5.5).
//!
//! # Sv32 (`M3`)
//!
//! In S and U with `satp.MODE` = Sv32, every fetch, load, and store is translated
//! (`docs/m3-design.md` §5.2, §5.4), with no TLB. The walk reads each PTE as an ordinary
//! `mem.v1` read over the `mem` port, one per cycle like any access, and feeds it to
//! [`sv32::step`]:
//!
//! ```text
//! Commit ──translated──▶ WalkIssue { Fetch, 1, satp.PPN }        (Wake(WALK) @ REQUEST)
//! FetchWait ──aligned load or store, translated──▶ WalkIssue { Data, 1, satp.PPN }
//! WalkIssue ──Wake(WALK)──▶ send ReadReq { PTE address, 4 } ──▶ WalkWait { txn }
//! WalkWait ──ReadResp @ COMPLETE──▶ sv32::step
//!    ├─ pointer ──▶ WalkIssue { level 0, table }                  (next cycle)
//!    ├─ leaf ─────▶ FetchIssue { pa } or MemIssue { pa }          (next cycle)
//!    └─ page fault, or the PTE read faults ──▶ CommitPending (trap, same tick)
//! ```
//!
//! The translated address is state: `FetchIssue`, `FetchWait`, `MemIssue`, `MemWait`,
//! and `CommitPending` hold `pa` (`None` when untranslated), so nothing is ever read
//! twice and a restored CPU never sends a request again. A page fault raises the page
//! fault of the access, and a PTE read the bus refuses raises the access's access fault;
//! both have `tval` = the virtual address, and both are delivered like any exception.
//! Alignment is checked before any walk.
//!
//! # Machine external interrupt (`M2`)
//!
//! The `M2` profile has a second port, `irq`, an `irq.v0` target. A `Level` delivered on
//! it in `Complete` sets the `irq` input level, which `mip.MEIP` reads, in any state,
//! `Halted` included; a repeated level changes nothing (§6.2, §7.1). The interrupt is
//! sampled at exactly one point: in the `Commit` handler that retires an instruction,
//! after its effects, `pc`, and `instret`, and after the instruction limit, which takes
//! priority (§5.1). If `mstatus.MIE`, `mie.MEIE`, and `mip.MEIP` are all set, the CPU
//! enters the handler right there (§5.2): the entry is not an instruction, does not
//! retire, and leaves the CPU in `FetchIssue` at `mtvec`, scheduled like any retirement,
//! so there is no interrupt state to snapshot (§6.4). A trapping instruction does not
//! retire, so no interrupt is sampled after it. The entry traces `rv32.interrupt` after
//! the retirement's `rv32.commit` (§6.5).

use std::fmt;
use std::num::NonZeroU64;

use systemscope_contracts::canonical::DecodeError;
use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{self, MemFault, MemMsg, ReadOutcome, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;

use crate::csr::{self, CsrFile, CsrOp, CsrSource, PrivInstr, decode_privileged};
use crate::decode::{Illegal, decode};
use crate::execute::{
    ExecOutcome, PendingEffect, PendingTrap, RegWrite, TrapCause, execute_alu, execute_control,
    execute_system,
};
use crate::instr::{Instr, Reg};
use crate::memory::{
    MemoryCompletionError, MemoryPlan, MemoryPrep, complete_memory, prepare_memory,
};
use crate::privilege::{self, M3State, Privilege, Spp, SupervisorInstr, decode_supervisor};
use crate::regfile::RegisterFile;
use crate::sv32::{self, Access};

/// The `mem` port: a `mem.v1` initiator, for fetches and data alike. It is the CPU's only
/// port in the `M1` profile.
pub const PORT: PortId = PortId(0);

/// The `irq` port of the `M2` profile: an `irq.v0` target (`docs/m2-design.md` §6.1).
pub const IRQ_PORT: PortId = PortId(1);

/// Wake token that sends the next fetch.
pub const FETCH: u64 = 0;

/// Wake token that sends the pending load or store.
pub const MEMORY: u64 = 1;

/// Wake token that commits the pending instruction.
pub const COMMIT: u64 = 2;

/// Wake token that sends the next PTE read of a walk (`M3`, `docs/m3-design.md` §5.4).
pub const WALK: u64 = 3;

/// Layout of [`Rv32iCpu`]'s snapshot in the `M1` profile.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Layout of [`Rv32iCpu`]'s snapshot in the `M2` profile: schema 1, then the CSRs.
pub const SNAPSHOT_SCHEMA_M2: u32 = 2;

/// Layout of [`Rv32iCpu`]'s snapshot in the `M3` profile: schema 2, then the mode and the
/// CSRs the `M3` profile adds (`docs/m3-design.md` §5.5).
pub const SNAPSHOT_SCHEMA_M3: u32 = 3;

/// Trace kind of a retired instruction.
pub const COMMIT_KIND: &str = "rv32.commit";

/// Trace kind of a trapping instruction.
pub const TRAP_KIND: &str = "rv32.trap";

/// Trace kind of an instruction-limit halt.
pub const HALT_KIND: &str = "rv32.halt";

/// Trace kind of a machine external interrupt entry (`M2`, `M3`).
pub const INTERRUPT_KIND: &str = "rv32.interrupt";

/// Trace kind of a delegated exception's delivery to S (`M3`).
pub const EXCEPTION_KIND: &str = "rv32.exception";

/// Which CPU an [`Rv32iCpu`] is (`docs/m2-design.md` §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rv32iProfile {
    /// The M1 CPU, unchanged: RV32I only, snapshot schema 1.
    M1,
    /// The M1 CPU plus the M2 privileged subset (Zicsr on eight CSRs, `MRET`), the `irq`
    /// port and the machine external interrupt, snapshot schema 2.
    M2,
    /// The `M2` CPU plus M, S, and U modes, the supervisor CSRs, `SRET`, `SFENCE.VMA`,
    /// delegated exceptions, the interrupt with modes, and Sv32 translation
    /// (`docs/m3-design.md` §5), snapshot schema 3.
    M3,
}

/// Construction parameters of an [`Rv32iCpu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rv32iConfig {
    /// The clock the CPU runs on.
    pub clock: ClockDomainId,
    /// The first `pc`. Must be 4-byte aligned.
    pub entry: u32,
    /// The CPU halts right after this many instructions retire.
    pub max_instructions: NonZeroU64,
    /// The profile. It is never encoded in the snapshot's configuration block: the
    /// snapshot schema implies it (1 is `M1`, 2 is `M2`, 3 is `M3`).
    pub profile: Rv32iProfile,
}

/// An [`Rv32iConfig`] the CPU cannot start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuConfigError {
    /// The entry point is not 4-byte aligned.
    MisalignedEntry(u32),
}

impl fmt::Display for CpuConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CpuConfigError::MisalignedEntry(entry) => {
                write!(f, "entry point {entry:#010x} is not 4-byte aligned")
            }
        }
    }
}

impl std::error::Error for CpuConfigError {}

/// A trap as the CPU records it: the cause, the trapping instruction's `pc`, and the trap
/// value (`docs/m1-design.md` §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RvTrap {
    /// Why the instruction trapped.
    pub cause: TrapCause,
    /// The address of the trapping instruction.
    pub pc: u32,
    /// The trap value.
    pub tval: u32,
}

/// Why the CPU stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Halt {
    /// An instruction trapped and did not retire.
    Trap(RvTrap),
    /// The instruction that brought `instret` to `max_instructions` retired.
    InstructionLimit,
}

/// Where the CPU is in its current instruction. Every state but `Halted` waits for exactly
/// one runtime-owned event.
#[derive(Clone, Debug, PartialEq, Eq)]
///
/// `pa` is the physical address a completed Sv32 walk produced (`M3`), or `None` when the
/// access is not translated and goes to its own address (`docs/m3-design.md` §5.4). In
/// `CommitPending` it is kept only for a translated load or store that retires, whose
/// `rv32.commit` record shows it as `paddr`.
enum State {
    /// Waiting for `Wake(FETCH)`.
    FetchIssue { pa: Option<u64> },
    /// The fetch `txn` is outstanding.
    FetchWait { txn: TxnId, pa: Option<u64> },
    /// Waiting for `Wake(MEMORY)` to send `plan`, for the instruction `insn`.
    MemIssue {
        insn: u32,
        plan: MemoryPlan,
        pa: Option<u64>,
    },
    /// The data request `txn` for `plan` is outstanding.
    MemWait {
        txn: TxnId,
        insn: u32,
        plan: MemoryPlan,
        pa: Option<u64>,
    },
    /// Waiting for `Wake(WALK)` to read the next PTE of `walk` (`M3`).
    WalkIssue { walk: Walk },
    /// The PTE read `txn` of `walk` is outstanding (`M3`).
    WalkWait { txn: TxnId, walk: Walk },
    /// Waiting for `Wake(COMMIT)` to apply `outcome`. `insn` is `None` only after a
    /// faulting fetch, which has no instruction.
    CommitPending {
        insn: Option<u32>,
        outcome: Outcome,
        pa: Option<u64>,
    },
    /// Stopped; nothing is pending.
    Halted(Halt),
}

/// An Sv32 walk in progress: the next PTE to read is at `level` in the table with PPN
/// `table` (`docs/m3-design.md` §5.4).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Walk {
    purpose: Purpose,
    level: u8,
    table: u32,
}

/// What a walk translates.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Purpose {
    /// The fetch at `pc`.
    Fetch,
    /// The load or store `plan` of the instruction `insn`. The snapshot stores `insn`
    /// only, and restore recomputes `plan`.
    Data { insn: u32, plan: MemoryPlan },
}

impl Purpose {
    fn access(&self) -> Access {
        match self {
            Purpose::Fetch => Access::Fetch,
            Purpose::Data {
                plan: MemoryPlan::Load(_),
                ..
            } => Access::Load,
            Purpose::Data {
                plan: MemoryPlan::Store(_),
                ..
            } => Access::Store,
        }
    }
}

/// The virtual address of `plan`.
fn plan_addr(plan: &MemoryPlan) -> u32 {
    match plan {
        MemoryPlan::Load(load) => load.addr,
        MemoryPlan::Store(store) => store.addr,
    }
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::FetchIssue { .. } => "fetch_issue",
            State::FetchWait { .. } => "fetch_wait",
            State::MemIssue { .. } => "mem_issue",
            State::MemWait { .. } => "mem_wait",
            State::WalkIssue { .. } => "walk_issue",
            State::WalkWait { .. } => "walk_wait",
            State::CommitPending { .. } => "commit_pending",
            State::Halted(_) => "halted",
        }
    }
}

/// What `Commit` applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// An RV32I retirement or a trap, as in M1.
    Exec(ExecOutcome),
    /// A CSR instruction on a supported CSR (`M2`, `M3`).
    Csr(PendingCsr),
    /// `MRET` (`M2`, `M3`). It reads `mepc` and `mstatus` in `Commit`.
    Mret,
    /// `SRET` (`M3` only). It reads `sepc` and `mstatus` in `Commit`.
    Sret,
}

/// A CSR instruction decoded and executed in `Complete`, applied in `Commit`
/// (`docs/m2-design.md` §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingCsr {
    /// The CSR, always a supported one.
    csr: u16,
    /// Write, set, or clear.
    op: CsrOp,
    /// Whether the CSR is written (§4.2 write suppression).
    write: bool,
    /// The `rs1` value read in `Complete`, or the zero-extended `uimm`.
    operand: u32,
    /// Gets the CSR's old value.
    rd: Reg,
    /// `pc + 4`.
    next_pc: u32,
}

/// What a fetched word needs next.
enum Step {
    /// The outcome is known without memory.
    Done(Outcome),
    /// An aligned load or store must access memory first.
    Memory(MemoryPlan),
}

/// What decides how a word executes: the profile, and in the `M3` profile the mode the
/// hart is in before the instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Isa {
    M1,
    M2,
    M3(Privilege),
}

/// Decodes and executes `word` at `pc` against `regs` for `isa`, through the pure layers
/// only.
fn step(isa: Isa, word: u32, pc: u32, regs: &RegisterFile) -> Step {
    match isa {
        Isa::M1 => {}
        Isa::M2 => {
            if let Some(instr) = decode_privileged(word) {
                return Step::Done(privileged(instr, word, pc, regs));
            }
        }
        Isa::M3(mode) => {
            if let Some(outcome) = privileged_m3(mode, word, pc, regs) {
                return Step::Done(outcome);
            }
        }
    }
    let instr = match decode(word) {
        Ok(instr) => instr,
        Err(Illegal { word }) => {
            return Step::Done(Outcome::Exec(ExecOutcome::Trap(PendingTrap {
                cause: TrapCause::IllegalInstruction,
                tval: word,
            })));
        }
    };
    let (rs1, rs2) = sources(&instr);
    let (a, b) = (regs.read(rs1), regs.read(rs2));
    // Each family's function accepts exactly the variants listed with it.
    let family = match instr {
        Instr::Lui { .. }
        | Instr::Auipc { .. }
        | Instr::OpImm { .. }
        | Instr::ShiftImm { .. }
        | Instr::Op { .. } => execute_alu(&instr, pc, a, b)
            .ok()
            .map(|e| Step::Done(Outcome::Exec(ExecOutcome::Effect(e)))),
        Instr::Jal { .. } | Instr::Jalr { .. } | Instr::Branch { .. } => {
            execute_control(&instr, pc, a, b)
                .ok()
                .map(|o| Step::Done(Outcome::Exec(o)))
        }
        Instr::Fence | Instr::Ecall | Instr::Ebreak => execute_system(&instr, pc)
            .ok()
            .map(|o| Step::Done(Outcome::Exec(ecall_cause(isa, o)))),
        Instr::Load { .. } | Instr::Store { .. } => {
            prepare_memory(&instr, pc, a, b)
                .ok()
                .map(|prep| match prep {
                    MemoryPrep::Request(plan) => Step::Memory(plan),
                    MemoryPrep::Trap(trap) => Step::Done(Outcome::Exec(ExecOutcome::Trap(trap))),
                })
        }
    };
    family.expect("every instruction variant is listed with its family")
}

/// The pending outcome of an `M2` privileged instruction: `IllegalInstruction` for an
/// unsupported CSR, whether or not it would write (§4.5); otherwise the CSR operation, with
/// its operand read now, or `MRET`.
fn privileged(instr: PrivInstr, word: u32, pc: u32, regs: &RegisterFile) -> Outcome {
    match instr {
        PrivInstr::Mret => Outcome::Mret,
        PrivInstr::Csr { csr, .. } if !csr::is_supported(csr) => illegal(word),
        PrivInstr::Csr { op, rd, src, csr } => {
            Outcome::Csr(csr_operation(op, rd, src, csr, instr.writes(), pc, regs))
        }
    }
}

/// The pending outcome of a privileged instruction in the `M3` profile, run in `mode`
/// (`docs/m3-design.md` §5.1); `None` if `word` is not one, so it decodes as RV32I.
///
/// A CSR instruction is illegal on a CSR off the `M3` whitelist, from a mode below the
/// CSR's, or when it writes a read-only CSR; `MRET` is legal only in M; `SRET` and
/// `SFENCE.VMA` are illegal in U. `SFENCE.VMA` retires as a no-op: there is no TLB.
fn privileged_m3(mode: Privilege, word: u32, pc: u32, regs: &RegisterFile) -> Option<Outcome> {
    if let Some(instr) = decode_privileged(word) {
        return Some(match instr {
            PrivInstr::Mret if mode == Privilege::Machine => Outcome::Mret,
            PrivInstr::Mret => illegal(word),
            PrivInstr::Csr { csr, .. }
                if !privilege::is_supported(csr)
                    || !privilege::accessible(csr, mode, instr.writes()) =>
            {
                illegal(word)
            }
            PrivInstr::Csr { op, rd, src, csr } => {
                Outcome::Csr(csr_operation(op, rd, src, csr, instr.writes(), pc, regs))
            }
        });
    }
    Some(match decode_supervisor(word)? {
        _ if mode == Privilege::User => illegal(word),
        SupervisorInstr::Sret => Outcome::Sret,
        SupervisorInstr::SfenceVma => Outcome::Exec(ExecOutcome::Effect(PendingEffect {
            reg_write: None,
            next_pc: pc.wrapping_add(4),
        })),
    })
}

/// `IllegalInstruction` for `word`.
fn illegal(word: u32) -> Outcome {
    Outcome::Exec(ExecOutcome::Trap(PendingTrap {
        cause: TrapCause::IllegalInstruction,
        tval: word,
    }))
}

/// A CSR instruction on a supported CSR, with its operand read now.
fn csr_operation(
    op: CsrOp,
    rd: Reg,
    src: CsrSource,
    csr: u16,
    write: bool,
    pc: u32,
    regs: &RegisterFile,
) -> PendingCsr {
    PendingCsr {
        csr,
        op,
        write,
        operand: match src {
            CsrSource::Reg(rs1) => regs.read(rs1),
            CsrSource::Imm(uimm) => u32::from(uimm),
        },
        rd,
        next_pc: pc.wrapping_add(4),
    }
}

/// In the `M3` profile, `ECALL`'s cause names the mode it runs in (`docs/m3-design.md`
/// §5.3): `EnvironmentCallFromU` in U, `EnvironmentCallFromS` in S, and `EnvironmentCall`
/// (code 11) in M. Every other outcome, and every outcome in `M1` and `M2`, is unchanged.
fn ecall_cause(isa: Isa, outcome: ExecOutcome) -> ExecOutcome {
    match (isa, outcome) {
        (
            Isa::M3(mode),
            ExecOutcome::Trap(PendingTrap {
                cause: TrapCause::EnvironmentCall,
                tval,
            }),
        ) => ExecOutcome::Trap(PendingTrap {
            cause: match mode {
                Privilege::User => TrapCause::EnvironmentCallFromU,
                Privilege::Supervisor => TrapCause::EnvironmentCallFromS,
                Privilege::Machine => TrapCause::EnvironmentCall,
            },
            tval,
        }),
        (_, outcome) => outcome,
    }
}

/// The source registers an instruction reads; `x0` for a source it does not have.
fn sources(instr: &Instr) -> (Reg, Reg) {
    match *instr {
        Instr::Jalr { rs1, .. }
        | Instr::Load { rs1, .. }
        | Instr::OpImm { rs1, .. }
        | Instr::ShiftImm { rs1, .. } => (rs1, Reg::ZERO),
        Instr::Branch { rs1, rs2, .. }
        | Instr::Store { rs1, rs2, .. }
        | Instr::Op { rs1, rs2, .. } => (rs1, rs2),
        Instr::Lui { .. }
        | Instr::Auipc { .. }
        | Instr::Jal { .. }
        | Instr::Fence
        | Instr::Ecall
        | Instr::Ebreak => (Reg::ZERO, Reg::ZERO),
    }
}

/// The session fault for a response that does not fit its plan.
fn completion_fault(e: MemoryCompletionError) -> SimError {
    SimError::ComponentFault(match e {
        MemoryCompletionError::DataLength { .. } => "rv32 cpu: load data of the wrong length",
        MemoryCompletionError::ResponseKind => "rv32 cpu: memory response of the wrong kind",
    })
}

/// The architectural RV32I CPU: one instruction at a time, fetched and executed through
/// `mem.v1`.
pub struct Rv32iCpu {
    config: Rv32iConfig,
    pc: u32,
    regs: RegisterFile,
    instret: u64,
    next_txn: u64,
    state: State,
    /// The machine CSRs. Always at reset in the `M1` profile, which cannot reach them.
    csrs: CsrFile,
    /// The mode and the CSRs the `M3` profile adds. Always at reset in `M1` and `M2`,
    /// which cannot reach them.
    m3: M3State,
}

impl Rv32iCpu {
    /// A CPU at reset: `pc = entry`, every register 0, about to fetch. In the `M2`
    /// profile, the CSRs are at reset too (`mstatus` reads `0x1800`, the rest 0).
    pub fn new(config: Rv32iConfig) -> Result<Rv32iCpu, CpuConfigError> {
        if !config.entry.is_multiple_of(4) {
            return Err(CpuConfigError::MisalignedEntry(config.entry));
        }
        Ok(Rv32iCpu {
            config,
            pc: config.entry,
            regs: RegisterFile::new(),
            instret: 0,
            next_txn: 0,
            state: State::FetchIssue { pa: None },
            csrs: CsrFile::new(),
            m3: M3State::new(),
        })
    }

    /// The machine CSRs (`M2`, `M3`; at reset and unused in `M1`). In the `M3` profile
    /// `mstatus` also has the fields in [`Rv32iCpu::m3`].
    pub fn csrs(&self) -> &CsrFile {
        &self.csrs
    }

    /// The mode and the CSRs the `M3` profile adds (at reset and unused in `M1` and
    /// `M2`).
    pub fn m3(&self) -> &M3State {
        &self.m3
    }

    /// How the next word executes.
    fn isa(&self) -> Isa {
        match self.config.profile {
            Rv32iProfile::M1 => Isa::M1,
            Rv32iProfile::M2 => Isa::M2,
            Rv32iProfile::M3 => Isa::M3(self.m3.privilege),
        }
    }

    /// Whether fetches, loads, and stores are translated now (`M3` only, §5.2).
    fn translating(&self) -> bool {
        self.config.profile == Rv32iProfile::M3 && self.m3.translates()
    }

    /// The virtual address `walk` translates.
    fn walk_va(&self, walk: &Walk) -> u32 {
        match &walk.purpose {
            Purpose::Fetch => self.pc,
            Purpose::Data { plan, .. } => plan_addr(plan),
        }
    }

    /// Goes on to the next fetch, at the next cycle's `Request`: straight to the bus, or,
    /// when translated, through a walk from `satp`'s root.
    fn fetch_next(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if self.translating() {
            self.state = State::WalkIssue {
                walk: Walk {
                    purpose: Purpose::Fetch,
                    level: 1,
                    table: self.m3.root(),
                },
            };
            self.wake_next_cycle(ctx, WALK)
        } else {
            self.state = State::FetchIssue { pa: None };
            self.wake_next_cycle(ctx, FETCH)
        }
    }

    /// `cause`'s name in this profile's traces and inspect.
    fn cause_name(&self, cause: TrapCause) -> &'static str {
        match self.config.profile {
            Rv32iProfile::M1 | Rv32iProfile::M2 => cause.name(),
            Rv32iProfile::M3 => cause.m3_name(),
        }
    }

    /// Reads a supported CSR under this profile's rules.
    fn read_csr(&self, csr: u16) -> Option<u32> {
        match self.config.profile {
            Rv32iProfile::M1 | Rv32iProfile::M2 => self.csrs.read(csr),
            Rv32iProfile::M3 => self.m3.read(&self.csrs, csr),
        }
    }

    /// Writes a supported CSR under this profile's rules.
    fn write_csr(&mut self, csr: u16, value: u32) -> Option<()> {
        match self.config.profile {
            Rv32iProfile::M1 | Rv32iProfile::M2 => self.csrs.write(csr, value),
            Rv32iProfile::M3 => self.m3.write(&mut self.csrs, csr, value),
        }
    }

    /// The architectural `pc`.
    pub fn pc(&self) -> u32 {
        self.pc
    }

    /// The architectural registers.
    pub fn registers(&self) -> &RegisterFile {
        &self.regs
    }

    /// The number of retired instructions.
    pub fn instret(&self) -> u64 {
        self.instret
    }

    /// Why the CPU stopped, if it has.
    pub fn halt(&self) -> Option<Halt> {
        match self.state {
            State::Halted(halt) => Some(halt),
            _ => None,
        }
    }

    /// Wakes `token` at the next cycle's `Request`.
    fn wake_next_cycle(&self, ctx: &mut dyn InitContext, token: u64) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: 1,
        };
        ctx.wake_self(when, Phase::Request, token)
    }

    /// Sends a request under the next `TxnId`, which is consumed only if the send succeeds.
    /// In the `M2` and `M3` profiles the counter never wraps: if it cannot advance, nothing
    /// is sent and the session faults (`docs/m2-design.md` §6.2). `M1` keeps its M1
    /// arithmetic.
    fn send(
        &mut self,
        ctx: &mut dyn SimContext,
        msg: impl FnOnce(TxnId) -> MemMsg,
    ) -> Result<TxnId, SimError> {
        let txn = TxnId(self.next_txn);
        match self.config.profile {
            Rv32iProfile::M1 => {
                ctx.send(PORT, msg(txn).into(), ScheduleWhen::Now, Phase::Request)?;
                self.next_txn += 1;
            }
            Rv32iProfile::M2 | Rv32iProfile::M3 => {
                let next = self
                    .next_txn
                    .checked_add(1)
                    .ok_or(SimError::ComponentFault("rv32 cpu: TxnId space exhausted"))?;
                ctx.send(PORT, msg(txn).into(), ScheduleWhen::Now, Phase::Request)?;
                self.next_txn = next;
            }
        }
        Ok(txn)
    }

    fn wake(&mut self, token: u64, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match (token, &self.state) {
            (FETCH, State::FetchIssue { pa }) => {
                let pa = *pa;
                let addr = pa.unwrap_or(u64::from(self.pc));
                let txn = self.send(ctx, |txn| MemMsg::ReadReq { txn, addr, len: 4 })?;
                self.state = State::FetchWait { txn, pa };
                Ok(())
            }
            (MEMORY, State::MemIssue { insn, plan, pa }) => {
                let (insn, plan, pa) = (*insn, plan.clone(), *pa);
                let addr = pa.unwrap_or(u64::from(plan_addr(&plan)));
                let txn = self.send(ctx, |txn| match &plan {
                    MemoryPlan::Load(load) => MemMsg::ReadReq {
                        txn,
                        addr,
                        len: load.width.bytes(),
                    },
                    MemoryPlan::Store(store) => MemMsg::WriteReq {
                        txn,
                        addr,
                        data: store.data.clone(),
                    },
                })?;
                self.state = State::MemWait {
                    txn,
                    insn,
                    plan,
                    pa,
                };
                Ok(())
            }
            (WALK, State::WalkIssue { walk }) => {
                let walk = walk.clone();
                let addr = sv32::pte_address(walk.table, self.walk_va(&walk), walk.level);
                let txn = self.send(ctx, |txn| MemMsg::ReadReq { txn, addr, len: 4 })?;
                self.state = State::WalkWait { txn, walk };
                Ok(())
            }
            (COMMIT, State::CommitPending { insn, outcome, pa }) => {
                let (insn, outcome, pa) = (*insn, *outcome, *pa);
                self.commit(insn, outcome, pa, ctx)
            }
            _ => Err(SimError::ComponentFault(
                "rv32 cpu: wake does not match the execution state",
            )),
        }
    }

    fn response(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let txn = match msg {
            MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => *txn,
            MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. } => {
                return Err(SimError::ComponentFault(
                    "rv32 cpu: request on the initiator port",
                ));
            }
        };
        let expected = match &self.state {
            State::FetchWait { txn, .. }
            | State::MemWait { txn, .. }
            | State::WalkWait { txn, .. } => *txn,
            State::FetchIssue { .. }
            | State::MemIssue { .. }
            | State::WalkIssue { .. }
            | State::CommitPending { .. }
            | State::Halted(_) => {
                return Err(SimError::ComponentFault(
                    "rv32 cpu: response with no transaction outstanding",
                ));
            }
        };
        if txn != expected {
            return Err(SimError::ComponentFault(
                "rv32 cpu: response for a txn that is not outstanding",
            ));
        }
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "rv32 cpu: response arrived outside COMPLETE",
            ));
        }
        let (insn, next, pa) = match &self.state {
            State::WalkWait { walk, .. } => {
                let walk = walk.clone();
                return self.walk_response(walk, msg, ctx);
            }
            State::FetchWait { .. } => match msg {
                MemMsg::ReadResp {
                    outcome: ReadOutcome::Data { data },
                    ..
                } => {
                    let bytes: [u8; 4] = data.as_slice().try_into().map_err(|_| {
                        SimError::ComponentFault("rv32 cpu: fetch returned other than 4 bytes")
                    })?;
                    let word = u32::from_le_bytes(bytes);
                    (
                        Some(word),
                        step(self.isa(), word, self.pc, &self.regs),
                        None,
                    )
                }
                MemMsg::ReadResp {
                    outcome:
                        ReadOutcome::Fault {
                            fault: MemFault::AccessFault,
                        },
                    ..
                } => (
                    None,
                    Step::Done(Outcome::Exec(ExecOutcome::Trap(PendingTrap {
                        cause: TrapCause::InstructionAccessFault,
                        tval: self.pc,
                    }))),
                    None,
                ),
                _ => {
                    return Err(SimError::ComponentFault(
                        "rv32 cpu: write response to a fetch",
                    ));
                }
            },
            State::MemWait { insn, plan, pa, .. } => {
                let outcome = complete_memory(plan, msg).map_err(completion_fault)?;
                // Only a retirement keeps the physical address, for its commit record.
                let pa = match outcome {
                    ExecOutcome::Effect(_) => *pa,
                    ExecOutcome::Trap(_) => None,
                };
                (Some(*insn), Step::Done(Outcome::Exec(outcome)), pa)
            }
            _ => unreachable!("checked above"),
        };
        match next {
            Step::Done(outcome) => {
                self.state = State::CommitPending { insn, outcome, pa };
                ctx.wake_self(ScheduleWhen::Now, Phase::Commit, COMMIT)
            }
            Step::Memory(plan) => {
                let insn = insn.expect("only a fetched word can need memory");
                if self.translating() {
                    self.state = State::WalkIssue {
                        walk: Walk {
                            purpose: Purpose::Data { insn, plan },
                            level: 1,
                            table: self.m3.root(),
                        },
                    };
                    self.wake_next_cycle(ctx, WALK)
                } else {
                    self.state = State::MemIssue {
                        insn,
                        plan,
                        pa: None,
                    };
                    self.wake_next_cycle(ctx, MEMORY)
                }
            }
        }
    }

    /// Takes the response to a PTE read of `walk` (§5.4): the next level, the translated
    /// access, or a trap. A page fault raises the access's page fault, and a PTE read the
    /// bus refuses raises its access fault (§5.3); either has `tval` = the virtual address
    /// and goes to `Commit` in this tick, like any trap.
    fn walk_response(
        &mut self,
        walk: Walk,
        msg: &MemMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let access = walk.purpose.access();
        let pte = match msg {
            MemMsg::ReadResp {
                outcome: ReadOutcome::Data { data },
                ..
            } => {
                let bytes: [u8; 4] = data.as_slice().try_into().map_err(|_| {
                    SimError::ComponentFault("rv32 cpu: PTE read returned other than 4 bytes")
                })?;
                Some(u32::from_le_bytes(bytes))
            }
            MemMsg::ReadResp {
                outcome:
                    ReadOutcome::Fault {
                        fault: MemFault::AccessFault,
                    },
                ..
            } => None,
            _ => {
                return Err(SimError::ComponentFault(
                    "rv32 cpu: write response to a PTE read",
                ));
            }
        };
        let va = self.walk_va(&walk);
        let context = sv32::Context {
            privilege: self.m3.privilege,
            sum: self.m3.sum,
            mxr: self.m3.mxr,
            access,
        };
        let fault = match pte.map(|pte| sv32::step(&context, va, walk.level, pte)) {
            Some(sv32::Step::Next { table }) => {
                self.state = State::WalkIssue {
                    walk: Walk {
                        level: walk.level - 1,
                        table,
                        ..walk
                    },
                };
                return self.wake_next_cycle(ctx, WALK);
            }
            Some(sv32::Step::Leaf { pa }) => {
                let pa = Some(pa);
                return match walk.purpose {
                    Purpose::Fetch => {
                        self.state = State::FetchIssue { pa };
                        self.wake_next_cycle(ctx, FETCH)
                    }
                    Purpose::Data { insn, plan } => {
                        self.state = State::MemIssue { insn, plan, pa };
                        self.wake_next_cycle(ctx, MEMORY)
                    }
                };
            }
            Some(sv32::Step::PageFault) => sv32::Fault::Page,
            None => sv32::Fault::Access,
        };
        let insn = match walk.purpose {
            Purpose::Fetch => None,
            Purpose::Data { insn, .. } => Some(insn),
        };
        let outcome = Outcome::Exec(ExecOutcome::Trap(PendingTrap {
            cause: fault.cause(access),
            tval: va,
        }));
        self.state = State::CommitPending {
            insn,
            outcome,
            pa: None,
        };
        ctx.wake_self(ScheduleWhen::Now, Phase::Commit, COMMIT)
    }

    /// Applies the pending outcome: the only place architectural state changes.
    fn commit(
        &mut self,
        insn: Option<u32>,
        outcome: Outcome,
        pa: Option<u64>,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        if let Outcome::Exec(ExecOutcome::Trap(PendingTrap { cause, tval })) = outcome {
            // A delegated exception (`M3`, docs/m3-design.md §5.3) is delivered to S: not a
            // retirement, so no instret, no instruction limit, and no interrupt sampling.
            if self.config.profile == Rv32iProfile::M3 {
                let (pc, from) = (self.pc, self.m3.privilege);
                if let Some(handler) = self.m3.take_exception(cause, pc, tval) {
                    ctx.trace(
                        EXCEPTION_KIND,
                        vec![
                            ("pc", Value::U64(u64::from(pc))),
                            ("insn", Value::U64(u64::from(insn.unwrap_or(0)))),
                            ("cause", Value::Str(cause.m3_name().to_owned())),
                            ("tval", Value::U64(u64::from(tval))),
                            ("from", Value::Str(from.name().to_owned())),
                            ("to", Value::Str(self.m3.privilege.name().to_owned())),
                        ],
                    );
                    self.pc = handler;
                    return self.fetch_next(ctx);
                }
            }
            ctx.trace(
                TRAP_KIND,
                vec![
                    ("pc", Value::U64(u64::from(self.pc))),
                    ("insn", Value::U64(u64::from(insn.unwrap_or(0)))),
                    ("cause", Value::Str(self.cause_name(cause).to_owned())),
                    ("tval", Value::U64(u64::from(tval))),
                ],
            );
            self.state = State::Halted(Halt::Trap(RvTrap {
                cause,
                pc: self.pc,
                tval,
            }));
            return Ok(());
        }
        let insn = insn.ok_or(SimError::ComponentFault(
            "rv32 cpu: a retiring instruction has no instruction word",
        ))?;
        // Trace fields come from the state before the instruction.
        let fields = match outcome {
            Outcome::Exec(ExecOutcome::Trap(_)) => unreachable!("handled above"),
            Outcome::Exec(ExecOutcome::Effect(effect)) => {
                let fields = self.commit_fields(insn, effect, pa);
                if let Some(RegWrite { rd, value }) = effect.reg_write {
                    self.regs.write(rd, value);
                }
                self.pc = effect.next_pc;
                fields
            }
            Outcome::Csr(op) => self.commit_csr(insn, op)?,
            Outcome::Mret => {
                let effect = PendingEffect {
                    reg_write: None,
                    next_pc: self.csrs.mepc,
                };
                let fields = self.commit_fields(insn, effect, None);
                self.pc = match self.config.profile {
                    Rv32iProfile::M1 | Rv32iProfile::M2 => self.csrs.mret(),
                    Rv32iProfile::M3 => self.m3.mret(&mut self.csrs),
                };
                fields
            }
            Outcome::Sret => {
                let effect = PendingEffect {
                    reg_write: None,
                    next_pc: self.m3.sepc,
                };
                let fields = self.commit_fields(insn, effect, None);
                self.pc = self.m3.sret();
                fields
            }
        };
        self.instret += 1;
        ctx.trace(COMMIT_KIND, fields);
        if self.instret >= self.config.max_instructions.get() {
            ctx.trace(HALT_KIND, vec![("instret", Value::U64(self.instret))]);
            self.state = State::Halted(Halt::InstructionLimit);
            return Ok(());
        }
        // The only interrupt sampling point (m2-design §5.1): after the retirement, with
        // its CSR effects and pc applied.
        if self.config.profile == Rv32iProfile::M2 && self.csrs.mei_eligible() {
            self.pc = self.csrs.take_mei(self.pc);
            ctx.trace(
                INTERRUPT_KIND,
                vec![
                    ("mepc", Value::U64(u64::from(self.csrs.mepc))),
                    ("mcause", Value::U64(u64::from(self.csrs.mcause))),
                    ("handler", Value::U64(u64::from(self.pc))),
                ],
            );
        } else if self.config.profile == Rv32iProfile::M3 && self.m3.mei_eligible(&self.csrs) {
            // docs/m3-design.md §5.1: eligible below M whatever MIE says; taken in M.
            let from = self.m3.privilege;
            self.pc = self.m3.take_mei(&mut self.csrs, self.pc);
            ctx.trace(
                INTERRUPT_KIND,
                vec![
                    ("mepc", Value::U64(u64::from(self.csrs.mepc))),
                    ("mcause", Value::U64(u64::from(self.csrs.mcause))),
                    ("handler", Value::U64(u64::from(self.pc))),
                    ("from", Value::Str(from.name().to_owned())),
                ],
            );
        }
        self.fetch_next(ctx)
    }

    /// Takes an `irq.v0` level on the `irq` port (`M2`, `M3`): valid only in `Complete`, in any
    /// execution state. It changes the level and nothing else; the interrupt is sampled
    /// only when an instruction retires.
    fn irq(&mut self, msg: &IrqMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "rv32 cpu: irq.v0 level arrived outside COMPLETE",
            ));
        }
        let IrqMsg::Level { asserted } = *msg;
        self.csrs.irq_level = asserted;
        Ok(())
    }

    /// Applies a CSR instruction (§6.3): reads the CSR, writes the new value under the
    /// CSR's rule unless the write is suppressed, gives `rd` the old value, and moves
    /// `pc` on. Returns the `rv32.commit` fields: the M1 ones, then `csr` and, when the
    /// instruction writes, `csr_value`, the value the CSR reads after the write.
    fn commit_csr(
        &mut self,
        insn: u32,
        op: PendingCsr,
    ) -> Result<Vec<(&'static str, Value)>, SimError> {
        let unsupported =
            || SimError::ComponentFault("rv32 cpu: pending CSR operation on an unsupported CSR");
        let old = self.read_csr(op.csr).ok_or_else(unsupported)?;
        let effect = PendingEffect {
            reg_write: Some(RegWrite {
                rd: op.rd,
                value: old,
            }),
            next_pc: op.next_pc,
        };
        let mut fields = self.commit_fields(insn, effect, None);
        fields.push(("csr", Value::U64(u64::from(op.csr))));
        if op.write {
            self.write_csr(op.csr, op.op.apply(old, op.operand))
                .ok_or_else(unsupported)?;
            let stored = self.read_csr(op.csr).ok_or_else(unsupported)?;
            fields.push(("csr_value", Value::U64(u64::from(stored))));
        }
        self.regs.write(op.rd, old);
        self.pc = op.next_pc;
        Ok(fields)
    }

    /// The `rv32.commit` fields (§5.7), computed before the effect is applied. The `M3`
    /// profile adds `priv`, the mode the instruction ran in, after `next_pc`, and for
    /// loads and stores `paddr` after `addr` (`docs/m3-design.md` §5.6): `pa`, the address
    /// the Sv32 walk produced, or without translation the address itself.
    fn commit_fields(
        &self,
        insn: u32,
        effect: PendingEffect,
        pa: Option<u64>,
    ) -> Vec<(&'static str, Value)> {
        // A write to x0 writes nothing, and is reported as no write.
        let (rd, rd_value) = effect
            .reg_write
            .filter(|w| w.rd != Reg::ZERO)
            .map_or((0, 0), |w| (w.rd.index(), w.value));
        let mut fields = vec![
            ("pc", Value::U64(u64::from(self.pc))),
            ("insn", Value::U64(u64::from(insn))),
            ("rd", Value::U64(u64::from(rd))),
            ("rd_value", Value::U64(u64::from(rd_value))),
            ("next_pc", Value::U64(u64::from(effect.next_pc))),
        ];
        let isa = self.isa();
        let m3 = if let Isa::M3(mode) = isa {
            fields.push(("priv", Value::U64(u64::from(mode.bits()))));
            true
        } else {
            false
        };
        match step(isa, insn, self.pc, &self.regs) {
            Step::Memory(MemoryPlan::Load(load)) => {
                fields.push(("addr", Value::U64(u64::from(load.addr))));
                if m3 {
                    fields.push(("paddr", Value::U64(pa.unwrap_or(u64::from(load.addr)))));
                }
            }
            Step::Memory(MemoryPlan::Store(store)) => {
                let value = store
                    .data
                    .iter()
                    .rev()
                    .fold(0u64, |v, &b| (v << 8) | u64::from(b));
                fields.push(("addr", Value::U64(u64::from(store.addr))));
                if m3 {
                    fields.push(("paddr", Value::U64(pa.unwrap_or(u64::from(store.addr)))));
                }
                fields.push(("width", Value::U64(store.data.len() as u64)));
                fields.push(("value", Value::U64(value)));
            }
            Step::Done(_) => {}
        }
        fields
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u32(self.config.clock.0);
        w.u32(self.config.entry);
        w.u64(self.config.max_instructions.get());
    }

    /// The restored state, checked against the instruction it belongs to (schemas 1 and
    /// 2; schema 3 restores through [`Rv32iCpu::restore_m3`]).
    fn read_state(
        &self,
        r: &mut SnapshotReader<'_>,
        pc: u32,
        regs: &RegisterFile,
        next_txn: u64,
    ) -> Result<State, RestoreError> {
        let invalid = RestoreError::InvalidState;
        let tag = |what, tag| RestoreError::Decode(DecodeError::InvalidTag { what, tag });
        // With one request at a time, the outstanding one is always the latest issued.
        let outstanding = |txn: u64| {
            if next_txn.checked_sub(1) == Some(txn) {
                Ok(TxnId(txn))
            } else {
                Err(invalid(
                    "rv32 cpu: outstanding txn is not the latest issued",
                ))
            }
        };
        let profile = self.config.profile;
        let isa = match profile {
            Rv32iProfile::M1 => Isa::M1,
            Rv32iProfile::M2 => Isa::M2,
            Rv32iProfile::M3 => unreachable!("schema 3 restores through restore_m3"),
        };
        let memory = |insn: u32| match step(isa, insn, pc, regs) {
            Step::Memory(plan) => Ok(plan),
            Step::Done(_) => Err(invalid(
                "rv32 cpu: pending memory instruction is not an aligned load or store",
            )),
        };
        Ok(match r.u8()? {
            0 => State::FetchIssue { pa: None },
            1 => State::FetchWait {
                txn: outstanding(r.u64()?)?,
                pa: None,
            },
            2 => {
                let insn = r.u32()?;
                State::MemIssue {
                    insn,
                    plan: memory(insn)?,
                    pa: None,
                }
            }
            3 => {
                let txn = outstanding(r.u64()?)?;
                let insn = r.u32()?;
                State::MemWait {
                    txn,
                    insn,
                    plan: memory(insn)?,
                    pa: None,
                }
            }
            4 => {
                let insn = match r.u8()? {
                    0 => None,
                    1 => Some(r.u32()?),
                    t => return Err(tag("rv32 cpu instruction", t)),
                };
                let outcome = read_outcome(r, profile)?;
                if !consistent(isa, false, insn, outcome, pc, regs) {
                    return Err(invalid(
                        "rv32 cpu: pending outcome does not match its instruction",
                    ));
                }
                State::CommitPending {
                    insn,
                    outcome,
                    pa: None,
                }
            }
            5 => State::Halted(match r.u8()? {
                0 => {
                    let cause = read_cause(r, profile)?;
                    let trap = RvTrap {
                        cause,
                        pc: r.u32()?,
                        tval: r.u32()?,
                    };
                    if trap.pc != pc {
                        return Err(invalid("rv32 cpu: trap pc is not the architectural pc"));
                    }
                    Halt::Trap(trap)
                }
                1 => Halt::InstructionLimit,
                t => return Err(tag("rv32 cpu halt", t)),
            }),
            t => return Err(tag("rv32 cpu state", t)),
        })
    }

    /// Restores a schema 3 snapshot (`docs/m3-design.md` §5.5) in three steps: decode every
    /// field to the end, checking only encodings; validate every invariant on the decoded
    /// values together; then replace the state at once. The state record comes before the
    /// mode and the CSRs it depends on, so nothing about it is checked until all are read.
    /// A restored walk or access waits for the event it was waiting for and never sends its
    /// request again.
    fn restore_m3(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        let flag = |r: &mut SnapshotReader<'_>| {
            r.bool()
                .map_err(|_| RestoreError::InvalidState("rv32 cpu: a CSR flag is not 0 or 1"))
        };

        // 1. Decode.
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        let config_matches = r.raw(config.as_bytes().len())? == config.as_bytes();
        let pc = r.u32()?;
        let mut regs = RegisterFile::new();
        for reg in stored_registers() {
            regs.write(reg, r.u32()?);
        }
        let instret = r.u64()?;
        let next_txn = r.u64()?;
        let record = decode_state_m3(r)?;
        let (mie, mpie, meie) = (flag(r)?, flag(r)?, flag(r)?);
        let csrs = CsrFile {
            mie,
            mpie,
            meie,
            mtvec: r.u32()?,
            mscratch: r.u32()?,
            mepc: r.u32()?,
            mcause: r.u32()?,
            mtval: r.u32()?,
            irq_level: flag(r)?,
        };
        let mode = r.u8()?;
        let (sie, spie) = (flag(r)?, flag(r)?);
        let (spp, mpp) = (r.u8()?, r.u8()?);
        let (sum, mxr) = (flag(r)?, flag(r)?);
        let (medeleg, stvec, sscratch, sepc, scause, stval, satp) = (
            r.u32()?,
            r.u32()?,
            r.u32()?,
            r.u32()?,
            r.u32()?,
            r.u32()?,
            r.u32()?,
        );

        // 2. Validate.
        if !config_matches {
            return Err(invalid(
                "rv32 cpu: snapshot was taken with a different configuration",
            ));
        }
        if !pc.is_multiple_of(4) {
            return Err(invalid("rv32 cpu: misaligned pc"));
        }
        if csrs.mtvec & 0b11 != 0 {
            return Err(invalid("rv32 cpu: mtvec MODE is not 0"));
        }
        if csrs.mepc & 0b11 != 0 {
            return Err(invalid("rv32 cpu: misaligned mepc"));
        }
        let m3 = M3State {
            privilege: Privilege::from_bits(mode)
                .ok_or(invalid("rv32 cpu: priv is not U, S, or M"))?,
            sie,
            spie,
            spp: Spp::from_bits(spp).ok_or(invalid("rv32 cpu: SPP is not U or S"))?,
            mpp: Privilege::from_bits(mpp).ok_or(invalid("rv32 cpu: MPP is not U, S, or M"))?,
            sum,
            mxr,
            medeleg,
            stvec,
            sscratch,
            sepc,
            scause,
            stval,
            satp,
        };
        if medeleg & !privilege::MEDELEG_MASK != 0 {
            return Err(invalid("rv32 cpu: medeleg has a bit outside its mask"));
        }
        if stvec & 0b11 != 0 {
            return Err(invalid("rv32 cpu: stvec MODE is not 0"));
        }
        if sepc & 0b11 != 0 {
            return Err(invalid("rv32 cpu: misaligned sepc"));
        }
        if !privilege::satp_reachable(satp) {
            return Err(invalid("rv32 cpu: satp is not a value a write can produce"));
        }
        let state = validate_state_m3(record, &m3, pc, &regs, next_txn)?;
        let limit = self.config.max_instructions.get();
        let consistent_count = match state {
            State::Halted(Halt::InstructionLimit) => instret == limit,
            _ => instret < limit,
        };
        if !consistent_count {
            return Err(invalid(
                "rv32 cpu: instret does not match the instruction limit",
            ));
        }

        // 3. Construct.
        self.pc = pc;
        self.regs = regs;
        self.instret = instret;
        self.next_txn = next_txn;
        self.state = state;
        self.csrs = csrs;
        self.m3 = m3;
        Ok(())
    }
}

/// A schema 3 state record, decoded but not yet checked against anything.
enum StateRecord {
    FetchIssue {
        pa: Option<u64>,
    },
    FetchWait {
        txn: u64,
        pa: Option<u64>,
    },
    MemIssue {
        insn: u32,
        pa: Option<u64>,
    },
    MemWait {
        txn: u64,
        insn: u32,
        pa: Option<u64>,
    },
    WalkIssue {
        walk: WalkRecord,
    },
    WalkWait {
        txn: u64,
        walk: WalkRecord,
    },
    CommitPending {
        insn: Option<u32>,
        outcome: Outcome,
        pa: Option<u64>,
    },
    Trap(RvTrap),
    InstructionLimit,
}

/// A walk as schema 3 stores it: the purpose's raw instruction, not its plan.
struct WalkRecord {
    /// `None` for a fetch, the instruction for a load or store.
    data: Option<u32>,
    level: u8,
    table: u32,
}

/// Schema 3's optional physical address: `u8` 0, or `u8` 1 and the `u64`.
fn write_pa(w: &mut SnapshotWriter, pa: Option<u64>) {
    match pa {
        None => w.u8(0),
        Some(pa) => {
            w.u8(1);
            w.u64(pa);
        }
    }
}

fn read_pa(r: &mut SnapshotReader<'_>) -> Result<Option<u64>, RestoreError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.u64()?)),
        t => Err(RestoreError::Decode(DecodeError::InvalidTag {
            what: "rv32 cpu physical address",
            tag: t,
        })),
    }
}

/// Schema 3's walk: the purpose (`u8` 0 fetch, or 1 data and the `u32` instruction), then
/// `level` (`u8`) and `table` (`u32`).
fn write_walk(w: &mut SnapshotWriter, walk: &Walk) {
    match &walk.purpose {
        Purpose::Fetch => w.u8(0),
        Purpose::Data { insn, .. } => {
            w.u8(1);
            w.u32(*insn);
        }
    }
    w.u8(walk.level);
    w.u32(walk.table);
}

fn read_walk(r: &mut SnapshotReader<'_>) -> Result<WalkRecord, RestoreError> {
    let data = match r.u8()? {
        0 => None,
        1 => Some(r.u32()?),
        t => {
            return Err(RestoreError::Decode(DecodeError::InvalidTag {
                what: "rv32 cpu walk purpose",
                tag: t,
            }));
        }
    };
    Ok(WalkRecord {
        data,
        level: r.u8()?,
        table: r.u32()?,
    })
}

/// Decodes a schema 3 state record, checking only its tags.
fn decode_state_m3(r: &mut SnapshotReader<'_>) -> Result<StateRecord, RestoreError> {
    let tag = |what, tag| RestoreError::Decode(DecodeError::InvalidTag { what, tag });
    Ok(match r.u8()? {
        0 => StateRecord::FetchIssue { pa: read_pa(r)? },
        1 => StateRecord::FetchWait {
            txn: r.u64()?,
            pa: read_pa(r)?,
        },
        2 => StateRecord::MemIssue {
            insn: r.u32()?,
            pa: read_pa(r)?,
        },
        3 => StateRecord::MemWait {
            txn: r.u64()?,
            insn: r.u32()?,
            pa: read_pa(r)?,
        },
        4 => {
            let insn = match r.u8()? {
                0 => None,
                1 => Some(r.u32()?),
                t => return Err(tag("rv32 cpu instruction", t)),
            };
            StateRecord::CommitPending {
                insn,
                outcome: read_outcome(r, Rv32iProfile::M3)?,
                pa: read_pa(r)?,
            }
        }
        5 => match r.u8()? {
            0 => StateRecord::Trap(RvTrap {
                cause: read_cause(r, Rv32iProfile::M3)?,
                pc: r.u32()?,
                tval: r.u32()?,
            }),
            1 => StateRecord::InstructionLimit,
            t => return Err(tag("rv32 cpu halt", t)),
        },
        6 => StateRecord::WalkIssue {
            walk: read_walk(r)?,
        },
        7 => StateRecord::WalkWait {
            txn: r.u64()?,
            walk: read_walk(r)?,
        },
        t => return Err(tag("rv32 cpu state", t)),
    })
}

/// Whether `cause` is a page fault, which only an Sv32 walk raises.
fn page_fault(cause: TrapCause) -> bool {
    matches!(
        cause,
        TrapCause::InstructionPageFault | TrapCause::LoadPageFault | TrapCause::StorePageFault
    )
}

/// Checks a decoded schema 3 state record against the decoded mode, CSRs, `pc`, registers,
/// and next `TxnId`, and builds the state.
///
/// Beyond the M3.2 checks, it rejects what translation cannot reach (§5.5): a walk while
/// translation is off, a `level` above 1, a level-1 `table` other than `satp.PPN`, a
/// `table` wider than a PPN; a `pa` present while translation is off or absent while it is
/// on, one wider than 34 bits, or one whose page offset is not the virtual address's; and
/// a page fault while translation is off. A `pa` in `CommitPending` belongs only to a load
/// or store that retires.
fn validate_state_m3(
    record: StateRecord,
    m3: &M3State,
    pc: u32,
    regs: &RegisterFile,
    next_txn: u64,
) -> Result<State, RestoreError> {
    let invalid = RestoreError::InvalidState;
    let isa = Isa::M3(m3.privilege);
    let translates = m3.translates();
    let outstanding = |txn: u64| {
        if next_txn.checked_sub(1) == Some(txn) {
            Ok(TxnId(txn))
        } else {
            Err(invalid(
                "rv32 cpu: outstanding txn is not the latest issued",
            ))
        }
    };
    let memory = |insn: u32| match step(isa, insn, pc, regs) {
        Step::Memory(plan) => Ok(plan),
        Step::Done(_) => Err(invalid(
            "rv32 cpu: pending memory instruction is not an aligned load or store",
        )),
    };
    // The translated address of an access to `va`: present exactly when translating.
    let translated = |pa: Option<u64>, va: u32| match (translates, pa) {
        (false, None) => Ok(None),
        (false, Some(_)) => Err(invalid(
            "rv32 cpu: physical address while translation is off",
        )),
        (true, None) => Err(invalid(
            "rv32 cpu: no physical address while translation is on",
        )),
        (true, Some(pa)) if pa >> sv32::PA_BITS != 0 => {
            Err(invalid("rv32 cpu: physical address wider than 34 bits"))
        }
        (true, Some(pa)) if pa & 0xFFF != u64::from(va & 0xFFF) => Err(invalid(
            "rv32 cpu: physical address offset is not the virtual address's",
        )),
        (true, Some(pa)) => Ok(Some(pa)),
    };
    let walk = |record: WalkRecord| {
        if !translates {
            return Err(invalid("rv32 cpu: walk while translation is off"));
        }
        if record.level > 1 {
            return Err(invalid("rv32 cpu: walk level above 1"));
        }
        if record.table & !privilege::SATP_PPN != 0 {
            return Err(invalid("rv32 cpu: walk table wider than a PPN"));
        }
        if record.level == 1 && record.table != m3.root() {
            return Err(invalid("rv32 cpu: level-1 walk table is not satp.PPN"));
        }
        let purpose = match record.data {
            None => Purpose::Fetch,
            Some(insn) => Purpose::Data {
                insn,
                plan: memory(insn)?,
            },
        };
        Ok(Walk {
            purpose,
            level: record.level,
            table: record.table,
        })
    };
    Ok(match record {
        StateRecord::FetchIssue { pa } => State::FetchIssue {
            pa: translated(pa, pc)?,
        },
        StateRecord::FetchWait { txn, pa } => State::FetchWait {
            txn: outstanding(txn)?,
            pa: translated(pa, pc)?,
        },
        StateRecord::MemIssue { insn, pa } => {
            let plan = memory(insn)?;
            State::MemIssue {
                insn,
                pa: translated(pa, plan_addr(&plan))?,
                plan,
            }
        }
        StateRecord::MemWait { txn, insn, pa } => {
            let plan = memory(insn)?;
            State::MemWait {
                txn: outstanding(txn)?,
                insn,
                pa: translated(pa, plan_addr(&plan))?,
                plan,
            }
        }
        StateRecord::WalkIssue { walk: record } => State::WalkIssue {
            walk: walk(record)?,
        },
        StateRecord::WalkWait { txn, walk: record } => State::WalkWait {
            txn: outstanding(txn)?,
            walk: walk(record)?,
        },
        StateRecord::CommitPending { insn, outcome, pa } => {
            if !consistent(isa, translates, insn, outcome, pc, regs) {
                return Err(invalid(
                    "rv32 cpu: pending outcome does not match its instruction",
                ));
            }
            let retiring_access = match (insn.map(|w| step(isa, w, pc, regs)), outcome) {
                (Some(Step::Memory(plan)), Outcome::Exec(ExecOutcome::Effect(_))) => Some(plan),
                _ => None,
            };
            let pa = match retiring_access {
                Some(plan) => translated(pa, plan_addr(&plan))?,
                None if pa.is_some() => {
                    return Err(invalid(
                        "rv32 cpu: physical address on an outcome that is not an access",
                    ));
                }
                None => None,
            };
            State::CommitPending { insn, outcome, pa }
        }
        StateRecord::Trap(trap) => {
            if page_fault(trap.cause) && !translates {
                return Err(invalid("rv32 cpu: page fault while translation is off"));
            }
            if trap.pc != pc {
                return Err(invalid("rv32 cpu: trap pc is not the architectural pc"));
            }
            // A halt never changes the mode, so the halted trap is one taken in it: not
            // delegated, and an `ECALL` names it.
            if m3.delegates(trap.cause) {
                return Err(invalid("rv32 cpu: halted on a delegated exception"));
            }
            let ecall = match m3.privilege {
                Privilege::User => TrapCause::EnvironmentCallFromU,
                Privilege::Supervisor => TrapCause::EnvironmentCallFromS,
                Privilege::Machine => TrapCause::EnvironmentCall,
            };
            let is_ecall = matches!(
                trap.cause,
                TrapCause::EnvironmentCallFromU
                    | TrapCause::EnvironmentCallFromS
                    | TrapCause::EnvironmentCall
            );
            if is_ecall && trap.cause != ecall {
                return Err(invalid("rv32 cpu: ecall cause does not match priv"));
            }
            State::Halted(Halt::Trap(trap))
        }
        StateRecord::InstructionLimit => State::Halted(Halt::InstructionLimit),
    })
}

/// Whether `outcome` can be the result of `insn` at `pc` with `regs` for `isa`: exactly
/// the pure result when memory is not involved (including a pending CSR operation,
/// `MRET`, or `SRET`), and a retirement or the matching access fault when it is. With
/// `translates`, the access may also have raised its page fault; the access fault then
/// also covers a PTE read the bus refused (`docs/m3-design.md` §5.3).
fn consistent(
    isa: Isa,
    translates: bool,
    insn: Option<u32>,
    outcome: Outcome,
    pc: u32,
    regs: &RegisterFile,
) -> bool {
    let fault = |access: Access, tval: u32, t: PendingTrap| {
        t == PendingTrap {
            cause: access.access_fault(),
            tval,
        } || (translates
            && t == PendingTrap {
                cause: access.page_fault(),
                tval,
            })
    };
    let Some(word) = insn else {
        return matches!(outcome, Outcome::Exec(ExecOutcome::Trap(t)) if fault(Access::Fetch, pc, t));
    };
    let plan = match step(isa, word, pc, regs) {
        Step::Done(expected) => return expected == outcome,
        Step::Memory(plan) => plan,
    };
    let Outcome::Exec(outcome) = outcome else {
        return false;
    };
    match (plan, outcome) {
        (MemoryPlan::Load(load), ExecOutcome::Effect(e)) => {
            e.next_pc == load.next_pc && e.reg_write.is_some_and(|w| w.rd == load.rd)
        }
        (MemoryPlan::Load(load), ExecOutcome::Trap(t)) => fault(Access::Load, load.addr, t),
        (MemoryPlan::Store(store), ExecOutcome::Effect(e)) => {
            e == PendingEffect {
                reg_write: None,
                next_pc: store.next_pc,
            }
        }
        (MemoryPlan::Store(store), ExecOutcome::Trap(t)) => fault(Access::Store, store.addr, t),
    }
}

/// Snapshot codes for trap causes in schemas 1 and 2, in `docs/m1-design.md` §6 table
/// order.
const CAUSES: [TrapCause; 9] = [
    TrapCause::InstructionAddressMisaligned,
    TrapCause::InstructionAccessFault,
    TrapCause::IllegalInstruction,
    TrapCause::Breakpoint,
    TrapCause::EnvironmentCall,
    TrapCause::LoadAddressMisaligned,
    TrapCause::LoadAccessFault,
    TrapCause::StoreAddressMisaligned,
    TrapCause::StoreAccessFault,
];

/// Snapshot codes for trap causes in schema 3: [`CAUSES`], then the causes `M3` adds
/// (`docs/m3-design.md` §5.5).
const CAUSES_M3: [TrapCause; 14] = [
    TrapCause::InstructionAddressMisaligned,
    TrapCause::InstructionAccessFault,
    TrapCause::IllegalInstruction,
    TrapCause::Breakpoint,
    TrapCause::EnvironmentCall,
    TrapCause::LoadAddressMisaligned,
    TrapCause::LoadAccessFault,
    TrapCause::StoreAddressMisaligned,
    TrapCause::StoreAccessFault,
    TrapCause::EnvironmentCallFromU,
    TrapCause::EnvironmentCallFromS,
    TrapCause::InstructionPageFault,
    TrapCause::LoadPageFault,
    TrapCause::StorePageFault,
];

/// The snapshot cause codes of `profile`'s schema.
fn causes(profile: Rv32iProfile) -> &'static [TrapCause] {
    match profile {
        Rv32iProfile::M1 | Rv32iProfile::M2 => &CAUSES,
        Rv32iProfile::M3 => &CAUSES_M3,
    }
}

/// Every cause a snapshot can hold is in [`CAUSES_M3`], and [`CAUSES`] is its prefix, so
/// a cause from `M1` or `M2` keeps its schema 1 code.
fn write_cause(w: &mut SnapshotWriter, cause: TrapCause) {
    let code = CAUSES_M3
        .iter()
        .position(|&c| c == cause)
        .expect("every cause is listed");
    w.u8(code as u8);
}

fn read_cause(
    r: &mut SnapshotReader<'_>,
    profile: Rv32iProfile,
) -> Result<TrapCause, RestoreError> {
    let code = r.u8()?;
    causes(profile)
        .get(usize::from(code))
        .copied()
        .ok_or(RestoreError::Decode(DecodeError::InvalidTag {
            what: "rv32 trap cause",
            tag: code,
        }))
}

/// Tags 0 (retirement) and 1 (trap) are schema 1's; tags 2 (CSR operation) and 3 (`MRET`)
/// exist only in schemas 2 and 3 (`docs/m2-design.md` §6.4), and tag 4 (`SRET`) only in
/// schema 3 (`docs/m3-design.md` §5.5).
fn write_outcome(w: &mut SnapshotWriter, outcome: Outcome) {
    match outcome {
        Outcome::Exec(ExecOutcome::Effect(e)) => {
            w.u8(0);
            match e.reg_write {
                None => w.u8(0),
                Some(RegWrite { rd, value }) => {
                    w.u8(1);
                    w.u8(rd.index());
                    w.u32(value);
                }
            }
            w.u32(e.next_pc);
        }
        Outcome::Exec(ExecOutcome::Trap(t)) => {
            w.u8(1);
            write_cause(w, t.cause);
            w.u32(t.tval);
        }
        Outcome::Csr(op) => {
            w.u8(2);
            w.u16(op.csr);
            let code = CSR_OPS
                .iter()
                .position(|&o| o == op.op)
                .expect("every operation is listed");
            w.u8(code as u8);
            w.bool(op.write);
            w.u32(op.operand);
            w.u8(op.rd.index());
            w.u32(op.next_pc);
        }
        Outcome::Mret => w.u8(3),
        Outcome::Sret => w.u8(4),
    }
}

fn read_outcome(
    r: &mut SnapshotReader<'_>,
    profile: Rv32iProfile,
) -> Result<Outcome, RestoreError> {
    let tag = |what, tag| RestoreError::Decode(DecodeError::InvalidTag { what, tag });
    Ok(match (r.u8()?, profile) {
        (0, _) => {
            let reg_write = match r.u8()? {
                0 => None,
                1 => {
                    let index = r.u8()?;
                    let rd = Reg::new(index).ok_or(tag("rv32 register", index))?;
                    Some(RegWrite {
                        rd,
                        value: r.u32()?,
                    })
                }
                t => return Err(tag("rv32 register write", t)),
            };
            Outcome::Exec(ExecOutcome::Effect(PendingEffect {
                reg_write,
                next_pc: r.u32()?,
            }))
        }
        (1, _) => Outcome::Exec(ExecOutcome::Trap(PendingTrap {
            cause: read_cause(r, profile)?,
            tval: r.u32()?,
        })),
        (2, Rv32iProfile::M2 | Rv32iProfile::M3) => {
            let csr = r.u16()?;
            let code = r.u8()?;
            let op = *CSR_OPS
                .get(usize::from(code))
                .ok_or(tag("rv32 CSR operation", code))?;
            let write = r.bool()?;
            let operand = r.u32()?;
            let index = r.u8()?;
            let rd = Reg::new(index).ok_or(tag("rv32 register", index))?;
            Outcome::Csr(PendingCsr {
                csr,
                op,
                write,
                operand,
                rd,
                next_pc: r.u32()?,
            })
        }
        (3, Rv32iProfile::M2 | Rv32iProfile::M3) => Outcome::Mret,
        (4, Rv32iProfile::M3) => Outcome::Sret,
        (t, _) => return Err(tag("rv32 outcome", t)),
    })
}

/// Snapshot codes for CSR operations.
const CSR_OPS: [CsrOp; 3] = [CsrOp::Write, CsrOp::Set, CsrOp::Clear];

/// Inspect names of `x1` to `x31`.
const REG_NAMES: [&str; 31] = [
    "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14", "x15",
    "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28",
    "x29", "x30", "x31",
];

/// Inspect names of the CSRs, in [`csr::SUPPORTED`] order.
const CSR_NAMES: [&str; 8] = [
    "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval",
];

/// Inspect names of the `M3` CSRs, in [`privilege::SUPPORTED`] order.
const CSR_NAMES_M3: [&str; 19] = [
    "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval", "sstatus", "sie",
    "sip", "stvec", "sscratch", "sepc", "scause", "stval", "satp", "medeleg", "mideleg",
];

/// `x1` to `x31`, in index order.
fn stored_registers() -> impl Iterator<Item = Reg> {
    (1..32).map(|i| Reg::new(i).expect("below 32"))
}

impl Component for Rv32iCpu {
    fn type_name(&self) -> &'static str {
        "rv32i.cpu"
    }

    /// `mem` in every profile; `irq` too in the `M2` and `M3` profiles.
    fn ports(&self) -> Vec<PortSpec> {
        let mut ports = vec![PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Initiator,
        }];
        if self.config.profile != Rv32iProfile::M1 {
            ports.push(PortSpec {
                name: "irq",
                protocol: irq_v0::PROTOCOL,
                role: Role::Target,
            });
        }
        ports
    }

    /// Schedules the first fetch at the first clock edge, in `Request`.
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: 0,
        };
        ctx.wake_self(when, Phase::Request, FETCH)
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match (self.config.profile, ev) {
            (_, Delivered::Wake { token }) => self.wake(*token, ctx),
            (
                Rv32iProfile::M1,
                Delivered::Message {
                    msg: Message::MemV1(msg),
                    ..
                },
            ) => self.response(msg, ctx),
            (Rv32iProfile::M1, Delivered::Message { .. }) => {
                Err(SimError::ComponentFault("rv32 cpu: message is not mem.v1"))
            }
            (Rv32iProfile::M2 | Rv32iProfile::M3, Delivered::Message { port, msg }) => {
                match (*port, msg) {
                    (PORT, Message::MemV1(msg)) => self.response(msg, ctx),
                    (PORT, _) => Err(SimError::ComponentFault(
                        "rv32 cpu: message on mem is not mem.v1",
                    )),
                    (IRQ_PORT, Message::Irq(msg)) => self.irq(msg, ctx),
                    (IRQ_PORT, _) => Err(SimError::ComponentFault(
                        "rv32 cpu: message on irq is not irq.v0",
                    )),
                    _ => Err(SimError::ComponentFault(
                        "rv32 cpu: message on an unknown port",
                    )),
                }
            }
        }
    }

    /// `pc`, `x1`…`x31`, `instret`, the execution state's name, for a walk (`M3`)
    /// `walk_purpose` (`fetch` or `data`), `walk_level`, and `walk_table`
    /// (`docs/m3-design.md` §5.6), and, once halted, the reason (§5.7). In the `M2` profile, then `mstatus`, `mie`, `mip`, `mtvec`,
    /// `mscratch`, `mepc`, `mcause`, and `mtval` as read (`docs/m2-design.md` §6.5). In
    /// the `M3` profile, then `priv` (0 U, 1 S, 3 M) and the 19 `M3` CSRs as read, in
    /// [`privilege::SUPPORTED`] order (`docs/m3-design.md` §5.6).
    fn inspect(&self) -> StateView {
        let mut fields = vec![("pc", Value::U64(u64::from(self.pc)))];
        for (name, reg) in REG_NAMES.iter().zip(stored_registers()) {
            fields.push((name, Value::U64(u64::from(self.regs.read(reg)))));
        }
        fields.push(("instret", Value::U64(self.instret)));
        fields.push(("state", Value::Str(self.state.name().to_owned())));
        if let State::WalkIssue { walk } | State::WalkWait { walk, .. } = &self.state {
            let purpose = match walk.purpose {
                Purpose::Fetch => "fetch",
                Purpose::Data { .. } => "data",
            };
            fields.push(("walk_purpose", Value::Str(purpose.to_owned())));
            fields.push(("walk_level", Value::U64(u64::from(walk.level))));
            fields.push(("walk_table", Value::U64(u64::from(walk.table))));
        }
        match self.state {
            State::Halted(Halt::Trap(trap)) => {
                fields.push(("halt", Value::Str("trap".to_owned())));
                fields.push(("cause", Value::Str(self.cause_name(trap.cause).to_owned())));
                fields.push(("trap_pc", Value::U64(u64::from(trap.pc))));
                fields.push(("tval", Value::U64(u64::from(trap.tval))));
            }
            State::Halted(Halt::InstructionLimit) => {
                fields.push(("halt", Value::Str("instruction_limit".to_owned())));
            }
            _ => {}
        }
        match self.config.profile {
            Rv32iProfile::M1 => {}
            Rv32iProfile::M2 => {
                for (name, csr) in CSR_NAMES.iter().zip(csr::SUPPORTED) {
                    let value = self.csrs.read(csr).expect("every listed CSR is supported");
                    fields.push((name, Value::U64(u64::from(value))));
                }
            }
            Rv32iProfile::M3 => {
                let mode = self.m3.privilege.bits();
                fields.push(("priv", Value::U64(u64::from(mode))));
                for (name, csr) in CSR_NAMES_M3.iter().zip(privilege::SUPPORTED) {
                    let value = self
                        .m3
                        .read(&self.csrs, csr)
                        .expect("every listed CSR is supported");
                    fields.push((name, Value::U64(u64::from(value))));
                }
            }
        }
        StateView { fields }
    }

    fn snapshot_schema_version(&self) -> u32 {
        match self.config.profile {
            Rv32iProfile::M1 => SNAPSHOT_SCHEMA,
            Rv32iProfile::M2 => SNAPSHOT_SCHEMA_M2,
            Rv32iProfile::M3 => SNAPSHOT_SCHEMA_M3,
        }
    }

    /// Schema 1: the configuration (clock, entry, instruction limit), `pc`, `x1`…`x31`,
    /// `instret`, the next `TxnId`, then the execution state. Pending instructions are
    /// stored as raw words; plans are recomputed from them on restore. Pending events are
    /// the runtime's and are not stored.
    ///
    /// Schema 2 (`M2`): schema 1, then `mstatus.MIE`, `mstatus.MPIE`, `mie.MEIE` (`u8`
    /// 0/1), `mtvec`, `mscratch`, `mepc`, `mcause`, `mtval` (`u32`), and the `irq` input
    /// level (`u8` 0/1). The profile itself is never written.
    ///
    /// Schema 3 (`M3`, `docs/m3-design.md` §5.5): schema 2, then `priv`, `mstatus.SIE`,
    /// `SPIE`, `SPP`, `MPP`, `SUM`, `MXR` (`u8`), `medeleg`, `stvec`, `sscratch`, `sepc`,
    /// `scause`, `stval`, and `satp` (`u32`). Its cause codes and outcome tags extend
    /// schema 2's. Its state records add the Sv32 walk (M3.3, §5.4): `FetchIssue`,
    /// `FetchWait`, `MemIssue`, `MemWait`, and `CommitPending` end with the optional
    /// physical address (`u8` 0, or `u8` 1 and a `u64`), and tags 6 (`WalkIssue`) and 7
    /// (`WalkWait`, with its `TxnId` first) hold the walk: purpose (`u8` 0 fetch, or 1
    /// data and the `u32` instruction), `level` (`u8`), and `table` (`u32`).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u32(self.pc);
        for reg in stored_registers() {
            w.u32(self.regs.read(reg));
        }
        w.u64(self.instret);
        w.u64(self.next_txn);
        // Schemas 1 and 2 never hold a physical address or a walk.
        let m3 = self.config.profile == Rv32iProfile::M3;
        let pa = |w: &mut SnapshotWriter, pa: Option<u64>| {
            if m3 {
                write_pa(w, pa);
            }
        };
        match &self.state {
            State::FetchIssue { pa: p } => {
                w.u8(0);
                pa(w, *p);
            }
            State::FetchWait { txn, pa: p } => {
                w.u8(1);
                w.u64(txn.0);
                pa(w, *p);
            }
            State::MemIssue { insn, pa: p, .. } => {
                w.u8(2);
                w.u32(*insn);
                pa(w, *p);
            }
            State::MemWait {
                txn, insn, pa: p, ..
            } => {
                w.u8(3);
                w.u64(txn.0);
                w.u32(*insn);
                pa(w, *p);
            }
            State::WalkIssue { walk } => {
                w.u8(6);
                write_walk(w, walk);
            }
            State::WalkWait { txn, walk } => {
                w.u8(7);
                w.u64(txn.0);
                write_walk(w, walk);
            }
            State::CommitPending {
                insn,
                outcome,
                pa: p,
            } => {
                w.u8(4);
                match insn {
                    None => w.u8(0),
                    Some(insn) => {
                        w.u8(1);
                        w.u32(*insn);
                    }
                }
                write_outcome(w, *outcome);
                pa(w, *p);
            }
            State::Halted(halt) => {
                w.u8(5);
                match halt {
                    Halt::Trap(trap) => {
                        w.u8(0);
                        write_cause(w, trap.cause);
                        w.u32(trap.pc);
                        w.u32(trap.tval);
                    }
                    Halt::InstructionLimit => w.u8(1),
                }
            }
        }
        if self.config.profile != Rv32iProfile::M1 {
            let c = &self.csrs;
            w.bool(c.mie);
            w.bool(c.mpie);
            w.bool(c.meie);
            w.u32(c.mtvec);
            w.u32(c.mscratch);
            w.u32(c.mepc);
            w.u32(c.mcause);
            w.u32(c.mtval);
            w.bool(c.irq_level);
        }
        if self.config.profile == Rv32iProfile::M3 {
            let m = &self.m3;
            w.u8(m.privilege.bits());
            w.bool(m.sie);
            w.bool(m.spie);
            w.u8(m.spp.bits());
            w.u8(m.mpp.bits());
            w.bool(m.sum);
            w.bool(m.mxr);
            w.u32(m.medeleg);
            w.u32(m.stvec);
            w.u32(m.sscratch);
            w.u32(m.sepc);
            w.u32(m.scause);
            w.u32(m.stval);
            w.u32(m.satp);
        }
    }

    /// Restores every field at once, after checking the snapshot against the configuration
    /// and the pending instruction. Sends nothing: pending events come back with the
    /// runtime's queue. Schema 3 decodes everything, then validates, then builds
    /// (`restore_m3`).
    fn restore(&mut self, r: &mut SnapshotReader<'_>, schema: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        if schema != self.snapshot_schema_version() {
            return Err(invalid(
                "rv32 cpu: snapshot schema does not match the CPU profile",
            ));
        }
        if self.config.profile == Rv32iProfile::M3 {
            return self.restore_m3(r);
        }
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(invalid(
                "rv32 cpu: snapshot was taken with a different configuration",
            ));
        }
        let pc = r.u32()?;
        if !pc.is_multiple_of(4) {
            return Err(invalid("rv32 cpu: misaligned pc"));
        }
        let mut regs = RegisterFile::new();
        for reg in stored_registers() {
            regs.write(reg, r.u32()?);
        }
        let instret = r.u64()?;
        let next_txn = r.u64()?;
        let state = self.read_state(r, pc, &regs, next_txn)?;
        let limit = self.config.max_instructions.get();
        let consistent_count = match state {
            State::Halted(Halt::InstructionLimit) => instret == limit,
            _ => instret < limit,
        };
        if !consistent_count {
            return Err(invalid(
                "rv32 cpu: instret does not match the instruction limit",
            ));
        }
        let csrs = match self.config.profile {
            Rv32iProfile::M1 => CsrFile::new(),
            Rv32iProfile::M2 => read_csrs(r)?,
            Rv32iProfile::M3 => unreachable!("schema 3 restores through restore_m3"),
        };
        self.pc = pc;
        self.regs = regs;
        self.instret = instret;
        self.next_txn = next_txn;
        self.state = state;
        self.csrs = csrs;
        Ok(())
    }
}

/// Schema 2's CSR block, checked: every flag 0 or 1, `mtvec` and `mepc` 4-byte aligned.
fn read_csrs(r: &mut SnapshotReader<'_>) -> Result<CsrFile, RestoreError> {
    let flag = |r: &mut SnapshotReader<'_>| {
        r.bool()
            .map_err(|_| RestoreError::InvalidState("rv32 cpu: a CSR flag is not 0 or 1"))
    };
    let aligned = |v: u32, what| {
        if v & 0b11 == 0 {
            Ok(v)
        } else {
            Err(RestoreError::InvalidState(what))
        }
    };
    let (mie, mpie, meie) = (flag(r)?, flag(r)?, flag(r)?);
    let mtvec = aligned(r.u32()?, "rv32 cpu: mtvec MODE is not 0")?;
    let mscratch = r.u32()?;
    let mepc = aligned(r.u32()?, "rv32 cpu: misaligned mepc")?;
    Ok(CsrFile {
        mie,
        mpie,
        meie,
        mtvec,
        mscratch,
        mepc,
        mcause: r.u32()?,
        mtval: r.u32()?,
        irq_level: flag(r)?,
    })
}
