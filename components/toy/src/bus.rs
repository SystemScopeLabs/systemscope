//! `ToyBus`: merges two `mem.v0` initiators onto one memory port.
//!
//! ```text
//! Read/WriteReq @ REQUEST on cpu|dma ──▶ queued; Wake(ARBITRATE) @ TRANSFER, same bus edge
//! Wake(ARBITRATE) ──▶ grant one head (round-robin), remap TxnId, send on mem @ TRANSFER
//! Read/WriteResp @ COMPLETE on mem ──▶ unmap, send on the upstream port @ COMPLETE
//! ```
//!
//! # Arbitration (`docs/m0-design.md` §9.1)
//!
//! One grant per bus edge. The priority pointer starts at the `cpu` port. If one queue is
//! non-empty its head wins; if both are, the pointer's head wins. After every grant the
//! pointer moves to the other port. The outcome depends only on which queues are non-empty
//! and on the pointer, and requests only arrive in `Request`, so every request that reaches
//! the bus at a tick takes part in that tick's arbitration.
//!
//! # Remapping
//!
//! Initiators number their transactions independently, so the CPU and the DMA may both
//! have `TxnId(n)` in flight. The bus forwards each request under its own downstream
//! `TxnId` and routes the response back through `downstream → (port, upstream TxnId)`.

use std::collections::{BTreeMap, VecDeque};

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;

/// Target port facing the CPU.
pub const CPU_PORT: PortId = PortId(0);

/// Target port facing the DMA.
pub const DMA_PORT: PortId = PortId(1);

/// Initiator port facing the memory.
pub const MEM_PORT: PortId = PortId(2);

/// Number of upstream ports, which are ports `0..UPSTREAM`.
const UPSTREAM: usize = 2;

/// Wake token that runs one arbitration.
pub const ARBITRATE: u64 = 0;

/// Layout of [`ToyBus`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Timing of a [`ToyBus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToyBusConfig {
    /// The clock the bus arbitrates on.
    pub clock: ClockDomainId,
}

/// Where a forwarded request's response goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Route {
    /// Upstream port index, `0..UPSTREAM`.
    port: u16,
    /// The initiator's own `TxnId`.
    txn: TxnId,
    write: bool,
}

/// A two-initiator, round-robin interconnect.
pub struct ToyBus {
    config: ToyBusConfig,
    /// Requests waiting for a grant, per upstream port, in arrival order.
    queues: [VecDeque<MemMsg>; UPSTREAM],
    /// Upstream port that wins when both queues are non-empty.
    priority: u16,
    arbitration_scheduled: bool,
    next_downstream: u64,
    /// Forwarded requests awaiting a response, by downstream `TxnId`.
    routes: BTreeMap<TxnId, Route>,
}

/// The `TxnId` of a request, or `None` for a response.
fn request_txn(msg: &MemMsg) -> Option<(TxnId, bool)> {
    match msg {
        MemMsg::ReadReq { txn, .. } => Some((*txn, false)),
        MemMsg::WriteReq { txn, .. } => Some((*txn, true)),
        MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => None,
    }
}

/// `msg` with its `TxnId` replaced.
fn with_txn(msg: MemMsg, txn: TxnId) -> MemMsg {
    match msg {
        MemMsg::ReadReq { addr, len, .. } => MemMsg::ReadReq { txn, addr, len },
        MemMsg::WriteReq { addr, data, .. } => MemMsg::WriteReq { txn, addr, data },
        MemMsg::ReadResp { data, .. } => MemMsg::ReadResp { txn, data },
        MemMsg::WriteResp { .. } => MemMsg::WriteResp { txn },
    }
}

impl ToyBus {
    /// Creates an idle bus.
    pub fn new(config: ToyBusConfig) -> ToyBus {
        ToyBus {
            config,
            queues: [VecDeque::new(), VecDeque::new()],
            priority: 0,
            arbitration_scheduled: false,
            next_downstream: 0,
            routes: BTreeMap::new(),
        }
    }

    fn schedule_arbitration(&mut self, ctx: &mut dyn SimContext, k: u64) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k,
        };
        ctx.wake_self(when, Phase::Transfer, ARBITRATE)?;
        self.arbitration_scheduled = true;
        Ok(())
    }

    fn arbitrate(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let waiting = [!self.queues[0].is_empty(), !self.queues[1].is_empty()];
        let winner = match waiting {
            [true, true] => self.priority,
            [true, false] => 0,
            [false, true] => 1,
            [false, false] => return Ok(()),
        };
        let contended = waiting == [true, true];
        self.priority = 1 - winner;
        let msg = self.queues[usize::from(winner)]
            .pop_front()
            .expect("the winner's queue is non-empty");
        let (txn, write) = request_txn(&msg).expect("only requests are queued");
        let downstream = TxnId(self.next_downstream);
        self.next_downstream += 1;
        self.routes.insert(
            downstream,
            Route {
                port: winner,
                txn,
                write,
            },
        );
        ctx.send(
            MEM_PORT,
            with_txn(msg, downstream).into(),
            ScheduleWhen::Now,
            Phase::Transfer,
        )?;
        ctx.trace(
            "toy.bus.grant",
            vec![
                ("port", Value::U64(u64::from(winner))),
                ("txn", Value::U64(txn.0)),
                ("downstream", Value::U64(downstream.0)),
                ("contended", Value::Bool(contended)),
            ],
        );
        Ok(())
    }

    fn route(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let (downstream, write) = match msg {
            MemMsg::ReadResp { txn, .. } => (*txn, false),
            MemMsg::WriteResp { txn } => (*txn, true),
            MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. } => {
                return Err(SimError::ComponentFault(
                    "toy bus: request on the memory port",
                ));
            }
        };
        let route = self
            .routes
            .remove(&downstream)
            .ok_or(SimError::ComponentFault(
                "toy bus: response for unknown txn",
            ))?;
        if route.write != write {
            return Err(SimError::ComponentFault("toy bus: response kind mismatch"));
        }
        ctx.send(
            PortId(route.port),
            with_txn(msg.clone(), route.txn).into(),
            ScheduleWhen::Now,
            ctx.phase(),
        )?;
        ctx.trace(
            "toy.bus.route",
            vec![
                ("downstream", Value::U64(downstream.0)),
                ("port", Value::U64(u64::from(route.port))),
                ("txn", Value::U64(route.txn.0)),
            ],
        );
        Ok(())
    }
}

impl Component for ToyBus {
    fn type_name(&self) -> &'static str {
        "toy.bus"
    }

    fn ports(&self) -> Vec<PortSpec> {
        let port = |name, role| PortSpec {
            name,
            protocol: mem::PROTOCOL,
            role,
        };
        vec![
            port("cpu", Role::Target),
            port("dma", Role::Target),
            port("mem", Role::Initiator),
        ]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Wake { token: ARBITRATE } => {
                self.arbitration_scheduled = false;
                self.arbitrate(ctx)?;
                if self.queues.iter().any(|q| !q.is_empty()) {
                    self.schedule_arbitration(ctx, 1)?;
                }
            }
            Delivered::Wake { .. } => {
                return Err(SimError::ComponentFault("toy bus: unknown wake token"));
            }
            Delivered::Message {
                port: MEM_PORT,
                msg: Message::Mem(msg),
            } => self.route(msg, ctx)?,
            Delivered::Message {
                port,
                msg: Message::Mem(msg),
            } => {
                if request_txn(msg).is_none() {
                    return Err(SimError::ComponentFault(
                        "toy bus: response on an upstream port",
                    ));
                }
                if ctx.phase() != Phase::Request {
                    return Err(SimError::ComponentFault(
                        "toy bus: request arrived after REQUEST",
                    ));
                }
                let queue = self
                    .queues
                    .get_mut(usize::from(port.0))
                    .ok_or(SimError::ComponentFault("toy bus: unknown port"))?;
                queue.push_back(msg.clone());
                if !self.arbitration_scheduled {
                    self.schedule_arbitration(ctx, 0)?;
                }
            }
        }
        Ok(())
    }

    /// Counters and queue depths; cheap enough to call at every observe point.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("queued_cpu", Value::U64(self.queues[0].len() as u64)),
                ("queued_dma", Value::U64(self.queues[1].len() as u64)),
                ("priority", Value::U64(u64::from(self.priority))),
                ("next_downstream", Value::U64(self.next_downstream)),
                ("routes", Value::U64(self.routes.len() as u64)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration, the next downstream `TxnId`, the priority pointer, the
    /// arbitration flag, both queues in order, then the routes by downstream `TxnId`.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.u32(self.config.clock.0);
        w.u64(self.next_downstream);
        w.u16(self.priority);
        w.bool(self.arbitration_scheduled);
        for queue in &self.queues {
            w.len(queue.len());
            for msg in queue {
                msg.encode(w);
            }
        }
        w.len(self.routes.len());
        for (downstream, route) in &self.routes {
            w.u64(downstream.0);
            w.u16(route.port);
            w.u64(route.txn.0);
            w.bool(route.write);
        }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        if r.u32()? != self.config.clock.0 {
            return Err(RestoreError::InvalidState(
                "toy bus: snapshot was taken with a different configuration",
            ));
        }
        self.next_downstream = r.u64()?;
        self.priority = r.u16()?;
        self.arbitration_scheduled = r.bool()?;
        if usize::from(self.priority) >= UPSTREAM {
            return Err(RestoreError::InvalidState("toy bus: priority out of range"));
        }
        for queue in &mut self.queues {
            *queue = VecDeque::new();
            for _ in 0..r.len()? {
                let msg = MemMsg::decode(r)?;
                if request_txn(&msg).is_none() {
                    return Err(RestoreError::InvalidState("toy bus: queued response"));
                }
                queue.push_back(msg);
            }
        }
        self.routes = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let downstream = TxnId(r.u64()?);
            if previous.is_some_and(|p| downstream <= p) || downstream.0 >= self.next_downstream {
                return Err(RestoreError::InvalidState(
                    "toy bus: routes out of order or not yet allocated",
                ));
            }
            previous = Some(downstream);
            let port = r.u16()?;
            if usize::from(port) >= UPSTREAM {
                return Err(RestoreError::InvalidState("toy bus: unknown route port"));
            }
            let txn = TxnId(r.u64()?);
            let write = r.bool()?;
            self.routes.insert(downstream, Route { port, txn, write });
        }
        Ok(())
    }
}
