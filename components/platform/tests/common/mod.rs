//! Test harnesses shared by the platform tests. Test-only: nothing here is part of the
//! crate.
//!
//! - [`MockCtx`] drives one component directly, recording what it sends (`mem.v1`,
//!   `irq.v0`, and `block.v0`), the wakes it schedules, and what it traces, so unit and
//!   property tests run without a runtime.
//! - [`Script`] is a stateless `mem.v1` initiator for runtime tests: it schedules every
//!   request during `init`, so its pending requests live in the runtime's queue and it
//!   has nothing to snapshot. Responses show up in the runtime's dispatch records.

#![allow(dead_code)]

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::BlockMsg;
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::{self, MemMsg, TxnId};
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Tick};
use systemscope_contracts::trace::Value;

/// One `send` a component made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub port: PortId,
    pub msg: MemMsg,
    pub when: ScheduleWhen,
    pub phase: Phase,
}

/// One `irq.v0` `send` a component made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrqSent {
    pub port: PortId,
    pub asserted: bool,
    pub when: ScheduleWhen,
    pub phase: Phase,
}

/// One `block.v0` `send` a component made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockSent {
    pub port: PortId,
    pub msg: BlockMsg,
    pub when: ScheduleWhen,
    pub phase: Phase,
}

/// One `wake_self` a component made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wake {
    pub when: ScheduleWhen,
    pub phase: Phase,
    pub token: u64,
}

/// Which protocol a `send` used, or a wake, in the order the component made them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendKind {
    Mem,
    Irq,
    Block,
    Wake,
}

/// One `trace` record a component made.
pub type Traced = (&'static str, Vec<(&'static str, Value)>);

/// Platform components use no randomness.
struct NoRng;

impl SimRng for NoRng {
    fn next_u64(&mut self) -> u64 {
        panic!("platform components must not draw random numbers")
    }
}

/// A `SimContext` that records instead of scheduling.
pub struct MockCtx {
    pub phase: Phase,
    pub sent: Vec<Sent>,
    pub irqs: Vec<IrqSent>,
    pub blocks: Vec<BlockSent>,
    pub wakes: Vec<Wake>,
    pub order: Vec<SendKind>,
    pub traced: Vec<Traced>,
    rng: NoRng,
}

impl MockCtx {
    pub fn new(phase: Phase) -> MockCtx {
        MockCtx {
            phase,
            sent: Vec::new(),
            irqs: Vec::new(),
            blocks: Vec::new(),
            wakes: Vec::new(),
            order: Vec::new(),
            traced: Vec::new(),
            rng: NoRng,
        }
    }

    /// Delivers `msg` on `port` of `component`, in this context's phase.
    pub fn deliver(
        &mut self,
        component: &mut dyn Component,
        port: PortId,
        msg: MemMsg,
    ) -> Result<(), SimError> {
        let ev = Delivered::Message {
            port,
            msg: msg.into(),
        };
        component.handle_event(&ev, self)
    }

    /// Delivers any message on `port` of `component`, in this context's phase.
    pub fn deliver_msg(
        &mut self,
        component: &mut dyn Component,
        port: PortId,
        msg: Message,
    ) -> Result<(), SimError> {
        component.handle_event(&Delivered::Message { port, msg }, self)
    }

    /// The only message sent since the last call, which is cleared.
    pub fn take_one(&mut self) -> Sent {
        assert_eq!(
            self.sent.len(),
            1,
            "expected exactly one send: {:?}",
            self.sent
        );
        self.sent.pop().unwrap()
    }
}

impl InitContext for MockCtx {
    fn component(&self) -> ComponentId {
        ComponentId(0)
    }

    fn send(
        &mut self,
        port: PortId,
        msg: Message,
        when: ScheduleWhen,
        phase: Phase,
    ) -> Result<(), SimError> {
        match msg {
            Message::MemV1(msg) => {
                self.order.push(SendKind::Mem);
                self.sent.push(Sent {
                    port,
                    msg,
                    when,
                    phase,
                });
            }
            Message::Irq(IrqMsg::Level { asserted }) => {
                self.order.push(SendKind::Irq);
                self.irqs.push(IrqSent {
                    port,
                    asserted,
                    when,
                    phase,
                });
            }
            Message::Block(msg) => {
                self.order.push(SendKind::Block);
                self.blocks.push(BlockSent {
                    port,
                    msg,
                    when,
                    phase,
                });
            }
            other => {
                panic!("platform components speak mem.v1, irq.v0, and block.v0 only: {other:?}")
            }
        }
        Ok(())
    }

    fn wake_self(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError> {
        self.order.push(SendKind::Wake);
        self.wakes.push(Wake { when, phase, token });
        Ok(())
    }

    fn rng(&mut self) -> &mut dyn SimRng {
        &mut self.rng
    }

    fn trace(&mut self, kind: &'static str, fields: Vec<(&'static str, Value)>) {
        self.traced.push((kind, fields));
    }
}

impl SimContext for MockCtx {
    fn now(&self) -> Tick {
        Tick::ZERO
    }

    fn phase(&self) -> Phase {
        self.phase
    }
}

pub fn read(txn: u64, addr: u64, len: u32) -> MemMsg {
    MemMsg::ReadReq {
        txn: TxnId(txn),
        addr,
        len,
    }
}

pub fn write(txn: u64, addr: u64, data: &[u8]) -> MemMsg {
    MemMsg::WriteReq {
        txn: TxnId(txn),
        addr,
        data: data.to_vec(),
    }
}

/// A component's snapshot bytes.
pub fn snapshot_of(component: &dyn Component) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    component.snapshot(&mut w);
    w.into_bytes()
}

/// Restores `bytes` into `component`, requiring every byte to be read.
pub fn restore_into(component: &mut dyn Component, bytes: &[u8]) -> Result<(), RestoreError> {
    let mut r = SnapshotReader::new(bytes);
    let schema = component.snapshot_schema_version();
    component.restore(&mut r, schema)?;
    r.finish().map_err(RestoreError::Decode)
}

/// A stateless initiator that sends `requests[i].1` at cycle `requests[i].0` of `clock`,
/// in `Request`, all scheduled during `init`.
pub struct Script {
    pub clock: ClockDomainId,
    pub requests: Vec<(u64, MemMsg)>,
}

impl Component for Script {
    fn type_name(&self) -> &'static str {
        "test.script"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for (k, msg) in &self.requests {
            let when = ScheduleWhen::Cycles {
                domain: self.clock,
                k: *k,
            };
            ctx.send(PortId(0), msg.clone().into(), when, Phase::Request)?;
        }
        Ok(())
    }

    /// Responses are recorded by the runtime's dispatch records; nothing to do.
    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Ok(())
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    /// Stateless: every pending request is in the runtime's queue.
    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}
