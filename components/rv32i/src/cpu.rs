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
//!                              └▶ trap: nothing changes ──▶ Halted
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
use systemscope_contracts::protocol::mem_v1::{self, MemFault, MemMsg, ReadOutcome, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;

use crate::decode::{Illegal, decode};
use crate::execute::{
    ExecOutcome, PendingEffect, PendingTrap, RegWrite, TrapCause, execute_alu, execute_control,
    execute_system,
};
use crate::instr::{Instr, Reg};
use crate::memory::{
    MemoryCompletionError, MemoryPlan, MemoryPrep, complete_memory, prepare_memory,
};
use crate::regfile::RegisterFile;

/// The CPU's only port: a `mem.v1` initiator named `mem`, for fetches and data alike.
pub const PORT: PortId = PortId(0);

/// Wake token that sends the next fetch.
pub const FETCH: u64 = 0;

/// Wake token that sends the pending load or store.
pub const MEMORY: u64 = 1;

/// Wake token that commits the pending instruction.
pub const COMMIT: u64 = 2;

/// Layout of [`Rv32iCpu`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Trace kind of a retired instruction.
pub const COMMIT_KIND: &str = "rv32.commit";

/// Trace kind of a trapping instruction.
pub const TRAP_KIND: &str = "rv32.trap";

/// Trace kind of an instruction-limit halt.
pub const HALT_KIND: &str = "rv32.halt";

/// Construction parameters of an [`Rv32iCpu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rv32iConfig {
    /// The clock the CPU runs on.
    pub clock: ClockDomainId,
    /// The first `pc`. Must be 4-byte aligned.
    pub entry: u32,
    /// The CPU halts right after this many instructions retire.
    pub max_instructions: NonZeroU64,
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
enum State {
    /// Waiting for `Wake(FETCH)`.
    FetchIssue,
    /// The fetch `txn` is outstanding.
    FetchWait { txn: TxnId },
    /// Waiting for `Wake(MEMORY)` to send `plan`, for the instruction `insn`.
    MemIssue { insn: u32, plan: MemoryPlan },
    /// The data request `txn` for `plan` is outstanding.
    MemWait {
        txn: TxnId,
        insn: u32,
        plan: MemoryPlan,
    },
    /// Waiting for `Wake(COMMIT)` to apply `outcome`. `insn` is `None` only after a
    /// faulting fetch, which has no instruction.
    CommitPending {
        insn: Option<u32>,
        outcome: ExecOutcome,
    },
    /// Stopped; nothing is pending.
    Halted(Halt),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::FetchIssue => "fetch_issue",
            State::FetchWait { .. } => "fetch_wait",
            State::MemIssue { .. } => "mem_issue",
            State::MemWait { .. } => "mem_wait",
            State::CommitPending { .. } => "commit_pending",
            State::Halted(_) => "halted",
        }
    }
}

/// What a fetched word needs next.
enum Step {
    /// The outcome is known without memory.
    Done(ExecOutcome),
    /// An aligned load or store must access memory first.
    Memory(MemoryPlan),
}

/// Decodes and executes `word` at `pc` against `regs`, through the pure layers only.
fn step(word: u32, pc: u32, regs: &RegisterFile) -> Step {
    let instr = match decode(word) {
        Ok(instr) => instr,
        Err(Illegal { word }) => {
            return Step::Done(ExecOutcome::Trap(PendingTrap {
                cause: TrapCause::IllegalInstruction,
                tval: word,
            }));
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
            .map(|e| Step::Done(ExecOutcome::Effect(e))),
        Instr::Jal { .. } | Instr::Jalr { .. } | Instr::Branch { .. } => {
            execute_control(&instr, pc, a, b).ok().map(Step::Done)
        }
        Instr::Fence | Instr::Ecall | Instr::Ebreak => {
            execute_system(&instr, pc).ok().map(Step::Done)
        }
        Instr::Load { .. } | Instr::Store { .. } => {
            prepare_memory(&instr, pc, a, b)
                .ok()
                .map(|prep| match prep {
                    MemoryPrep::Request(plan) => Step::Memory(plan),
                    MemoryPrep::Trap(trap) => Step::Done(ExecOutcome::Trap(trap)),
                })
        }
    };
    family.expect("every instruction variant is listed with its family")
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
}

impl Rv32iCpu {
    /// A CPU at reset: `pc = entry`, every register 0, about to fetch.
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
            state: State::FetchIssue,
        })
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
    fn send(
        &mut self,
        ctx: &mut dyn SimContext,
        msg: impl FnOnce(TxnId) -> MemMsg,
    ) -> Result<TxnId, SimError> {
        let txn = TxnId(self.next_txn);
        ctx.send(PORT, msg(txn).into(), ScheduleWhen::Now, Phase::Request)?;
        self.next_txn += 1;
        Ok(txn)
    }

    fn wake(&mut self, token: u64, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match (token, &self.state) {
            (FETCH, State::FetchIssue) => {
                let addr = u64::from(self.pc);
                let txn = self.send(ctx, |txn| MemMsg::ReadReq { txn, addr, len: 4 })?;
                self.state = State::FetchWait { txn };
                Ok(())
            }
            (MEMORY, State::MemIssue { insn, plan }) => {
                let (insn, plan) = (*insn, plan.clone());
                let txn = self.send(ctx, |txn| match &plan {
                    MemoryPlan::Load(load) => MemMsg::ReadReq {
                        txn,
                        addr: u64::from(load.addr),
                        len: load.width.bytes(),
                    },
                    MemoryPlan::Store(store) => MemMsg::WriteReq {
                        txn,
                        addr: u64::from(store.addr),
                        data: store.data.clone(),
                    },
                })?;
                self.state = State::MemWait { txn, insn, plan };
                Ok(())
            }
            (COMMIT, State::CommitPending { insn, outcome }) => {
                let (insn, outcome) = (*insn, *outcome);
                self.commit(insn, outcome, ctx)
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
            State::FetchWait { txn } | State::MemWait { txn, .. } => *txn,
            State::FetchIssue
            | State::MemIssue { .. }
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
        let (insn, next) = match &self.state {
            State::FetchWait { .. } => match msg {
                MemMsg::ReadResp {
                    outcome: ReadOutcome::Data { data },
                    ..
                } => {
                    let bytes: [u8; 4] = data.as_slice().try_into().map_err(|_| {
                        SimError::ComponentFault("rv32 cpu: fetch returned other than 4 bytes")
                    })?;
                    let word = u32::from_le_bytes(bytes);
                    (Some(word), step(word, self.pc, &self.regs))
                }
                MemMsg::ReadResp {
                    outcome:
                        ReadOutcome::Fault {
                            fault: MemFault::AccessFault,
                        },
                    ..
                } => (
                    None,
                    Step::Done(ExecOutcome::Trap(PendingTrap {
                        cause: TrapCause::InstructionAccessFault,
                        tval: self.pc,
                    })),
                ),
                _ => {
                    return Err(SimError::ComponentFault(
                        "rv32 cpu: write response to a fetch",
                    ));
                }
            },
            State::MemWait { insn, plan, .. } => {
                let outcome = complete_memory(plan, msg).map_err(completion_fault)?;
                (Some(*insn), Step::Done(outcome))
            }
            _ => unreachable!("checked above"),
        };
        match next {
            Step::Done(outcome) => {
                self.state = State::CommitPending { insn, outcome };
                ctx.wake_self(ScheduleWhen::Now, Phase::Commit, COMMIT)
            }
            Step::Memory(plan) => {
                let insn = insn.expect("only a fetched word can need memory");
                self.state = State::MemIssue { insn, plan };
                self.wake_next_cycle(ctx, MEMORY)
            }
        }
    }

    /// Applies the pending outcome: the only place architectural state changes.
    fn commit(
        &mut self,
        insn: Option<u32>,
        outcome: ExecOutcome,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let effect = match outcome {
            ExecOutcome::Trap(PendingTrap { cause, tval }) => {
                ctx.trace(
                    TRAP_KIND,
                    vec![
                        ("pc", Value::U64(u64::from(self.pc))),
                        ("insn", Value::U64(u64::from(insn.unwrap_or(0)))),
                        ("cause", Value::Str(cause.name().to_owned())),
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
            ExecOutcome::Effect(effect) => effect,
        };
        let insn = insn.ok_or(SimError::ComponentFault(
            "rv32 cpu: a retiring instruction has no instruction word",
        ))?;
        // Trace fields come from the state before the instruction.
        let fields = self.commit_fields(insn, effect);
        if let Some(RegWrite { rd, value }) = effect.reg_write {
            self.regs.write(rd, value);
        }
        self.pc = effect.next_pc;
        self.instret += 1;
        ctx.trace(COMMIT_KIND, fields);
        if self.instret >= self.config.max_instructions.get() {
            ctx.trace(HALT_KIND, vec![("instret", Value::U64(self.instret))]);
            self.state = State::Halted(Halt::InstructionLimit);
            Ok(())
        } else {
            self.state = State::FetchIssue;
            self.wake_next_cycle(ctx, FETCH)
        }
    }

    /// The `rv32.commit` fields (§5.7), computed before the effect is applied.
    fn commit_fields(&self, insn: u32, effect: PendingEffect) -> Vec<(&'static str, Value)> {
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
        match step(insn, self.pc, &self.regs) {
            Step::Memory(MemoryPlan::Load(load)) => {
                fields.push(("addr", Value::U64(u64::from(load.addr))));
            }
            Step::Memory(MemoryPlan::Store(store)) => {
                let value = store
                    .data
                    .iter()
                    .rev()
                    .fold(0u64, |v, &b| (v << 8) | u64::from(b));
                fields.push(("addr", Value::U64(u64::from(store.addr))));
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

    /// The restored state, checked against the instruction it belongs to.
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
        let memory = |insn: u32| match step(insn, pc, regs) {
            Step::Memory(plan) => Ok(plan),
            Step::Done(_) => Err(invalid(
                "rv32 cpu: pending memory instruction is not an aligned load or store",
            )),
        };
        Ok(match r.u8()? {
            0 => State::FetchIssue,
            1 => State::FetchWait {
                txn: outstanding(r.u64()?)?,
            },
            2 => {
                let insn = r.u32()?;
                State::MemIssue {
                    insn,
                    plan: memory(insn)?,
                }
            }
            3 => {
                let txn = outstanding(r.u64()?)?;
                let insn = r.u32()?;
                State::MemWait {
                    txn,
                    insn,
                    plan: memory(insn)?,
                }
            }
            4 => {
                let insn = match r.u8()? {
                    0 => None,
                    1 => Some(r.u32()?),
                    t => return Err(tag("rv32 cpu instruction", t)),
                };
                let outcome = read_outcome(r)?;
                if !consistent(insn, outcome, pc, regs) {
                    return Err(invalid(
                        "rv32 cpu: pending outcome does not match its instruction",
                    ));
                }
                State::CommitPending { insn, outcome }
            }
            5 => State::Halted(match r.u8()? {
                0 => {
                    let cause = read_cause(r)?;
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
}

/// Whether `outcome` can be the result of `insn` at `pc` with `regs`: exactly the pure
/// result when memory is not involved, and a retirement or the matching access fault when
/// it is.
fn consistent(insn: Option<u32>, outcome: ExecOutcome, pc: u32, regs: &RegisterFile) -> bool {
    let Some(word) = insn else {
        return outcome
            == ExecOutcome::Trap(PendingTrap {
                cause: TrapCause::InstructionAccessFault,
                tval: pc,
            });
    };
    match (step(word, pc, regs), outcome) {
        (Step::Done(expected), outcome) => expected == outcome,
        (Step::Memory(MemoryPlan::Load(load)), ExecOutcome::Effect(e)) => {
            e.next_pc == load.next_pc && e.reg_write.is_some_and(|w| w.rd == load.rd)
        }
        (Step::Memory(MemoryPlan::Load(load)), ExecOutcome::Trap(t)) => {
            t == PendingTrap {
                cause: TrapCause::LoadAccessFault,
                tval: load.addr,
            }
        }
        (Step::Memory(MemoryPlan::Store(store)), ExecOutcome::Effect(e)) => {
            e == PendingEffect {
                reg_write: None,
                next_pc: store.next_pc,
            }
        }
        (Step::Memory(MemoryPlan::Store(store)), ExecOutcome::Trap(t)) => {
            t == PendingTrap {
                cause: TrapCause::StoreAccessFault,
                tval: store.addr,
            }
        }
    }
}

/// Snapshot codes for trap causes, in `docs/m1-design.md` §6 table order.
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

fn write_cause(w: &mut SnapshotWriter, cause: TrapCause) {
    let code = CAUSES
        .iter()
        .position(|&c| c == cause)
        .expect("every cause is listed");
    w.u8(code as u8);
}

fn read_cause(r: &mut SnapshotReader<'_>) -> Result<TrapCause, RestoreError> {
    let code = r.u8()?;
    CAUSES
        .get(usize::from(code))
        .copied()
        .ok_or(RestoreError::Decode(DecodeError::InvalidTag {
            what: "rv32 trap cause",
            tag: code,
        }))
}

fn write_outcome(w: &mut SnapshotWriter, outcome: ExecOutcome) {
    match outcome {
        ExecOutcome::Effect(e) => {
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
        ExecOutcome::Trap(t) => {
            w.u8(1);
            write_cause(w, t.cause);
            w.u32(t.tval);
        }
    }
}

fn read_outcome(r: &mut SnapshotReader<'_>) -> Result<ExecOutcome, RestoreError> {
    let tag = |what, tag| RestoreError::Decode(DecodeError::InvalidTag { what, tag });
    Ok(match r.u8()? {
        0 => {
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
            ExecOutcome::Effect(PendingEffect {
                reg_write,
                next_pc: r.u32()?,
            })
        }
        1 => ExecOutcome::Trap(PendingTrap {
            cause: read_cause(r)?,
            tval: r.u32()?,
        }),
        t => return Err(tag("rv32 outcome", t)),
    })
}

/// Inspect names of `x1` to `x31`.
const REG_NAMES: [&str; 31] = [
    "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14", "x15",
    "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28",
    "x29", "x30", "x31",
];

/// `x1` to `x31`, in index order.
fn stored_registers() -> impl Iterator<Item = Reg> {
    (1..32).map(|i| Reg::new(i).expect("below 32"))
}

impl Component for Rv32iCpu {
    fn type_name(&self) -> &'static str {
        "rv32i.cpu"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Initiator,
        }]
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
        match ev {
            Delivered::Wake { token } => self.wake(*token, ctx),
            Delivered::Message {
                msg: Message::MemV1(msg),
                ..
            } => self.response(msg, ctx),
            Delivered::Message { .. } => {
                Err(SimError::ComponentFault("rv32 cpu: message is not mem.v1"))
            }
        }
    }

    /// `pc`, `x1`…`x31`, `instret`, the execution state's name, and, once halted, the
    /// reason (§5.7).
    fn inspect(&self) -> StateView {
        let mut fields = vec![("pc", Value::U64(u64::from(self.pc)))];
        for (name, reg) in REG_NAMES.iter().zip(stored_registers()) {
            fields.push((name, Value::U64(u64::from(self.regs.read(reg)))));
        }
        fields.push(("instret", Value::U64(self.instret)));
        fields.push(("state", Value::Str(self.state.name().to_owned())));
        match self.state {
            State::Halted(Halt::Trap(trap)) => {
                fields.push(("halt", Value::Str("trap".to_owned())));
                fields.push(("cause", Value::Str(trap.cause.name().to_owned())));
                fields.push(("trap_pc", Value::U64(u64::from(trap.pc))));
                fields.push(("tval", Value::U64(u64::from(trap.tval))));
            }
            State::Halted(Halt::InstructionLimit) => {
                fields.push(("halt", Value::Str("instruction_limit".to_owned())));
            }
            _ => {}
        }
        StateView { fields }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (clock, entry, instruction limit), `pc`, `x1`…`x31`,
    /// `instret`, the next `TxnId`, then the execution state. Pending instructions are
    /// stored as raw words; plans are recomputed from them on restore. Pending events are
    /// the runtime's and are not stored.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u32(self.pc);
        for reg in stored_registers() {
            w.u32(self.regs.read(reg));
        }
        w.u64(self.instret);
        w.u64(self.next_txn);
        match &self.state {
            State::FetchIssue => w.u8(0),
            State::FetchWait { txn } => {
                w.u8(1);
                w.u64(txn.0);
            }
            State::MemIssue { insn, .. } => {
                w.u8(2);
                w.u32(*insn);
            }
            State::MemWait { txn, insn, .. } => {
                w.u8(3);
                w.u64(txn.0);
                w.u32(*insn);
            }
            State::CommitPending { insn, outcome } => {
                w.u8(4);
                match insn {
                    None => w.u8(0),
                    Some(insn) => {
                        w.u8(1);
                        w.u32(*insn);
                    }
                }
                write_outcome(w, *outcome);
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
    }

    /// Restores every field at once, after checking the snapshot against the configuration
    /// and the pending instruction. Sends nothing: pending events come back with the
    /// runtime's queue.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
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
        self.pc = pc;
        self.regs = regs;
        self.instret = instret;
        self.next_txn = next_txn;
        self.state = state;
        Ok(())
    }
}
