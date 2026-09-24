//! `SimpleIrqController`: a level-sensitive interrupt aggregator (`docs/m2-design.md`
//! §7.2).
//!
//! It latches nothing and has no PLIC features: no priorities, thresholds, claim/complete,
//! or edge detection. Each source's line level is stored as it arrives, software selects
//! sources with `ENABLE`, and the output to the CPU is
//!
//! ```text
//! meip = (pending & enable) != 0
//! ```
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `src0` … `src{N−1}`, `irq.v0` targets
//! ([`SimpleIrqController::src_port`]); `cpu`, an `irq.v0` initiator
//! ([`SimpleIrqController::cpu_port`]); and `mem`, a `mem.v1` target, the MMIO window
//! ([`SimpleIrqController::mem_port`]).
//!
//! # Levels
//!
//! A `Level` on `srcI`, which must arrive in `Complete`, sets bit *I* of `pending` to the
//! line's level; a level equal to the stored one changes nothing and is not an error
//! (§7.1). `pending` follows the source levels and software cannot clear it: the device
//! that asserted a line deasserts it, after software acknowledges the device itself.
//!
//! Whenever a source level or `ENABLE` changes, the controller recomputes `meip` in the
//! same handler and, only if it differs from `out`, the last level sent, sends
//! `Level { asserted: meip }` on `cpu` (`Now`, `Complete`) and updates `out`. Every line
//! is deasserted at reset, so nothing is sent until the output first rises.
//!
//! # Register map
//!
//! Offsets from the controller's own start, as [`AddressBus`](crate::AddressBus)
//! forwards them; the window is [`SIZE`] bytes.
//!
//! | Offset | Register | Access | Behavior |
//! |---|---|---|---|
//! | `0x0` | `PENDING` | read, 4 bytes | `pending` (bits outside `valid_mask` read 0) |
//! | `0x4` | `ENABLE` | read/write, 4 bytes | read: `enable`; write: `enable ← v & valid_mask` |
//!
//! Every other well-formed request gets a [`MemFault::AccessFault`] response and changes
//! nothing: a write to `PENDING`, any width other than 4, any other offset, and a request
//! that crosses the window or whose last byte would lie past `u64::MAX`. A zero-length
//! request is a protocol violation and faults the session, as for the RAM and the UART.
//!
//! # Ordering semantics
//!
//! The same as the RAM's: a request takes effect when it is accepted, and its response
//! follows after the configured latency, in `Complete`. An `ENABLE` write changes
//! `enable`, and with it `meip` and any `Level` sent on `cpu`, at acceptance; the later
//! `WriteResp` changes nothing.

use std::fmt;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// Offset of the `PENDING` register.
pub const PENDING: u64 = 0x0;

/// Offset of the `ENABLE` register.
pub const ENABLE: u64 = 0x4;

/// The size of the controller's window: `PENDING`, then `ENABLE`.
pub const SIZE: u64 = 8;

/// The most sources a controller has.
pub const MAX_SOURCES: u8 = 32;

/// The trace record of a source level change, with `source` and `asserted`.
pub const LEVEL_KIND: &str = "platform.irq.level";

/// The trace record of an output change, with `asserted`.
pub const MEIP_KIND: &str = "platform.irq.meip";

/// Layout of [`SimpleIrqController`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// The source port names, by index.
const SRC_NAMES: [&str; MAX_SOURCES as usize] = [
    "src0", "src1", "src2", "src3", "src4", "src5", "src6", "src7", "src8", "src9", "src10",
    "src11", "src12", "src13", "src14", "src15", "src16", "src17", "src18", "src19", "src20",
    "src21", "src22", "src23", "src24", "src25", "src26", "src27", "src28", "src29", "src30",
    "src31",
];

/// The bits of `pending` and `enable` that `sources` sources use. The only definition of
/// the mask: everything that stores, reads, or checks those registers goes through it.
pub fn valid_mask(sources: u8) -> u32 {
    if sources == 32 {
        u32::MAX
    } else {
        (1u32 << sources) - 1
    }
}

/// Source count and timing of a [`SimpleIrqController`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IrqControllerConfig {
    /// The number of sources, N, in `1..=MAX_SOURCES`.
    pub sources: u8,
    /// Delay from accepting a request to sending its response, as for
    /// [`RamConfig`](crate::RamConfig).
    pub latency: LinkLatency,
}

/// Why a [`SimpleIrqController`] cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqControllerConfigError {
    /// The source count is 0.
    NoSources,
    /// The source count is larger than [`MAX_SOURCES`].
    TooManySources(u8),
}

impl fmt::Display for IrqControllerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrqControllerConfigError::NoSources => f.write_str("IRQ controller has no sources"),
            IrqControllerConfigError::TooManySources(n) => {
                write!(f, "IRQ controller has {n} sources, more than {MAX_SOURCES}")
            }
        }
    }
}

impl std::error::Error for IrqControllerConfigError {}

/// A level-sensitive IRQ controller with N sources and a `PENDING`/`ENABLE` window.
pub struct SimpleIrqController {
    config: IrqControllerConfig,
    valid_mask: u32,
    /// Bit *i*: the level last received on `srcI`.
    pending: u32,
    /// Bit *i*: source *i* is enabled.
    enable: u32,
    /// The level last sent on `cpu`.
    out: bool,
}

impl SimpleIrqController {
    /// Creates a controller with every line deasserted and every source disabled.
    pub fn new(
        config: IrqControllerConfig,
    ) -> Result<SimpleIrqController, IrqControllerConfigError> {
        match config.sources {
            0 => return Err(IrqControllerConfigError::NoSources),
            n if n > MAX_SOURCES => return Err(IrqControllerConfigError::TooManySources(n)),
            _ => {}
        }
        Ok(SimpleIrqController {
            config,
            valid_mask: valid_mask(config.sources),
            pending: 0,
            enable: 0,
            out: false,
        })
    }

    /// The port of source `index`, which must be below the source count.
    pub fn src_port(&self, index: u8) -> PortId {
        assert!(index < self.config.sources, "no source {index}");
        PortId(u16::from(index))
    }

    /// The `cpu` port, after the sources.
    pub fn cpu_port(&self) -> PortId {
        PortId(u16::from(self.config.sources))
    }

    /// The `mem` port, last.
    pub fn mem_port(&self) -> PortId {
        PortId(u16::from(self.config.sources) + 1)
    }

    /// `pending`: bit *i* is the current level of source *i*.
    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// `enable`.
    pub fn enable(&self) -> u32 {
        self.enable
    }

    /// The level last sent on `cpu`.
    pub fn out(&self) -> bool {
        self.out
    }

    /// Recomputes `meip` after an input change and sends it on `cpu` if it changed.
    fn update_output(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let meip = (self.pending & self.enable) != 0;
        if meip == self.out {
            return Ok(());
        }
        self.out = meip;
        ctx.trace(MEIP_KIND, vec![("asserted", Value::Bool(meip))]);
        ctx.send(
            self.cpu_port(),
            IrqMsg::Level { asserted: meip }.into(),
            ScheduleWhen::Now,
            Phase::Complete,
        )
    }

    /// Takes a level on source `index`.
    fn level(
        &mut self,
        index: u16,
        msg: &IrqMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "irq controller: irq.v0 level arrived outside COMPLETE",
            ));
        }
        let IrqMsg::Level { asserted } = *msg;
        let bit = 1u32 << index;
        let pending = if asserted {
            self.pending | bit
        } else {
            self.pending & !bit
        };
        if pending == self.pending {
            return Ok(());
        }
        self.pending = pending;
        ctx.trace(
            LEVEL_KIND,
            vec![
                ("source", Value::U64(u64::from(index))),
                ("asserted", Value::Bool(asserted)),
            ],
        );
        self.update_output(ctx)
    }

    /// Accepts a request on `mem`: reads or writes a register now, and sends the response
    /// after the configured latency, in `Complete`.
    fn request(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let first = match msg.access() {
            None => {
                return Err(SimError::ComponentFault(
                    "irq controller: response on the target port",
                ));
            }
            Some(Access::Empty) => {
                return Err(SimError::ComponentFault(
                    "irq controller: zero-length request",
                ));
            }
            Some(Access::OutOfRange) => None,
            Some(Access::Bytes { first, .. }) => Some(first),
        };
        let fault = MemFault::AccessFault;
        let resp = match msg {
            MemMsg::ReadReq { txn, len, .. } => MemMsg::ReadResp {
                txn: *txn,
                outcome: match (first, *len) {
                    (Some(PENDING), 4) => ReadOutcome::Data {
                        data: (self.pending & self.valid_mask).to_le_bytes().to_vec(),
                    },
                    (Some(ENABLE), 4) => ReadOutcome::Data {
                        data: self.enable.to_le_bytes().to_vec(),
                    },
                    _ => ReadOutcome::Fault { fault },
                },
            },
            MemMsg::WriteReq { txn, data, .. } => MemMsg::WriteResp {
                txn: *txn,
                outcome: match (first, <[u8; 4]>::try_from(data.as_slice())) {
                    (Some(ENABLE), Ok(bytes)) => {
                        self.enable = u32::from_le_bytes(bytes) & self.valid_mask;
                        self.update_output(ctx)?;
                        WriteOutcome::Done
                    }
                    _ => WriteOutcome::Fault { fault },
                },
            },
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "irq controller: response on the target port",
                ));
            }
        };
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(self.mem_port(), resp.into(), when, Phase::Complete)
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u8(self.config.sources);
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

impl Component for SimpleIrqController {
    fn type_name(&self) -> &'static str {
        "platform.irqc"
    }

    /// `src0` … `src{N−1}`, then `cpu`, then `mem`.
    fn ports(&self) -> Vec<PortSpec> {
        let mut ports: Vec<PortSpec> = SRC_NAMES[..usize::from(self.config.sources)]
            .iter()
            .map(|&name| PortSpec {
                name,
                protocol: irq_v0::PROTOCOL,
                role: Role::Target,
            })
            .collect();
        ports.push(PortSpec {
            name: "cpu",
            protocol: irq_v0::PROTOCOL,
            role: Role::Initiator,
        });
        ports.push(PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Target,
        });
        ports
    }

    /// Nothing to send: every line starts deasserted, and so does `cpu`.
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message { port, msg } = ev else {
            return Err(SimError::ComponentFault("irq controller: unexpected wake"));
        };
        let sources = u16::from(self.config.sources);
        match (port.0, msg) {
            (i, Message::Irq(msg)) if i < sources => self.level(i, msg, ctx),
            (i, _) if i < sources => Err(SimError::ComponentFault(
                "irq controller: message on a source is not irq.v0",
            )),
            (i, _) if i == sources => Err(SimError::ComponentFault(
                "irq controller: message on the cpu port",
            )),
            (i, Message::MemV1(msg)) if i == sources + 1 => self.request(msg, ctx),
            (i, _) if i == sources + 1 => Err(SimError::ComponentFault(
                "irq controller: message on mem is not mem.v1",
            )),
            _ => Err(SimError::ComponentFault(
                "irq controller: message on an unknown port",
            )),
        }
    }

    /// The source count, `pending`, `enable`, and `out`.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("sources", Value::U64(u64::from(self.config.sources))),
                ("pending", Value::U64(u64::from(self.pending))),
                ("enable", Value::U64(u64::from(self.enable))),
                ("out", Value::Bool(self.out)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (`sources`, latency), then `pending`, `enable`, and
    /// `out`.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u32(self.pending);
        w.u32(self.enable);
        w.bool(self.out);
    }

    /// Replaces the state with the snapshot's. Rejects a different source count or
    /// latency, a bit outside `valid_mask` in `pending` or `enable`, and an `out` other
    /// than `(pending & enable) != 0`, which no run produces: `out` is updated in the
    /// handler that changes its inputs.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        if r.u8()? != self.config.sources {
            return Err(RestoreError::InvalidState(
                "irq controller: snapshot has a different source count",
            ));
        }
        let mut latency = SnapshotWriter::new();
        self.write_config(&mut latency);
        let latency = &latency.as_bytes()[1..];
        if r.raw(latency.len())? != latency {
            return Err(RestoreError::InvalidState(
                "irq controller: snapshot has a different latency",
            ));
        }
        let pending = r.u32()?;
        let enable = r.u32()?;
        let out = r.bool()?;
        if pending & !self.valid_mask != 0 {
            return Err(RestoreError::InvalidState(
                "irq controller: pending bit outside the sources",
            ));
        }
        if enable & !self.valid_mask != 0 {
            return Err(RestoreError::InvalidState(
                "irq controller: enable bit outside the sources",
            ));
        }
        if out != ((pending & enable) != 0) {
            return Err(RestoreError::InvalidState(
                "irq controller: out is not (pending & enable) != 0",
            ));
        }
        self.pending = pending;
        self.enable = enable;
        self.out = out;
        Ok(())
    }
}
