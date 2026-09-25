//! `DmaBlockController`: a block-device DMA controller (`docs/m2-design.md` §9). This is
//! its control plane (M2.6): the MMIO register file, the command lifecycle, validation,
//! `REJECTED`, and the interrupt line.
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `mem`, a `mem.v1` target, the MMIO window;
//! `dma`, a `mem.v1` initiator (bus master); `blk`, a `block.v0` initiator; and `irq`, an
//! `irq.v0` initiator. `dma` and `blk` belong to the engine (§9.5–§9.7, M2.7): nothing is
//! ever sent on them yet, and anything arriving on them faults the session, since no
//! request is ever outstanding.
//!
//! # Register map
//!
//! Offsets from the controller's own start; the window is [`SIZE`] bytes. Every register
//! is 32 bits and accepts only aligned 4-byte accesses.
//!
//! | Offset | Register | Access | Behavior |
//! |---|---|---|---|
//! | `0x00` | `COMMAND` | write | `1` READ, `2` WRITE; anything else fails validation. Reads fault |
//! | `0x04` | `STATUS` | read; W1C bit 2 | bit 0 `BUSY`, bit 1 `DONE`, bit 2 `REJECTED`, bits `[15:8]` `ERROR` |
//! | `0x08` | `LBA` | read/write | first block of the next command |
//! | `0x0C` | `MEM_ADDR` | read/write | DMA start address of the next command |
//! | `0x10` | `BLOCK_COUNT` | read/write | number of blocks of the next command |
//! | `0x14` | `IRQ_ENABLE` | read/write | bit 0 enables the completion interrupt; other bits read 0 |
//! | `0x18` | `IRQ_STATUS` / `ACK` | read; W1C bit 0 | read: bit 0 = `DONE`; writing bit 0 acknowledges |
//! | `0x1C` | reserved | — | faults |
//!
//! Every other well-formed request (another width, a misaligned access, a read of
//! `COMMAND`, the reserved word, an offset past the window) gets a
//! [`MemFault::AccessFault`] response and changes nothing. A zero-length request faults
//! the session. Requests take effect at acceptance; the response follows after the
//! configured latency, in `Complete`.
//!
//! # Lifecycle
//!
//! | State | `BUSY` | `DONE` | `ERROR` |
//! |---|---|---|---|
//! | IDLE | 0 | 0 | 0 |
//! | BUSY | 1 | 0 | 0 |
//! | DONE | 0 | 1 | the code |
//!
//! - A `COMMAND` write in IDLE latches `LBA`, `MEM_ADDR`, and `BLOCK_COUNT` and is
//!   validated against them in the fixed order of §9.4: command, count, LBA range,
//!   alignment, DMA range. The first failing check completes the command at once (DONE,
//!   `ERROR` = its code) with no other effect. A command that passes goes to BUSY with the
//!   latched descriptor and the engine in `Issue`, at block 0, beat 0.
//! - A `COMMAND` write in BUSY or DONE is rejected: `REJECTED` is set and nothing else
//!   changes. Only writing 1 to `STATUS` bit 2 clears `REJECTED`.
//! - `ACK` in DONE clears `DONE` and `ERROR`, back to IDLE; elsewhere it does nothing.
//! - The interrupt line is `DONE && IRQ_ENABLE.bit0`, sent on `irq` (`Now`, `Complete`)
//!   only when it changes.
//!
//! # The engine boundary
//!
//! Issuing and completing transfers (§9.6, the `Wake(ISSUE)` that starts them, and §9.7)
//! is the engine, M2.7. Here an accepted command stays BUSY, at block 0, beat 0, with no
//! wake scheduled and nothing sent: it neither moves data nor completes. The snapshot
//! already has the whole §9.9 layout, with the engine fields at those values.

use std::fmt;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// Offset of `COMMAND`.
pub const COMMAND: u64 = 0x00;
/// Offset of `STATUS`.
pub const STATUS: u64 = 0x04;
/// Offset of `LBA`.
pub const LBA: u64 = 0x08;
/// Offset of `MEM_ADDR`.
pub const MEM_ADDR: u64 = 0x0C;
/// Offset of `BLOCK_COUNT`.
pub const BLOCK_COUNT: u64 = 0x10;
/// Offset of `IRQ_ENABLE`.
pub const IRQ_ENABLE: u64 = 0x14;
/// Offset of `IRQ_STATUS`, read.
pub const IRQ_STATUS: u64 = 0x18;
/// Offset of `ACK`, written: the same word as `IRQ_STATUS`.
pub const ACK: u64 = 0x18;
/// The size of the controller's window.
pub const SIZE: u64 = 0x20;

/// `COMMAND` value of a READ (media → RAM).
pub const OP_READ: u32 = 1;
/// `COMMAND` value of a WRITE (RAM → media).
pub const OP_WRITE: u32 = 2;

/// `STATUS` bit 0.
pub const STATUS_BUSY: u32 = 1 << 0;
/// `STATUS` bit 1.
pub const STATUS_DONE: u32 = 1 << 1;
/// `STATUS` bit 2, write 1 to clear.
pub const STATUS_REJECTED: u32 = 1 << 2;
/// Position of `ERROR` in `STATUS`, bits `[15:8]`.
pub const STATUS_ERROR_SHIFT: u32 = 8;

/// `ERROR`: `COMMAND` is not 1 or 2.
pub const BAD_COMMAND: u8 = 1;
/// `ERROR`: `BLOCK_COUNT` is 0.
pub const BAD_COUNT: u8 = 2;
/// `ERROR`: `LBA + BLOCK_COUNT > capacity_blocks`.
pub const LBA_RANGE: u8 = 3;
/// `ERROR`: `MEM_ADDR` is not 16-byte aligned.
pub const DMA_ALIGN: u8 = 4;
/// `ERROR`: the transfer does not lie inside the DMA aperture.
pub const DMA_RANGE: u8 = 5;

/// The largest `capacity_blocks`: the controller's LBAs are 32 bits.
pub const MAX_CAPACITY_BLOCKS: u64 = 1 << 32;
/// DMA addresses are below this: the controller's addresses are 32 bits.
pub const ADDRESS_LIMIT: u64 = 1 << 32;
/// `MEM_ADDR` alignment: one DMA beat.
pub const BEAT_SIZE: u64 = 16;

/// Trace kind of a `COMMAND` written in IDLE.
pub const COMMAND_KIND: &str = "platform.blk.command";
/// Trace kind of a completion.
pub const DONE_KIND: &str = "platform.blk.done";
/// Trace kind of a `COMMAND` rejected in BUSY or DONE.
pub const REJECTED_KIND: &str = "platform.blk.rejected";

/// Layout of [`DmaBlockController`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// The MMIO window, a `mem.v1` target.
pub const MEM_PORT: PortId = PortId(0);
/// The DMA master, a `mem.v1` initiator.
pub const DMA_PORT: PortId = PortId(1);
/// The media link, a `block.v0` initiator.
pub const BLK_PORT: PortId = PortId(2);
/// The interrupt line, an `irq.v0` initiator.
pub const IRQ_PORT: PortId = PortId(3);

const BLOCK_BYTES: u64 = block_v0::BLOCK_SIZE as u64;

/// Configuration of a [`DmaBlockController`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DmaBlockControllerConfig {
    /// The engine's clock domain.
    pub clock: ClockDomainId,
    /// Delay from accepting an MMIO request to sending its response.
    pub latency: LinkLatency,
    /// Blocks the controller validates commands against, in
    /// `1..=`[`MAX_CAPACITY_BLOCKS`]. It is the controller's own contract; it need not
    /// equal any media's capacity.
    pub capacity_blocks: u64,
    /// First address of the DMA aperture.
    pub dma_base: u64,
    /// Size of the DMA aperture in bytes: at least 1, and `dma_base + dma_size` at most
    /// [`ADDRESS_LIMIT`]. It may span several regions or holes of the bus map.
    pub dma_size: u64,
}

/// Why a [`DmaBlockController`] cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaBlockControllerConfigError {
    /// `capacity_blocks` is 0 or above [`MAX_CAPACITY_BLOCKS`].
    Capacity(u64),
    /// `dma_size` is 0.
    EmptyAperture,
    /// `dma_base + dma_size` overflows or exceeds [`ADDRESS_LIMIT`].
    ApertureOutOfRange,
}

impl fmt::Display for DmaBlockControllerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmaBlockControllerConfigError::Capacity(n) => {
                write!(f, "controller capacity {n} is not in 1..=2^32 blocks")
            }
            DmaBlockControllerConfigError::EmptyAperture => f.write_str("DMA aperture is empty"),
            DmaBlockControllerConfigError::ApertureOutOfRange => {
                f.write_str("DMA aperture reaches past 0x1_0000_0000")
            }
        }
    }
}

impl std::error::Error for DmaBlockControllerConfigError {}

/// What an accepted command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Operation {
    /// Media → RAM.
    Read,
    /// RAM → media.
    Write,
}

impl Operation {
    /// The `COMMAND` value, as the snapshot stores it.
    fn code(self) -> u8 {
        match self {
            Operation::Read => 1,
            Operation::Write => 2,
        }
    }
}

/// The registers a command latched when it was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Latched {
    op: Operation,
    lba: u32,
    addr: u32,
    count: u32,
}

/// The engine's position (§9.6). The engine is M2.7; here it is only ever `Idle`, or
/// `Issue` at block 0, beat 0 while BUSY.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Engine {
    Idle,
    Issue,
}

/// The block-device DMA controller's control plane.
pub struct DmaBlockController {
    config: DmaBlockControllerConfig,
    lba: u32,
    mem_addr: u32,
    block_count: u32,
    /// Bit 0 only.
    irq_enable: u32,
    busy: bool,
    done: bool,
    rejected: bool,
    error: u8,
    /// Present exactly when BUSY.
    latched: Option<Latched>,
    engine: Engine,
    /// Next `TxnId` on `dma`; nothing is sent there yet.
    dma_txn: TxnId,
    /// Next `TxnId` on `blk`; nothing is sent there yet.
    blk_txn: TxnId,
    /// The last level sent on `irq`; deasserted at reset.
    irq_level: bool,
}

impl DmaBlockController {
    /// Creates a controller in IDLE with every register 0 and the line deasserted.
    ///
    /// Rejects a capacity outside `1..=2^32` blocks, an empty aperture, and an aperture
    /// reaching past `0x1_0000_0000`.
    pub fn new(
        config: DmaBlockControllerConfig,
    ) -> Result<DmaBlockController, DmaBlockControllerConfigError> {
        if config.capacity_blocks == 0 || config.capacity_blocks > MAX_CAPACITY_BLOCKS {
            return Err(DmaBlockControllerConfigError::Capacity(
                config.capacity_blocks,
            ));
        }
        if config.dma_size == 0 {
            return Err(DmaBlockControllerConfigError::EmptyAperture);
        }
        match config.dma_base.checked_add(config.dma_size) {
            Some(end) if end <= ADDRESS_LIMIT => {}
            _ => return Err(DmaBlockControllerConfigError::ApertureOutOfRange),
        }
        Ok(DmaBlockController {
            config,
            lba: 0,
            mem_addr: 0,
            block_count: 0,
            irq_enable: 0,
            busy: false,
            done: false,
            rejected: false,
            error: 0,
            latched: None,
            engine: Engine::Idle,
            dma_txn: TxnId(0),
            blk_txn: TxnId(0),
            irq_level: false,
        })
    }

    /// The `STATUS` register.
    fn status(&self) -> u32 {
        let mut status = u32::from(self.error) << STATUS_ERROR_SHIFT;
        if self.busy {
            status |= STATUS_BUSY;
        }
        if self.done {
            status |= STATUS_DONE;
        }
        if self.rejected {
            status |= STATUS_REJECTED;
        }
        status
    }

    /// Checks a command against the latched registers, in the order of §9.4, with
    /// checked 64-bit arithmetic: the operation, or the first failing check's code.
    fn validate(&self, command: u32, lba: u32, addr: u32, count: u32) -> Result<Operation, u8> {
        let op = match command {
            OP_READ => Operation::Read,
            OP_WRITE => Operation::Write,
            _ => return Err(BAD_COMMAND),
        };
        if count == 0 {
            return Err(BAD_COUNT);
        }
        match u64::from(lba).checked_add(u64::from(count)) {
            Some(end) if end <= self.config.capacity_blocks => {}
            _ => return Err(LBA_RANGE),
        }
        if u64::from(addr) % BEAT_SIZE != 0 {
            return Err(DMA_ALIGN);
        }
        let start = u64::from(addr);
        let aperture_end = self.config.dma_base + self.config.dma_size;
        match u64::from(count)
            .checked_mul(BLOCK_BYTES)
            .and_then(|total| start.checked_add(total))
        {
            Some(end) if start >= self.config.dma_base && end <= aperture_end => Ok(op),
            _ => Err(DMA_RANGE),
        }
    }

    /// Recomputes the interrupt line and sends it on `irq` if it changed.
    fn update_irq(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let level = self.done && self.irq_enable & 1 != 0;
        if level == self.irq_level {
            return Ok(());
        }
        self.irq_level = level;
        ctx.send(
            IRQ_PORT,
            IrqMsg::Level { asserted: level }.into(),
            ScheduleWhen::Now,
            Phase::Complete,
        )
    }

    /// A `COMMAND` write.
    fn command(&mut self, value: u32, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if self.busy || self.done {
            self.rejected = true;
            ctx.trace(REJECTED_KIND, vec![]);
            return Ok(());
        }
        let (lba, addr, count) = (self.lba, self.mem_addr, self.block_count);
        let result = self.validate(value, lba, addr, count);
        ctx.trace(
            COMMAND_KIND,
            vec![
                ("op", Value::U64(u64::from(value))),
                ("lba", Value::U64(u64::from(lba))),
                ("addr", Value::U64(u64::from(addr))),
                ("count", Value::U64(u64::from(count))),
                ("accepted", Value::Bool(result.is_ok())),
            ],
        );
        match result {
            Ok(op) => {
                self.busy = true;
                self.latched = Some(Latched {
                    op,
                    lba,
                    addr,
                    count,
                });
                self.engine = Engine::Issue;
                Ok(())
            }
            Err(code) => {
                self.done = true;
                self.error = code;
                ctx.trace(DONE_KIND, vec![("error", Value::U64(u64::from(code)))]);
                self.update_irq(ctx)
            }
        }
    }

    /// The register at `offset` as a read sees it, or `None` if it cannot be read.
    fn read_register(&self, offset: u64) -> Option<u32> {
        match offset {
            STATUS => Some(self.status()),
            LBA => Some(self.lba),
            MEM_ADDR => Some(self.mem_addr),
            BLOCK_COUNT => Some(self.block_count),
            IRQ_ENABLE => Some(self.irq_enable),
            IRQ_STATUS => Some(u32::from(self.done)),
            _ => None,
        }
    }

    /// Writes `value` to the register at `offset`; `false`, having changed nothing, if it
    /// cannot be written.
    fn write_register(
        &mut self,
        offset: u64,
        value: u32,
        ctx: &mut dyn SimContext,
    ) -> Result<bool, SimError> {
        match offset {
            COMMAND => self.command(value, ctx)?,
            STATUS => {
                if value & STATUS_REJECTED != 0 {
                    self.rejected = false;
                }
            }
            LBA => self.lba = value,
            MEM_ADDR => self.mem_addr = value,
            BLOCK_COUNT => self.block_count = value,
            IRQ_ENABLE => {
                self.irq_enable = value & 1;
                self.update_irq(ctx)?;
            }
            ACK => {
                if value & 1 != 0 && self.done {
                    self.done = false;
                    self.error = 0;
                    self.update_irq(ctx)?;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Accepts a request on `mem`: reads or writes a register now, and sends the response
    /// after the configured latency, in `Complete`.
    fn request(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        // The offset of an aligned 4-byte access inside the window, if it is one.
        let word = match msg.access() {
            None => {
                return Err(SimError::ComponentFault(
                    "dma controller: response on the mem port",
                ));
            }
            Some(Access::Empty) => {
                return Err(SimError::ComponentFault(
                    "dma controller: zero-length request",
                ));
            }
            Some(Access::OutOfRange) => None,
            Some(Access::Bytes { first, last }) => {
                (last - first == 3 && first % 4 == 0 && first < SIZE).then_some(first)
            }
        };
        let fault = MemFault::AccessFault;
        let resp = match msg {
            MemMsg::ReadReq { txn, .. } => MemMsg::ReadResp {
                txn: *txn,
                outcome: match word.and_then(|offset| self.read_register(offset)) {
                    Some(value) => ReadOutcome::Data {
                        data: value.to_le_bytes().to_vec(),
                    },
                    None => ReadOutcome::Fault { fault },
                },
            },
            MemMsg::WriteReq { txn, data, .. } => {
                let written = match (word, <[u8; 4]>::try_from(data.as_slice())) {
                    (Some(offset), Ok(bytes)) => {
                        self.write_register(offset, u32::from_le_bytes(bytes), ctx)?
                    }
                    _ => false,
                };
                MemMsg::WriteResp {
                    txn: *txn,
                    outcome: if written {
                        WriteOutcome::Done
                    } else {
                        WriteOutcome::Fault { fault }
                    },
                }
            }
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "dma controller: response on the mem port",
                ));
            }
        };
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(MEM_PORT, resp.into(), when, Phase::Complete)
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u32(self.config.clock.0);
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
        w.u64(self.config.capacity_blocks);
        w.u64(self.config.dma_base);
        w.u64(self.config.dma_size);
    }
}

impl Component for DmaBlockController {
    fn type_name(&self) -> &'static str {
        "platform.blk"
    }

    /// `mem`, `dma`, `blk`, `irq`.
    fn ports(&self) -> Vec<PortSpec> {
        vec![
            PortSpec {
                name: "mem",
                protocol: mem_v1::PROTOCOL,
                role: Role::Target,
            },
            PortSpec {
                name: "dma",
                protocol: mem_v1::PROTOCOL,
                role: Role::Initiator,
            },
            PortSpec {
                name: "blk",
                protocol: block_v0::PROTOCOL,
                role: Role::Initiator,
            },
            PortSpec {
                name: "irq",
                protocol: irq_v0::PROTOCOL,
                role: Role::Initiator,
            },
        ]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    /// Serves MMIO requests on `mem`. Nothing else may arrive: no engine request is ever
    /// outstanding on `dma` or `blk`, `irq` is an output, and no wake is scheduled.
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                port: MEM_PORT,
                msg: Message::MemV1(msg),
            } => self.request(msg, ctx),
            Delivered::Message {
                port: DMA_PORT | BLK_PORT,
                ..
            } => Err(SimError::ComponentFault(
                "dma controller: engine message with no request outstanding",
            )),
            _ => Err(SimError::ComponentFault(
                "dma controller: unexpected delivery",
            )),
        }
    }

    /// The registers, the lifecycle state, the latched command, and the engine position.
    fn inspect(&self) -> StateView {
        let state = if self.busy {
            "busy"
        } else if self.done {
            "done"
        } else {
            "idle"
        };
        let command = match self.latched {
            None => "none".to_string(),
            Some(l) => format!(
                "{} lba={:#x} addr={:#x} count={}",
                match l.op {
                    Operation::Read => "read",
                    Operation::Write => "write",
                },
                l.lba,
                l.addr,
                l.count
            ),
        };
        let engine = match self.engine {
            Engine::Idle => "idle",
            Engine::Issue => "issue",
        };
        StateView {
            fields: vec![
                ("lba", Value::U64(u64::from(self.lba))),
                ("mem_addr", Value::U64(u64::from(self.mem_addr))),
                ("block_count", Value::U64(u64::from(self.block_count))),
                ("irq_enable", Value::U64(u64::from(self.irq_enable))),
                ("status", Value::U64(u64::from(self.status()))),
                ("state", Value::Str(state.to_string())),
                ("error", Value::U64(u64::from(self.error))),
                ("rejected", Value::Bool(self.rejected)),
                ("command", Value::Str(command)),
                ("engine", Value::Str(engine.to_string())),
                ("block", Value::U64(0)),
                ("beat", Value::U64(0)),
                ("irq", Value::Bool(self.irq_level)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1, in the order of §9.9:
    ///
    /// 1. config: `clock` (`u32`), latency (tag `u8`, then `u128` femtoseconds or `u32`
    ///    domain and `u64` cycles), `capacity_blocks`, `dma_base`, `dma_size` (`u64`s);
    /// 2. registers: `LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `IRQ_ENABLE` (`u32`s);
    /// 3. `BUSY`, `DONE`, `REJECTED` (`bool`s), `ERROR` (`u8`);
    /// 4. when BUSY only: the latched operation (`u8` 1 or 2), `LBA`, `MEM_ADDR`,
    ///    `BLOCK_COUNT` (`u32`s);
    /// 5. the engine state (`u8`: 0 `Idle`, 1 `Issue`; the engine adds 2 `WaitMedia` and
    ///    3 `WaitBeat`, each followed by its `u64` txn), block index *i* (`u32`), beat
    ///    index *j* (`u8`);
    /// 6. the block buffer (length-prefixed bytes);
    /// 7. the next `dma` and `blk` `TxnId`s (`u64`s);
    /// 8. the last IRQ level sent (`bool`).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u32(self.lba);
        w.u32(self.mem_addr);
        w.u32(self.block_count);
        w.u32(self.irq_enable);
        w.bool(self.busy);
        w.bool(self.done);
        w.bool(self.rejected);
        w.u8(self.error);
        if let Some(l) = self.latched {
            w.u8(l.op.code());
            w.u32(l.lba);
            w.u32(l.addr);
            w.u32(l.count);
        }
        w.u8(match self.engine {
            Engine::Idle => 0,
            Engine::Issue => 1,
        });
        w.u32(0);
        w.u8(0);
        w.bytes(&[]);
        w.u64(self.dma_txn.0);
        w.u64(self.blk_txn.0);
        w.bool(self.irq_level);
    }

    /// Replaces the whole state with the snapshot's, or changes nothing.
    ///
    /// Rejects a different configuration and every state no run can produce:
    /// `IRQ_ENABLE` bits other than bit 0; BUSY with DONE; an `ERROR` outside DONE, or in
    /// DONE other than a validation code (1–5); a latched command that fails validation;
    /// an engine state other than `Idle` without BUSY or other than `Issue` with it;
    /// engine indices other than 0; a non-empty buffer; a used `TxnId`; and an IRQ level
    /// other than `DONE && IRQ_ENABLE.bit0`.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(invalid("dma controller: snapshot has a different config"));
        }
        let (lba, mem_addr, block_count, irq_enable) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
        if irq_enable & !1 != 0 {
            return Err(invalid("dma controller: IRQ_ENABLE has unused bits"));
        }
        let (busy, done, rejected, error) = (r.bool()?, r.bool()?, r.bool()?, r.u8()?);
        if busy && done {
            return Err(invalid("dma controller: BUSY and DONE together"));
        }
        let error_ok = if done {
            (BAD_COMMAND..=DMA_RANGE).contains(&error)
        } else {
            error == 0
        };
        if !error_ok {
            return Err(invalid("dma controller: ERROR does not match the state"));
        }
        let latched = if busy {
            let (op, lba, addr, count) = (r.u8()?, r.u32()?, r.u32()?, r.u32()?);
            let op = self
                .validate(u32::from(op), lba, addr, count)
                .map_err(|_| invalid("dma controller: latched command fails validation"))?;
            Some(Latched {
                op,
                lba,
                addr,
                count,
            })
        } else {
            None
        };
        let engine = match r.u8()? {
            0 => Engine::Idle,
            1 => Engine::Issue,
            _ => return Err(invalid("dma controller: engine state not reachable")),
        };
        if (engine == Engine::Issue) != busy {
            return Err(invalid("dma controller: engine state does not match BUSY"));
        }
        if r.u32()? != 0 || r.u8()? != 0 {
            return Err(invalid("dma controller: engine has made progress"));
        }
        if !r.bytes()?.is_empty() {
            return Err(invalid("dma controller: block buffer not empty"));
        }
        if r.u64()? != 0 || r.u64()? != 0 {
            return Err(invalid("dma controller: a TxnId was used"));
        }
        let irq_level = r.bool()?;
        if irq_level != (done && irq_enable & 1 != 0) {
            return Err(invalid(
                "dma controller: IRQ level is not DONE && IRQ_ENABLE",
            ));
        }
        self.lba = lba;
        self.mem_addr = mem_addr;
        self.block_count = block_count;
        self.irq_enable = irq_enable;
        self.busy = busy;
        self.done = done;
        self.rejected = rejected;
        self.error = error;
        self.latched = latched;
        self.engine = engine;
        self.dma_txn = TxnId(0);
        self.blk_txn = TxnId(0);
        self.irq_level = irq_level;
        Ok(())
    }
}
