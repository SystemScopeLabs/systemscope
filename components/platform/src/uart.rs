//! `SimpleUart`: a transmit-only, memory-mapped output device (`docs/m1-design.md` §7.3).
//!
//! It is SystemScope-specific, not a 16550: no receive path, FIFO, baud rate, or
//! interrupts. Every byte written to TX is appended to an output buffer, which is the
//! device's whole state.
//!
//! # Register map
//!
//! Addresses are offsets from the UART's own start, as
//! [`AddressBus`](crate::AddressBus) forwards them.
//!
//! | Offset | Register | Access | Behavior |
//! |---|---|---|---|
//! | `0x0` | TX | write, exactly 1 byte | appends the byte to the output |
//! | `0x4` | STATUS | read, 1, 2, or 4 bytes | the `u32` [`STATUS_VALUE`], little-endian |
//!
//! Every other well-formed request gets a [`MemFault::AccessFault`] response and changes
//! nothing: a read of TX, a TX write of any other width, a write to STATUS, and any request
//! touching offsets `0x1`–`0x3` or `0x5` and above, including one that starts at a register
//! and runs past it, or whose last byte would lie past `u64::MAX`. A zero-length request is
//! a protocol violation and faults the session with [`SimError::ComponentFault`], as the
//! RAM and the bus do.
//!
//! # Ordering semantics
//!
//! The same as the RAM's: a request is accepted when its event is dispatched, a TX byte is
//! appended at acceptance, and the response follows after the configured latency, in
//! `Complete`. A pending response is an event in the runtime's queue, so the UART keeps no
//! copy of it, and a restore never replays a write: the output after a restore is exactly
//! the snapshot's.
//!
//! Output is raw bytes, never decoded: any value `0x00`–`0xff` is kept as written.

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// The UART's only port: a `mem.v1` target named `mem`.
pub const PORT: PortId = PortId(0);

/// Offset of the TX register.
pub const TX: u64 = 0x0;

/// Offset of the STATUS register.
pub const STATUS: u64 = 0x4;

/// What STATUS reads as: bit 0, TX ready, is always set.
pub const STATUS_VALUE: u32 = 1;

/// The size of the UART's window: TX, three reserved bytes, STATUS, three reserved bytes.
pub const SIZE: u64 = 8;

/// The trace record for each accepted TX byte, with one field, `byte`.
pub const TX_KIND: &str = "platform.uart.tx";

/// How many of the last output bytes [`SimpleUart`]'s `inspect()` shows.
pub const INSPECT_TAIL: usize = 64;

/// Layout of [`SimpleUart`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Timing of a [`SimpleUart`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UartConfig {
    /// Delay from accepting a request to sending its response, as for
    /// [`RamConfig`](crate::RamConfig).
    pub latency: LinkLatency,
}

/// A transmit-only UART serving `mem.v1` requests at offsets `[0, SIZE)`.
pub struct SimpleUart {
    config: UartConfig,
    output: Vec<u8>,
}

impl SimpleUart {
    /// Creates a UART with an empty output.
    pub fn new(config: UartConfig) -> SimpleUart {
        SimpleUart {
            config,
            output: Vec::new(),
        }
    }

    /// Every byte accepted at TX so far, in acceptance order.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    fn write_latency(&self, w: &mut SnapshotWriter) {
        match self.config.latency {
            LinkLatency::After(d) => {
                w.u8(0);
                w.u128(d.as_femtoseconds());
            }
            LinkLatency::Cycles { domain, k } => {
                w.u8(1);
                w.u32(domain.0);
                w.u64(k);
            }
        }
    }
}

/// The STATUS bytes for a read of `len` bytes at `first`, if that read is allowed.
fn status_read(first: u64, len: usize) -> Option<Vec<u8>> {
    (first == STATUS && matches!(len, 1 | 2 | 4))
        .then(|| STATUS_VALUE.to_le_bytes()[..len].to_vec())
}

impl Component for SimpleUart {
    fn type_name(&self) -> &'static str {
        "platform.uart"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    /// Accepts a request: appends a TX byte or samples STATUS now, and sends the response
    /// after the configured latency, in `Complete`.
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } = ev
        else {
            return Err(SimError::ComponentFault("uart: unexpected delivery"));
        };
        let first = match msg.access() {
            None => {
                return Err(SimError::ComponentFault(
                    "uart: response on the target port",
                ));
            }
            Some(Access::Empty) => {
                return Err(SimError::ComponentFault("uart: zero-length request"));
            }
            Some(Access::OutOfRange) => None,
            Some(Access::Bytes { first, .. }) => Some(first),
        };
        let fault = MemFault::AccessFault;
        let resp = match msg {
            MemMsg::ReadReq { txn, len, .. } => MemMsg::ReadResp {
                txn: *txn,
                outcome: match first.and_then(|first| status_read(first, *len as usize)) {
                    Some(data) => ReadOutcome::Data { data },
                    None => ReadOutcome::Fault { fault },
                },
            },
            MemMsg::WriteReq { txn, data, .. } => MemMsg::WriteResp {
                txn: *txn,
                outcome: match (first, data.as_slice()) {
                    (Some(TX), &[byte]) => {
                        self.output.push(byte);
                        ctx.trace(TX_KIND, vec![("byte", Value::U64(u64::from(byte)))]);
                        WriteOutcome::Done
                    }
                    _ => WriteOutcome::Fault { fault },
                },
            },
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "uart: response on the target port",
                ));
            }
        };
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(PORT, resp.into(), when, Phase::Complete)
    }

    /// `tx_len`, the number of output bytes, and `tx_tail`, the last [`INSPECT_TAIL`] of
    /// them (all of them if fewer), so the view stays bounded however long a program
    /// prints.
    fn inspect(&self) -> StateView {
        let tail = &self.output[self.output.len().saturating_sub(INSPECT_TAIL)..];
        StateView {
            fields: vec![
                ("tx_len", Value::U64(self.output.len() as u64)),
                ("tx_tail", Value::Bytes(tail.to_vec())),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the latency, then the output bytes, length-prefixed.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_latency(w);
        w.bytes(&self.output);
    }

    /// Replaces the output with the snapshot's. Rejects a different latency.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let mut latency = SnapshotWriter::new();
        self.write_latency(&mut latency);
        if r.raw(latency.as_bytes().len())? != latency.as_bytes() {
            return Err(RestoreError::InvalidState(
                "uart: snapshot has a different latency",
            ));
        }
        self.output = r.bytes()?.to_vec();
        Ok(())
    }
}
