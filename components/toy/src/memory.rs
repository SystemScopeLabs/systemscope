//! `ToyMemory`: an F1 byte-addressable memory with fixed read and write latency.

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg};
use systemscope_contracts::time::Duration;

/// The memory's only port: a `mem.v0` target.
pub const PORT: PortId = PortId(0);

/// Size and timing of a [`ToyMemory`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToyMemoryConfig {
    /// Capacity in bytes. Addresses outside `0..size` fault.
    pub size: u32,
    /// Delay from receiving a read to sending its response.
    pub read_latency: Duration,
    /// Delay from receiving a write to sending its response.
    pub write_latency: Duration,
}

/// Applies each request on arrival and responds in `Complete` after a fixed latency.
pub struct ToyMemory {
    config: ToyMemoryConfig,
    bytes: Vec<u8>,
}

impl ToyMemory {
    /// Creates a zero-filled memory.
    pub fn new(config: ToyMemoryConfig) -> ToyMemory {
        ToyMemory {
            config,
            bytes: vec![0; config.size as usize],
        }
    }

    fn range(&self, addr: u64, len: usize) -> Result<std::ops::Range<usize>, SimError> {
        let start = usize::try_from(addr).ok();
        start
            .and_then(|s| Some(s..s.checked_add(len)?))
            .filter(|r| r.end <= self.bytes.len())
            .ok_or(SimError::ComponentFault("toy memory: address out of range"))
    }
}

impl Component for ToyMemory {
    fn type_name(&self) -> &'static str {
        "toy.memory"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            msg: Message::Mem(msg),
            ..
        } = ev
        else {
            return Err(SimError::ComponentFault("toy memory: unexpected delivery"));
        };
        let (resp, latency) = match msg {
            MemMsg::ReadReq { txn, addr, len } => {
                let range = self.range(*addr, *len as usize)?;
                let data = self.bytes[range].to_vec();
                let resp = MemMsg::ReadResp { txn: *txn, data };
                (resp, self.config.read_latency)
            }
            MemMsg::WriteReq { txn, addr, data } => {
                let range = self.range(*addr, data.len())?;
                self.bytes[range].copy_from_slice(data);
                (MemMsg::WriteResp { txn: *txn }, self.config.write_latency)
            }
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "toy memory: response on target port",
                ));
            }
        };
        ctx.send(
            PORT,
            resp.into(),
            ScheduleWhen::After(latency),
            Phase::Complete,
        )
    }
}
