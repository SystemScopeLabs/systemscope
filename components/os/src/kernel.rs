//! `ModeledKernel`: the component around the pure cores (`docs/m3-design.md` §6.2, §6.3,
//! §6.8).
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `gate`, a `mem.v1` target, the `kgate` window;
//! and `mem`, a `mem.v1` initiator, the bus master `kernel0`.
//!
//! # Two modes
//!
//! - **Prototype** ([`ModeledKernel::new`]): the M3.4a gate prototype, unchanged. An
//!   `ENTER` runs the scripted operation or the failure shutdown of [`crate::core`].
//! - **Processes** ([`ModeledKernel::with_processes`]): the M3.4b process model of
//!   [`crate::procop`]. The first `ENTER` boots the plan's images; every later one is a
//!   trap. The prototype operations never run in this mode.
//!
//! Both share the gate, the engine, and the snapshot's first sections.
//!
//! # The gate
//!
//! `ENTER` is offset [`ENTER`], 4 bytes, write-only. Every other well-formed request (a
//! read, another width or offset) is answered with [`MemFault::AccessFault`] after the
//! configured latency and changes nothing; a zero-length request faults the session.
//!
//! An `ENTER` write is accepted when it is dispatched, like any target's request, but it
//! is **not answered**: the kernel holds its `TxnId` (the downstream one the bus assigned
//! on its `kgate` port) and starts an operation. The CPU stays in `MemWait` on the store
//! for the whole operation. When the operation's last access completes, the kernel sends
//! `WriteResp(Done)` for the held `TxnId` in that `Complete`, exactly once, and returns to
//! `Idle`. A second `ENTER` while one is held faults the session: the CPU is stalled on
//! the first, so no run reaches it.
//!
//! # The engine
//!
//! ```text
//! Idle ──ENTER──▶ Issue (Wake(ISSUE) @ Request, next kernel cycle)
//! Issue ──Wake(ISSUE)──▶ whitelist check ──▶ send the access, Now @ Request ──▶ Wait { txn }
//! Wait ──response @ Complete──▶ Issue (Wake(ISSUE), next cycle)
//!                           └─▶ last access: WriteResp(Done) for the held txn, Now @ Complete ──▶ Idle
//! ```
//!
//! At most one access is outstanding, under a fresh `TxnId` from the `mem` port's
//! counter, which never wraps. An access the whitelist refuses faults the session before
//! anything is sent. Session faults (§6.3): a response with the wrong `TxnId`, kind, or
//! phase, or while nothing is outstanding; read data of the wrong length; a `Fault` on the
//! kernel's own access (every access goes to a granted range the configuration says
//! exists); a wake that does not match the state; and, with processes, a transition that
//! would break a process invariant.
//!
//! # Snapshot
//!
//! The kernel's part of a held entry is the held `TxnId` and its operation. The CPU's
//! `MemWait` and the bus's active `kgate` transaction are theirs; none is copied here, and
//! no request or wake in flight is ever kernel state. **Restore resumes by waiting, never
//! by reissuing**: a `Wait` restores to waiting for the response already in the runtime's
//! queue, and an `Issue` to waiting for its wake. With processes, the snapshot adds the
//! kernel's metadata (§6.8): the PCBs, the run queue, the running PID, and the frame
//! bitmap. Never RAM contents, never CPU state.

use std::collections::VecDeque;

use systemscope_contracts::canonical::DecodeError;
use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{
    self, Access as BusAccess, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

use crate::config::{KernelConfig, KernelConfigError};
use crate::core::{Access, Completion, Op, Operation};
use crate::frames::Frames;
use crate::image::{PlanError, ProcessPlan, perm_bits};
use crate::process::{Context, Pcb, ProcState, Processes, perms_from_bits};
use crate::procop::{Life, Model, Note, ProcOp, Stage};
use crate::pte;
use crate::space::{Region, Space, frames_needed, megapage_conflict};

/// The offset of `ENTER` in the `kgate` window.
pub const ENTER: u64 = 0x0;
/// The size of the `kgate` window (§11.1).
pub const GATE_SIZE: u64 = 0x8;

/// The `kgate` window, a `mem.v1` target.
pub const GATE_PORT: PortId = PortId(0);
/// The bus master `kernel0`, a `mem.v1` initiator.
pub const MEM_PORT: PortId = PortId(1);

/// The wake token that sends the operation's next access.
pub const ISSUE: u64 = 0;

/// Layout of [`ModeledKernel`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Trace kind of an accepted `ENTER`: `txn` (the held one), `value`, `op`.
pub const ENTER_KIND: &str = "os.gate.enter";
/// Trace kind of the held `ENTER`'s release: `txn`.
pub const RELEASE_KIND: &str = "os.gate.release";
/// Trace kind of a shutdown the kernel decides (§6.8): `reason`, `detail`.
pub const SHUTDOWN_KIND: &str = "os.shutdown";

/// The snapshot tag of a process-mode operation, after the prototype's 0 and 1.
const PROC_OP_TAG: u8 = 2;

/// An operation of either mode.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Work {
    /// A prototype operation.
    Proto(Operation),
    /// A process-mode operation.
    Proc(ProcOp),
}

impl Work {
    fn access(&self, config: &KernelConfig) -> Option<Access> {
        match self {
            Work::Proto(op) => op.access(config),
            Work::Proc(op) => op.access(config),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Work::Proto(op) => op.op().name(),
            Work::Proc(op) => op.stage().name(),
        }
    }

    fn step(&self) -> u32 {
        match self {
            Work::Proto(op) => op.step(),
            Work::Proc(op) => op.step(),
        }
    }
}

/// The engine's position (§6.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum State {
    /// No operation; nothing held.
    Idle,
    /// The operation's next access is due at `Wake(ISSUE)`.
    Issue(Work),
    /// The operation's current access is outstanding under `txn`.
    Wait(Work, TxnId),
}

/// The modeled kernel: the `kgate` held entry and the bus-master engine.
pub struct ModeledKernel {
    config: KernelConfig,
    state: State,
    /// The held `ENTER`'s downstream `TxnId`; present exactly when the state is not
    /// `Idle`.
    held: Option<TxnId>,
    /// The next `TxnId` on `mem`.
    next_txn: TxnId,
    /// The process model, in process mode.
    model: Option<Model>,
}

fn violation(v: crate::process::Violation) -> SimError {
    SimError::ComponentFault(v.0)
}

impl ModeledKernel {
    /// Creates an idle prototype kernel with nothing held and the `mem` counter at 0, or
    /// the first rule of [`KernelConfig::validate`] the configuration breaks.
    pub fn new(config: KernelConfig) -> Result<ModeledKernel, KernelConfigError> {
        config.validate()?;
        Ok(ModeledKernel {
            config,
            state: State::Idle,
            held: None,
            next_txn: TxnId(0),
            model: None,
        })
    }

    /// Creates a kernel with processes, awaiting boot: no process, an empty queue, and an
    /// all-free pool; or the first rule of [`ProcessPlan::validate`] the configuration
    /// and plan break.
    pub fn with_processes(
        config: KernelConfig,
        plan: ProcessPlan,
    ) -> Result<ModeledKernel, PlanError> {
        plan.validate(&config)?;
        let frames = Frames::new(config.frame_pool.base, config.frame_pool.size);
        Ok(ModeledKernel {
            config,
            state: State::Idle,
            held: None,
            next_txn: TxnId(0),
            model: Some(Model {
                plan,
                procs: Processes::new(frames),
                life: Life::AwaitBoot,
            }),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &KernelConfig {
        &self.config
    }

    /// The held `ENTER`'s `TxnId`, if one is held.
    pub fn held(&self) -> Option<TxnId> {
        self.held
    }

    /// The processes, in process mode.
    pub fn processes(&self) -> Option<&Processes> {
        self.model.as_ref().map(|m| &m.procs)
    }

    fn work(&self) -> Option<&Work> {
        match &self.state {
            State::Idle => None,
            State::Issue(w) | State::Wait(w, _) => Some(w),
        }
    }

    fn wake_next_cycle(&self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: 1,
        };
        ctx.wake_self(when, Phase::Request, ISSUE)
    }

    /// A request on `gate`.
    fn gate_request(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let enter = match msg.access() {
            None => {
                return Err(SimError::ComponentFault(
                    "modeled kernel: response on the gate port",
                ));
            }
            Some(BusAccess::Empty) => {
                return Err(SimError::ComponentFault(
                    "modeled kernel: zero-length gate request",
                ));
            }
            Some(BusAccess::Bytes { first, last }) => first == ENTER && last == ENTER + 3,
            Some(BusAccess::OutOfRange) => false,
        };
        match msg {
            MemMsg::WriteReq { txn, data, .. } if enter => {
                let value = u32::from_le_bytes(data.as_slice().try_into().expect("4 bytes"));
                self.enter(*txn, value, ctx)
            }
            MemMsg::WriteReq { txn, .. } => self.refuse(
                MemMsg::WriteResp {
                    txn: *txn,
                    outcome: WriteOutcome::Fault {
                        fault: MemFault::AccessFault,
                    },
                },
                ctx,
            ),
            MemMsg::ReadReq { txn, .. } => self.refuse(
                MemMsg::ReadResp {
                    txn: *txn,
                    outcome: ReadOutcome::Fault {
                        fault: MemFault::AccessFault,
                    },
                },
                ctx,
            ),
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => Err(SimError::ComponentFault(
                "modeled kernel: response on the gate port",
            )),
        }
    }

    /// Answers a gate request the kernel does not serve with `resp` after the latency.
    fn refuse(&self, resp: MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(GATE_PORT, resp.into(), when, Phase::Complete)
    }

    fn emit(notes: Vec<Note>, ctx: &mut dyn SimContext) {
        for n in notes {
            ctx.trace(n.kind, n.fields);
        }
    }

    /// An `ENTER` write: hold it and start the operation.
    fn enter(&mut self, txn: TxnId, value: u32, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if self.held.is_some() {
            return Err(SimError::ComponentFault(
                "modeled kernel: second ENTER while one is held",
            ));
        }
        let enter_note = |op: &str| Note {
            kind: ENTER_KIND,
            fields: vec![
                ("txn", Value::U64(txn.0)),
                ("value", Value::U64(u64::from(value))),
                ("op", Value::Str(op.to_owned())),
            ],
        };
        let work = match &mut self.model {
            Some(model) => {
                let mut notes = vec![enter_note(model.op_name(&self.config, value))];
                let op = model
                    .start(&self.config, value, &mut notes)
                    .map_err(violation)?;
                Self::emit(notes, ctx);
                Work::Proc(op)
            }
            None => {
                let op = Operation::start(&self.config, value);
                let mut notes = vec![enter_note(op.op().name())];
                if op.op() == Op::Shutdown {
                    notes.push(Note {
                        kind: SHUTDOWN_KIND,
                        fields: vec![
                            ("reason", Value::U64(1)),
                            (
                                "detail",
                                Value::Str("ENTER value is not the trap frame".to_owned()),
                            ),
                        ],
                    });
                }
                Self::emit(notes, ctx);
                Work::Proto(op)
            }
        };
        self.held = Some(txn);
        self.state = State::Issue(work);
        self.wake_next_cycle(ctx)
    }

    /// `Wake(ISSUE)`: checks the due access against the whitelist and sends it.
    fn issue(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let State::Issue(work) = &self.state else {
            return Err(SimError::ComponentFault(
                "modeled kernel: ISSUE wake with nothing to issue",
            ));
        };
        let access = work.access(&self.config).ok_or(SimError::ComponentFault(
            "modeled kernel: ISSUE wake after the last access",
        ))?;
        if !self.config.permits(access.addr(), access.len()) {
            return Err(SimError::ComponentFault(
                "modeled kernel: access outside the whitelist",
            ));
        }
        let txn = self.next_txn;
        let next = txn.0.checked_add(1).ok_or(SimError::ComponentFault(
            "modeled kernel: TxnId space exhausted",
        ))?;
        let msg = match access {
            Access::Read { addr, len } => MemMsg::ReadReq { txn, addr, len },
            Access::Write { addr, data } => MemMsg::WriteReq { txn, addr, data },
        };
        ctx.send(MEM_PORT, msg.into(), ScheduleWhen::Now, Phase::Request)?;
        self.next_txn = TxnId(next);
        let State::Issue(work) = std::mem::replace(&mut self.state, State::Idle) else {
            unreachable!("checked above")
        };
        self.state = State::Wait(work, txn);
        Ok(())
    }

    /// A message on `mem`: the response to the outstanding access, in `Complete`.
    fn mem_response(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let State::Wait(work, expected) = &self.state else {
            return Err(SimError::ComponentFault(
                "modeled kernel: mem.v1 message with no access outstanding",
            ));
        };
        let (txn, completion) = match msg {
            MemMsg::ReadResp { txn, outcome } => match outcome {
                ReadOutcome::Data { data } => (*txn, Some(Completion::Data(data.clone()))),
                ReadOutcome::Fault { .. } => (*txn, None),
            },
            MemMsg::WriteResp { txn, outcome } => match outcome {
                WriteOutcome::Done => (*txn, Some(Completion::Written)),
                WriteOutcome::Fault { .. } => (*txn, None),
            },
            MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. } => {
                return Err(SimError::ComponentFault(
                    "modeled kernel: request on the mem port",
                ));
            }
        };
        if txn != *expected {
            return Err(SimError::ComponentFault(
                "modeled kernel: response for a txn that is not outstanding",
            ));
        }
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "modeled kernel: response arrived outside COMPLETE",
            ));
        }
        let wrote = matches!(work.access(&self.config), Some(Access::Write { .. }));
        if wrote != matches!(msg, MemMsg::WriteResp { .. }) {
            return Err(SimError::ComponentFault(
                "modeled kernel: response kind mismatch",
            ));
        }
        let completion = completion.ok_or(SimError::ComponentFault(
            "modeled kernel: its own access faulted",
        ))?;
        let wrong_length =
            |_| SimError::ComponentFault("modeled kernel: read data of the wrong length");
        let next = match work.clone() {
            Work::Proto(mut op) => {
                op.complete(&self.config, completion)
                    .map_err(wrong_length)?;
                (!op.is_finished(&self.config)).then_some(Work::Proto(op))
            }
            Work::Proc(mut op) => {
                op.complete(&self.config, completion)
                    .map_err(wrong_length)?;
                if op.is_finished(&self.config) {
                    let model = self.model.as_mut().expect("a process op has a model");
                    let mut notes = Vec::new();
                    let next = model
                        .advance(&self.config, op, &mut notes)
                        .map_err(violation)?;
                    Self::emit(notes, ctx);
                    next.map(Work::Proc)
                } else {
                    Some(Work::Proc(op))
                }
            }
        };
        match next {
            None => self.release(ctx),
            Some(work) => {
                self.state = State::Issue(work);
                self.wake_next_cycle(ctx)
            }
        }
    }

    /// The operation has finished: answers the held `ENTER` once and returns to `Idle`.
    fn release(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let txn = self.held.ok_or(SimError::ComponentFault(
            "modeled kernel: an operation without a held ENTER",
        ))?;
        let resp = MemMsg::WriteResp {
            txn,
            outcome: WriteOutcome::Done,
        };
        ctx.send(GATE_PORT, resp.into(), ScheduleWhen::Now, Phase::Complete)?;
        ctx.trace(RELEASE_KIND, vec![("txn", Value::U64(txn.0))]);
        self.held = None;
        self.state = State::Idle;
        Ok(())
    }
}

/// A PCB's state as inspect shows it: the name, with the fields of a terminal state.
fn state_text(state: ProcState) -> String {
    match state {
        ProcState::Exited { status } => format!("exited({status})"),
        ProcState::Faulted { cause, epc, tval } => {
            format!("faulted({cause:#x},{epc:#x},{tval:#x})")
        }
        other => other.name().to_owned(),
    }
}

impl Component for ModeledKernel {
    fn type_name(&self) -> &'static str {
        "os.modeled_kernel"
    }

    /// `gate`, `mem`.
    fn ports(&self) -> Vec<PortSpec> {
        vec![
            PortSpec {
                name: "gate",
                protocol: mem_v1::PROTOCOL,
                role: Role::Target,
            },
            PortSpec {
                name: "mem",
                protocol: mem_v1::PROTOCOL,
                role: Role::Initiator,
            },
        ]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    /// Serves `gate` requests, `Wake(ISSUE)`, and the response to the outstanding access
    /// on `mem`. Anything else faults the session.
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                port: GATE_PORT,
                msg: Message::MemV1(msg),
            } => self.gate_request(msg, ctx),
            Delivered::Message {
                port: MEM_PORT,
                msg: Message::MemV1(msg),
            } => self.mem_response(msg, ctx),
            Delivered::Wake { token: ISSUE } => self.issue(ctx),
            _ => Err(SimError::ComponentFault(
                "modeled kernel: unexpected delivery",
            )),
        }
    }

    /// `phase` (`idle`, `issue`, `wait`), `op` (`none`, or the operation: `script` and
    /// `shutdown` in prototype mode, the stage `create`, `read_frame`, `dispatch`, or
    /// `shutdown` with processes), `step`, `held_txn` and `mem_txn` (the outstanding
    /// access's, `none` or the number), `next_txn`, and the due or outstanding access as
    /// `pending_kind` (`none`, `read`, `write`), `pending_addr`, and `pending_len` (0 with
    /// `none`).
    ///
    /// With processes, also (§6.8): `life` (`await_boot`, `up`, `down`), `running` (the
    /// PID or `none`), `queue` (PIDs head first, comma-separated), `processes` (one
    /// `pid:state:root` per PCB, the root PPN in hex, `;`-separated), and `free_frames`.
    fn inspect(&self) -> StateView {
        let phase = match self.state {
            State::Idle => "idle",
            State::Issue(_) => "issue",
            State::Wait(..) => "wait",
        };
        let work = self.work();
        let access = work.and_then(|w| w.access(&self.config));
        let number = |t: Option<TxnId>| t.map_or("none".to_owned(), |t| t.0.to_string());
        let mem_txn = match self.state {
            State::Wait(_, txn) => Some(txn),
            State::Idle | State::Issue(_) => None,
        };
        let mut fields = vec![
            ("phase", Value::Str(phase.to_owned())),
            ("op", Value::Str(work.map_or("none", Work::name).to_owned())),
            ("step", Value::U64(work.map_or(0, |w| u64::from(w.step())))),
            ("held_txn", Value::Str(number(self.held))),
            ("mem_txn", Value::Str(number(mem_txn))),
            ("next_txn", Value::U64(self.next_txn.0)),
            (
                "pending_kind",
                Value::Str(
                    match access {
                        None => "none",
                        Some(Access::Read { .. }) => "read",
                        Some(Access::Write { .. }) => "write",
                    }
                    .to_owned(),
                ),
            ),
            (
                "pending_addr",
                Value::U64(access.as_ref().map_or(0, Access::addr)),
            ),
            (
                "pending_len",
                Value::U64(access.as_ref().map_or(0, Access::len)),
            ),
        ];
        if let Some(model) = &self.model {
            let procs = &model.procs;
            let life = match model.life {
                Life::AwaitBoot => "await_boot",
                Life::Up => "up",
                Life::Down => "down",
            };
            let queue: Vec<String> = procs.queue().iter().map(u32::to_string).collect();
            let table: Vec<String> = procs
                .pcbs()
                .iter()
                .map(|p| format!("{}:{}:{:#x}", p.pid, state_text(p.state), p.root))
                .collect();
            fields.extend([
                ("life", Value::Str(life.to_owned())),
                (
                    "running",
                    Value::Str(procs.current().map_or("none".to_owned(), |p| p.to_string())),
                ),
                ("queue", Value::Str(queue.join(","))),
                ("processes", Value::Str(table.join(";"))),
                (
                    "free_frames",
                    Value::U64(procs.frames().free_count() as u64),
                ),
            ]);
        }
        StateView { fields }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1:
    ///
    /// 1. the configuration: `clock` (`u32`); `latency` (tag `u8`, then `u128`
    ///    femtoseconds or `u32` domain and `u64` cycles); the RAM and `kgate` windows
    ///    (`u64` base, `u64` size each); the trap frame (`u32`); staging, the frame pool,
    ///    and the block controller windows; UART TX (`u64`);
    /// 2. with processes only, the plan: the marker `0xA5`; `USER_BASE`, `STACK_TOP`,
    ///    `STACK_PAGES` (`u32` each); the images (count, then per image `staged`,
    ///    `file_len`, `entry`, and the segments: count, then `index` `u16`, `vaddr`,
    ///    `memsz`, permission bits `u8`, and the pages: count, then `va`, permission bits,
    ///    and the copy as tag `u8` 0, or 1 then `file_offset`, `page_offset`, `len`);
    /// 3. the state (`u8`: 0 `Idle`, 1 `Issue`, 2 `Wait` followed by the outstanding
    ///    `u64` txn), then, unless `Idle`, the operation: in prototype mode `u8` 0
    ///    `Script` or 1 `Shutdown`, its step (`u32`), and its working data
    ///    (length-prefixed bytes); with processes `u8` 2, the stage (`u8`: 0 `Create`
    ///    with the PID and its reserved frames as a count and PPNs, 1 `ReadFrame`, 2
    ///    `Dispatch` with the PID and the 33 context words, 3 `Shutdown` with the
    ///    reason), its step, and its working data;
    /// 4. the held `ENTER` (tag `u8` 0, or 1 then the `u64` txn);
    /// 5. the next `mem` `TxnId` (`u64`);
    /// 6. with processes only: the life (`u8`: 0 `AwaitBoot`, 1 `Up`, 2 `Down`); the
    ///    PCBs in PID order (count, then per PCB the PID; the state `u8` 0 `Ready`, 1
    ///    `Running`, 2 `Exited` with the status as `u32`, 3 `Faulted` with `cause`,
    ///    `epc`, `tval`; the context as tag 0, or 1 then 33 words; the root PPN; the
    ///    table PPNs as a count and PPNs; the regions: count, then `va`, permission bits,
    ///    and the frames as a count and PPNs); the run queue head first (count and
    ///    PIDs); the running PID (tag 0, or 1 then the PID); and the frame bitmap
    ///    (length-prefixed bytes).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.config.encode(w);
        if let Some(model) = &self.model {
            model.plan.encode(w);
        }
        match &self.state {
            State::Idle => w.u8(0),
            State::Issue(_) => w.u8(1),
            State::Wait(_, txn) => {
                w.u8(2);
                w.u64(txn.0);
            }
        }
        match self.work() {
            None => {}
            Some(Work::Proto(op)) => {
                w.u8(match op.op() {
                    Op::Script => 0,
                    Op::Shutdown => 1,
                });
                w.u32(op.step());
                w.bytes(op.data());
            }
            Some(Work::Proc(op)) => {
                w.u8(PROC_OP_TAG);
                encode_stage(op.stage(), w);
                w.u32(op.step());
                w.bytes(op.data());
            }
        }
        match self.held {
            None => w.u8(0),
            Some(txn) => {
                w.u8(1);
                w.u64(txn.0);
            }
        }
        w.u64(self.next_txn.0);
        if let Some(model) = &self.model {
            encode_model(model, w);
        }
    }

    /// Replaces the whole state with the snapshot's, or changes nothing.
    ///
    /// Rejects a different configuration or plan and every state no run can produce
    /// (§6.8): a held `ENTER` with `Idle`, or an operation without one; an operation of
    /// the other mode; an operation step at or past its stage's last access, or working
    /// data that does not belong to it; a due or outstanding access the whitelist
    /// refuses; and a `Wait` whose txn is not the latest issued (next `TxnId` − 1).
    ///
    /// With processes it also rejects every table that breaks an invariant of
    /// [`Processes::check`] (two `Running`; a current PID that is not the `Running`
    /// process; a queue entry that is not a distinct `Ready` process, or a `Ready` one
    /// missing from it; a context on a process that is not `Ready`, or none on a `Ready`
    /// one; a frame allocated but owned by no live process or creation, or the reverse; a
    /// frame owned twice); a live process whose tables and regions are not those its
    /// image builds in its frames; `Idle` after boot with no process running; boot state
    /// before the first `ENTER`; and an operation its stage cannot be in: a creation that
    /// is not the next image's, with frames that are not the lowest free ones it needs, a
    /// frame read with no process running, a dispatch of another PID than the running
    /// one, or a shutdown that was not decided.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        let mut config = SnapshotWriter::new();
        self.config.encode(&mut config);
        if let Some(model) = &self.model {
            model.plan.encode(&mut config);
        }
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(invalid("modeled kernel: snapshot has a different config"));
        }
        let tag = r.u8()?;
        let outstanding = match tag {
            0 | 1 => None,
            2 => Some(TxnId(r.u64()?)),
            tag => {
                return Err(RestoreError::Decode(DecodeError::InvalidTag {
                    what: "modeled kernel state",
                    tag,
                }));
            }
        };
        let raw_op = if tag == 0 { None } else { Some(decode_op(r)?) };
        let held = match r.u8()? {
            0 => None,
            1 => Some(TxnId(r.u64()?)),
            tag => {
                return Err(RestoreError::Decode(DecodeError::InvalidTag {
                    what: "modeled kernel held ENTER",
                    tag,
                }));
            }
        };
        let next_txn = r.u64()?;
        let model = match &self.model {
            None => None,
            Some(m) => Some(decode_model(r, &self.config, &m.plan)?),
        };
        let work = match (raw_op, &model) {
            (None, _) => None,
            (Some(RawOp::Proto(op, step, data)), None) => Some(Work::Proto(
                Operation::from_parts(&self.config, op, step, data).ok_or(invalid(
                    "modeled kernel: operation step or working data not reachable",
                ))?,
            )),
            (Some(RawOp::Proc(stage, step, data)), Some(model)) => {
                Some(Work::Proc(proc_op(&self.config, model, stage, step, data)?))
            }
            _ => {
                return Err(invalid("modeled kernel: operation of the other mode"));
            }
        };
        if held.is_some() != work.is_some() {
            return Err(invalid(
                "modeled kernel: a held ENTER without an operation, or the reverse",
            ));
        }
        if let Some(work) = &work {
            let access = work
                .access(&self.config)
                .expect("from_parts checked the step");
            if !self.config.permits(access.addr(), access.len()) {
                return Err(invalid(
                    "modeled kernel: pending access outside the whitelist",
                ));
            }
        }
        if let Some(txn) = outstanding
            && next_txn.checked_sub(1) != Some(txn.0)
        {
            return Err(invalid(
                "modeled kernel: outstanding txn is not the latest issued",
            ));
        }
        let mut model = model;
        if let Some(model) = &mut model {
            check_life(model, work.as_ref())?;
        }
        self.state = match (work, outstanding) {
            (None, _) => State::Idle,
            (Some(work), None) => State::Issue(work),
            (Some(work), Some(txn)) => State::Wait(work, txn),
        };
        self.held = held;
        self.next_txn = TxnId(next_txn);
        if model.is_some() {
            self.model = model;
        }
        Ok(())
    }
}

/// A decoded operation before it is checked.
enum RawOp {
    Proto(Op, u32, Vec<u8>),
    Proc(RawStage, u32, Vec<u8>),
}

/// A decoded stage before it is checked.
enum RawStage {
    Create { pid: u32, frames: Vec<u32> },
    ReadFrame,
    Dispatch { pid: u32, context: Context },
    Shutdown { reason: u32 },
}

fn encode_context(c: &Context, w: &mut SnapshotWriter) {
    for &word in c.regs.iter().chain([&c.pc, &c.sstatus]) {
        w.u32(word);
    }
}

fn decode_context(r: &mut SnapshotReader<'_>) -> Result<Context, RestoreError> {
    let mut words = [0u32; 33];
    for word in &mut words {
        *word = r.u32()?;
    }
    Ok(Context {
        regs: std::array::from_fn(|i| words[i]),
        pc: words[31],
        sstatus: words[32],
    })
}

fn encode_ppns(ppns: &[u32], w: &mut SnapshotWriter) {
    w.len(ppns.len());
    for &p in ppns {
        w.u32(p);
    }
}

/// A count of elements at least `min_bytes` long each, bounded by what remains, so a
/// corrupt count never allocates.
fn count(r: &mut SnapshotReader<'_>, min_bytes: usize) -> Result<usize, RestoreError> {
    let n = r.len()?;
    if n.saturating_mul(min_bytes) > r.remaining() {
        return Err(RestoreError::Decode(DecodeError::Truncated));
    }
    Ok(n)
}

fn decode_ppns(r: &mut SnapshotReader<'_>) -> Result<Vec<u32>, RestoreError> {
    let n = count(r, 4)?;
    (0..n).map(|_| Ok(r.u32()?)).collect()
}

fn encode_stage(stage: &Stage, w: &mut SnapshotWriter) {
    match stage {
        Stage::Create { pid, space } => {
            w.u8(0);
            w.u32(*pid);
            encode_ppns(space.frames(), w);
        }
        Stage::ReadFrame => w.u8(1),
        Stage::Dispatch { pid, context, .. } => {
            w.u8(2);
            w.u32(*pid);
            encode_context(context, w);
        }
        Stage::Shutdown { reason } => {
            w.u8(3);
            w.u32(*reason);
        }
    }
}

fn decode_op(r: &mut SnapshotReader<'_>) -> Result<RawOp, RestoreError> {
    let tag = r.u8()?;
    let op = match tag {
        0 => Some(Op::Script),
        1 => Some(Op::Shutdown),
        PROC_OP_TAG => None,
        tag => {
            return Err(RestoreError::Decode(DecodeError::InvalidTag {
                what: "modeled kernel operation",
                tag,
            }));
        }
    };
    let stage = match op {
        Some(_) => None,
        None => Some(match r.u8()? {
            0 => RawStage::Create {
                pid: r.u32()?,
                frames: decode_ppns(r)?,
            },
            1 => RawStage::ReadFrame,
            2 => RawStage::Dispatch {
                pid: r.u32()?,
                context: decode_context(r)?,
            },
            3 => RawStage::Shutdown { reason: r.u32()? },
            tag => {
                return Err(RestoreError::Decode(DecodeError::InvalidTag {
                    what: "modeled kernel stage",
                    tag,
                }));
            }
        }),
    };
    let step = r.u32()?;
    let data = r.bytes()?.to_vec();
    Ok(match (op, stage) {
        (Some(op), _) => RawOp::Proto(op, step, data),
        (None, Some(stage)) => RawOp::Proc(stage, step, data),
        (None, None) => unreachable!("a process op has a stage"),
    })
}

fn encode_model(model: &Model, w: &mut SnapshotWriter) {
    w.u8(match model.life {
        Life::AwaitBoot => 0,
        Life::Up => 1,
        Life::Down => 2,
    });
    let procs = &model.procs;
    w.len(procs.pcbs().len());
    for p in procs.pcbs() {
        w.u32(p.pid);
        match p.state {
            ProcState::Ready => w.u8(0),
            ProcState::Running => w.u8(1),
            ProcState::Exited { status } => {
                w.u8(2);
                w.u32(status as u32);
            }
            ProcState::Faulted { cause, epc, tval } => {
                w.u8(3);
                w.u32(cause);
                w.u32(epc);
                w.u32(tval);
            }
        }
        match &p.context {
            None => w.u8(0),
            Some(c) => {
                w.u8(1);
                encode_context(c, w);
            }
        }
        w.u32(p.root);
        encode_ppns(&p.tables, w);
        w.len(p.regions.len());
        for region in &p.regions {
            w.u32(region.va);
            w.u8(perm_bits(region.perms));
            encode_ppns(&region.frames, w);
        }
    }
    w.len(procs.queue().len());
    for &pid in procs.queue() {
        w.u32(pid);
    }
    match procs.current() {
        None => w.u8(0),
        Some(pid) => {
            w.u8(1);
            w.u32(pid);
        }
    }
    w.bytes(&procs.frames().bitmap());
}

fn bad_tag(what: &'static str, tag: u8) -> RestoreError {
    RestoreError::Decode(DecodeError::InvalidTag { what, tag })
}

/// Decodes and checks the process section; the operation is checked against it later.
fn decode_model(
    r: &mut SnapshotReader<'_>,
    config: &KernelConfig,
    plan: &ProcessPlan,
) -> Result<Model, RestoreError> {
    let invalid = RestoreError::InvalidState;
    let life = match r.u8()? {
        0 => Life::AwaitBoot,
        1 => Life::Up,
        2 => Life::Down,
        tag => return Err(bad_tag("modeled kernel life", tag)),
    };
    let n = count(r, 4)?;
    let mut pcbs = Vec::with_capacity(n);
    for _ in 0..n {
        let pid = r.u32()?;
        let state = match r.u8()? {
            0 => ProcState::Ready,
            1 => ProcState::Running,
            2 => ProcState::Exited {
                status: r.u32()? as i32,
            },
            3 => ProcState::Faulted {
                cause: r.u32()?,
                epc: r.u32()?,
                tval: r.u32()?,
            },
            tag => return Err(bad_tag("modeled kernel process state", tag)),
        };
        let context = match r.u8()? {
            0 => None,
            1 => Some(decode_context(r)?),
            tag => return Err(bad_tag("modeled kernel context", tag)),
        };
        let root = r.u32()?;
        let tables = decode_ppns(r)?;
        let regions_n = count(r, 9)?;
        let mut regions = Vec::with_capacity(regions_n);
        for _ in 0..regions_n {
            let va = r.u32()?;
            let bits = r.u8()?;
            let perms = perms_from_bits(bits).ok_or(bad_tag("modeled kernel perms", bits))?;
            regions.push(Region {
                va,
                perms,
                frames: decode_ppns(r)?,
            });
        }
        pcbs.push(Pcb {
            pid,
            state,
            context,
            root,
            tables,
            regions,
        });
    }
    let queue: VecDeque<u32> = decode_ppns(r)?.into();
    let current = match r.u8()? {
        0 => None,
        1 => Some(r.u32()?),
        tag => return Err(bad_tag("modeled kernel running PID", tag)),
    };
    let bitmap = r.bytes()?.to_vec();
    let pool = Frames::new(config.frame_pool.base, config.frame_pool.size);
    if bitmap.len() != pool.count().div_ceil(8) {
        return Err(invalid("modeled kernel: frame bitmap of the wrong size"));
    }
    if (pool.count()..bitmap.len() * 8).any(|i| Frames::bit(&bitmap, i)) {
        return Err(invalid("modeled kernel: frame bitmap bit past the pool"));
    }
    // Owners from the live PCBs; the creation in flight, if any, is added when the
    // operation is checked. Duplicates and strays are caught by `Processes::check`.
    let mut owners: Vec<u32> = (0..pool.count())
        .map(|i| if Frames::bit(&bitmap, i) { u32::MAX } else { 0 })
        .collect();
    for p in &pcbs {
        for ppn in p.frames() {
            if let Some(o) = ppn
                .checked_sub(pool.base())
                .and_then(|i| owners.get_mut(i as usize))
                && *o == u32::MAX
            {
                *o = p.pid;
            }
        }
    }
    for p in &pcbs {
        if p.state.is_terminal() {
            continue;
        }
        let Some(boot) = p
            .pid
            .checked_sub(1)
            .and_then(|i| plan.images.get(i as usize))
        else {
            return Err(invalid("modeled kernel: a PID outside the plan"));
        };
        let mut frames = p.frames();
        frames.sort_unstable();
        if frames.len() != frames_needed(&boot.image, &plan.layout)
            || megapage_conflict(&boot.image, config)
        {
            return Err(invalid(
                "modeled kernel: a process's frames do not fit its image",
            ));
        }
        let space = Space::build(config, &plan.layout, boot, &frames);
        if space.root != p.root || space.tables != p.tables || space.regions != p.regions {
            return Err(invalid(
                "modeled kernel: a process's tables or regions are not its image's",
            ));
        }
    }
    Ok(Model {
        plan: plan.clone(),
        procs: Processes::from_parts(
            pcbs,
            queue,
            current,
            Frames::from_owners(pool.base(), owners),
        ),
        life,
    })
}

/// Checks a decoded process-mode stage against the restored model and builds its
/// operation. For a creation, the in-flight frames get their owner here.
fn proc_op(
    config: &KernelConfig,
    model: &Model,
    stage: RawStage,
    step: u32,
    data: Vec<u8>,
) -> Result<ProcOp, RestoreError> {
    let invalid = RestoreError::InvalidState;
    let procs = &model.procs;
    let stage = match stage {
        RawStage::Create { pid, frames } => {
            let Some(boot) = pid
                .checked_sub(1)
                .and_then(|i| model.plan.images.get(i as usize))
            else {
                return Err(invalid(
                    "modeled kernel: creation of a PID outside the plan",
                ));
            };
            if model.life != Life::Up
                || procs.current().is_some()
                || procs.pcbs().iter().any(|p| p.pid >= pid)
            {
                return Err(invalid("modeled kernel: creation out of boot order"));
            }
            let ascending = frames.windows(2).all(|w| w[0] < w[1]);
            if !ascending
                || frames.len() != frames_needed(&boot.image, &model.plan.layout)
                || megapage_conflict(&boot.image, config)
            {
                return Err(invalid(
                    "modeled kernel: creation frames do not fit the image",
                ));
            }
            Stage::Create {
                pid,
                space: Space::build(config, &model.plan.layout, boot, &frames),
            }
        }
        RawStage::ReadFrame => {
            if model.life != Life::Up || procs.current().is_none() {
                return Err(invalid(
                    "modeled kernel: frame read with no process running",
                ));
            }
            Stage::ReadFrame
        }
        RawStage::Dispatch { pid, context } => {
            if model.life != Life::Up || procs.current() != Some(pid) {
                return Err(invalid(
                    "modeled kernel: dispatch of a process that is not running",
                ));
            }
            let root = procs.pcb(pid).map(|p| p.root).unwrap_or_default();
            Stage::Dispatch {
                pid,
                context,
                satp: pte::satp(root),
            }
        }
        RawStage::Shutdown { reason } => {
            if model.life != Life::Down || reason > 1 {
                return Err(invalid("modeled kernel: a shutdown that was not decided"));
            }
            Stage::Shutdown { reason }
        }
    };
    ProcOp::from_parts(config, stage, step, data).ok_or(invalid(
        "modeled kernel: operation step or working data not reachable",
    ))
}

/// The checks that tie the process table to the life and the operation. On success the
/// creation's frames, if any, get their owner.
fn check_life(model: &mut Model, work: Option<&Work>) -> Result<(), RestoreError> {
    let invalid = RestoreError::InvalidState;
    let procs = &model.procs;
    let creating = match work {
        Some(Work::Proc(op)) => match op.stage() {
            Stage::Create { pid, space } => Some((*pid, space.frames())),
            _ => None,
        },
        _ => None,
    };
    // Frames the bitmap marks but no PCB claimed are the creation's, if they are its.
    let mut owners: Vec<u32> = (0..procs.frames().count())
        .map(|i| {
            procs
                .frames()
                .owner(procs.frames().base() + i as u32)
                .unwrap_or(0)
        })
        .collect();
    let base = procs.frames().base();
    if let Some((pid, frames)) = creating {
        for &ppn in frames {
            match ppn
                .checked_sub(base)
                .and_then(|i| owners.get_mut(i as usize))
            {
                Some(o) if *o == u32::MAX => *o = pid,
                _ => {
                    return Err(invalid(
                        "modeled kernel: a creation frame not marked allocated",
                    ));
                }
            }
        }
        // Lowest free first: no frame below the creation's last one may be free.
        let last = frames.last().copied().unwrap_or(base);
        if owners[..(last - base) as usize].contains(&0) {
            return Err(invalid(
                "modeled kernel: creation frames are not the lowest free ones",
            ));
        }
    }
    if owners.contains(&u32::MAX) {
        return Err(invalid(
            "modeled kernel: an allocated frame no live process owns",
        ));
    }
    let checked = Processes::from_parts(
        procs.pcbs().to_vec(),
        procs.queue().clone(),
        procs.current(),
        Frames::from_owners(base, owners),
    );
    checked
        .check(model.plan.images.len(), creating)
        .map_err(|v| invalid(v.0))?;
    match (model.life, work) {
        (Life::AwaitBoot, Some(_)) => {
            return Err(invalid("modeled kernel: an operation before boot"));
        }
        (Life::AwaitBoot, None) if !procs.pcbs().is_empty() => {
            return Err(invalid("modeled kernel: processes before boot"));
        }
        (Life::Up, None) if procs.current().is_none() => {
            return Err(invalid(
                "modeled kernel: idle after boot with no process running",
            ));
        }
        _ => {}
    }
    model.procs = checked;
    Ok(())
}
