//! `ModeledKernel`: the component around the pure core (`docs/m3-design.md` §6.2, §6.3,
//! §6.8).
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `gate`, a `mem.v1` target, the `kgate` window;
//! and `mem`, a `mem.v1` initiator, the bus master `kernel0`.
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
//! exists); a wake that does not match the state.
//!
//! # Snapshot
//!
//! The kernel's part of a held entry is the held `TxnId` and its operation. The CPU's
//! `MemWait` and the bus's active `kgate` transaction are theirs; none is copied here, and
//! no request or wake in flight is ever kernel state. **Restore resumes by waiting, never
//! by reissuing**: a `Wait` restores to waiting for the response already in the runtime's
//! queue, and an `Issue` to waiting for its wake.

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

/// The engine's position (§6.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum State {
    /// No operation; nothing held.
    Idle,
    /// The operation's next access is due at `Wake(ISSUE)`.
    Issue(Operation),
    /// The operation's current access is outstanding under `txn`.
    Wait(Operation, TxnId),
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
}

impl ModeledKernel {
    /// Creates an idle kernel with nothing held and the `mem` counter at 0, or the first
    /// rule of [`KernelConfig::validate`] the configuration breaks.
    pub fn new(config: KernelConfig) -> Result<ModeledKernel, KernelConfigError> {
        config.validate()?;
        Ok(ModeledKernel {
            config,
            state: State::Idle,
            held: None,
            next_txn: TxnId(0),
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

    fn operation(&self) -> Option<&Operation> {
        match &self.state {
            State::Idle => None,
            State::Issue(op) | State::Wait(op, _) => Some(op),
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

    /// An `ENTER` write: hold it and start the operation.
    fn enter(&mut self, txn: TxnId, value: u32, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if self.held.is_some() {
            return Err(SimError::ComponentFault(
                "modeled kernel: second ENTER while one is held",
            ));
        }
        let op = Operation::start(&self.config, value);
        ctx.trace(
            ENTER_KIND,
            vec![
                ("txn", Value::U64(txn.0)),
                ("value", Value::U64(u64::from(value))),
                ("op", Value::Str(op.op().name().to_owned())),
            ],
        );
        if op.op() == Op::Shutdown {
            ctx.trace(
                SHUTDOWN_KIND,
                vec![
                    ("reason", Value::U64(1)),
                    (
                        "detail",
                        Value::Str("ENTER value is not the trap frame".to_owned()),
                    ),
                ],
            );
        }
        self.held = Some(txn);
        self.state = State::Issue(op);
        self.wake_next_cycle(ctx)
    }

    /// `Wake(ISSUE)`: checks the due access against the whitelist and sends it.
    fn issue(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let State::Issue(op) = &self.state else {
            return Err(SimError::ComponentFault(
                "modeled kernel: ISSUE wake with nothing to issue",
            ));
        };
        let access = op.access(&self.config).ok_or(SimError::ComponentFault(
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
        let State::Issue(op) = std::mem::replace(&mut self.state, State::Idle) else {
            unreachable!("checked above")
        };
        self.state = State::Wait(op, txn);
        Ok(())
    }

    /// A message on `mem`: the response to the outstanding access, in `Complete`.
    fn mem_response(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let State::Wait(op, expected) = &self.state else {
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
        let wrote = matches!(op.access(&self.config), Some(Access::Write { .. }));
        if wrote != matches!(msg, MemMsg::WriteResp { .. }) {
            return Err(SimError::ComponentFault(
                "modeled kernel: response kind mismatch",
            ));
        }
        let completion = completion.ok_or(SimError::ComponentFault(
            "modeled kernel: its own access faulted",
        ))?;
        let mut op = op.clone();
        op.complete(&self.config, completion).map_err(|_| {
            SimError::ComponentFault("modeled kernel: read data of the wrong length")
        })?;
        if op.is_finished(&self.config) {
            return self.release(ctx);
        }
        self.state = State::Issue(op);
        self.wake_next_cycle(ctx)
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

    /// `phase` (`idle`, `issue`, `wait`), `op` (`none`, `script`, `shutdown`), `step`,
    /// `held_txn` and `mem_txn` (the outstanding access's, `none` or the number),
    /// `next_txn`, and the due or outstanding access as `pending_kind` (`none`, `read`,
    /// `write`), `pending_addr`, and `pending_len` (0 with `none`).
    fn inspect(&self) -> StateView {
        let phase = match self.state {
            State::Idle => "idle",
            State::Issue(_) => "issue",
            State::Wait(..) => "wait",
        };
        let op = self.operation();
        let access = op.and_then(|op| op.access(&self.config));
        let number = |t: Option<TxnId>| t.map_or("none".to_owned(), |t| t.0.to_string());
        let mem_txn = match self.state {
            State::Wait(_, txn) => Some(txn),
            State::Idle | State::Issue(_) => None,
        };
        StateView {
            fields: vec![
                ("phase", Value::Str(phase.to_owned())),
                (
                    "op",
                    Value::Str(op.map_or("none", |op| op.op().name()).to_owned()),
                ),
                ("step", Value::U64(op.map_or(0, |op| u64::from(op.step())))),
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
            ],
        }
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
    /// 2. the state (`u8`: 0 `Idle`, 1 `Issue`, 2 `Wait` followed by the outstanding
    ///    `u64` txn), then, unless `Idle`, the operation (`u8`: 0 `Script`, 1 `Shutdown`),
    ///    its step (`u32`), and its working data (length-prefixed bytes);
    /// 3. the held `ENTER` (tag `u8` 0, or 1 then the `u64` txn);
    /// 4. the next `mem` `TxnId` (`u64`).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.config.encode(w);
        match &self.state {
            State::Idle => w.u8(0),
            State::Issue(_) => w.u8(1),
            State::Wait(_, txn) => {
                w.u8(2);
                w.u64(txn.0);
            }
        }
        if let Some(op) = self.operation() {
            w.u8(match op.op() {
                Op::Script => 0,
                Op::Shutdown => 1,
            });
            w.u32(op.step());
            w.bytes(op.data());
        }
        match self.held {
            None => w.u8(0),
            Some(txn) => {
                w.u8(1);
                w.u64(txn.0);
            }
        }
        w.u64(self.next_txn.0);
    }

    /// Replaces the whole state with the snapshot's, or changes nothing.
    ///
    /// Rejects a different configuration and every state no run can produce (§6.8): a
    /// held `ENTER` with `Idle`, or an operation without one; an operation step at or past
    /// its last access, or working data that does not belong to it; a due or outstanding
    /// access the whitelist refuses; and a `Wait` whose txn is not the latest issued
    /// (next `TxnId` − 1).
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        let mut config = SnapshotWriter::new();
        self.config.encode(&mut config);
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
        let op = if tag == 0 {
            None
        } else {
            let op = match r.u8()? {
                0 => Op::Script,
                1 => Op::Shutdown,
                tag => {
                    return Err(RestoreError::Decode(DecodeError::InvalidTag {
                        what: "modeled kernel operation",
                        tag,
                    }));
                }
            };
            let step = r.u32()?;
            let data = r.bytes()?.to_vec();
            Some(
                Operation::from_parts(&self.config, op, step, data).ok_or(invalid(
                    "modeled kernel: operation step or working data not reachable",
                ))?,
            )
        };
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
        if held.is_some() != op.is_some() {
            return Err(invalid(
                "modeled kernel: a held ENTER without an operation, or the reverse",
            ));
        }
        if let Some(op) = &op {
            let access = op
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
        self.state = match (op, outstanding) {
            (None, _) => State::Idle,
            (Some(op), None) => State::Issue(op),
            (Some(op), Some(txn)) => State::Wait(op, txn),
        };
        self.held = held;
        self.next_txn = TxnId(next_txn);
        Ok(())
    }
}
