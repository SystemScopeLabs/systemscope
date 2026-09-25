//! `DmaBlockController` WRITE engine (`docs/m2-design.md` §9.5, §9.6, §9.7, §9.9; M2.7b):
//! 16-byte RAM read beats, 32 per block, the block buffer growing by 16 bytes per beat,
//! `WriteBlock` only after all 32 beats with the buffer dropped once it is outstanding,
//! one outstanding request in total, multi-block progression, fresh checked `TxnId`s,
//! completion and its interrupt, the descriptor latch, protocol violations, snapshots of
//! the reachable WRITE positions, and checkpoints.
//!
//! As in `dma_read.rs`, most tests drive the controller through [`World`], a mock RAM and
//! media answering the one outstanding request at a time, which checks every event; an
//! independent oracle predicts the requests and the final media. Then a real runtime with
//! a bus, a RAM, and a `SimpleBlockMedia` checks the `WriteBlock` payloads the media
//! received, reads the blocks back with a READ, and checks timing, every-event
//! checkpoints, and determinism.
//!
//! The engine's failure paths (§9.7) belong to M2.7c; here they are only shown not to
//! panic.

mod common;

use std::collections::BTreeSet;

use common::{MockCtx, Script, Traced, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{
    BlockMsg, BlockReadOutcome, BlockWriteOutcome, MediaError,
};
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::dma::{
    BLK_PORT, COMMAND_KIND, DMA_PORT, DONE_KIND, IRQ_PORT, ISSUE, MEM_PORT, REJECTED_KIND,
};
use systemscope_platform::ram::{RamConfig, RamImage};
use systemscope_platform::{
    BlockMediaConfig, DmaBlockController, DmaBlockControllerConfig, MediaImage, MultiMasterBus,
    MultiMasterBusConfig, Ram, Region, SimpleBlockMedia,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

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

const BUSY: u32 = 1;
const DONE: u32 = 2;
const REJECTED: u32 = 4;

const BLOCK: usize = 512;
const BEAT: usize = 16;
const BEATS: usize = 32;

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

/// The RAM: every 16-byte beat is distinguishable from every other, its first two bytes
/// being its beat number in the whole RAM.
fn ram() -> Vec<u8> {
    (0..RAM_SIZE)
        .map(|a| match a % BEAT {
            0 => (a / BEAT) as u8,
            1 => ((a / BEAT) >> 8) as u8,
            k => (a / BEAT * 7 + k * 13) as u8 ^ 0x5a,
        })
        .collect()
}

/// The media before the WRITE: every block filled with a marker of its own.
fn old_media() -> Vec<Vec<u8>> {
    (0..CAPACITY as usize)
        .map(|b| vec![0xe0 | b as u8; BLOCK])
        .collect()
}

/// One engine request, as the controller sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Req {
    Beat { txn: u64, addr: u64 },
    Write { txn: u64, lba: u64, data: Vec<u8> },
}

/// A request without its `txn`, as the oracle predicts it: a beat with the bytes it
/// read, or a `WriteBlock`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Beat(u64, Vec<u8>),
    Write(u64, Vec<u8>),
}

/// The snapshot's lifecycle and engine fields, decoded from the frozen layout (§9.9)
/// without the component's code.
#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    busy: bool,
    done: bool,
    rejected: bool,
    error: u8,
    latched: Option<(u8, u32, u32, u32)>,
    engine: u8,
    txn: Option<u64>,
    block: u32,
    beat: u8,
    buffer: Vec<u8>,
    dma_txn: u64,
    blk_txn: u64,
    irq: bool,
}

fn view(c: &DmaBlockController) -> View {
    let bytes = snapshot_of(c);
    let mut r = SnapshotReader::new(&bytes);
    r.u32().unwrap();
    if r.u8().unwrap() == 0 {
        r.u128().unwrap();
    } else {
        r.u32().unwrap();
        r.u64().unwrap();
    }
    for _ in 0..3 {
        r.u64().unwrap();
    }
    for _ in 0..4 {
        r.u32().unwrap();
    }
    let (busy, done, rejected, error) = (
        r.bool().unwrap(),
        r.bool().unwrap(),
        r.bool().unwrap(),
        r.u8().unwrap(),
    );
    let latched = busy.then(|| {
        (
            r.u8().unwrap(),
            r.u32().unwrap(),
            r.u32().unwrap(),
            r.u32().unwrap(),
        )
    });
    let engine = r.u8().unwrap();
    let txn = (engine >= 2).then(|| r.u64().unwrap());
    let (block, beat) = (r.u32().unwrap(), r.u8().unwrap());
    let buffer = r.bytes().unwrap().to_vec();
    let (dma_txn, blk_txn) = (r.u64().unwrap(), r.u64().unwrap());
    let irq = r.bool().unwrap();
    r.finish().unwrap();
    View {
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
    }
}

fn issue_wake() -> (ScheduleWhen, Phase, u64) {
    (
        ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 1,
        },
        Phase::Request,
        ISSUE,
    )
}

/// The controller, a perfect RAM and media, and the one pending wake or outstanding
/// request the runtime's queue would hold.
struct World {
    c: DmaBlockController,
    ram: Vec<u8>,
    media: Vec<Vec<u8>>,
    wake: bool,
    outstanding: Option<Req>,
    log: Vec<Req>,
    levels: Vec<bool>,
    traced: Vec<Traced>,
    /// Restore the controller into a fresh one after every event.
    checkpoint_every_event: bool,
}

impl World {
    fn new() -> World {
        World::with(DmaBlockController::new(config()).unwrap())
    }

    fn with(c: DmaBlockController) -> World {
        World {
            c,
            ram: ram(),
            media: old_media(),
            wake: false,
            outstanding: None,
            log: Vec::new(),
            levels: Vec::new(),
            traced: Vec::new(),
            checkpoint_every_event: false,
        }
    }

    /// Records what a context scheduled, checking that a wake is `Wake(ISSUE)` at the next
    /// cycle's `Request` and is never scheduled beside another wake or a request.
    fn absorb(&mut self, ctx: &MockCtx) {
        assert!(ctx.wakes.len() <= 1, "{:?}", ctx.wakes);
        for w in &ctx.wakes {
            assert_eq!((w.when, w.phase, w.token), issue_wake());
            assert!(!self.wake && self.outstanding.is_none());
            self.wake = true;
        }
        for i in &ctx.irqs {
            assert_eq!(
                (i.port, i.when, i.phase),
                (IRQ_PORT, ScheduleWhen::Now, Phase::Complete)
            );
            self.levels.push(i.asserted);
        }
        self.traced.extend(ctx.traced.iter().cloned());
    }

    /// An MMIO write, which must never send an engine request.
    fn write_reg(&mut self, offset: u64, value: u32) -> WriteOutcome {
        let mut ctx = MockCtx::new(Phase::Transfer);
        ctx.deliver(
            &mut self.c,
            MEM_PORT,
            write(9, offset, &value.to_le_bytes()),
        )
        .unwrap();
        assert!(ctx.blocks.is_empty());
        let sent = ctx.take_one();
        assert_eq!(sent.port, MEM_PORT);
        self.absorb(&ctx);
        let MemMsg::WriteResp { outcome, .. } = sent.msg else {
            panic!("{:?}", sent.msg)
        };
        outcome
    }

    fn read_reg(&mut self, offset: u64) -> u32 {
        let mut ctx = MockCtx::new(Phase::Transfer);
        ctx.deliver(&mut self.c, MEM_PORT, read(9, offset, 4))
            .unwrap();
        assert!(ctx.blocks.is_empty() && ctx.wakes.is_empty() && ctx.irqs.is_empty());
        let MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } = ctx.take_one().msg
        else {
            panic!("register {offset:#x} faulted")
        };
        u32::from_le_bytes(data.try_into().unwrap())
    }

    fn status(&mut self) -> u32 {
        self.read_reg(R_STATUS)
    }

    fn registers(&mut self, lba: u32, addr: u32, count: u32) {
        for (offset, value) in [(R_LBA, lba), (R_MEM_ADDR, addr), (R_BLOCK_COUNT, count)] {
            assert_eq!(self.write_reg(offset, value), WriteOutcome::Done);
        }
    }

    fn command(&mut self, command: u32, lba: u32, addr: u32, count: u32) {
        self.registers(lba, addr, count);
        assert_eq!(self.write_reg(R_COMMAND, command), WriteOutcome::Done);
    }

    /// Delivers the pending `Wake(ISSUE)`: exactly one request, `Now` in `Request`, and
    /// never a READ's.
    fn issue(&mut self) {
        assert!(self.wake && self.outstanding.is_none());
        self.wake = false;
        let mut ctx = MockCtx::new(Phase::Request);
        self.c
            .handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx)
            .unwrap();
        assert!(ctx.wakes.is_empty() && ctx.irqs.is_empty() && ctx.traced.is_empty());
        assert_eq!(ctx.order.len(), 1, "one request per wake: {:?}", ctx.order);
        let req = if let Some(b) = ctx.blocks.pop() {
            assert_eq!(
                (b.port, b.when, b.phase),
                (BLK_PORT, ScheduleWhen::Now, Phase::Request)
            );
            let BlockMsg::WriteBlock { txn, lba, data } = b.msg else {
                panic!("{:?}", b.msg)
            };
            Req::Write {
                txn: txn.0,
                lba,
                data,
            }
        } else {
            let s = ctx.sent.pop().unwrap();
            assert_eq!(
                (s.port, s.when, s.phase),
                (DMA_PORT, ScheduleWhen::Now, Phase::Request)
            );
            let MemMsg::ReadReq { txn, addr, len } = s.msg else {
                panic!("{:?}", s.msg)
            };
            assert_eq!(len, 16, "a beat is 16 bytes");
            Req::Beat { txn: txn.0, addr }
        };
        self.log.push(req.clone());
        self.outstanding = Some(req);
    }

    /// The successful result of `req`, applying a `WriteBlock` to the media.
    fn result_of(&mut self, req: &Req) -> (PortId, Message) {
        match req {
            Req::Beat { txn, addr } => (
                DMA_PORT,
                MemMsg::ReadResp {
                    txn: TxnId(*txn),
                    outcome: ReadOutcome::Data {
                        data: self.ram[*addr as usize..][..BEAT].to_vec(),
                    },
                }
                .into(),
            ),
            Req::Write { txn, lba, data } => {
                assert_eq!(data.len(), BLOCK);
                self.media[*lba as usize] = data.clone();
                (
                    BLK_PORT,
                    BlockMsg::WriteResult {
                        txn: TxnId(*txn),
                        outcome: BlockWriteOutcome::Done,
                    }
                    .into(),
                )
            }
        }
    }

    /// Delivers the outstanding request's result in `Complete`: nothing is sent.
    fn respond(&mut self) {
        let req = self.outstanding.take().unwrap();
        let (port, msg) = self.result_of(&req);
        let mut ctx = MockCtx::new(Phase::Complete);
        ctx.deliver_msg(&mut self.c, port, msg).unwrap();
        assert!(ctx.sent.is_empty() && ctx.blocks.is_empty());
        self.absorb(&ctx);
        let completed = ctx.traced.iter().any(|(kind, _)| *kind == DONE_KIND);
        assert!(self.wake != completed, "{:?}", ctx.traced);
        // Only a `WriteResult` can complete a WRITE.
        assert!(!completed || matches!(req, Req::Write { .. }));
    }

    fn step(&mut self) -> bool {
        if self.wake {
            self.issue();
        } else if self.outstanding.is_some() {
            self.respond();
        } else {
            return false;
        }
        if self.checkpoint_every_event {
            self.checkpoint();
        }
        true
    }

    fn run(&mut self) {
        while self.step() {}
    }

    /// Replaces the controller with a fresh one restored from its snapshot. The pending
    /// wake or request stays where the runtime keeps it: in the queue, not re-sent.
    fn checkpoint(&mut self) {
        let bytes = snapshot_of(&self.c);
        let mut fresh = DmaBlockController::new(config()).unwrap();
        restore_into(&mut fresh, &bytes).unwrap();
        assert_eq!(snapshot_of(&fresh), bytes);
        assert_eq!(fresh.inspect(), self.c.inspect());
        self.c = fresh;
    }

    /// The log as oracle operations; RAM does not change during a WRITE.
    fn ops(&self) -> Vec<Op> {
        self.log
            .iter()
            .map(|r| match r {
                Req::Beat { addr, .. } => {
                    Op::Beat(*addr, self.ram[*addr as usize..][..BEAT].to_vec())
                }
                Req::Write { lba, data, .. } => Op::Write(*lba, data.clone()),
            })
            .collect()
    }

    fn view_busy(&self) -> bool {
        view(&self.c).busy
    }
}

/// The WRITE transfer of §9.5–§9.7, written from the text: the requests in order and the
/// media they leave.
fn oracle(
    ram: &[u8],
    media: &[Vec<u8>],
    lba: u32,
    addr: u32,
    count: u32,
) -> (Vec<Op>, Vec<Vec<u8>>) {
    let mut ops = Vec::new();
    let mut media = media.to_vec();
    for k in 0..count as usize {
        let mut block = Vec::new();
        for j in 0..BEATS {
            let at = addr as usize + k * BLOCK + j * BEAT;
            let bytes = ram[at..at + BEAT].to_vec();
            block.extend_from_slice(&bytes);
            ops.push(Op::Beat(at as u64, bytes));
        }
        let target = lba as usize + k;
        ops.push(Op::Write(target as u64, block.clone()));
        media[target] = block;
    }
    (ops, media)
}

fn command_record(op: u64, lba: u64, addr: u64, count: u64, accepted: bool) -> Traced {
    (
        COMMAND_KIND,
        vec![
            ("op", Value::U64(op)),
            ("lba", Value::U64(lba)),
            ("addr", Value::U64(addr)),
            ("count", Value::U64(count)),
            ("accepted", Value::Bool(accepted)),
        ],
    )
}

fn done_record(error: u64) -> Traced {
    (DONE_KIND, vec![("error", Value::U64(error))])
}

/// Runs a WRITE in a fresh world and checks it against the oracle; the world afterwards.
fn write_and_check(lba: u32, addr: u32, count: u32, irq: bool) -> World {
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, u32::from(irq));
    let media = w.media.clone();
    w.command(WRITE, lba, addr, count);
    assert!(
        w.wake && w.log.is_empty(),
        "the engine starts at Wake(ISSUE)"
    );
    w.run();
    let (expected, final_media) = oracle(&w.ram, &media, lba, addr, count);
    assert_eq!(w.ops(), expected);
    assert!(w.media == final_media, "media differs from the oracle");
    assert!(w.ram == ram(), "a WRITE never changes RAM");
    assert_eq!(w.status(), DONE);
    assert_eq!(w.levels, if irq { vec![true] } else { vec![] });
    assert_eq!(
        w.traced,
        vec![
            command_record(2, lba.into(), addr.into(), count.into(), true),
            done_record(0),
        ]
    );
    w
}

// --- WRITE flow ---

#[test]
fn a_one_block_write_is_32_beats_then_one_write_block() {
    let w = write_and_check(5, 0x1200, 1, false);
    assert_eq!(w.log.len(), BEATS + 1);
    for (j, req) in w.log[..BEATS].iter().enumerate() {
        assert_eq!(
            *req,
            Req::Beat {
                txn: j as u64,
                addr: 0x1200 + 16 * j as u64
            }
        );
    }
    let Req::Write { txn, lba, data } = &w.log[BEATS] else {
        panic!("{:?}", w.log[BEATS])
    };
    assert_eq!((*txn, *lba), (0, 5));
    assert_eq!(*data, ram()[0x1200..0x1400]);
    // Only block 5 changed.
    for (b, block) in w.media.iter().enumerate() {
        if b == 5 {
            assert_eq!(block[..], ram()[0x1200..0x1400]);
        } else {
            assert_eq!(*block, old_media()[b], "block {b}");
        }
    }
}

#[test]
fn the_write_block_payload_is_the_beats_in_order() {
    let w = write_and_check(0, 0x2000, 1, false);
    let Some(Req::Write { data, .. }) = w.log.last() else {
        panic!()
    };
    // Beat j of the RAM at 0x2000 is RAM beat 0x200 + j: its number leads its 16 bytes.
    for j in 0..BEATS {
        let chunk = &data[j * BEAT..][..BEAT];
        let number = u16::from_le_bytes([chunk[0], chunk[1]]);
        assert_eq!(number as usize, 0x200 + j, "beat {j}");
        assert_eq!(*chunk, ram()[0x2000 + j * BEAT..][..BEAT]);
    }
}

#[test]
fn a_multi_block_write_reads_each_block_then_writes_it() {
    let w = write_and_check(3, 0x1000, 2, false);
    assert_eq!(w.log.len(), 2 * (BEATS + 1));
    // 32 beats, WriteBlock(3), 32 beats, WriteBlock(4), and nothing interleaves.
    for (n, req) in w.log.iter().enumerate() {
        match (n, req) {
            (32, Req::Write { txn: 0, lba: 3, .. }) | (65, Req::Write { txn: 1, lba: 4, .. }) => {}
            (n, Req::Beat { txn, addr }) if n != 32 && n != 65 => {
                let k = if n < 32 { n } else { n - 1 };
                assert_eq!((*txn, *addr), (k as u64, 0x1000 + 16 * k as u64));
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(w.media[3][..], ram()[0x1000..0x1200]);
    assert_eq!(w.media[4][..], ram()[0x1200..0x1400]);
    // The largest case: every block of the controller from the top of the aperture.
    let w = write_and_check(0, 0x3000, 16, false);
    assert_eq!(w.log.len(), 16 * (BEATS + 1));
    for b in 0..16 {
        assert_eq!(w.media[b][..], ram()[0x3000 + b * BLOCK..][..BLOCK]);
    }
}

#[test]
fn the_buffer_grows_by_one_beat_and_is_dropped_once_write_block_is_outstanding() {
    let mut w = World::new();
    w.command(WRITE, 7, 0x1400, 2);
    // Accepted: `Issue` at block 0, beat 0, empty buffer, the wake pending.
    let v = view(&w.c);
    assert_eq!((v.engine, v.block, v.beat, v.buffer.len()), (1, 0, 0, 0));
    let mut positions = vec![(1, 0, 0, 0)];
    while w.step() {
        let v = view(&w.c);
        if !v.busy {
            break;
        }
        positions.push((v.engine, v.block, v.beat, v.buffer.len()));
        let at = 0x1400 + v.block as usize * BLOCK;
        match v.engine {
            // Issuing or waiting for beat j: the 16j bytes read so far.
            1 | 3 if v.beat < 32 => {
                assert_eq!(v.buffer.len(), 16 * v.beat as usize);
                assert_eq!(v.buffer, ram()[at..at + v.buffer.len()]);
            }
            // After beat 31: all 512 bytes, until the `WriteBlock` is sent.
            1 => {
                assert_eq!((v.beat, v.buffer.len()), (32, 512));
                assert_eq!(v.buffer, ram()[at..at + BLOCK]);
            }
            // `WriteBlock` outstanding: none, since the request carries the data.
            2 => assert_eq!((v.beat, v.buffer.len()), (32, 0)),
            other => panic!("unreachable engine {other}"),
        }
    }
    // The exact positions of block 0, then the start of block 1.
    let mut expected = vec![(1, 0, 0, 0)];
    for j in 0..32u8 {
        expected.push((3, 0, j, 16 * j as usize));
        expected.push((1, 0, j + 1, 16 * (j as usize + 1)));
    }
    expected.extend([(2, 0, 32, 0), (1, 1, 0, 0), (3, 1, 0, 0), (1, 1, 1, 16)]);
    assert_eq!(positions[..expected.len()], expected[..]);
    // Completion drops everything.
    let v = view(&w.c);
    assert_eq!(
        (v.engine, v.txn, v.block, v.beat, v.buffer.len(), v.latched),
        (0, None, 0, 0, 0, None)
    );
}

#[test]
fn the_buffer_boundaries_are_exact() {
    let mut w = World::new();
    w.command(WRITE, 0, 0x1000, 1);
    // Steps to the position where beat `j` is outstanding.
    let to_beat = |w: &mut World, j: u8| {
        while !(view(&w.c).engine == 3 && view(&w.c).beat == j) {
            assert!(w.step());
        }
        view(&w.c).buffer.len()
    };
    assert_eq!(to_beat(&mut w, 0), 0);
    assert_eq!(to_beat(&mut w, 1), 16);
    assert_eq!(to_beat(&mut w, 15), 240);
    assert_eq!(to_beat(&mut w, 31), 496);
    // Beat 31's response: 512, and the next request is the `WriteBlock`.
    w.step();
    let v = view(&w.c);
    assert_eq!((v.engine, v.beat, v.buffer.len()), (1, 32, 512));
    assert!(w.wake);
    w.step();
    let v = view(&w.c);
    assert_eq!((v.engine, v.beat, v.buffer.len()), (2, 32, 0));
    assert!(matches!(&w.outstanding, Some(Req::Write { data, .. }) if data.len() == 512));
}

#[test]
fn inspect_shows_the_engine_position() {
    let mut w = World::new();
    w.command(WRITE, 1, 0x1000, 2);
    let at = |w: &World| {
        let f = w.c.inspect().fields;
        let get = |n: &str| f.iter().find(|(k, _)| *k == n).unwrap().1.clone();
        (get("engine"), get("block"), get("beat"))
    };
    let s = |x: &str| Value::Str(x.into());
    assert_eq!(at(&w), (s("issue"), Value::U64(0), Value::U64(0)));
    w.step();
    assert_eq!(at(&w), (s("wait_beat"), Value::U64(0), Value::U64(0)));
    for _ in 0..2 * 20 {
        w.step();
    }
    assert_eq!(at(&w), (s("wait_beat"), Value::U64(0), Value::U64(20)));
    for _ in 0..2 * 12 - 1 {
        w.step();
    }
    assert_eq!(at(&w), (s("issue"), Value::U64(0), Value::U64(32)));
    w.step();
    assert_eq!(at(&w), (s("wait_media"), Value::U64(0), Value::U64(32)));
    w.step();
    assert_eq!(at(&w), (s("issue"), Value::U64(1), Value::U64(0)));
    w.run();
    assert_eq!(at(&w), (s("idle"), Value::U64(0), Value::U64(0)));
}

// --- Completion and the interrupt ---

#[test]
fn completion_sets_done_with_error_0_and_follows_irq_enable() {
    // Disabled: DONE, no level; enabling afterwards asserts it.
    let mut w = write_and_check(0, 0x1000, 1, false);
    assert_eq!(w.read_reg(0x18), 1);
    assert_eq!(w.write_reg(R_IRQ_ENABLE, 1), WriteOutcome::Done);
    assert_eq!(w.levels, [true]);
    // Enabled: exactly one assertion at completion, one deassertion at ACK.
    let mut w = write_and_check(2, 0x1800, 3, true);
    assert_eq!(w.write_reg(R_ACK, 1), WriteOutcome::Done);
    assert_eq!(w.levels, [true, false]);
    assert_eq!(w.status(), 0);
    // Nothing is asserted while the engine runs, and only the last `WriteResult`
    // completes.
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, 1);
    w.command(WRITE, 0, 0x1000, 2);
    while w.view_busy() {
        assert!(w.levels.is_empty());
        w.step();
    }
    assert!(matches!(w.log.last(), Some(Req::Write { lba: 1, .. })));
    assert_eq!(w.levels, [true]);
    // A READ after ACK completes and asserts again, and the counters carry on.
    w.write_reg(R_ACK, 1);
    w.command(READ, 4, 0x2000, 1);
    assert_eq!((view(&w.c).dma_txn, view(&w.c).blk_txn), (64, 2));
}

#[test]
fn busy_holds_at_every_stage_and_a_second_command_is_rejected() {
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, 1);
    let media = w.media.clone();
    w.command(WRITE, 6, 0x2400, 2);
    let mut n = 0;
    while w.view_busy() {
        let rejected = if n > 0 { REJECTED } else { 0 };
        assert_eq!(w.status(), BUSY | rejected);
        assert_eq!(w.read_reg(0x18), 0);
        // Every stage of the first block, then every fifth event.
        if n < 70 || n % 5 == 0 {
            // A command at this stage: REJECTED, nothing else changes, nothing starts.
            let before = view(&w.c);
            let (log, wake, outstanding) = (w.log.len(), w.wake, w.outstanding.clone());
            let traced = w.traced.len();
            for command in [READ, WRITE, 0] {
                assert_eq!(w.write_reg(R_COMMAND, command), WriteOutcome::Done);
            }
            assert_eq!(w.traced[traced..], vec![(REJECTED_KIND, vec![]); 3][..]);
            assert_eq!(
                view(&w.c),
                View {
                    rejected: true,
                    ..before
                }
            );
            assert_eq!(
                (w.log.len(), w.wake, w.outstanding.clone()),
                (log, wake, outstanding)
            );
            assert!(w.levels.is_empty());
        }
        w.step();
        n += 1;
    }
    let (expected, final_media) = oracle(&w.ram, &media, 6, 0x2400, 2);
    assert_eq!(w.ops(), expected);
    assert!(w.media == final_media);
    assert_eq!(w.status(), DONE | REJECTED);
    assert_eq!(w.levels, [true]);
}

// --- The descriptor latch ---

#[test]
fn the_transfer_uses_the_latched_descriptor_not_the_registers() {
    let mut w = World::new();
    let media = w.media.clone();
    w.command(WRITE, 1, 0x1200, 3);
    // Rewrite the registers at once, mid-block, while the `WriteBlock` is outstanding,
    // and between blocks.
    w.registers(9, 0x3000, 1);
    for _ in 0..40 {
        w.step();
    }
    w.registers(12, 0x4000, 2);
    while view(&w.c).engine != 2 {
        w.step();
    }
    w.registers(4, 0x2000, 5);
    w.step();
    w.registers(0, 0x1000, 4);
    w.run();
    let (expected, final_media) = oracle(&w.ram, &media, 1, 0x1200, 3);
    assert_eq!(w.ops(), expected);
    assert!(w.media == final_media);
    assert_eq!(
        (
            w.read_reg(R_LBA),
            w.read_reg(R_MEM_ADDR),
            w.read_reg(R_BLOCK_COUNT)
        ),
        (0, 0x1000, 4)
    );
}

// --- Transaction identity ---

#[test]
fn every_request_gets_a_fresh_txn_from_its_ports_counter() {
    let mut w = write_and_check(0, 0x1000, 2, false);
    let blk: Vec<u64> = w
        .log
        .iter()
        .filter_map(|r| match r {
            Req::Write { txn, .. } => Some(*txn),
            _ => None,
        })
        .collect();
    let dma: Vec<u64> = w
        .log
        .iter()
        .filter_map(|r| match r {
            Req::Beat { txn, .. } => Some(*txn),
            _ => None,
        })
        .collect();
    assert_eq!(blk, [0, 1]);
    assert_eq!(dma, (0..64).collect::<Vec<_>>());
    assert_eq!((view(&w.c).dma_txn, view(&w.c).blk_txn), (64, 2));
    // The counters carry over to the next command: nothing is reused.
    w.write_reg(R_ACK, 1);
    w.command(WRITE, 9, 0x2000, 1);
    w.run();
    let later: Vec<Req> = w.log[66..].to_vec();
    assert_eq!(later.len(), 33);
    for (j, req) in later[..32].iter().enumerate() {
        assert!(
            matches!(req, Req::Beat { txn, .. } if *txn == 64 + j as u64),
            "{req:?}"
        );
    }
    assert!(matches!(later[32], Req::Write { txn: 2, lba: 9, .. }));
}

/// Requires delivering `msg` on `port` in `phase` to fault the session and change
/// nothing, sending nothing.
fn assert_violation(w: &mut World, port: PortId, msg: Message, phase: Phase) {
    let before = snapshot_of(&w.c);
    let mut ctx = MockCtx::new(phase);
    let result = ctx.deliver_msg(&mut w.c, port, msg.clone());
    assert!(
        matches!(result, Err(SimError::ComponentFault(_))),
        "{port:?} {msg:?} in {phase:?}: {result:?}"
    );
    assert!(ctx.order.is_empty() && ctx.traced.is_empty(), "{msg:?}");
    assert_eq!(snapshot_of(&w.c), before, "{msg:?}");
}

fn read_resp(txn: u64, len: usize) -> Message {
    MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Data {
            data: vec![0x77; len],
        },
    }
    .into()
}

fn write_result(txn: u64) -> Message {
    BlockMsg::WriteResult {
        txn: TxnId(txn),
        outcome: BlockWriteOutcome::Done,
    }
    .into()
}

fn assert_second_wake_faults(w: &mut World) {
    let before = snapshot_of(&w.c);
    let mut ctx = MockCtx::new(Phase::Request);
    let result =
        w.c.handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx);
    assert!(matches!(result, Err(SimError::ComponentFault(_))));
    assert!(ctx.order.is_empty());
    assert_eq!(snapshot_of(&w.c), before);
}

#[test]
fn a_result_must_match_the_one_outstanding_request() {
    let mut w = World::new();
    // Advance the counters so stale and future txns exist on both ports.
    w.command(WRITE, 0, 0x1000, 1);
    w.run();
    w.write_reg(R_ACK, 1);
    w.command(WRITE, 1, 0x1200, 2);
    // `Issue`, wake pending: nothing is outstanding.
    for (port, msg) in [
        (DMA_PORT, read_resp(32, 16)),
        (DMA_PORT, read_resp(31, 16)),
        (BLK_PORT, write_result(1)),
        (BLK_PORT, write_result(0)),
    ] {
        assert_violation(&mut w, port, msg, Phase::Complete);
    }
    w.step();
    // `WaitBeat { txn: 32 }`, beat 0.
    assert_eq!(
        w.outstanding,
        Some(Req::Beat {
            txn: 32,
            addr: 0x1200
        })
    );
    let wrong_beat = |txn: u64| -> Vec<(PortId, Message, Phase)> {
        vec![
            // Stale, future, and far-off txns.
            (DMA_PORT, read_resp(txn - 1, 16), Phase::Complete),
            (DMA_PORT, read_resp(txn + 1, 16), Phase::Complete),
            (DMA_PORT, read_resp(u64::MAX, 16), Phase::Complete),
            // Not 16 bytes: never truncated or padded.
            (DMA_PORT, read_resp(txn, 0), Phase::Complete),
            (DMA_PORT, read_resp(txn, 15), Phase::Complete),
            (DMA_PORT, read_resp(txn, 17), Phase::Complete),
            (DMA_PORT, read_resp(txn, 32), Phase::Complete),
            // Outside `Complete`.
            (DMA_PORT, read_resp(txn, 16), Phase::Request),
            (DMA_PORT, read_resp(txn, 16), Phase::Transfer),
            (DMA_PORT, read_resp(txn, 16), Phase::Commit),
            // The wrong kind: a READ's beat response, or a request.
            (
                DMA_PORT,
                MemMsg::WriteResp {
                    txn: TxnId(txn),
                    outcome: WriteOutcome::Done,
                }
                .into(),
                Phase::Complete,
            ),
            (
                DMA_PORT,
                MemMsg::ReadReq {
                    txn: TxnId(txn),
                    addr: 0x1200,
                    len: 16,
                }
                .into(),
                Phase::Complete,
            ),
            // The wrong port or protocol.
            (BLK_PORT, write_result(1), Phase::Complete),
            (BLK_PORT, read_resp(txn, 16), Phase::Complete),
            (MEM_PORT, read_resp(txn, 16), Phase::Complete),
            (IRQ_PORT, read_resp(txn, 16), Phase::Complete),
            (
                DMA_PORT,
                IrqMsg::Level { asserted: true }.into(),
                Phase::Complete,
            ),
        ]
    };
    for (port, msg, phase) in wrong_beat(32) {
        assert_violation(&mut w, port, msg, phase);
    }
    assert_second_wake_faults(&mut w);
    // Mid-block: beat 9 outstanding, the buffer holding 144 bytes.
    for _ in 0..18 {
        w.step();
    }
    assert!(matches!(w.outstanding, Some(Req::Beat { txn: 41, .. })));
    assert_eq!(view(&w.c).buffer.len(), 144);
    for (port, msg, phase) in wrong_beat(41) {
        assert_violation(&mut w, port, msg, phase);
    }
    // Buffer full, `WriteBlock` due: nothing is outstanding.
    while view(&w.c).beat != 32 {
        w.step();
    }
    assert!(w.wake);
    assert_violation(&mut w, DMA_PORT, read_resp(63, 16), Phase::Complete);
    assert_violation(&mut w, BLK_PORT, write_result(1), Phase::Complete);
    w.step();
    // `WaitMedia { txn: 1 }`.
    assert!(matches!(
        w.outstanding,
        Some(Req::Write { txn: 1, lba: 1, .. })
    ));
    let wrong: Vec<(PortId, Message, Phase)> = vec![
        (BLK_PORT, write_result(0), Phase::Complete),
        (BLK_PORT, write_result(2), Phase::Complete),
        (BLK_PORT, write_result(u64::MAX), Phase::Complete),
        (BLK_PORT, write_result(1), Phase::Request),
        (BLK_PORT, write_result(1), Phase::Transfer),
        (BLK_PORT, write_result(1), Phase::Commit),
        // A READ's media result, or a request.
        (
            BLK_PORT,
            BlockMsg::ReadResult {
                txn: TxnId(1),
                outcome: BlockReadOutcome::Data { data: vec![0; 512] },
            }
            .into(),
            Phase::Complete,
        ),
        (
            BLK_PORT,
            BlockMsg::WriteBlock {
                txn: TxnId(1),
                lba: 1,
                data: vec![0; 512],
            }
            .into(),
            Phase::Complete,
        ),
        (DMA_PORT, read_resp(63, 16), Phase::Complete),
        (DMA_PORT, write_result(1), Phase::Complete),
        (MEM_PORT, write_result(1), Phase::Complete),
    ];
    for (port, msg, phase) in wrong {
        assert_violation(&mut w, port, msg, phase);
    }
    assert_second_wake_faults(&mut w);
    // The run then completes normally, and a late duplicate faults.
    w.run();
    assert_eq!(w.status(), DONE);
    assert_violation(&mut w, BLK_PORT, write_result(2), Phase::Complete);
    assert_violation(&mut w, DMA_PORT, read_resp(95, 16), Phase::Complete);
}

/// The snapshot of an IDLE controller whose next `dma` and `blk` txns are these.
fn idle_with_counters(dma_txn: u64, blk_txn: u64) -> DmaBlockController {
    let mut bytes = snapshot_of(&DmaBlockController::new(config()).unwrap());
    let n = bytes.len();
    bytes[n - 17..n - 9].copy_from_slice(&dma_txn.to_le_bytes());
    bytes[n - 9..n - 1].copy_from_slice(&blk_txn.to_le_bytes());
    let mut c = DmaBlockController::new(config()).unwrap();
    restore_into(&mut c, &bytes).unwrap();
    c
}

/// Delivers `Wake(ISSUE)` and requires a session fault with nothing sent and nothing
/// changed.
fn assert_exhausted(w: &mut World) {
    assert!(w.wake);
    let before = snapshot_of(&w.c);
    let mut ctx = MockCtx::new(Phase::Request);
    let result =
        w.c.handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx);
    assert!(
        matches!(result, Err(SimError::ComponentFault(_))),
        "{result:?}"
    );
    assert!(ctx.order.is_empty(), "nothing sent");
    assert_eq!(snapshot_of(&w.c), before, "counter and engine unchanged");
}

#[test]
fn txn_counters_never_wrap() {
    // `dma`: 32 beats end exactly at u64::MAX - 1; the next beat faults.
    let mut w = World::with(idle_with_counters(u64::MAX - 32, 0));
    w.command(WRITE, 0, 0x1000, 1);
    w.run();
    assert!(matches!(w.log[31], Req::Beat { txn, .. } if txn == u64::MAX - 1));
    assert_eq!(view(&w.c).dma_txn, u64::MAX);
    w.write_reg(R_ACK, 1);
    w.command(WRITE, 1, 0x1000, 1);
    assert_exhausted(&mut w);
    assert_eq!(view(&w.c).buffer.len(), 0);
    // `blk`: the last allocatable txn is u64::MAX - 1; the next `WriteBlock` faults after
    // all 32 beats, with the full buffer kept and the media untouched.
    let mut w = World::with(idle_with_counters(0, u64::MAX - 1));
    w.command(WRITE, 0, 0x1000, 1);
    w.run();
    assert!(matches!(w.log[32], Req::Write { txn, .. } if txn == u64::MAX - 1));
    assert_eq!(view(&w.c).blk_txn, u64::MAX);
    w.write_reg(R_ACK, 1);
    let media = w.media.clone();
    w.command(WRITE, 1, 0x1200, 1);
    for _ in 0..2 * BEATS {
        w.step();
    }
    assert_exhausted(&mut w);
    let v = view(&w.c);
    assert_eq!((v.engine, v.beat, v.buffer.len()), (1, 32, 512));
    assert!(w.media == media);
}

// --- Failure paths: M2.7c; here only shown not to panic ---

#[test]
fn engine_failure_results_do_not_panic() {
    let mut w = World::new();
    w.command(WRITE, 0, 0x1000, 2);
    for _ in 0..5 {
        w.step();
    }
    let Some(Req::Beat { txn, .. }) = w.outstanding.clone() else {
        panic!()
    };
    let fault = MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    let mut ctx = MockCtx::new(Phase::Complete);
    assert!(ctx.deliver_msg(&mut w.c, DMA_PORT, fault.into()).is_ok());
    assert!(!view(&w.c).busy);
    let mut w = World::new();
    w.command(WRITE, 0, 0x1000, 2);
    for _ in 0..2 * BEATS + 1 {
        w.step();
    }
    let Some(Req::Write { txn, .. }) = w.outstanding.clone() else {
        panic!()
    };
    let error = BlockMsg::WriteResult {
        txn: TxnId(txn),
        outcome: BlockWriteOutcome::Error {
            error: MediaError::BadBlock,
        },
    };
    let mut ctx = MockCtx::new(Phase::Complete);
    assert!(ctx.deliver_msg(&mut w.c, BLK_PORT, error.into()).is_ok());
    assert!(!view(&w.c).busy);
}

// --- Checkpoints (mock) ---

/// A named position and its test on the snapshot.
type Point = (&'static str, fn(&View) -> bool);

/// Named positions of a three-block WRITE, each identified from the snapshot.
fn named_points() -> Vec<Point> {
    vec![
        ("accepted, before the first beat", |v| {
            v.busy && v.engine == 1 && v.block == 0 && v.beat == 0
        }),
        ("beat 0 outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 0
        }),
        ("mid-block, issuing beat 7", |v| {
            v.engine == 1 && v.block == 0 && v.beat == 7
        }),
        ("mid-block, beat 7 outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 7
        }),
        ("beat 31 outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 31
        }),
        ("buffer full, before WriteBlock", |v| {
            v.engine == 1 && v.block == 0 && v.beat == 32
        }),
        ("WriteBlock outstanding", |v| v.engine == 2 && v.block == 0),
        ("between blocks", |v| {
            v.engine == 1 && v.block == 1 && v.beat == 0
        }),
        ("second block, beat 20 outstanding", |v| {
            v.engine == 3 && v.block == 1 && v.beat == 20
        }),
        ("final WriteBlock outstanding", |v| {
            v.engine == 2 && v.block == 2
        }),
        ("completed", |v| v.done),
    ]
}

fn three_block_write(w: &mut World) {
    w.write_reg(R_IRQ_ENABLE, 1);
    w.command(WRITE, 10, 0x2200, 3);
}

#[test]
fn named_checkpoints_resume_identically() {
    let mut reference = World::new();
    three_block_write(&mut reference);
    reference.run();
    for (name, at) in named_points() {
        let mut w = World::new();
        three_block_write(&mut w);
        while !at(&view(&w.c)) {
            assert!(w.step(), "never reached {name}");
        }
        w.checkpoint();
        w.run();
        assert_eq!(w.log, reference.log, "{name}");
        assert!(w.media == reference.media, "{name}");
        assert_eq!(w.levels, reference.levels, "{name}");
        assert_eq!(w.traced, reference.traced, "{name}");
        assert_eq!(snapshot_of(&w.c), snapshot_of(&reference.c), "{name}");
    }
}

#[test]
fn a_checkpoint_after_every_event_resumes_identically() {
    let mut reference = World::new();
    three_block_write(&mut reference);
    reference.run();
    let mut w = World::new();
    w.checkpoint_every_event = true;
    three_block_write(&mut w);
    w.checkpoint();
    w.run();
    assert_eq!(w.log, reference.log);
    assert!(w.media == reference.media);
    assert_eq!(w.levels, reference.levels);
    assert_eq!(w.traced, reference.traced);
    assert_eq!(snapshot_of(&w.c), snapshot_of(&reference.c));
}

// --- Independent oracle, property ---

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn writes_match_the_oracle(
        (lba, count) in (1u32..=4).prop_flat_map(|n| (0..=(CAPACITY as u32 - n), Just(n))),
        slot in 0u32..(APERTURE as u32 / 16),
        irq in any::<bool>(),
        seed in any::<u64>(),
        checkpoint_at in prop::option::of(0usize..300),
    ) {
        // An aligned address whose transfer fits the aperture.
        let span = count * 512;
        let slots = (APERTURE as u32 - span) / 16 + 1;
        let addr = BASE as u32 + (slot % slots) * 16;
        let mut w = World::new();
        // Random RAM contents.
        let mut x = seed | 1;
        for b in w.ram.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        w.write_reg(R_IRQ_ENABLE, u32::from(irq));
        let (ram, media) = (w.ram.clone(), w.media.clone());
        w.command(WRITE, lba, addr, count);
        let mut steps = 0;
        loop {
            if Some(steps) == checkpoint_at {
                w.checkpoint();
            }
            if !w.step() {
                break;
            }
            steps += 1;
        }
        let (expected, final_media) = oracle(&ram, &media, lba, addr, count);
        prop_assert_eq!(w.ops(), expected);
        prop_assert!(w.media == final_media);
        prop_assert!(w.ram == ram);
        prop_assert_eq!(w.log.len(), count as usize * 33);
        prop_assert_eq!(steps, count as usize * 33 * 2);
        prop_assert_eq!(w.status(), DONE);
        prop_assert_eq!(w.levels.clone(), if irq { vec![true] } else { vec![] });
        let v = view(&w.c);
        prop_assert_eq!((v.dma_txn, v.blk_txn), (u64::from(count) * 32, u64::from(count)));
    }
}

// --- Runtime ---

const TICKS_PER_CYCLE: u64 = 1000;
const MMIO: u64 = 0x2000_0000;

/// An `irq.v0` target that accepts levels in `Complete`; the runtime records them.
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

struct Ids {
    host: ComponentId,
    ctl: ComponentId,
    media: ComponentId,
    sink: ComponentId,
}

/// A host and the controller's `dma` as the two masters of a bus with an empty RAM at 0
/// and the controller's window at [`MMIO`]; the controller's `blk` to a
/// `SimpleBlockMedia` holding [`old_media`], its `irq` to a sink. Every link is one cycle.
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
                size: RAM_SIZE as u64,
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
            size: RAM_SIZE as u64,
            latency: cycles(1),
        },
        &RamImage {
            image_hash: [0; 32],
            segments: vec![],
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let bytes = old_media().concat();
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: CAPACITY,
            latency: cycles(2),
            bad_blocks: BTreeSet::new(),
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
            ctl,
            media,
            sink,
        },
    )
}

fn w32(offset: u64, value: u32) -> MemMsg {
    write(0, MMIO + offset, &value.to_le_bytes())
}

/// The host fills `[addr, addr + 512 count)` with [`ram`]'s bytes in 16-byte writes,
/// then WRITEs `count` blocks from there to `lba` with the interrupt enabled, rewrites
/// the registers while it runs, and has a second command rejected. At cycle `readback`
/// it reads `STATUS`, ACKs, READs the same blocks back to `back`, and at `readback +
/// 2000` reads `STATUS` and `[back, back + 512 count)` in 16-byte reads. Every request
/// has its own txn.
fn workload(lba: u32, addr: u32, count: u32, back: u32, readback: u64) -> Vec<(u64, MemMsg)> {
    let pattern = ram();
    let span = count as usize * BLOCK;
    let mut requests: Vec<(u64, MemMsg)> = (0..span)
        .step_by(BEAT)
        .enumerate()
        .map(|(k, at)| {
            let a = addr as usize + at;
            (k as u64, write(0, a as u64, &pattern[a..a + BEAT]))
        })
        .collect();
    let t = requests.len() as u64 + 4;
    requests.extend([
        (t, w32(R_LBA, lba)),
        (t + 1, w32(R_MEM_ADDR, addr)),
        (t + 2, w32(R_BLOCK_COUNT, count)),
        (t + 3, w32(R_IRQ_ENABLE, 1)),
        (t + 4, w32(R_COMMAND, WRITE)),
        (t + 6, w32(R_LBA, 0)),
        (t + 7, w32(R_MEM_ADDR, back)),
        (t + 8, w32(R_BLOCK_COUNT, 1)),
        (t + 10, w32(R_COMMAND, READ)),
        (readback, read(0, MMIO + R_STATUS, 4)),
        (readback + 1, w32(R_ACK, 1)),
        (readback + 2, w32(R_LBA, lba)),
        (readback + 3, w32(R_BLOCK_COUNT, count)),
        (readback + 4, w32(R_COMMAND, READ)),
        (readback + 2000, read(0, MMIO + R_STATUS, 4)),
    ]);
    for (k, at) in (0..span).step_by(BEAT).enumerate() {
        requests.push((
            readback + 2001 + k as u64,
            read(0, u64::from(back) + at as u64, 16),
        ));
    }
    for (i, (_, msg)) in requests.iter_mut().enumerate() {
        match msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => *txn = TxnId(i as u64),
            _ => unreachable!(),
        }
    }
    requests
}

fn run_all(rt: &mut Runtime) -> Vec<Dispatched> {
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    events
}

fn cycle(e: &Dispatched) -> u64 {
    e.key.tick.0 / TICKS_PER_CYCLE
}

#[test]
fn a_runtime_write_puts_the_ram_bytes_on_the_media() {
    let (lba, addr, count, back, readback) = (9, 0x1600, 3, 0x3000, 3000);
    let requests = workload(lba, addr, count, back, readback);
    let (mut rt, ids) = build(&requests);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_all(&mut rt);
    let pattern = ram();
    let block = |k: usize| &pattern[addr as usize + k * BLOCK..][..BLOCK];
    // The media received exactly the three WriteBlocks, in order, each the exact RAM
    // block, then the READ's three ReadBlocks.
    let media: Vec<BlockMsg> = events
        .iter()
        .filter(|e| e.target == ids.media)
        .map(|e| {
            assert_eq!(e.key.phase, Phase::Request);
            match &e.delivery {
                Delivered::Message {
                    msg: Message::Block(m),
                    ..
                } => m.clone(),
                other => panic!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(media.len(), 6);
    for (k, msg) in media[..3].iter().enumerate() {
        let BlockMsg::WriteBlock { txn, lba: at, data } = msg else {
            panic!("{msg:?}")
        };
        assert_eq!((txn.0, *at), (k as u64, u64::from(lba) + k as u64));
        assert!(data[..] == *block(k), "WriteBlock {k}");
    }
    for (k, msg) in media[3..].iter().enumerate() {
        assert_eq!(
            *msg,
            BlockMsg::ReadBlock {
                txn: TxnId(3 + k as u64),
                lba: u64::from(lba) + k as u64
            }
        );
    }
    // Host responses: STATUS reads DONE | REJECTED (REJECTED is sticky) at the read-back
    // and at the end, and the blocks read back from the media are the RAM's.
    let responses: Vec<(u64, MemMsg)> = events
        .iter()
        .filter(|e| e.target == ids.host)
        .map(|e| {
            let Delivered::Message {
                msg: Message::MemV1(m),
                ..
            } = &e.delivery
            else {
                panic!("{e:?}")
            };
            let txn = match m {
                MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => txn.0,
                other => panic!("{other:?}"),
            };
            (txn, m.clone())
        })
        .collect();
    assert_eq!(responses.len(), requests.len());
    let data = |txn: u64| match &responses.iter().find(|(t, _)| *t == txn).unwrap().1 {
        MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } => data.clone(),
        other => panic!("{other:?}"),
    };
    let base = (count as usize * BLOCK / BEAT) as u64;
    assert_eq!(data(base + 9), (DONE | REJECTED).to_le_bytes());
    assert_eq!(data(base + 14), (DONE | REJECTED).to_le_bytes());
    let back_bytes: Vec<u8> = (base + 15..requests.len() as u64).flat_map(data).collect();
    assert!(back_bytes[..] == pattern[addr as usize..][..count as usize * BLOCK]);
    // The WRITE's engine events strictly alternate wake, result: 32 beats and a
    // WriteBlock per block, each wake one cycle after the result before it.
    let engine: Vec<&Dispatched> = events
        .iter()
        .filter(|e| e.target == ids.ctl)
        .filter(|e| !matches!(e.delivery, Delivered::Message { port: MEM_PORT, .. }))
        .take(2 * 3 * 33)
        .collect();
    for (n, pair) in engine.chunks(2).enumerate() {
        assert!(matches!(pair[0].delivery, Delivered::Wake { token: ISSUE }));
        assert_eq!(pair[0].key.phase, Phase::Request);
        let port = if n % 33 == 32 { BLK_PORT } else { DMA_PORT };
        assert!(
            matches!(pair[1].delivery, Delivered::Message { port: p, .. } if p == port),
            "event {n}"
        );
        assert_eq!(pair[1].key.phase, Phase::Complete);
    }
    for w in engine[1..].windows(2).step_by(2) {
        assert_eq!(cycle(w[1]), cycle(w[0]) + 1);
    }
    // Two completions, two assertions, one ACK between them.
    let levels: Vec<bool> = events
        .iter()
        .filter(|e| e.target == ids.sink)
        .map(|e| match e.delivery {
            Delivered::Message {
                msg: Message::Irq(IrqMsg::Level { asserted }),
                ..
            } => asserted,
            _ => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(levels, [true, false, true]);
    // Trace: the WRITE, the rejection, its completion before the read-back, then the READ.
    let trace = rt.take_trace().unwrap();
    let (at, traced): (Vec<u64>, Vec<Traced>) = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == ids.ctl)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0 / TICKS_PER_CYCLE, (r.kind, r.fields.clone()))
        })
        .unzip();
    assert_eq!(
        traced,
        [
            command_record(2, lba.into(), addr.into(), count.into(), true),
            (REJECTED_KIND, vec![]),
            done_record(0),
            command_record(1, lba.into(), back.into(), count.into(), true),
            done_record(0),
        ]
    );
    assert!(at[2] < readback);
    // Timing: the first wake one cycle after the accepting MMIO write.
    assert_eq!(cycle(engine[0]), at[0] + 1);
}

fn checkpoint_workload() -> Vec<(u64, MemMsg)> {
    workload(12, 0x4c00, 2, 0x1000, 1200)
}

#[test]
fn runtime_checkpoints_at_every_event_boundary_resume_identically() {
    let requests = checkpoint_workload();
    let reference = {
        let (mut rt, _) = build(&requests);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events = run_all(&mut rt);
        let snap = rt.snapshot().unwrap();
        (
            events,
            rt.take_trace().unwrap(),
            snap,
            rt.state_digest().unwrap(),
            rt.execution_digest(),
        )
    };
    // The WRITE must be finished before the read-back.
    assert!(reference.1.records.iter().any(|r| r.kind == DONE_KIND
        && matches!(r.at, TraceAt::Event(k) if k.tick.0 < 1200 * TICKS_PER_CYCLE)));
    for k in 0..=reference.0.len() {
        let (mut rt, _) = build(&requests);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let prefix = rt.take_trace().unwrap();
        let (mut fresh, _) = build(&requests);
        fresh.restore(&bytes).unwrap();
        assert_eq!(
            fresh.snapshot().unwrap(),
            bytes,
            "checkpoint after {k} events"
        );
        fresh.resume_trace(prefix).unwrap();
        let rest = run_all(&mut fresh);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(
            fresh.snapshot().unwrap(),
            reference.2,
            "checkpoint after {k}"
        );
        assert_eq!(
            fresh.take_trace().unwrap(),
            reference.1,
            "checkpoint after {k}"
        );
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.3, reference.4),
            "checkpoint after {k} events"
        );
    }
}

#[test]
fn runtime_writes_are_deterministic() {
    let run = || {
        let (mut rt, _) = build(&checkpoint_workload());
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events = run_all(&mut rt);
        (
            events,
            rt.snapshot().unwrap(),
            rt.state_digest().unwrap(),
            rt.execution_digest(),
            rt.take_trace().unwrap(),
        )
    };
    assert_eq!(run(), run());
}
