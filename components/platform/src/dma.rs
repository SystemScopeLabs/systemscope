//! `DmaBlockController`: a block-device DMA controller (`docs/m2-design.md` §9): the MMIO
//! register file, the command lifecycle, validation, `REJECTED`, and the interrupt line
//! (M2.6), and the engine's READ (M2.7a) and WRITE (M2.7b) paths.
//!
//! # Ports
//!
//! In [`ports()`](Component::ports) order: `mem`, a `mem.v1` target, the MMIO window;
//! `dma`, a `mem.v1` initiator (bus master); `blk`, a `block.v0` initiator; and `irq`, an
//! `irq.v0` initiator. `dma` and `blk` carry the engine's requests (§9.5, §9.6); a message
//! arriving on them that is not the result of the one outstanding request faults the
//! session (§9.8).
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
//!   latched descriptor and the engine in `Issue`, at block 0, beat 0, and schedules
//!   `Wake(ISSUE)`.
//! - A `COMMAND` write in BUSY or DONE is rejected: `REJECTED` is set and nothing else
//!   changes. Only writing 1 to `STATUS` bit 2 clears `REJECTED`.
//! - `ACK` in DONE clears `DONE` and `ERROR`, back to IDLE; elsewhere it does nothing.
//! - The interrupt line is `DONE && IRQ_ENABLE.bit0`, sent on `irq` (`Now`, `Complete`)
//!   only when it changes.
//!
//! # Engine
//!
//! The engine mirrors the CPU's state machine (§9.6):
//!
//! ```text
//! Idle ──valid COMMAND @ Transfer──▶ Issue (Wake(ISSUE) @ Request, next controller cycle)
//! Issue ──Wake(ISSUE)──▶ send the next block.v0 request or beat, Now ──▶ WaitMedia / WaitBeat { txn }
//! WaitMedia / WaitBeat ──result @ Complete──▶ Issue (Wake(ISSUE), next cycle) | Done
//! ```
//!
//! For block *i* of a READ, `Issue` with an empty block buffer sends
//! `ReadBlock(LBA + i)`; its `Data` (exactly 512 bytes) fills the buffer, and `Issue` then
//! sends beat *j* as a 16-byte `WriteReq` of `buffer[16j..16j + 16]` to
//! `MEM_ADDR + 512i + 16j`, one beat at a time. After beat 31's `Done` the buffer is
//! dropped and block *i* + 1 starts; after the last block's, the command completes with
//! `ERROR = 0`. At most one request is outstanding in total, and each is sent under a
//! fresh `TxnId` from its port's counter, which never wraps (§9.5, §6.2).
//!
//! For block *i* of a WRITE, `Issue` at beat *j* < 32 sends a 16-byte `ReadReq` of
//! `MEM_ADDR + 512i + 16j`, and its `Data` (exactly 16 bytes) is appended to the buffer,
//! which therefore holds 16 × *j* bytes while beat *j* is due or outstanding. After beat
//! 31 the beat index is 32 and the buffer holds all 512 bytes; `Issue` then sends
//! `WriteBlock(LBA + i, buffer)` and drops the buffer, since the request carries the
//! data. Only its `Done` starts block *i* + 1 at beat 0, or completes the command after
//! the last block. No `WriteBlock` is sent before all 32 beats of its block were read.
//!
//! A media `Error` ends the command with `ERROR = 7` (MEDIA_ERROR), and a beat's `Fault`
//! with `ERROR = 6` (DMA_FAULT), as §9.7 and §9.8 require of a device error; nothing
//! further is issued. These failure paths are not yet closed (M2.7c).

use std::fmt;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{
    self, BlockMsg, BlockReadOutcome, BlockWriteOutcome,
};
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
/// `ERROR`: a DMA beat was answered with `Fault`.
pub const DMA_FAULT: u8 = 6;
/// `ERROR`: the media answered with `Error`.
pub const MEDIA_ERROR: u8 = 7;

/// The largest `capacity_blocks`: the controller's LBAs are 32 bits.
pub const MAX_CAPACITY_BLOCKS: u64 = 1 << 32;
/// DMA addresses are below this: the controller's addresses are 32 bits.
pub const ADDRESS_LIMIT: u64 = 1 << 32;
/// `MEM_ADDR` alignment: one DMA beat.
pub const BEAT_SIZE: u64 = 16;
/// Beats per block: 512 / 16.
pub const BEATS_PER_BLOCK: u8 = 32;

/// Wake token that sends the engine's next request.
pub const ISSUE: u64 = 0;

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

/// The engine's position (§9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Engine {
    /// No command, or DONE.
    Idle,
    /// The next request is due at `Wake(ISSUE)`. READ: `ReadBlock` if the buffer is
    /// empty, else beat *j*. WRITE: beat *j* below 32, else `WriteBlock`.
    Issue,
    /// `ReadBlock { txn }` (READ) or `WriteBlock { txn }` (WRITE) of block *i* is
    /// outstanding.
    WaitMedia(TxnId),
    /// Beat *j* of block *i* is outstanding: `WriteReq { txn }` (READ) or
    /// `ReadReq { txn }` (WRITE).
    WaitBeat(TxnId),
}

/// The block-device DMA controller.
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
    /// Block index *i* of the latched command; 0 when `Idle`.
    block: u32,
    /// Beat index *j* in block *i*, below [`BEATS_PER_BLOCK`], or equal to it while a
    /// WRITE's `WriteBlock` is due or outstanding; 0 when `Idle`.
    beat: u8,
    /// Block *i*'s data as §9.9 stores it. READ: from its `Data` through its beat 31's
    /// `Done`. WRITE: the 16 × *j* bytes read so far, until the `WriteBlock` is sent.
    buffer: Vec<u8>,
    /// Next `TxnId` on `dma`.
    dma_txn: TxnId,
    /// Next `TxnId` on `blk`.
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
            block: 0,
            beat: 0,
            buffer: Vec::new(),
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
                self.wake_next_cycle(ctx)
            }
            Err(code) => self.complete(code, ctx),
        }
    }

    /// Wakes the engine at the next controller cycle's `Request`.
    fn wake_next_cycle(&self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: 1,
        };
        ctx.wake_self(when, Phase::Request, ISSUE)
    }

    /// Ends the command, or a failed validation, with `code`: DONE, the engine `Idle`,
    /// and the line recomputed.
    fn complete(&mut self, code: u8, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.busy = false;
        self.done = true;
        self.error = code;
        self.latched = None;
        self.engine = Engine::Idle;
        self.block = 0;
        self.beat = 0;
        self.buffer = Vec::new();
        ctx.trace(DONE_KIND, vec![("error", Value::U64(u64::from(code)))]);
        self.update_irq(ctx)
    }

    /// The value after `counter`, or a session fault with nothing sent and the counter
    /// kept (§6.2). The caller stores it once the send succeeded.
    fn next_txn(counter: TxnId) -> Result<u64, SimError> {
        counter.0.checked_add(1).ok_or(SimError::ComponentFault(
            "dma controller: TxnId space exhausted",
        ))
    }

    /// `Wake(ISSUE)`: sends the latched command's next request, `Now` in `Request`.
    fn issue(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let latched = match (self.engine, self.latched) {
            (Engine::Issue, Some(latched)) => latched,
            _ => {
                return Err(SimError::ComponentFault(
                    "dma controller: ISSUE wake with nothing to issue",
                ));
            }
        };
        match latched.op {
            Operation::Read if self.buffer.is_empty() => {
                let lba = self.block_lba(latched)?;
                self.send_block(
                    BlockMsg::ReadBlock {
                        txn: self.blk_txn,
                        lba,
                    },
                    ctx,
                )
            }
            Operation::Read => {
                let addr = self.beat_addr(latched)?;
                let start = usize::from(self.beat) * BEAT_SIZE as usize;
                let data = self.buffer[start..start + BEAT_SIZE as usize].to_vec();
                let txn = self.dma_txn;
                self.send_beat(MemMsg::WriteReq { txn, addr, data }, ctx)
            }
            Operation::Write if self.beat == BEATS_PER_BLOCK => {
                let lba = self.block_lba(latched)?;
                let data = self.buffer.clone();
                self.send_block(
                    BlockMsg::WriteBlock {
                        txn: self.blk_txn,
                        lba,
                        data,
                    },
                    ctx,
                )?;
                // The request carries the data: none is kept while it is outstanding.
                self.buffer = Vec::new();
                Ok(())
            }
            Operation::Write => {
                let addr = self.beat_addr(latched)?;
                let txn = self.dma_txn;
                let len = BEAT_SIZE as u32;
                self.send_beat(MemMsg::ReadReq { txn, addr, len }, ctx)
            }
        }
    }

    /// The media block of block index *i*: `LBA + i`.
    fn block_lba(&self, latched: Latched) -> Result<u64, SimError> {
        u64::from(latched.lba)
            .checked_add(u64::from(self.block))
            .ok_or(SimError::ComponentFault(
                "dma controller: engine index overflow",
            ))
    }

    /// The address of beat *j* of block *i*: `MEM_ADDR + 512i + 16j`.
    fn beat_addr(&self, latched: Latched) -> Result<u64, SimError> {
        let offset = u64::from(self.beat) * BEAT_SIZE;
        u64::from(self.block)
            .checked_mul(BLOCK_BYTES)
            .and_then(|block| block.checked_add(offset))
            .and_then(|delta| u64::from(latched.addr).checked_add(delta))
            .ok_or(SimError::ComponentFault(
                "dma controller: engine index overflow",
            ))
    }

    /// Sends `msg`, which carries the next `blk` txn, and waits for its result.
    fn send_block(&mut self, msg: BlockMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let txn = self.blk_txn;
        let next = Self::next_txn(txn)?;
        ctx.send(BLK_PORT, msg.into(), ScheduleWhen::Now, Phase::Request)?;
        self.blk_txn = TxnId(next);
        self.engine = Engine::WaitMedia(txn);
        Ok(())
    }

    /// Sends the beat `msg`, which carries the next `dma` txn, and waits for its response.
    fn send_beat(&mut self, msg: MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let txn = self.dma_txn;
        let next = Self::next_txn(txn)?;
        ctx.send(DMA_PORT, msg.into(), ScheduleWhen::Now, Phase::Request)?;
        self.dma_txn = TxnId(next);
        self.engine = Engine::WaitBeat(txn);
        Ok(())
    }

    /// Checks that a result for `txn` answers the outstanding `expected` in `Complete`.
    fn check_result(txn: TxnId, expected: TxnId, ctx: &dyn SimContext) -> Result<(), SimError> {
        if txn != expected {
            return Err(SimError::ComponentFault(
                "dma controller: result for a txn that is not outstanding",
            ));
        }
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "dma controller: result arrived outside COMPLETE",
            ));
        }
        Ok(())
    }

    /// A message on `blk`: the result of the outstanding `ReadBlock` or `WriteBlock`, in
    /// `Complete`.
    fn media_result(&mut self, msg: &BlockMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let (Engine::WaitMedia(expected), Some(latched)) = (self.engine, self.latched) else {
            return Err(SimError::ComponentFault(
                "dma controller: block.v0 message with no request outstanding",
            ));
        };
        match (latched.op, msg) {
            (Operation::Read, BlockMsg::ReadResult { txn, outcome }) => {
                Self::check_result(*txn, expected, ctx)?;
                match outcome {
                    BlockReadOutcome::Data { data } => {
                        if data.len() != block_v0::BLOCK_SIZE {
                            return Err(SimError::ComponentFault(
                                "dma controller: block data is not 512 bytes",
                            ));
                        }
                        self.buffer = data.clone();
                        self.engine = Engine::Issue;
                        self.wake_next_cycle(ctx)
                    }
                    BlockReadOutcome::Error { .. } => self.complete(MEDIA_ERROR, ctx),
                }
            }
            (Operation::Write, BlockMsg::WriteResult { txn, outcome }) => {
                Self::check_result(*txn, expected, ctx)?;
                match outcome {
                    BlockWriteOutcome::Done => self.next_block(latched, ctx),
                    BlockWriteOutcome::Error { .. } => self.complete(MEDIA_ERROR, ctx),
                }
            }
            _ => Err(SimError::ComponentFault(
                "dma controller: block.v0 message is not the outstanding request's result",
            )),
        }
    }

    /// A message on `dma`: the response to the outstanding beat, in `Complete`.
    fn beat_result(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let (Engine::WaitBeat(expected), Some(latched)) = (self.engine, self.latched) else {
            return Err(SimError::ComponentFault(
                "dma controller: mem.v1 message with no beat outstanding",
            ));
        };
        match (latched.op, msg) {
            (Operation::Read, MemMsg::WriteResp { txn, outcome }) => {
                Self::check_result(*txn, expected, ctx)?;
                if let WriteOutcome::Fault { .. } = outcome {
                    return self.complete(DMA_FAULT, ctx);
                }
                self.beat += 1;
                if self.beat == BEATS_PER_BLOCK {
                    return self.next_block(latched, ctx);
                }
            }
            (Operation::Write, MemMsg::ReadResp { txn, outcome }) => {
                Self::check_result(*txn, expected, ctx)?;
                let data = match outcome {
                    ReadOutcome::Data { data } => data,
                    ReadOutcome::Fault { .. } => return self.complete(DMA_FAULT, ctx),
                };
                if data.len() as u64 != BEAT_SIZE {
                    return Err(SimError::ComponentFault(
                        "dma controller: beat data is not 16 bytes",
                    ));
                }
                // After beat 31, `Issue` at beat 32 sends the `WriteBlock`.
                self.buffer.extend_from_slice(data);
                self.beat += 1;
            }
            _ => {
                return Err(SimError::ComponentFault(
                    "dma controller: mem.v1 message is not the outstanding beat's response",
                ));
            }
        }
        self.engine = Engine::Issue;
        self.wake_next_cycle(ctx)
    }

    /// Block *i* is done: starts block *i* + 1 at beat 0 with an empty buffer, or
    /// completes the command after the last block.
    fn next_block(&mut self, latched: Latched, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.beat = 0;
        self.buffer = Vec::new();
        self.block = self.block.checked_add(1).ok_or(SimError::ComponentFault(
            "dma controller: engine index overflow",
        ))?;
        if self.block == latched.count {
            return self.complete(0, ctx);
        }
        self.engine = Engine::Issue;
        self.wake_next_cycle(ctx)
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

    /// Serves MMIO requests on `mem`, `Wake(ISSUE)`, and the results of the outstanding
    /// engine request on `dma` and `blk`. Anything else faults the session: `irq` is an
    /// output, and a message of the wrong protocol, kind, or `txn` is a protocol
    /// violation (§9.8).
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                port: MEM_PORT,
                msg: Message::MemV1(msg),
            } => self.request(msg, ctx),
            Delivered::Message {
                port: DMA_PORT,
                msg: Message::MemV1(msg),
            } => self.beat_result(msg, ctx),
            Delivered::Message {
                port: BLK_PORT,
                msg: Message::Block(msg),
            } => self.media_result(msg, ctx),
            Delivered::Wake { token: ISSUE } => self.issue(ctx),
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
            Engine::WaitMedia(_) => "wait_media",
            Engine::WaitBeat(_) => "wait_beat",
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
                ("block", Value::U64(u64::from(self.block))),
                ("beat", Value::U64(u64::from(self.beat))),
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
    /// 5. the engine state (`u8`: 0 `Idle`, 1 `Issue`, 2 `WaitMedia`, 3 `WaitBeat`, the
    ///    last two followed by the outstanding `u64` txn), block index *i* (`u32`), beat
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
        match self.engine {
            Engine::Idle => w.u8(0),
            Engine::Issue => w.u8(1),
            Engine::WaitMedia(txn) => {
                w.u8(2);
                w.u64(txn.0);
            }
            Engine::WaitBeat(txn) => {
                w.u8(3);
                w.u64(txn.0);
            }
        }
        w.u32(self.block);
        w.u8(self.beat);
        w.bytes(&self.buffer);
        w.u64(self.dma_txn.0);
        w.u64(self.blk_txn.0);
        w.bool(self.irq_level);
    }

    /// Replaces the whole state with the snapshot's, or changes nothing.
    ///
    /// Rejects a different configuration and every state no run can produce:
    /// `IRQ_ENABLE` bits other than bit 0; BUSY with DONE; an `ERROR` outside DONE, or in
    /// DONE above 7; a latched command that fails validation; an engine state other than
    /// `Idle` without BUSY, or `Idle` with it; engine indices or a buffer outside `Idle`'s
    /// zeros and empty buffer; a block index not below the latched count; a beat index
    /// past 31 (READ) or 32 (WRITE, its `WriteBlock`); a buffer length other than the one
    /// its engine position requires (§9.9); an outstanding `txn` that is not the latest
    /// issued on its port; and an IRQ level other than `DONE && IRQ_ENABLE.bit0`.
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
            error <= MEDIA_ERROR
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
            2 => Engine::WaitMedia(TxnId(r.u64()?)),
            3 => Engine::WaitBeat(TxnId(r.u64()?)),
            _ => return Err(invalid("dma controller: unknown engine state")),
        };
        let (block, beat) = (r.u32()?, r.u8()?);
        let buffer = r.bytes()?.to_vec();
        let (dma_txn, blk_txn) = (r.u64()?, r.u64()?);
        let irq_level = r.bool()?;
        let full = buffer.len() == block_v0::BLOCK_SIZE;
        let latest = |next: u64, txn: TxnId| next.checked_sub(1) == Some(txn.0);
        let engine_ok = match latched {
            None => engine == Engine::Idle && block == 0 && beat == 0 && buffer.is_empty(),
            Some(Latched {
                op: Operation::Write,
                count,
                ..
            }) => {
                let read_so_far = buffer.len() == usize::from(beat) * BEAT_SIZE as usize;
                block < count
                    && beat <= BEATS_PER_BLOCK
                    && match engine {
                        Engine::Idle => false,
                        Engine::Issue => read_so_far,
                        Engine::WaitBeat(txn) => {
                            beat < BEATS_PER_BLOCK && read_so_far && latest(dma_txn, txn)
                        }
                        Engine::WaitMedia(txn) => {
                            beat == BEATS_PER_BLOCK && buffer.is_empty() && latest(blk_txn, txn)
                        }
                    }
            }
            Some(Latched {
                op: Operation::Read,
                count,
                ..
            }) => {
                block < count
                    && beat < BEATS_PER_BLOCK
                    && match engine {
                        Engine::Idle => false,
                        Engine::Issue => full || (buffer.is_empty() && beat == 0),
                        Engine::WaitMedia(txn) => {
                            buffer.is_empty() && beat == 0 && latest(blk_txn, txn)
                        }
                        Engine::WaitBeat(txn) => full && latest(dma_txn, txn),
                    }
            }
        };
        if !engine_ok {
            return Err(invalid(
                "dma controller: engine position not reachable for the command",
            ));
        }
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
        self.block = block;
        self.beat = beat;
        self.buffer = buffer;
        self.dma_txn = TxnId(dma_txn);
        self.blk_txn = TxnId(blk_txn);
        self.irq_level = irq_level;
        Ok(())
    }
}
