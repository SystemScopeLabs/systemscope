//! `DmaBlockController` snapshot state space (`docs/m2-design.md` §9.3–§9.6, §9.9, §13.2;
//! M2.7d): which schema-1 snapshots `restore` accepts, and that a restore never
//! duplicates or loses a wake, a request, or a response.
//!
//! The legality of a snapshot is decided by an oracle written from the frozen text, not
//! from the component: [`MATRIX`] lists every engine position §9.9 allows (the rows of its
//! buffer table, with the index ranges of §9.5 and the outstanding `txn` of §9.6), and
//! [`legality`] adds the lifecycle rules of §9.3, the codes of §9.4, and the IRQ line. It
//! works on fields decoded by this file's own reader. Against it:
//!
//! - a matrix of every combination of command, lifecycle, engine state, block and beat
//!   index, buffer length, and outstanding `txn` around their boundaries;
//! - every state of many mock runs (success, every failure, preflight errors, rejections,
//!   ACK), which must all be legal and must reach every legal position;
//! - each of those states with one invariant perturbed at a time, by category;
//! - a property test over boundary states.
//!
//! In each, `restore` must agree with the oracle, an accepted snapshot must re-encode to
//! the same bytes, and a rejected one must leave the controller unchanged. Then restored
//! positions are continued with a mock RAM and media, and real runtimes (a bus whose RAM
//! region ends inside the DMA aperture, a `SimpleBlockMedia` with a bad block) are
//! checkpointed after every event of READ and WRITE successes, a READ and a WRITE
//! `DMA_FAULT`, and a WRITE `MEDIA_ERROR`, checking which wake, request, or response each
//! checkpoint leaves in the queue and that the restored run delivers it exactly once.

mod common;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use common::{MockCtx, Script, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{
    BlockMsg, BlockReadOutcome, BlockWriteOutcome, MediaError,
};
use systemscope_contracts::protocol::irq_v0;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_platform::dma::{BLK_PORT, DMA_PORT, IRQ_PORT, ISSUE, MEM_PORT};
use systemscope_platform::ram::{RamConfig, RamImage, Segment};
use systemscope_platform::{
    BlockMediaConfig, DmaBlockController, DmaBlockControllerConfig, MediaImage, MultiMasterBus,
    MultiMasterBusConfig, Ram, Region, SimpleBlockMedia,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;

const CLOCK: ClockDomainId = ClockDomainId(3);

/// The frozen values, written out independently of the crate's constants.
const R_COMMAND: u64 = 0x00;
const R_STATUS: u64 = 0x04;
const R_LBA: u64 = 0x08;
const R_MEM_ADDR: u64 = 0x0C;
const R_BLOCK_COUNT: u64 = 0x10;
const R_IRQ_ENABLE: u64 = 0x14;
const R_ACK: u64 = 0x18;

const READ: u32 = 1;
const WRITE: u32 = 2;

const DONE: u32 = 2;
const REJECTED: u32 = 4;

const BLOCK: usize = 512;
const BEAT: usize = 16;

/// 16 controller blocks; a 16 KiB aperture at 0x1000 inside a 64 KiB RAM.
const CAPACITY: u64 = 16;
const BASE: u64 = 0x1000;
const APERTURE: u64 = 0x4000;
const RAM_SIZE: usize = 0x1_0000;

fn config() -> DmaBlockControllerConfig {
    DmaBlockControllerConfig {
        clock: CLOCK,
        latency: LinkLatency::Cycles {
            domain: CLOCK,
            k: 2,
        },
        capacity_blocks: CAPACITY,
        dma_base: BASE,
        dma_size: APERTURE,
    }
}

fn controller() -> DmaBlockController {
    DmaBlockController::new(config()).unwrap()
}

/// The config part of [`config`]'s snapshot, written from the frozen layout.
fn header() -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u8(1);
    w.u32(CLOCK.0);
    w.u64(2);
    w.u64(CAPACITY);
    w.u64(BASE);
    w.u64(APERTURE);
    w.into_bytes()
}

/// A distinctive buffer of `len` bytes.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|k| (k as u8).wrapping_mul(37).wrapping_add(seed) ^ (k >> 8) as u8)
        .collect()
}

/// The RAM before a command: every byte differs from [`media_block`]'s.
fn old_ram() -> Vec<u8> {
    (0..RAM_SIZE)
        .map(|a| ((a * 7 + a / BEAT * 3) % 253) as u8 | 0x80)
        .collect()
}

/// Media block `b` before a command.
fn media_block(b: usize) -> Vec<u8> {
    (0..BLOCK)
        .map(|k| ((b * 31 + k * 5 + k / BEAT * 11) % 127) as u8)
        .collect()
}

fn old_media() -> Vec<Vec<u8>> {
    (0..CAPACITY as usize).map(media_block).collect()
}

// --- Snapshot fields, encoded and decoded from the frozen layout ---

/// Every field of a schema-1 snapshot after the config (§9.9 and the `snapshot` rustdoc
/// order). The `bool`s are raw bytes so that invalid ones can be written; `latched` and
/// `txn` are written exactly when present, whatever the other fields say.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fields {
    /// `LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `IRQ_ENABLE`.
    registers: [u32; 4],
    busy: u8,
    done: u8,
    rejected: u8,
    error: u8,
    /// Operation, `LBA`, `MEM_ADDR`, `BLOCK_COUNT`.
    latched: Option<(u8, u32, u32, u32)>,
    /// 0 `Idle`, 1 `Issue`, 2 `WaitMedia`, 3 `WaitBeat`.
    engine: u8,
    txn: Option<u64>,
    block: u32,
    beat: u8,
    buffer: Vec<u8>,
    dma_txn: u64,
    blk_txn: u64,
    irq: u8,
}

impl Fields {
    fn reset() -> Fields {
        Fields {
            registers: [0; 4],
            busy: 0,
            done: 0,
            rejected: 0,
            error: 0,
            latched: None,
            engine: 0,
            txn: None,
            block: 0,
            beat: 0,
            buffer: Vec::new(),
            dma_txn: 0,
            blk_txn: 0,
            irq: 0,
        }
    }

    /// BUSY with `op` over blocks 2, 3, 4 at 0x1100, the registers since reprogrammed.
    fn busy(op: u8, engine: u8, block: u32, beat: u8, buffer: Vec<u8>, txn: Option<u64>) -> Fields {
        Fields {
            registers: [9, 0x3000, 1, 1],
            busy: 1,
            latched: Some((op, 2, 0x1100, 3)),
            engine,
            txn,
            block,
            beat,
            buffer,
            dma_txn: 500,
            blk_txn: 7,
            ..Fields::reset()
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = SnapshotWriter::new();
        w.raw(&header());
        for r in self.registers {
            w.u32(r);
        }
        w.u8(self.busy);
        w.u8(self.done);
        w.u8(self.rejected);
        w.u8(self.error);
        if let Some((op, lba, addr, count)) = self.latched {
            w.u8(op);
            w.u32(lba);
            w.u32(addr);
            w.u32(count);
        }
        w.u8(self.engine);
        if let Some(txn) = self.txn {
            w.u64(txn);
        }
        w.u32(self.block);
        w.u8(self.beat);
        w.bytes(&self.buffer);
        w.u64(self.dma_txn);
        w.u64(self.blk_txn);
        w.u8(self.irq);
        w.into_bytes()
    }

    /// The fields of a schema-1 snapshot of [`config`], or `None` if `bytes` is not one:
    /// another config, a `bool` or engine tag out of range, a missing field, or trailing
    /// bytes. The latched command is present exactly when BUSY, the `txn` exactly in
    /// `WaitMedia` and `WaitBeat`.
    fn decode(bytes: &[u8]) -> Option<Fields> {
        let header = header();
        let body = bytes.strip_prefix(header.as_slice())?;
        let mut r = SnapshotReader::new(body);
        let mut registers = [0; 4];
        for reg in &mut registers {
            *reg = r.u32().ok()?;
        }
        let busy = u8::from(r.bool().ok()?);
        let done = u8::from(r.bool().ok()?);
        let rejected = u8::from(r.bool().ok()?);
        let error = r.u8().ok()?;
        let latched = if busy == 1 {
            Some((r.u8().ok()?, r.u32().ok()?, r.u32().ok()?, r.u32().ok()?))
        } else {
            None
        };
        let engine = r.u8().ok()?;
        let txn = match engine {
            0 | 1 => None,
            2 | 3 => Some(r.u64().ok()?),
            _ => return None,
        };
        let block = r.u32().ok()?;
        let beat = r.u8().ok()?;
        let buffer = r.bytes().ok()?.to_vec();
        let dma_txn = r.u64().ok()?;
        let blk_txn = r.u64().ok()?;
        let irq = u8::from(r.bool().ok()?);
        r.finish().ok()?;
        Some(Fields {
            registers,
            busy,
            done,
            rejected,
            error,
            latched,
            engine,
            txn,
            block,
            beat,
            buffer,
            dma_txn,
            blk_txn,
            irq,
        })
    }
}

// --- The legality oracle ---

/// A legal engine position, one per row of [`MATRIX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Idle,
    ReadIssueBlock,
    ReadWaitMedia,
    ReadIssueBeat,
    ReadWaitBeat,
    WriteIssueBeat,
    WriteWaitBeat,
    WriteIssueBlock,
    WriteWaitMedia,
}

/// The buffer length an engine position requires.
#[derive(Clone, Copy, Debug)]
enum Buf {
    Empty,
    Full,
    /// 16 × *j*: the beats already read.
    Beats,
}

/// Where the outstanding request went, whose counter its `txn` must be the latest of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Port {
    None,
    Dma,
    Blk,
}

struct Row {
    family: Family,
    /// The latched operation, 0 for none (not BUSY).
    op: u8,
    engine: u8,
    /// The beat indices *j* allowed, inclusive.
    beats: (u8, u8),
    buffer: Buf,
    port: Port,
    why: &'static str,
}

/// Every engine position a run can snapshot. Anything else is rejected.
const MATRIX: [Row; 9] = [
    Row {
        family: Family::Idle,
        op: 0,
        engine: 0,
        beats: (0, 0),
        buffer: Buf::Empty,
        port: Port::None,
        why: "§9.9: Idle stores no buffer; reset, DONE, and after ACK",
    },
    Row {
        family: Family::ReadIssueBlock,
        op: 1,
        engine: 1,
        beats: (0, 0),
        buffer: Buf::Empty,
        port: Port::None,
        why: "§9.9 row 1: waiting to issue ReadBlock of block i, no buffer",
    },
    Row {
        family: Family::ReadWaitMedia,
        op: 1,
        engine: 2,
        beats: (0, 0),
        buffer: Buf::Empty,
        port: Port::Blk,
        why: "§9.9 row 1: ReadBlock outstanding, no buffer",
    },
    Row {
        family: Family::ReadIssueBeat,
        op: 1,
        engine: 1,
        beats: (0, 31),
        buffer: Buf::Full,
        port: Port::None,
        why: "§9.9 row 2: Data accepted, issuing beat j, all 512 bytes",
    },
    Row {
        family: Family::ReadWaitBeat,
        op: 1,
        engine: 3,
        beats: (0, 31),
        buffer: Buf::Full,
        port: Port::Dma,
        why: "§9.9 row 2: beat j outstanding up to beat 31's Done, all 512 bytes",
    },
    Row {
        family: Family::WriteIssueBeat,
        op: 2,
        engine: 1,
        beats: (0, 31),
        buffer: Buf::Beats,
        port: Port::None,
        why: "§9.9 row 3: issuing beat j, the 16j bytes read",
    },
    Row {
        family: Family::WriteWaitBeat,
        op: 2,
        engine: 3,
        beats: (0, 31),
        buffer: Buf::Beats,
        port: Port::Dma,
        why: "§9.9 row 3: beat j outstanding, the 16j bytes read",
    },
    Row {
        family: Family::WriteIssueBlock,
        op: 2,
        engine: 1,
        beats: (32, 32),
        buffer: Buf::Full,
        port: Port::None,
        why: "§9.9 row 4: all 32 beats read, issuing WriteBlock (j = 32), 512 bytes",
    },
    Row {
        family: Family::WriteWaitMedia,
        op: 2,
        engine: 2,
        beats: (32, 32),
        buffer: Buf::Empty,
        port: Port::Blk,
        why: "§9.9 row 4: WriteBlock outstanding carries the data (j = 32), no buffer",
    },
];

/// Why a snapshot is not one a run can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    /// Not a schema-1 encoding of this controller (§9.9, the `snapshot` layout).
    Encoding,
    /// `IRQ_ENABLE` bits other than bit 0 (§9.2: they read 0 and writes ignore them).
    IrqEnableBits,
    /// BUSY with DONE (§9.3 table, §9.9).
    BusyAndDone,
    /// `ERROR` other than 0 outside DONE (§9.3 table).
    ErrorOutsideDone,
    /// `ERROR` 8–255 (§9.4: unused).
    UnusedError,
    /// A latched command that fails validation (§9.4: only a valid command is latched).
    LatchedInvalid,
    /// An engine state other than `Idle` without BUSY, or `Idle` with it (§9.6, §9.9).
    EngineState,
    /// A beat index outside its engine position's range (§9.5, §9.9).
    BeatIndex,
    /// A block index at or past the latched count, or not 0 in `Idle` (§9.9).
    BlockIndex,
    /// A buffer length other than the engine position requires (§9.9 table).
    BufferLength,
    /// An outstanding `txn` that is not the latest issued on its port (§9.5, §9.9).
    Txn,
    /// An IRQ level other than `DONE && IRQ_ENABLE.bit0` (§9.3, §9.9).
    IrqLevel,
}

/// §9.4 against [`config`], in `u64`.
fn preflight(op: u8, lba: u32, addr: u32, count: u32) -> bool {
    let (lba, addr, count) = (u64::from(lba), u64::from(addr), u64::from(count));
    (op == 1 || op == 2)
        && count != 0
        && lba + count <= CAPACITY
        && addr % 16 == 0
        && addr >= BASE
        && addr + count * 512 <= BASE + APERTURE
}

/// The frozen verdict on decoded fields: the engine position, or why no run produces
/// them.
fn legality(f: &Fields) -> Result<Family, Reason> {
    let enable = f.registers[3];
    let (busy, done) = (f.busy == 1, f.done == 1);
    if enable > 1 {
        return Err(Reason::IrqEnableBits);
    }
    if busy && done {
        return Err(Reason::BusyAndDone);
    }
    if f.error > 7 {
        return Err(Reason::UnusedError);
    }
    if !done && f.error != 0 {
        return Err(Reason::ErrorOutsideDone);
    }
    if f.irq != u8::from(done && enable == 1) {
        return Err(Reason::IrqLevel);
    }
    let (op, count) = match f.latched {
        None => (0, 0),
        Some((op, lba, addr, count)) if preflight(op, lba, addr, count) => (op, count),
        Some(_) => return Err(Reason::LatchedInvalid),
    };
    let rows: Vec<&Row> = MATRIX
        .iter()
        .filter(|r| r.op == op && r.engine == f.engine)
        .collect();
    if rows.is_empty() {
        return Err(Reason::EngineState);
    }
    let rows: Vec<&Row> = rows
        .into_iter()
        .filter(|r| (r.beats.0..=r.beats.1).contains(&f.beat))
        .collect();
    if rows.is_empty() {
        return Err(Reason::BeatIndex);
    }
    let row = rows
        .into_iter()
        .find(|r| {
            f.buffer.len()
                == match r.buffer {
                    Buf::Empty => 0,
                    Buf::Full => BLOCK,
                    Buf::Beats => BEAT * usize::from(f.beat),
                }
        })
        .ok_or(Reason::BufferLength)?;
    if !(if op == 0 {
        f.block == 0
    } else {
        f.block < count
    }) {
        return Err(Reason::BlockIndex);
    }
    let latest = |next: u64| next.checked_sub(1);
    let txn_ok = match row.port {
        Port::None => f.txn.is_none(),
        Port::Dma => f.txn.is_some() && f.txn == latest(f.dma_txn),
        Port::Blk => f.txn.is_some() && f.txn == latest(f.blk_txn),
    };
    if !txn_ok {
        return Err(Reason::Txn);
    }
    Ok(row.family)
}

fn verdict(bytes: &[u8]) -> Result<Family, Reason> {
    Fields::decode(bytes).map_or(Err(Reason::Encoding), |f| legality(&f))
}

/// Every legal (position, beat index) pair.
fn legal_positions() -> BTreeSet<(Family, u8)> {
    MATRIX
        .iter()
        .flat_map(|r| (r.beats.0..=r.beats.1).map(move |j| (r.family, j)))
        .collect()
}

/// Controllers to restore into, each in a known state: `restore` must agree with the
/// oracle, re-encode what it accepts byte for byte, and leave the controller exactly as
/// it was when it rejects.
struct Targets(Vec<(DmaBlockController, Vec<u8>)>);

impl Targets {
    fn new() -> Targets {
        let states = [
            Fields::reset(),
            Fields {
                dma_txn: 41,
                blk_txn: 1,
                ..Fields::busy(2, 3, 1, 7, pattern(112, 5), Some(40))
            },
            Fields {
                registers: [1, 2, 3, 1],
                done: 1,
                error: 6,
                rejected: 1,
                irq: 1,
                dma_txn: 77,
                blk_txn: 3,
                ..Fields::reset()
            },
        ];
        Targets(
            states
                .iter()
                .map(|f| {
                    let bytes = f.encode();
                    let mut c = controller();
                    restore_into(&mut c, &bytes).unwrap();
                    (c, bytes)
                })
                .collect(),
        )
    }

    /// Restores `bytes` into the first two targets (or all of them with `all`) through
    /// `Component::restore` itself. An accepted snapshot must re-encode to the same bytes.
    /// A rejection must leave the controller unchanged, since the controller validates
    /// everything before assigning. Trailing bytes are the one case `restore` cannot see:
    /// it reads a complete snapshot and returns, and the outer reader
    /// (`common::restore_into`, the runtime) rejects the rest and discards the session.
    /// There the oracle must say `Encoding` and the prefix read must be legal itself.
    fn check(&mut self, bytes: &[u8], all: bool) -> Result<Family, Reason> {
        let expected = verdict(bytes);
        let n = if all { self.0.len() } else { 2 };
        for (c, own) in self.0.iter_mut().take(n) {
            let view = c.inspect();
            let mut r = SnapshotReader::new(bytes);
            let restored = c.restore(&mut r, 1);
            let left = r.remaining();
            match (expected, restored) {
                (Ok(_), Ok(())) if left == 0 => {
                    assert_eq!(
                        snapshot_of(c),
                        bytes,
                        "accepted, then re-encoded differently"
                    );
                }
                (Err(Reason::Encoding), Ok(())) if left > 0 => {
                    let prefix = &bytes[..bytes.len() - left];
                    assert!(
                        verdict(prefix).is_ok(),
                        "restore accepted an illegal prefix"
                    );
                    assert_eq!(snapshot_of(c), prefix);
                }
                (Err(_), Err(_)) => {
                    assert_eq!(snapshot_of(c), *own, "a rejected restore changed the state");
                    assert_eq!(c.inspect(), view);
                }
                (e, r) => panic!(
                    "oracle {e:?}, restore {r:?} with {left} bytes left, for {:?}",
                    Fields::decode(bytes)
                ),
            }
            restore_into(c, own).unwrap();
        }
        expected
    }
}

// --- A scripted RAM and media around the controller ---

/// Where the scripted RAM or media fails, by block *i* of the command and beat *j*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Media(u32),
    Beat(u32, u8),
}

/// One engine request, as the controller sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Req {
    ReadBlock { txn: u64, lba: u64 },
    WriteBlock { txn: u64, lba: u64, data: Vec<u8> },
    Store { txn: u64, addr: u64, data: Vec<u8> },
    Load { txn: u64, addr: u64 },
}

/// The controller with a RAM and media that answer its one outstanding request, the one
/// pending wake the runtime's queue would hold, and the snapshot after every event.
struct World {
    c: DmaBlockController,
    ram: Vec<u8>,
    media: Vec<Vec<u8>>,
    fault: Fault,
    /// The command's LBA and address, to place each request.
    cmd: (u64, u64),
    wake: bool,
    outstanding: Option<Req>,
    log: Vec<Req>,
    levels: Vec<bool>,
    seen: Vec<Vec<u8>>,
}

impl World {
    fn new(fault: Fault) -> World {
        World::around(controller(), fault)
    }

    fn around(c: DmaBlockController, fault: Fault) -> World {
        let mut w = World {
            c,
            ram: old_ram(),
            media: old_media(),
            fault,
            cmd: (0, 0),
            wake: false,
            outstanding: None,
            log: Vec::new(),
            levels: Vec::new(),
            seen: Vec::new(),
        };
        w.seen.push(snapshot_of(&w.c));
        w
    }

    /// A controller restored from `f`, with the wake or request its position has pending:
    /// a WRITE's outstanding `WriteBlock` carries `write_block` (the runtime's copy of the
    /// data).
    fn resumed(f: &Fields, write_block: Vec<u8>) -> World {
        let mut c = controller();
        restore_into(&mut c, &f.encode()).unwrap();
        let mut w = World::around(c, Fault::None);
        let (op, lba, addr, _) = f.latched.unwrap();
        w.cmd = (lba.into(), addr.into());
        let beat_addr = u64::from(addr) + 512 * u64::from(f.block) + 16 * u64::from(f.beat);
        let block_lba = u64::from(lba) + u64::from(f.block);
        let j = BEAT * usize::from(f.beat);
        w.wake = f.engine == 1;
        w.outstanding = match (op, f.engine, f.txn) {
            (_, 1, None) => None,
            (1, 2, Some(txn)) => Some(Req::ReadBlock {
                txn,
                lba: block_lba,
            }),
            (1, 3, Some(txn)) => Some(Req::Store {
                txn,
                addr: beat_addr,
                data: f.buffer[j..j + BEAT].to_vec(),
            }),
            (2, 2, Some(txn)) => Some(Req::WriteBlock {
                txn,
                lba: block_lba,
                data: write_block,
            }),
            (2, 3, Some(txn)) => Some(Req::Load {
                txn,
                addr: beat_addr,
            }),
            other => panic!("{other:?}"),
        };
        w
    }

    fn absorb(&mut self, ctx: &MockCtx) {
        for wake in &ctx.wakes {
            assert_eq!(wake.token, ISSUE);
            assert!(
                !self.wake && self.outstanding.is_none(),
                "a second pending item"
            );
            self.wake = true;
        }
        for i in &ctx.irqs {
            assert_eq!(i.port, IRQ_PORT);
            self.levels.push(i.asserted);
        }
        self.seen.push(snapshot_of(&self.c));
    }

    fn write_reg(&mut self, offset: u64, value: u32) {
        let mut ctx = MockCtx::new(Phase::Transfer);
        ctx.deliver(
            &mut self.c,
            MEM_PORT,
            write(9, offset, &value.to_le_bytes()),
        )
        .unwrap();
        ctx.take_one();
        self.absorb(&ctx);
    }

    fn status(&mut self) -> u32 {
        let mut ctx = MockCtx::new(Phase::Transfer);
        ctx.deliver(&mut self.c, MEM_PORT, read(9, R_STATUS, 4))
            .unwrap();
        let MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } = ctx.take_one().msg
        else {
            panic!("STATUS faulted")
        };
        u32::from_le_bytes(data.try_into().unwrap())
    }

    fn command(&mut self, command: u32, lba: u32, addr: u32, count: u32) {
        self.cmd = (lba.into(), addr.into());
        for (offset, value) in [
            (R_LBA, lba),
            (R_MEM_ADDR, addr),
            (R_BLOCK_COUNT, count),
            (R_COMMAND, command),
        ] {
            self.write_reg(offset, value);
        }
    }

    fn issue(&mut self) -> Result<(), SimError> {
        let mut ctx = MockCtx::new(Phase::Request);
        self.c
            .handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx)?;
        self.wake = false;
        assert_eq!(ctx.order.len(), 1, "one request per wake");
        let req = if let Some(b) = ctx.blocks.pop() {
            assert_eq!(
                (b.port, b.when, b.phase),
                (BLK_PORT, ScheduleWhen::Now, Phase::Request)
            );
            match b.msg {
                BlockMsg::ReadBlock { txn, lba } => Req::ReadBlock { txn: txn.0, lba },
                BlockMsg::WriteBlock { txn, lba, data } => Req::WriteBlock {
                    txn: txn.0,
                    lba,
                    data,
                },
                other => panic!("{other:?}"),
            }
        } else {
            let s = ctx.sent.pop().unwrap();
            assert_eq!(
                (s.port, s.when, s.phase),
                (DMA_PORT, ScheduleWhen::Now, Phase::Request)
            );
            match s.msg {
                MemMsg::WriteReq { txn, addr, data } => Req::Store {
                    txn: txn.0,
                    addr,
                    data,
                },
                MemMsg::ReadReq { txn, addr, len } => {
                    assert_eq!(len, 16);
                    Req::Load { txn: txn.0, addr }
                }
                other => panic!("{other:?}"),
            }
        };
        self.log.push(req.clone());
        self.outstanding = Some(req);
        self.absorb(&ctx);
        Ok(())
    }

    /// Block *i* and beat *j* of a beat address within the command.
    fn position(&self, addr: u64) -> (u32, u8) {
        let delta = addr - self.cmd.1;
        ((delta / 512) as u32, (delta % 512 / 16) as u8)
    }

    fn result_of(&mut self, req: &Req) -> (systemscope_contracts::component::PortId, Message) {
        match req {
            Req::ReadBlock { txn, lba } => {
                let outcome = if self.fault == Fault::Media((lba - self.cmd.0) as u32) {
                    BlockReadOutcome::Error {
                        error: MediaError::BadBlock,
                    }
                } else {
                    BlockReadOutcome::Data {
                        data: self.media[*lba as usize].clone(),
                    }
                };
                let txn = TxnId(*txn);
                (BLK_PORT, BlockMsg::ReadResult { txn, outcome }.into())
            }
            Req::WriteBlock { txn, lba, data } => {
                let outcome = if self.fault == Fault::Media((lba - self.cmd.0) as u32) {
                    BlockWriteOutcome::Error {
                        error: MediaError::BadBlock,
                    }
                } else {
                    self.media[*lba as usize] = data.clone();
                    BlockWriteOutcome::Done
                };
                let txn = TxnId(*txn);
                (BLK_PORT, BlockMsg::WriteResult { txn, outcome }.into())
            }
            Req::Store { txn, addr, data } => {
                let (i, j) = self.position(*addr);
                let outcome = if self.fault == Fault::Beat(i, j) {
                    WriteOutcome::Fault {
                        fault: MemFault::AccessFault,
                    }
                } else {
                    self.ram[*addr as usize..][..data.len()].copy_from_slice(data);
                    WriteOutcome::Done
                };
                let txn = TxnId(*txn);
                (DMA_PORT, MemMsg::WriteResp { txn, outcome }.into())
            }
            Req::Load { txn, addr } => {
                let (i, j) = self.position(*addr);
                let outcome = if self.fault == Fault::Beat(i, j) {
                    ReadOutcome::Fault {
                        fault: MemFault::AccessFault,
                    }
                } else {
                    ReadOutcome::Data {
                        data: self.ram[*addr as usize..][..BEAT].to_vec(),
                    }
                };
                let txn = TxnId(*txn);
                (DMA_PORT, MemMsg::ReadResp { txn, outcome }.into())
            }
        }
    }

    fn respond(&mut self) {
        let req = self.outstanding.take().unwrap();
        let (port, msg) = self.result_of(&req);
        let mut ctx = MockCtx::new(Phase::Complete);
        ctx.deliver_msg(&mut self.c, port, msg).unwrap();
        assert!(ctx.sent.is_empty() && ctx.blocks.is_empty());
        self.absorb(&ctx);
    }

    fn step(&mut self) -> bool {
        if self.wake {
            self.issue().unwrap();
        } else if self.outstanding.is_some() {
            self.respond();
        } else {
            return false;
        }
        true
    }

    fn run(&mut self) {
        while self.step() {}
    }

    fn fields(&self) -> Fields {
        Fields::decode(&snapshot_of(&self.c)).unwrap()
    }
}

/// Snapshots after every event of runs covering every position, lifecycle state, code,
/// and `REJECTED` combination: READ and WRITE of blocks 2, 3, 4 at 0x1100, succeeding
/// or failing at the first, middle, and last block, with the interrupt off and on and
/// with a rejected command mid-transfer; each preflight code; and ACK after each.
fn reachable() -> Vec<Vec<u8>> {
    let mut seen = Vec::new();
    for irq in [false, true] {
        for reject in [false, true] {
            for (op, fault) in [
                (READ, Fault::None),
                (WRITE, Fault::None),
                (READ, Fault::Media(0)),
                (READ, Fault::Beat(1, 15)),
                (READ, Fault::Media(1)),
                (READ, Fault::Beat(2, 31)),
                (WRITE, Fault::Beat(0, 0)),
                (WRITE, Fault::Beat(1, 15)),
                (WRITE, Fault::Media(1)),
                (WRITE, Fault::Media(2)),
            ] {
                let mut w = World::new(fault);
                w.write_reg(R_IRQ_ENABLE, u32::from(irq));
                w.command(op, 2, 0x1100, 3);
                if reject {
                    w.step();
                    w.write_reg(R_COMMAND, op);
                }
                w.run();
                w.write_reg(R_ACK, 1);
                seen.extend(w.seen);
            }
            // Preflight codes 1–5, then a rejection in DONE, then ACK.
            for (op, lba, addr, count) in [
                (3, 0, 0x1000, 1),
                (READ, 0, 0x1000, 0),
                (WRITE, 15, 0x1000, 2),
                (READ, 0, 0x1008, 1),
                (WRITE, 0, 0x0f00, 1),
            ] {
                let mut w = World::new(Fault::None);
                w.write_reg(R_IRQ_ENABLE, u32::from(irq));
                w.command(op, lba, addr, count);
                if reject {
                    w.write_reg(R_COMMAND, READ);
                }
                w.write_reg(R_ACK, 1);
                w.write_reg(R_IRQ_ENABLE, 0);
                seen.extend(w.seen);
            }
        }
    }
    seen
}

// --- The legality matrix ---

#[test]
fn the_matrix_rows_are_the_frozen_buffer_table() {
    // One row per §9.9 table entry and engine state, with its reason; the legal (row,
    // j) pairs: Idle, READ before and waiting for ReadBlock, READ issuing and waiting for
    // beats 0..=31, WRITE issuing and waiting for beats 0..=31, WRITE's WriteBlock at 32.
    assert!(MATRIX.iter().all(|r| !r.why.is_empty()));
    assert_eq!(
        legal_positions().len(),
        1 + 1 + 1 + 32 + 32 + 32 + 32 + 1 + 1
    );
    let lens: Vec<(Family, usize)> = [
        (Family::ReadWaitMedia, 0),
        (Family::ReadWaitBeat, 31),
        (Family::WriteWaitBeat, 7),
        (Family::WriteIssueBlock, 32),
        (Family::WriteWaitMedia, 32),
    ]
    .iter()
    .map(|&(family, j)| {
        let row = MATRIX.iter().find(|r| r.family == family).unwrap();
        let len = match row.buffer {
            Buf::Empty => 0,
            Buf::Full => 512,
            Buf::Beats => 16 * j,
        };
        (family, len)
    })
    .collect();
    assert_eq!(
        lens,
        [
            (Family::ReadWaitMedia, 0),
            (Family::ReadWaitBeat, 512),
            (Family::WriteWaitBeat, 112),
            (Family::WriteIssueBlock, 512),
            (Family::WriteWaitMedia, 0),
        ]
    );
}

#[test]
fn restore_agrees_with_the_matrix_over_every_engine_combination() {
    let mut t = Targets::new();
    let mut verdicts: BTreeMap<Result<Family, Reason>, usize> = BTreeMap::new();
    let mut valid = BTreeSet::new();
    for op in [1u8, 2] {
        for engine in 0u8..=3 {
            let txns: Vec<Option<u64>> = match engine {
                // The latest dma, the latest blk, stale, future.
                2 | 3 => vec![Some(499), Some(6), Some(498), Some(500), Some(5), Some(7)],
                _ => vec![None],
            };
            for block in 0u32..=4 {
                for beat in [0u8, 1, 15, 16, 30, 31, 32, 33, 255] {
                    let j16 = 16 * usize::from(beat);
                    let mut lens: BTreeSet<usize> =
                        [0, 16, 496, 512, 528, j16, j16 + 16].into_iter().collect();
                    if j16 >= 16 {
                        lens.insert(j16 - 16);
                    }
                    for &len in &lens {
                        for &txn in &txns {
                            for (rejected, enable) in [(0, 0), (1, 1)] {
                                let f = Fields {
                                    rejected,
                                    registers: [9, 0x3000, 1, enable],
                                    ..Fields::busy(op, engine, block, beat, pattern(len, 1), txn)
                                };
                                let v = t.check(&f.encode(), false);
                                if let Ok(family) = v {
                                    valid.insert((family, beat, block));
                                }
                                *verdicts.entry(v).or_default() += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    // Every busy position at the first, middle, and last block, each with its full j
    // range; and nothing else.
    let expected: BTreeSet<(Family, u8, u32)> = legal_positions()
        .into_iter()
        .filter(|(f, _)| *f != Family::Idle)
        .flat_map(|(f, j)| (0..3).map(move |i| (f, j, i)))
        .filter(|(_, j, _)| [0, 1, 15, 16, 30, 31, 32].contains(j))
        .collect();
    assert_eq!(valid, expected);
    for reason in [
        Reason::EngineState,
        Reason::BeatIndex,
        Reason::BlockIndex,
        Reason::BufferLength,
        Reason::Txn,
    ] {
        assert!(
            verdicts.contains_key(&Err(reason)),
            "{reason:?} never exercised"
        );
    }
}

#[test]
fn restore_agrees_with_the_matrix_over_every_lifecycle_combination() {
    let mut t = Targets::new();
    let mut verdicts: BTreeMap<Result<Family, Reason>, usize> = BTreeMap::new();
    for busy in [0u8, 1] {
        for done in [0u8, 1] {
            for error in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 255] {
                for rejected in [0u8, 1] {
                    for enable in [0u32, 1, 2, 3] {
                        for irq in [0u8, 1] {
                            let base = if busy == 1 {
                                Fields::busy(1, 1, 0, 0, vec![], None)
                            } else {
                                Fields::reset()
                            };
                            let f = Fields {
                                busy,
                                done,
                                error,
                                rejected,
                                irq,
                                registers: [5, 0x2000, 2, enable],
                                ..base
                            };
                            let v = t.check(&f.encode(), true);
                            *verdicts.entry(v).or_default() += 1;
                        }
                    }
                }
            }
        }
    }
    // Legal: IDLE (ERROR 0), BUSY (ERROR 0), DONE with 0–7, each with REJECTED 0 or 1,
    // IRQ_ENABLE 0 or 1, and the level that follows: 2 × 2 + 2 × 2 + 8 × 2 × 2.
    assert_eq!(verdicts.get(&Ok(Family::Idle)), Some(&(4 + 32)));
    assert_eq!(verdicts.get(&Ok(Family::ReadIssueBlock)), Some(&4));
    for reason in [
        Reason::IrqEnableBits,
        Reason::BusyAndDone,
        Reason::ErrorOutsideDone,
        Reason::UnusedError,
        Reason::IrqLevel,
    ] {
        assert!(
            verdicts.contains_key(&Err(reason)),
            "{reason:?} never exercised"
        );
    }
}

#[test]
fn restore_agrees_with_the_matrix_over_idle_positions_and_descriptors() {
    let mut t = Targets::new();
    // Not BUSY: only Idle at i = j = 0 with no buffer, whatever DONE and the counters.
    for done in [0u8, 1] {
        for engine in 0u8..=4 {
            for (block, beat, len) in [
                (0, 0, 0),
                (1, 0, 0),
                (0, 1, 0),
                (0, 32, 0),
                (0, 0, 16),
                (0, 0, 512),
            ] {
                let txn = matches!(engine, 2 | 3).then_some(4);
                let f = Fields {
                    done,
                    irq: done,
                    registers: [0, 0, 0, 1],
                    engine,
                    txn,
                    block,
                    beat,
                    buffer: pattern(len, 2),
                    dma_txn: 5,
                    blk_txn: 5,
                    ..Fields::reset()
                };
                let v = t.check(&f.encode(), true);
                assert_eq!(
                    v.is_ok(),
                    engine == 0 && (block, beat, len) == (0, 0, 0),
                    "{f:?}"
                );
            }
        }
    }
    // Latched commands: only those that pass §9.4 against this config.
    for (latched, ok) in [
        ((1, 2, 0x1100, 3), true),
        ((2, 13, 0x4400, 3), true),
        ((1, 0, 0x1000, 16), true),
        ((1, 1, 0x1000, 16), false),
        ((2, 0, 0x1000, 32), false),
        ((0, 2, 0x1100, 3), false),
        ((3, 2, 0x1100, 3), false),
        ((1, 2, 0x1100, 0), false),
        ((1, 14, 0x1100, 3), false),
        ((2, 2, 0x1108, 3), false),
        ((1, 2, 0x0ff0, 3), false),
        ((2, 2, 0x4c00, 3), false),
    ] {
        let f = Fields {
            latched: Some(latched),
            ..Fields::busy(1, 1, 0, 0, vec![], None)
        };
        assert_eq!(t.check(&f.encode(), true).is_ok(), ok, "{latched:?}");
    }
    // BUSY without a descriptor, and a descriptor without BUSY, do not decode.
    let f = Fields {
        latched: None,
        ..Fields::busy(1, 1, 0, 0, vec![], None)
    };
    assert_eq!(t.check(&f.encode(), true), Err(Reason::Encoding));
    let f = Fields {
        latched: Some((1, 2, 0x1100, 3)),
        ..Fields::reset()
    };
    assert_eq!(t.check(&f.encode(), true), Err(Reason::Encoding));
}

// --- Reachable states ---

#[test]
fn every_reachable_state_is_legal_and_every_legal_position_is_reached() {
    let mut t = Targets::new();
    let mut positions = BTreeSet::new();
    let mut blocks = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    let unique: BTreeSet<Vec<u8>> = reachable().into_iter().collect();
    for bytes in &unique {
        let family = t
            .check(bytes, false)
            .unwrap_or_else(|r| panic!("unreachable by {r:?}"));
        let f = Fields::decode(bytes).unwrap();
        positions.insert((family, f.beat));
        blocks.insert((family, f.block));
        if f.busy == 0 {
            terminal.insert((f.done, f.error, f.rejected, f.registers[3]));
        }
    }
    assert_eq!(
        positions,
        legal_positions(),
        "a legal position no run reaches"
    );
    for row in &MATRIX[1..] {
        for i in 0..3 {
            assert!(
                blocks.contains(&(row.family, i)),
                "{:?} at block {i}",
                row.family
            );
        }
    }
    // DONE with every code, with and without REJECTED and the interrupt; IDLE after ACK
    // with and without REJECTED.
    for error in 0..=7 {
        for rejected in [0, 1] {
            for enable in [0, 1] {
                assert!(
                    terminal.contains(&(1, error, rejected, enable)),
                    "{error} {rejected} {enable}"
                );
            }
        }
    }
    assert!(terminal.contains(&(0, 0, 0, 0)) && terminal.contains(&(0, 0, 1, 0)));
}

/// One perturbation category of the invalid-state generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Buffer,
    Block,
    Beat,
    Engine,
    Command,
    Descriptor,
    OutstandingTxn,
    Counter,
    Status,
    Irq,
    Error,
}

/// `f` with one invariant perturbed, for every category.
fn perturb(f: &Fields) -> Vec<(Category, Fields)> {
    let mut out = Vec::new();
    let mut push = |cat, g: Fields| {
        if g != *f {
            out.push((cat, g));
        }
    };
    let len = f.buffer.len();
    for l in [
        0,
        16,
        496,
        512,
        528,
        len + 16,
        len + 1,
        len.saturating_sub(16),
        len.saturating_sub(1),
    ] {
        push(
            Category::Buffer,
            Fields {
                buffer: pattern(l, 3),
                ..f.clone()
            },
        );
    }
    let count = f.latched.map_or(0, |l| l.3);
    for b in [
        count,
        count + 1,
        u32::MAX,
        f.block + 1,
        f.block.wrapping_sub(1),
    ] {
        push(
            Category::Block,
            Fields {
                block: b,
                ..f.clone()
            },
        );
    }
    for j in [
        f.beat.wrapping_add(1),
        f.beat.wrapping_sub(1),
        0,
        31,
        32,
        33,
        255,
    ] {
        push(
            Category::Beat,
            Fields {
                beat: j,
                ..f.clone()
            },
        );
    }
    let op = f.latched.map_or(0, |l| l.0);
    for e in 0u8..=4 {
        // Only the engine changes: a `txn` where the new state stores one, the latest
        // of the port that state would use.
        let txn = match (op, e) {
            (1, 2) | (2, 2) => f.blk_txn.checked_sub(1),
            (_, 3) => f.dma_txn.checked_sub(1),
            (0, 2) => Some(0),
            _ => None,
        };
        push(
            Category::Engine,
            Fields {
                engine: e,
                txn,
                ..f.clone()
            },
        );
    }
    if let Some((op, lba, addr, count)) = f.latched {
        for l in [
            (3 - op, lba, addr, count),
            (0, lba, addr, count),
            (3, lba, addr, count),
            (op, lba, addr, 0),
            (op, 15, addr, count),
            (op, lba, addr + 8, count),
            (op, lba, 0x0ff0, count),
            (op, lba, 0x4e00, count),
        ] {
            push(
                Category::Command,
                Fields {
                    latched: Some(l),
                    ..f.clone()
                },
            );
        }
        push(
            Category::Descriptor,
            Fields {
                latched: None,
                ..f.clone()
            },
        );
        push(
            Category::Descriptor,
            Fields {
                busy: 0,
                ..f.clone()
            },
        );
    } else {
        push(
            Category::Descriptor,
            Fields {
                latched: Some((1, 2, 0x1100, 3)),
                ..f.clone()
            },
        );
        push(
            Category::Descriptor,
            Fields {
                busy: 1,
                ..f.clone()
            },
        );
    }
    match f.txn {
        Some(t) => {
            for other in [
                t.wrapping_sub(1),
                t.wrapping_add(1),
                f.dma_txn.wrapping_sub(1),
                f.blk_txn.wrapping_sub(1),
                0,
                u64::MAX,
            ] {
                push(
                    Category::OutstandingTxn,
                    Fields {
                        txn: Some(other),
                        ..f.clone()
                    },
                );
            }
            push(
                Category::OutstandingTxn,
                Fields {
                    txn: None,
                    ..f.clone()
                },
            );
        }
        None => {
            push(
                Category::OutstandingTxn,
                Fields {
                    txn: f.dma_txn.checked_sub(1).or(Some(0)),
                    ..f.clone()
                },
            );
        }
    }
    for (d, b) in [
        (f.dma_txn.wrapping_add(1), f.blk_txn),
        (f.dma_txn.wrapping_sub(1), f.blk_txn),
        (0, f.blk_txn),
        (f.dma_txn, f.blk_txn.wrapping_add(1)),
        (f.dma_txn, f.blk_txn.wrapping_sub(1)),
        (f.dma_txn, 0),
    ] {
        push(
            Category::Counter,
            Fields {
                dma_txn: d,
                blk_txn: b,
                ..f.clone()
            },
        );
    }
    push(
        Category::Status,
        Fields {
            busy: 1,
            done: 1,
            ..f.clone()
        },
    );
    push(
        Category::Status,
        Fields {
            done: 1 - f.done.min(1),
            ..f.clone()
        },
    );
    push(
        Category::Status,
        Fields {
            done: 2,
            ..f.clone()
        },
    );
    push(
        Category::Status,
        Fields {
            rejected: 2,
            ..f.clone()
        },
    );
    push(
        Category::Status,
        Fields {
            busy: 2,
            ..f.clone()
        },
    );
    push(
        Category::Irq,
        Fields {
            irq: 1 - f.irq.min(1),
            ..f.clone()
        },
    );
    push(
        Category::Irq,
        Fields {
            irq: 2,
            ..f.clone()
        },
    );
    for enable in [2, 3, 0x8000_0001, 1 - f.registers[3].min(1)] {
        let mut registers = f.registers;
        registers[3] = enable;
        push(
            Category::Irq,
            Fields {
                registers,
                ..f.clone()
            },
        );
    }
    for error in [8, 255, 6, 7, 1, 0, f.error.wrapping_add(1)] {
        push(Category::Error, Fields { error, ..f.clone() });
    }
    out
}

#[test]
fn every_single_perturbation_of_a_reachable_state_is_judged_like_the_oracle() {
    let mut t = Targets::new();
    let mut invalid: BTreeMap<Category, usize> = BTreeMap::new();
    let mut total = 0;
    let unique: BTreeSet<Vec<u8>> = reachable().into_iter().collect();
    for bytes in &unique {
        let f = Fields::decode(bytes).unwrap();
        for (cat, g) in perturb(&f) {
            total += 1;
            if t.check(&g.encode(), true).is_err() {
                *invalid.entry(cat).or_default() += 1;
            }
        }
    }
    for cat in [
        Category::Buffer,
        Category::Block,
        Category::Beat,
        Category::Engine,
        Category::Command,
        Category::Descriptor,
        Category::OutstandingTxn,
        Category::Counter,
        Category::Status,
        Category::Irq,
        Category::Error,
    ] {
        assert!(
            invalid.get(&cat).copied().unwrap_or(0) > 0,
            "{cat:?} never rejected"
        );
    }
    let rejected: usize = invalid.values().sum();
    assert!(
        rejected > 10_000 && total > rejected,
        "{rejected} of {total}"
    );
}

// --- Canonical states ---

#[test]
fn terminal_states_are_canonical() {
    // Reset.
    assert_eq!(
        Fields::decode(&snapshot_of(&controller())).unwrap(),
        Fields::reset()
    );
    let idle = |f: &Fields| {
        (
            f.busy,
            f.latched,
            f.engine,
            f.txn,
            f.block,
            f.beat,
            f.buffer.len(),
        )
    };
    // Success, and each execution error, from every position; then ACK.
    for (op, fault, error, dma, blk) in [
        (READ, Fault::None, 0, 96, 3),
        (WRITE, Fault::None, 0, 96, 3),
        (READ, Fault::Beat(1, 15), 6, 48, 2),
        (READ, Fault::Media(2), 7, 64, 3),
        (WRITE, Fault::Beat(2, 31), 6, 96, 2),
        (WRITE, Fault::Media(0), 7, 32, 1),
    ] {
        let mut w = World::new(fault);
        w.write_reg(R_IRQ_ENABLE, 1);
        w.command(op, 2, 0x1100, 3);
        w.run();
        let f = w.fields();
        assert_eq!(idle(&f), (0, None, 0, None, 0, 0, 0), "{op} {fault:?}");
        assert_eq!((f.done, f.error, f.irq), (1, error, 1));
        // The counters include every request issued, the failing one too.
        assert_eq!((f.dma_txn, f.blk_txn), (dma, blk), "{op} {fault:?}");
        assert_eq!(f.registers, [2, 0x1100, 3, 1]);
        w.write_reg(R_ACK, 1);
        let g = w.fields();
        assert_eq!(
            g,
            Fields {
                done: 0,
                error: 0,
                irq: 0,
                ..f.clone()
            }
        );
    }
    // A preflight error: DONE with its code, no request, counters untouched.
    let mut w = World::new(Fault::None);
    w.command(READ, 0, 0x1000, 0);
    assert!(!w.wake && w.log.is_empty());
    assert_eq!(
        w.fields(),
        Fields {
            registers: [0, 0x1000, 0, 0],
            done: 1,
            error: 2,
            ..Fields::reset()
        }
    );
}

/// The whole snapshot of a mid-flight WRITE, byte by byte: the M2.6 layout, unchanged.
#[test]
fn a_mid_flight_snapshot_has_the_frozen_bytes() {
    let mut w = World::new(Fault::None);
    for (k, b) in w.ram[0x1100..0x1110].iter_mut().enumerate() {
        *b = k as u8;
    }
    w.write_reg(R_IRQ_ENABLE, 1);
    w.command(WRITE, 2, 0x1100, 3);
    w.write_reg(R_STATUS, REJECTED);
    w.step(); // beat 0 sent
    w.step(); // beat 0 read
    w.step(); // beat 1 sent
    w.write_reg(R_LBA, 0xdead_beef);
    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        // config: clock 3; latency Cycles (1) domain 3, k 2; capacity 16; base 0x1000;
        // size 0x4000
        3, 0, 0, 0,
        1, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
        16, 0, 0, 0, 0, 0, 0, 0,
        0x00, 0x10, 0, 0, 0, 0, 0, 0,
        0x00, 0x40, 0, 0, 0, 0, 0, 0,
        // LBA 0xdeadbeef, MEM_ADDR 0x1100, BLOCK_COUNT 3, IRQ_ENABLE 1
        0xef, 0xbe, 0xad, 0xde,
        0x00, 0x11, 0, 0,
        3, 0, 0, 0,
        1, 0, 0, 0,
        // BUSY, DONE, REJECTED, ERROR
        1, 0, 0, 0,
        // latched: WRITE, LBA 2, MEM_ADDR 0x1100, count 3
        2, 2, 0, 0, 0, 0x00, 0x11, 0, 0, 3, 0, 0, 0,
        // WaitBeat, txn 1; block 0; beat 1
        3, 1, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0,
        1,
        // buffer: 16 bytes, beat 0
        16, 0, 0, 0,
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        // next dma txn 2, next blk txn 0, IRQ level 0
        2, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
        0,
    ];
    assert_eq!(snapshot_of(&w.c), expected);
}

// --- Continuation from restored positions ---

/// How many `ReadBlock`, `WriteBlock`, RAM-write, and RAM-read requests a run sent.
fn kinds(log: &[Req]) -> (usize, usize, usize, usize) {
    let mut n = (0, 0, 0, 0);
    for r in log {
        match r {
            Req::ReadBlock { .. } => n.0 += 1,
            Req::WriteBlock { .. } => n.1 += 1,
            Req::Store { .. } => n.2 += 1,
            Req::Load { .. } => n.3 += 1,
        }
    }
    n
}

#[test]
fn a_restored_read_beat_continues_from_the_restored_buffer_and_latched_command() {
    // READ of blocks 2, 3, 4 to 0x1100, block 1, beat 5 outstanding, with a buffer that is
    // not media block 3's; the registers were reprogrammed.
    let b = pattern(512, 0x40);
    let f = Fields {
        dma_txn: 100,
        blk_txn: 7,
        ..Fields::busy(1, 3, 1, 5, b.clone(), Some(99))
    };
    let mut w = World::resumed(&f, vec![]);
    w.run();
    let old = old_ram();
    // Block 0 and beats 0–4 of block 1 were not rewritten; beat 5 on come from the buffer;
    // block 2 from the media.
    assert!(w.ram[..0x1300 + 80] == old[..0x1300 + 80]);
    assert!(w.ram[0x1350..0x1500] == b[80..]);
    assert!(w.ram[0x1500..0x1700] == media_block(4)[..]);
    assert!(w.ram[0x1700..] == old[0x1700..]);
    // Beats 5 (restored, answered here) … 31, then block 2: one ReadBlock, 32 beats.
    assert_eq!(kinds(&w.log), (1, 0, 26 + 32, 0));
    assert_eq!(
        w.log[0],
        Req::Store {
            txn: 100,
            addr: 0x1360,
            data: b[96..112].to_vec()
        }
    );
    assert_eq!(w.log[26], Req::ReadBlock { txn: 7, lba: 4 });
    assert_eq!(w.status(), DONE);
}

#[test]
fn a_restored_read_before_its_last_beat_writes_beat_31_once() {
    let b = pattern(512, 0x41);
    let f = Fields {
        blk_txn: 2,
        ..Fields::busy(1, 1, 1, 31, b.clone(), None)
    };
    let mut w = World::resumed(&f, vec![]);
    w.run();
    assert_eq!(kinds(&w.log), (1, 0, 1 + 32, 0));
    assert_eq!(
        w.log[0],
        Req::Store {
            txn: 500,
            addr: 0x14f0,
            data: b[496..].to_vec()
        }
    );
    assert_eq!(w.log[1], Req::ReadBlock { txn: 2, lba: 4 });
    // A READ's last block: ReadBlock of LBA + 2 once, then done.
    let f = Fields::busy(1, 1, 2, 0, vec![], None);
    let mut w = World::resumed(&f, vec![]);
    w.run();
    assert_eq!(kinds(&w.log), (1, 0, 32, 0));
    assert_eq!(w.log[0], Req::ReadBlock { txn: 7, lba: 4 });
    assert!(w.ram[0x1500..0x1700] == media_block(4)[..]);
    assert_eq!(w.status(), DONE);
}

#[test]
fn a_restored_write_beat_continues_from_the_restored_prefix() {
    // WRITE from 0x1100 to blocks 2, 3, 4, block 1, beat 7 outstanding, with a 112-byte
    // prefix that is not the RAM's.
    let p = pattern(112, 0x50);
    let f = Fields {
        dma_txn: 41,
        blk_txn: 1,
        ..Fields::busy(2, 3, 1, 7, p.clone(), Some(40))
    };
    let mut w = World::resumed(&f, vec![]);
    w.run();
    let ram = old_ram();
    assert_eq!(w.media[2], media_block(2), "block 0 is not written again");
    assert!(w.media[3][..112] == p[..] && w.media[3][112..] == ram[0x1300 + 112..0x1500]);
    assert!(w.media[4][..] == ram[0x1500..0x1700]);
    // Beat 7 (restored, answered here), beats 8–31, WriteBlock 3; 32 beats, WriteBlock 4.
    assert_eq!(kinds(&w.log), (0, 2, 0, 24 + 32));
    assert_eq!(
        w.log[0],
        Req::Load {
            txn: 41,
            addr: 0x1380
        }
    );
    assert_eq!(w.status(), DONE);
}

#[test]
fn a_restored_write_block_sends_exactly_the_restored_buffer_once() {
    // j = 32 with the 512 bytes: the next request is its WriteBlock.
    let x = pattern(512, 0x60);
    let f = Fields {
        blk_txn: 9,
        ..Fields::busy(2, 1, 2, 32, x.clone(), None)
    };
    let mut w = World::resumed(&f, vec![]);
    w.run();
    assert_eq!(
        w.log,
        [Req::WriteBlock {
            txn: 9,
            lba: 4,
            data: x.clone()
        }]
    );
    assert_eq!(w.media[4], x);
    assert_eq!(w.status(), DONE);
    // WriteBlock outstanding: its result starts the next block at beat 0, once.
    let f = Fields {
        blk_txn: 9,
        ..Fields::busy(2, 2, 0, 32, vec![], Some(8))
    };
    let mut w = World::resumed(&f, x.clone());
    w.run();
    assert_eq!(w.media[2], x, "the outstanding WriteBlock's own data");
    assert_eq!(kinds(&w.log), (0, 2, 0, 64));
    assert_eq!(
        w.log[0],
        Req::Load {
            txn: 500,
            addr: 0x1300
        }
    );
    let ram = old_ram();
    assert!(w.media[3][..] == ram[0x1300..0x1500] && w.media[4][..] == ram[0x1500..0x1700]);
}

#[test]
fn restored_counters_never_wrap() {
    // The next dma txn is the last value: the next beat faults the session and sends
    // nothing, the state unchanged.
    let f = Fields {
        dma_txn: u64::MAX,
        ..Fields::busy(2, 1, 0, 5, pattern(80, 1), None)
    };
    let mut w = World::resumed(&f, vec![]);
    let before = snapshot_of(&w.c);
    assert!(matches!(w.issue(), Err(SimError::ComponentFault(_))));
    assert!(w.log.is_empty());
    assert_eq!(snapshot_of(&w.c), before);
    // One below: the beat goes out with it, and that is the outstanding, latest txn.
    let f = Fields {
        dma_txn: u64::MAX - 1,
        ..f
    };
    let mut w = World::resumed(&f, vec![]);
    w.step();
    assert_eq!(
        w.log,
        [Req::Load {
            txn: u64::MAX - 1,
            addr: 0x1150
        }]
    );
    let g = w.fields();
    assert_eq!(
        (g.engine, g.txn, g.dma_txn),
        (3, Some(u64::MAX - 1), u64::MAX)
    );
    let mut t = Targets::new();
    assert_eq!(t.check(&g.encode(), true), Ok(Family::WriteWaitBeat));
    // No txn was ever issued: nothing can be outstanding.
    let g = Fields {
        dma_txn: 0,
        txn: Some(u64::MAX),
        ..g
    };
    assert_eq!(t.check(&g.encode(), true), Err(Reason::Txn));
    let g = Fields {
        blk_txn: 0,
        ..Fields::busy(1, 2, 0, 0, vec![], Some(u64::MAX))
    };
    assert_eq!(t.check(&g.encode(), true), Err(Reason::Txn));
}

// --- Property: the independent oracle against restore ---

fn fields_near_boundaries() -> impl Strategy<Value = Fields> {
    (
        (
            prop_oneof![Just(1u8), Just(2), 0u8..=4],
            prop_oneof![Just(1u32), Just(3), Just(0), Just(17)],
            prop_oneof![Just(2u32), Just(13), Just(15)],
            prop_oneof![Just(0x1100u32), Just(0x1108), Just(0x0ff0), Just(0x4a00)],
            any::<bool>(),
        ),
        (
            0u8..=4,
            prop_oneof![0u32..=4, Just(u32::MAX)],
            prop_oneof![0u8..=33, Just(255u8)],
            (0usize..9, any::<u8>()),
            0usize..6,
        ),
        (
            prop_oneof![
                Just(0u64),
                Just(1),
                2u64..600,
                Just(u64::MAX - 1),
                Just(u64::MAX)
            ],
            prop_oneof![Just(0u64), Just(1), 2u64..20, Just(u64::MAX)],
            (0u8..=1, 0u8..=1, 0u8..=1),
            prop_oneof![Just(0u8), Just(6), Just(7), 1u8..=5, Just(8), Just(200)],
            (0u32..=3, 0u8..=1),
            any::<bool>(),
        ),
    )
        .prop_map(
            |(
                (op, count, lba, addr, has_latched),
                (engine, block, beat, (len_kind, seed), txn_kind),
                (dma_txn, blk_txn, (busy, done, rejected), error, (enable, irq), consistent),
            )| {
                let j16 = 16 * usize::from(beat);
                let len = [
                    0,
                    512,
                    j16,
                    j16 + 16,
                    j16.saturating_sub(16),
                    496,
                    528,
                    16,
                    j16 + 1,
                ][len_kind];
                let txn = [
                    dma_txn.checked_sub(1),
                    blk_txn.checked_sub(1),
                    Some(dma_txn),
                    Some(blk_txn.wrapping_sub(2)),
                    None,
                    Some(0),
                ][txn_kind];
                let mut f = Fields {
                    registers: [7, 0x2000, 1, enable],
                    busy,
                    done,
                    rejected,
                    error,
                    latched: has_latched.then_some((op, lba, addr, count)),
                    engine,
                    txn,
                    block,
                    beat,
                    buffer: pattern(len, seed),
                    dma_txn,
                    blk_txn,
                    irq,
                };
                if consistent {
                    // Mostly-legal shapes, so both verdicts are common.
                    f.busy = u8::from(f.latched.is_some());
                    f.done &= 1 - f.busy;
                    if f.done == 0 {
                        f.error = 0;
                    }
                    f.registers[3] &= 1;
                    f.irq = u8::from(f.done == 1 && f.registers[3] == 1);
                    if f.busy == 0 {
                        f.engine = 0;
                        f.block = 0;
                        f.beat = 0;
                        f.buffer.clear();
                    }
                    let port = match (op, f.engine) {
                        (_, 3) => dma_txn.checked_sub(1),
                        (_, 2) => blk_txn.checked_sub(1),
                        _ => None,
                    };
                    f.txn = if matches!(f.engine, 2 | 3) {
                        port.or(Some(0))
                    } else {
                        None
                    };
                }
                f
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn restore_accepts_exactly_what_the_oracle_calls_legal(f in fields_near_boundaries()) {
        let mut t = Targets::new();
        let _ = t.check(&f.encode(), true);
    }
}

// --- Runtime: every event, and who owns what is in flight ---

const MMIO: u64 = 0x2000_0000;
/// The bus's RAM region ends 15 beats into 0x2000: the aperture contains a hole.
const RAM_END: u64 = 0x20f0;
/// A media block that answers `Error { BadBlock }`.
const BAD: u64 = 9;

struct IrqSink;

impl Component for IrqSink {
    fn type_name(&self) -> &'static str {
        "test.irq_sink"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                msg: Message::Irq(_),
                ..
            } if ctx.phase() == Phase::Complete => Ok(()),
            _ => Err(SimError::ComponentFault("irq sink: unexpected delivery")),
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Ids {
    host: ComponentId,
    bus: ComponentId,
    ctl: ComponentId,
    media: ComponentId,
    sink: ComponentId,
}

/// A host and the controller's `dma` as the masters of a bus with [`old_ram`] over `[0,
/// RAM_END)`, nothing from there to the controller's window at [`MMIO`] (so the DMA
/// aperture `[0x1000, 0x5000)` contains a hole), and a `SimpleBlockMedia` of
/// [`old_media`] with block [`BAD`] bad.
fn build(requests: &[(u64, MemMsg)]) -> (Runtime, Ids) {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let host = t.add_component(
        "soc.cpu",
        Box::new(Script {
            clock,
            requests: requests.to_vec(),
        }),
    );
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: vec!["cpu", "dma"],
        regions: vec![
            Region {
                name: "ram",
                base: 0,
                size: RAM_END,
            },
            Region {
                name: "blk",
                base: MMIO,
                size: 0x20,
            },
        ],
        clock,
    })
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = Ram::new(
        RamConfig {
            size: RAM_END,
            latency: cycles(1),
        },
        &RamImage {
            image_hash: [0; 32],
            segments: vec![Segment {
                offset: 0,
                bytes: old_ram()[..RAM_END as usize].to_vec(),
            }],
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let bytes = old_media().concat();
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: CAPACITY,
            latency: cycles(2),
            bad_blocks: BTreeSet::from([BAD]),
        },
        &MediaImage {
            image_hash: *blake3::hash(&bytes).as_bytes(),
            bytes,
        },
    )
    .unwrap();
    let media = t.add_component("soc.disk", Box::new(disk));
    let ctl = DmaBlockController::new(DmaBlockControllerConfig {
        clock,
        latency: cycles(1),
        capacity_blocks: CAPACITY,
        dma_base: BASE,
        dma_size: APERTURE,
    })
    .unwrap();
    let ctl = t.add_component("soc.blk", Box::new(ctl));
    let sink = t.add_component("soc.irqc", Box::new(IrqSink));
    t.connect((host, "mem"), (bus, "cpu"), Some(cycles(1)));
    t.connect((ctl, "dma"), (bus, "dma"), Some(cycles(1)));
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
    t.connect((bus, "blk"), (ctl, "mem"), Some(cycles(1)));
    t.connect((ctl, "blk"), (media, "blk"), Some(cycles(1)));
    t.connect((ctl, "irq"), (sink, "irq"), None);
    (
        t.elaborate(SessionConfig::default()).unwrap(),
        Ids {
            host,
            bus,
            ctl,
            media,
            sink,
        },
    )
}

fn w32(offset: u64, value: u32) -> MemMsg {
    write(0, MMIO + offset, &value.to_le_bytes())
}

/// The interrupt enabled, then one command every [`GAP`] cycles, each followed by a
/// `STATUS` read and an ACK, then 16-byte host reads of `[from, to)`; every request with
/// its own txn. `STATUS` of command *n* is response `1 + 6n + 4`, the reads from
/// response `1 + 6 × commands`.
fn workload(list: &[(u32, u32, u32, u32)], (from, to): (u64, u64)) -> Vec<(u64, MemMsg)> {
    const GAP: u64 = 900;
    let mut requests = vec![(0, w32(R_IRQ_ENABLE, 1))];
    for (n, &(op, lba, addr, count)) in list.iter().enumerate() {
        let t = 1 + n as u64 * GAP;
        requests.extend([
            (t, w32(R_LBA, lba)),
            (t + 1, w32(R_MEM_ADDR, addr)),
            (t + 2, w32(R_BLOCK_COUNT, count)),
            (t + 3, w32(R_COMMAND, op)),
            (t + GAP - 3, read(0, MMIO + R_STATUS, 4)),
            (t + GAP - 2, w32(R_ACK, 1)),
        ]);
    }
    let at = 1 + list.len() as u64 * GAP;
    requests.extend(
        (from..to)
            .step_by(BEAT)
            .enumerate()
            .map(|(k, a)| (at + k as u64, read(0, a, 16))),
    );
    for (i, (_, msg)) in requests.iter_mut().enumerate() {
        match msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => *txn = TxnId(i as u64),
            _ => unreachable!(),
        }
    }
    requests
}

/// What a checkpoint leaves in flight for the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum InFlight {
    /// Nothing: no command, or DONE.
    Nothing,
    /// A `Wake(ISSUE)` is queued.
    Wake,
    /// The controller's request is queued, not yet at its target.
    Request(Kind),
    /// Its target took the request; the response is on its way back.
    Response(Kind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    ReadBlock,
    WriteBlock,
    Store,
    Load,
}

/// The controller's engine and command after each event, and the trace records so far.
#[derive(Default)]
struct Log {
    engines: Vec<(String, String)>,
    records: usize,
    marks: Vec<usize>,
}

struct Probe {
    ctl: ComponentId,
    log: Rc<RefCell<Log>>,
}

impl Observer for Probe {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let fields = world.inspect(self.ctl).unwrap().fields;
        let get = |n: &str| match &fields.iter().find(|(k, _)| *k == n).unwrap().1 {
            Value::Str(s) => s.clone(),
            other => panic!("{other:?}"),
        };
        let mut log = self.log.borrow_mut();
        log.engines.push((get("engine"), get("command")));
        let n = log.records;
        log.marks.push(n);
        Control::Continue
    }

    fn on_trace(&mut self, _: &TraceRecord) {
        self.log.borrow_mut().records += 1;
    }
}

fn txn_of(d: &Delivered) -> Option<u64> {
    match d {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => Some(match m {
            MemMsg::ReadReq { txn, .. }
            | MemMsg::WriteReq { txn, .. }
            | MemMsg::ReadResp { txn, .. }
            | MemMsg::WriteResp { txn, .. } => txn.0,
        }),
        Delivered::Message {
            msg: Message::Block(m),
            ..
        } => Some(match m {
            BlockMsg::ReadBlock { txn, .. }
            | BlockMsg::WriteBlock { txn, .. }
            | BlockMsg::ReadResult { txn, .. }
            | BlockMsg::WriteResult { txn, .. } => txn.0,
        }),
        _ => None,
    }
}

/// The kind of a request the controller sent, as its target receives it.
fn request_kind(e: &Dispatched, ids: Ids) -> Option<Kind> {
    if e.source != ids.ctl || (e.target != ids.media && e.target != ids.bus) {
        return None;
    }
    match &e.delivery {
        Delivered::Message {
            msg: Message::Block(BlockMsg::ReadBlock { .. }),
            ..
        } => Some(Kind::ReadBlock),
        Delivered::Message {
            msg: Message::Block(BlockMsg::WriteBlock { .. }),
            ..
        } => Some(Kind::WriteBlock),
        Delivered::Message {
            msg: Message::MemV1(MemMsg::WriteReq { .. }),
            ..
        } => Some(Kind::Store),
        Delivered::Message {
            msg: Message::MemV1(MemMsg::ReadReq { .. }),
            ..
        } => Some(Kind::Load),
        _ => None,
    }
}

/// Whether `e` delivers a result to the controller on `dma` or `blk`.
fn is_result(e: &Dispatched, ids: Ids) -> bool {
    e.target == ids.ctl
        && matches!(
            e.delivery,
            Delivered::Message { port, .. } if port == DMA_PORT || port == BLK_PORT
        )
}

/// Whether `e` delivers the result of a `kind` request with `txn` to the controller: the
/// two ports count their txns independently, so the port tells them apart.
fn is_result_of(e: &Dispatched, ids: Ids, kind: Kind, txn: Option<u64>) -> bool {
    let port = match kind {
        Kind::ReadBlock | Kind::WriteBlock => BLK_PORT,
        Kind::Store | Kind::Load => DMA_PORT,
    };
    e.target == ids.ctl
        && matches!(e.delivery, Delivered::Message { port: p, .. } if p == port)
        && txn_of(&e.delivery) == txn
}

fn is_wake(e: &Dispatched, ids: Ids) -> bool {
    e.target == ids.ctl && matches!(e.delivery, Delivered::Wake { .. })
}

/// What checkpoint `k` (after `events[..k]`, the controller in `engine`) leaves in flight.
fn in_flight(events: &[Dispatched], k: usize, engine: &str, ids: Ids) -> InFlight {
    let prefix = &events[..k];
    let last_request = prefix
        .iter()
        .rev()
        .find_map(|e| request_kind(e, ids).map(|kind| (kind, e)));
    match engine {
        "idle" => InFlight::Nothing,
        "issue" => InFlight::Wake,
        "wait_media" | "wait_beat" => {
            let requests = prefix
                .iter()
                .filter(|e| request_kind(e, ids).is_some())
                .count();
            let results = prefix.iter().filter(|e| is_result(e, ids)).count();
            if requests == results {
                let next = events[k..]
                    .iter()
                    .find_map(|e| request_kind(e, ids))
                    .unwrap();
                InFlight::Request(next)
            } else {
                assert_eq!(requests, results + 1);
                InFlight::Response(last_request.unwrap().0)
            }
        }
        other => panic!("{other}"),
    }
}

/// Request counts: `ReadBlock`, `WriteBlock`, RAM writes, RAM reads.
fn count(events: &[Dispatched], ids: Ids) -> [usize; 4] {
    let mut n = [0; 4];
    for e in events {
        match request_kind(e, ids) {
            Some(Kind::ReadBlock) => n[0] += 1,
            Some(Kind::WriteBlock) => n[1] += 1,
            Some(Kind::Store) => n[2] += 1,
            Some(Kind::Load) => n[3] += 1,
            None => {}
        }
    }
    n
}

struct Resumed {
    events: Vec<Dispatched>,
    ids: Ids,
    flights: BTreeSet<InFlight>,
}

/// Runs `requests` once, checkpointing after every event, then restores each checkpoint
/// into a fresh session and requires the rest of the run, the final snapshot, the trace,
/// and both digests to equal the uninterrupted run's; the restored queue to be the
/// checkpoint's (so restore scheduled nothing); and whatever the checkpoint left in
/// flight (a wake, a request, or a response) to be delivered exactly once.
fn resume_everywhere(requests: &[(u64, MemMsg)], counts: [usize; 4]) -> Resumed {
    let (mut rt, ids) = build(requests);
    let log = Rc::new(RefCell::new(Log::default()));
    rt.add_observer(Box::new(Probe {
        ctl: ids.ctl,
        log: Rc::clone(&log),
    }));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let mut points = vec![(
        rt.snapshot().unwrap(),
        log.borrow().records,
        rt.pending(),
        rt.peek_key(),
    )];
    let mut events = Vec::new();
    while let Some(e) = rt.step().unwrap() {
        events.push(e);
        let records = *log.borrow().marks.last().unwrap();
        points.push((rt.snapshot().unwrap(), records, rt.pending(), rt.peek_key()));
    }
    assert_eq!(rt.fault(), None);
    let trace = rt.take_trace().unwrap();
    let last = rt.snapshot().unwrap();
    let digests = (rt.state_digest().unwrap(), rt.execution_digest());
    assert_eq!(count(&events, ids), counts, "request counts");
    let engines = log.borrow().engines.clone();
    let mut flights = BTreeSet::new();
    for (k, (bytes, records, pending, peek)) in points.iter().enumerate() {
        let engine = if k == 0 {
            "idle"
        } else {
            engines[k - 1].0.as_str()
        };
        let flight = in_flight(&events, k, engine, ids);
        flights.insert(flight);
        let (mut fresh, _) = build(requests);
        fresh.restore(bytes).unwrap();
        assert_eq!(
            (fresh.pending(), fresh.peek_key()),
            (*pending, *peek),
            "queue after {k}"
        );
        fresh
            .resume_trace(Trace {
                header: trace.header.clone(),
                records: trace.records[..*records].to_vec(),
            })
            .unwrap();
        let rest: Vec<Dispatched> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert!(
            rest == events[k..],
            "checkpoint after {k} events ({flight:?})"
        );
        assert_eq!(fresh.snapshot().unwrap(), last, "checkpoint after {k}");
        assert_eq!(fresh.take_trace().unwrap(), trace, "checkpoint after {k}");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            digests
        );
        // No request added or lost across the checkpoint.
        let (a, b) = (count(&events[..k], ids), count(&rest, ids));
        assert_eq!([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]], counts);
        // Ownership of what was in flight.
        match flight {
            InFlight::Nothing => {}
            InFlight::Wake => {
                let upto = rest
                    .iter()
                    .position(|e| request_kind(e, ids).is_some())
                    .unwrap();
                let wakes = rest[..upto].iter().filter(|e| is_wake(e, ids)).count();
                assert_eq!(wakes, 1, "one wake before the next request, after {k}");
            }
            InFlight::Request(kind) => {
                let req = rest
                    .iter()
                    .find(|e| request_kind(e, ids).is_some())
                    .unwrap();
                let txn = txn_of(&req.delivery);
                let at_target = |es: &[Dispatched]| {
                    es.iter()
                        .filter(|e| {
                            request_kind(e, ids) == Some(kind) && txn_of(&e.delivery) == txn
                        })
                        .count()
                };
                assert_eq!(
                    (at_target(&events[..k]), at_target(&rest)),
                    (0, 1),
                    "after {k}"
                );
                let results = rest
                    .iter()
                    .filter(|e| is_result_of(e, ids, kind, txn))
                    .count();
                assert_eq!(results, 1, "after {k}");
            }
            InFlight::Response(kind) => {
                let req = events[..k]
                    .iter()
                    .rev()
                    .find(|e| request_kind(e, ids) == Some(kind))
                    .unwrap();
                let txn = txn_of(&req.delivery);
                let resent = rest
                    .iter()
                    .filter(|e| request_kind(e, ids) == Some(kind) && txn_of(&e.delivery) == txn)
                    .count();
                let results = |es: &[Dispatched]| {
                    es.iter()
                        .filter(|e| is_result_of(e, ids, kind, txn))
                        .count()
                };
                assert_eq!(
                    (resent, results(&events[..k]), results(&rest)),
                    (0, 0, 1),
                    "after {k}"
                );
            }
        }
    }
    Resumed {
        events,
        ids,
        flights,
    }
}

fn responses(r: &Resumed) -> Vec<MemMsg> {
    let mut out: Vec<(u64, MemMsg)> = r
        .events
        .iter()
        .filter(|e| e.target == r.ids.host)
        .map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::MemV1(m),
                ..
            } => (txn_of(&e.delivery).unwrap(), m.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    out.sort_by_key(|(txn, _)| *txn);
    out.into_iter().map(|(_, m)| m).collect()
}

fn data_of(m: &MemMsg) -> Vec<u8> {
    match m {
        MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } => data.clone(),
        other => panic!("{other:?}"),
    }
}

/// `STATUS` after each of `n` commands, and the bytes the host read back.
fn outcome(r: &Resumed, n: usize) -> (Vec<u32>, Vec<u8>) {
    let resp = responses(r);
    let status = (0..n)
        .map(|c| u32::from_le_bytes(data_of(&resp[1 + 6 * c + 4]).try_into().unwrap()))
        .collect();
    let back = resp[1 + 6 * n..].iter().flat_map(data_of).collect();
    (status, back)
}

fn levels(r: &Resumed) -> Vec<bool> {
    r.events
        .iter()
        .filter(|e| e.target == r.ids.sink)
        .map(|e| match e.delivery {
            Delivered::Message {
                msg: Message::Irq(irq_v0::IrqMsg::Level { asserted }),
                ..
            } => asserted,
            _ => panic!("{e:?}"),
        })
        .collect()
}

fn media_writes(r: &Resumed) -> Vec<u64> {
    r.events
        .iter()
        .filter_map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::Block(BlockMsg::WriteBlock { lba, .. }),
                ..
            } if e.target == r.ids.media => Some(*lba),
            _ => None,
        })
        .collect()
}

fn read_flights() -> BTreeSet<InFlight> {
    [
        InFlight::Nothing,
        InFlight::Wake,
        InFlight::Request(Kind::ReadBlock),
        InFlight::Response(Kind::ReadBlock),
        InFlight::Request(Kind::Store),
        InFlight::Response(Kind::Store),
    ]
    .into_iter()
    .collect()
}

fn all_flights() -> BTreeSet<InFlight> {
    let mut all = read_flights();
    all.extend([
        InFlight::Request(Kind::Load),
        InFlight::Response(Kind::Load),
        InFlight::Request(Kind::WriteBlock),
        InFlight::Response(Kind::WriteBlock),
    ]);
    all
}

#[test]
fn a_read_resumes_from_every_event() {
    let r = resume_everywhere(
        &workload(&[(READ, 4, 0x1000, 3)], (0x1000, 0x1600)),
        [3, 0, 96, 0],
    );
    let (status, back) = outcome(&r, 1);
    assert_eq!(status, [DONE]);
    assert!(back == [media_block(4), media_block(5), media_block(6)].concat());
    assert_eq!(levels(&r), [true, false]);
    assert_eq!(r.flights, read_flights());
}

#[test]
fn a_write_resumes_from_every_event() {
    let r = resume_everywhere(
        &workload(
            &[(WRITE, 12, 0x1400, 3), (READ, 12, 0x1a00, 3)],
            (0x1a00, 0x2000),
        ),
        [3, 3, 96, 96],
    );
    let (status, back) = outcome(&r, 2);
    assert_eq!(status, [DONE, DONE]);
    assert!(back[..] == old_ram()[0x1400..0x1a00]);
    assert_eq!(media_writes(&r), [12, 13, 14]);
    assert_eq!(levels(&r), [true, false, true, false]);
    assert_eq!(r.flights, all_flights());
}

#[test]
fn a_read_dma_fault_resumes_from_every_event() {
    // Block 1 (media block 5) runs into the hole at beat 15.
    let r = resume_everywhere(
        &workload(&[(READ, 4, 0x1e00, 3)], (0x1e00, RAM_END)),
        [2, 0, 48, 0],
    );
    let (status, back) = outcome(&r, 1);
    assert_eq!(status, [DONE | 6 << 8]);
    assert!(back[..0x200] == media_block(4)[..], "block 0 in RAM");
    assert!(
        back[0x200..] == media_block(5)[..0xf0],
        "block 1's beats 0–14 in RAM"
    );
    assert_eq!(levels(&r), [true, false]);
    assert_eq!(r.flights, read_flights());
}

#[test]
fn a_write_ram_fault_resumes_from_every_event() {
    // Block 1 (from 0x2000) runs into the hole at beat 15 before its WriteBlock; then
    // blocks 12–14 are read back.
    let r = resume_everywhere(
        &workload(
            &[(WRITE, 12, 0x1e00, 3), (READ, 12, 0x1000, 3)],
            (0x1000, 0x1600),
        ),
        [3, 1, 96, 48],
    );
    let (status, back) = outcome(&r, 2);
    assert_eq!(status, [DONE | 6 << 8, DONE]);
    assert_eq!(media_writes(&r), [12]);
    let expected = [
        old_ram()[0x1e00..0x2000].to_vec(),
        media_block(13),
        media_block(14),
    ]
    .concat();
    assert!(
        back == expected,
        "block 0 committed, blocks 1 and 2 untouched"
    );
    assert_eq!(levels(&r), [true, false, true, false]);
    assert_eq!(r.flights, all_flights());
}

#[test]
fn a_write_media_error_resumes_from_every_event() {
    // Block 0 (LBA 8) commits, block 1 (LBA 9) is bad, block 2 (LBA 10) never starts;
    // then blocks 8 and 10 are read back.
    let r = resume_everywhere(
        &workload(
            &[
                (WRITE, 8, 0x1000, 3),
                (READ, 8, 0x1600, 1),
                (READ, 10, 0x1800, 1),
            ],
            (0x1600, 0x1a00),
        ),
        [2, 2, 64, 64],
    );
    let (status, back) = outcome(&r, 3);
    assert_eq!(status, [DONE | 7 << 8, DONE, DONE]);
    assert_eq!(media_writes(&r), [8, 9]);
    let expected = [old_ram()[0x1000..0x1200].to_vec(), media_block(10)].concat();
    assert!(back == expected);
    assert_eq!(levels(&r), [true, false, true, false, true, false]);
    assert_eq!(r.flights, all_flights());
}

#[test]
fn tracing_and_observers_change_nothing() {
    let requests = workload(
        &[(WRITE, 12, 0x1e00, 3), (READ, 4, 0x1000, 2)],
        (0x1000, 0x1400),
    );
    let run = |observed: bool| {
        let (mut rt, ids) = build(&requests);
        if observed {
            rt.add_observer(Box::new(Probe {
                ctl: ids.ctl,
                log: Rc::new(RefCell::new(Log::default())),
            }));
            rt.start_trace().unwrap();
            for t in [100, 1000, 1900] {
                rt.observe_at(Tick(t * 1000));
            }
        }
        rt.init().unwrap();
        let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (
            events,
            rt.snapshot().unwrap(),
            rt.state_digest().unwrap(),
            rt.execution_digest(),
        )
    };
    let (plain, observed) = (run(false), run(true));
    assert!(plain.0 == observed.0, "events or their timing differ");
    assert_eq!(
        (plain.1, plain.2, plain.3),
        (observed.1, observed.2, observed.3)
    );
}
