//! `DmaBlockController` READ engine (`docs/m2-design.md` §9.5, §9.6, §9.9; M2.7a):
//! `Wake(ISSUE)` scheduling, `ReadBlock`, the 512-byte block buffer, 16-byte RAM write
//! beats, one outstanding request in total, multi-block progression, fresh checked
//! `TxnId`s, completion and its interrupt, the descriptor latch while the engine runs,
//! protocol violations, snapshots of the reachable READ positions, and checkpoints. The
//! WRITE engine is tested in `dma_write.rs`.
//!
//! Most tests drive the controller through [`World`]: a mock media and RAM that answer
//! the controller's one outstanding request at a time, standing in for the runtime's
//! event queue. `World` checks every event: a wake sends exactly one request, `Now` in
//! `Request`; a result sends nothing and schedules the next wake or completes; a wake and
//! an outstanding request never coexist. An independent oracle predicts the requests and
//! the final RAM. Then a real runtime with a bus, a RAM, and a `SimpleBlockMedia` checks
//! the RAM bytes through the bus, the timing, every-event checkpoints, and determinism.
//!
//! The engine's failure paths (§9.7) are covered by `dma_faults.rs`; here they are only
//! shown not to panic.

mod common;

use std::collections::{BTreeMap, BTreeSet};

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

/// Block `b`'s deterministic contents: every beat of every block differs.
fn pattern(b: usize) -> Vec<u8> {
    (0..BLOCK)
        .map(|k| ((b * 97 + k * 31 + (k / BEAT) * 7) % 251) as u8 ^ (b as u8).rotate_left(3))
        .collect()
}

fn media() -> Vec<Vec<u8>> {
    (0..CAPACITY as usize).map(pattern).collect()
}

/// One engine request, as the controller sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Req {
    Read { txn: u64, lba: u64 },
    Beat { txn: u64, addr: u64, data: Vec<u8> },
}

/// A request without its `txn`, as the oracle predicts it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Read(u64),
    Beat(u64, Vec<u8>),
}

impl Req {
    fn op(&self) -> Op {
        match self {
            Req::Read { lba, .. } => Op::Read(*lba),
            Req::Beat { addr, data, .. } => Op::Beat(*addr, data.clone()),
        }
    }
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

/// The controller, a perfect media and RAM, and the one pending wake or outstanding
/// request the runtime's queue would hold.
struct World {
    c: DmaBlockController,
    media: Vec<Vec<u8>>,
    ram: Vec<u8>,
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
            media: media(),
            ram: vec![0; RAM_SIZE],
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

    fn command(&mut self, command: u32, lba: u32, addr: u32, count: u32) {
        for (offset, value) in [
            (R_LBA, lba),
            (R_MEM_ADDR, addr),
            (R_BLOCK_COUNT, count),
            (R_COMMAND, command),
        ] {
            assert_eq!(self.write_reg(offset, value), WriteOutcome::Done);
        }
    }

    /// Delivers the pending `Wake(ISSUE)`: exactly one request, `Now` in `Request`.
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
            let BlockMsg::ReadBlock { txn, lba } = b.msg else {
                panic!("{:?}", b.msg)
            };
            Req::Read { txn: txn.0, lba }
        } else {
            let s = ctx.sent.pop().unwrap();
            assert_eq!(
                (s.port, s.when, s.phase),
                (DMA_PORT, ScheduleWhen::Now, Phase::Request)
            );
            let MemMsg::WriteReq { txn, addr, data } = s.msg else {
                panic!("{:?}", s.msg)
            };
            Req::Beat {
                txn: txn.0,
                addr,
                data,
            }
        };
        self.log.push(req.clone());
        self.outstanding = Some(req);
    }

    /// The successful result of `req`, applying a beat to the RAM.
    fn result_of(&mut self, req: &Req) -> (PortId, Message) {
        match req {
            Req::Read { txn, lba } => (
                BLK_PORT,
                BlockMsg::ReadResult {
                    txn: TxnId(*txn),
                    outcome: BlockReadOutcome::Data {
                        data: self.media[*lba as usize].clone(),
                    },
                }
                .into(),
            ),
            Req::Beat { txn, addr, data } => {
                self.ram[*addr as usize..][..data.len()].copy_from_slice(data);
                (
                    DMA_PORT,
                    MemMsg::WriteResp {
                        txn: TxnId(*txn),
                        outcome: WriteOutcome::Done,
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
        // A result either schedules the next wake or completes the command.
        let completed = ctx.traced.iter().any(|(kind, _)| *kind == DONE_KIND);
        assert!(self.wake != completed, "{ctx:?}", ctx = ctx.traced);
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
}

/// The READ transfer of §9.5–§9.6, written from the text: the requests in order and the
/// RAM they leave.
fn oracle(media: &[Vec<u8>], ram: &[u8], lba: u32, addr: u32, count: u32) -> (Vec<Op>, Vec<u8>) {
    let mut ops = Vec::new();
    let mut ram = ram.to_vec();
    for k in 0..u64::from(count) {
        let block = u64::from(lba) + k;
        ops.push(Op::Read(block));
        let data = &media[block as usize];
        for j in 0..BEATS as u64 {
            let at = u64::from(addr) + k * BLOCK as u64 + j * BEAT as u64;
            let bytes = data[(j as usize) * BEAT..][..BEAT].to_vec();
            ram[at as usize..][..BEAT].copy_from_slice(&bytes);
            ops.push(Op::Beat(at, bytes));
        }
    }
    (ops, ram)
}

fn ops(log: &[Req]) -> Vec<Op> {
    log.iter().map(Req::op).collect()
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

/// Runs a READ in a fresh world and checks it against the oracle; the world afterwards.
fn read_and_check(lba: u32, addr: u32, count: u32, irq: bool) -> World {
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, u32::from(irq));
    let ram = w.ram.clone();
    w.command(READ, lba, addr, count);
    assert!(
        w.wake && w.log.is_empty(),
        "the engine starts at Wake(ISSUE)"
    );
    w.run();
    let (expected, final_ram) = oracle(&w.media, &ram, lba, addr, count);
    assert_eq!(ops(&w.log), expected);
    assert!(w.ram == final_ram, "RAM differs from the oracle");
    assert_eq!(w.status(), DONE);
    assert_eq!(w.levels, if irq { vec![true] } else { vec![] });
    assert_eq!(
        w.traced,
        vec![
            command_record(1, lba.into(), addr.into(), count.into(), true),
            done_record(0),
        ]
    );
    w
}

// --- READ flow ---

#[test]
fn a_one_block_read_is_one_read_block_then_32_beats() {
    let w = read_and_check(5, 0x1200, 1, false);
    assert_eq!(w.log.len(), 1 + BEATS);
    assert_eq!(w.log[0], Req::Read { txn: 0, lba: 5 });
    for (j, req) in w.log[1..].iter().enumerate() {
        let Req::Beat { txn, addr, data } = req else {
            panic!("{req:?}")
        };
        assert_eq!(*txn, j as u64);
        assert_eq!(*addr, 0x1200 + 16 * j as u64);
        assert_eq!(data.len(), 16);
        assert_eq!(*data, pattern(5)[16 * j..16 * (j + 1)]);
    }
    // All 512 bytes arrived, and nothing else changed.
    assert_eq!(w.ram[0x1200..0x1400], pattern(5)[..]);
    assert!(w.ram[..0x1200].iter().all(|&b| b == 0));
    assert!(w.ram[0x1400..].iter().all(|&b| b == 0));
}

#[test]
fn a_multi_block_read_reads_each_block_then_writes_it() {
    let w = read_and_check(3, 0x1000, 2, false);
    assert_eq!(w.log.len(), 2 * (1 + BEATS));
    assert_eq!(w.log[0], Req::Read { txn: 0, lba: 3 });
    assert_eq!(w.log[1 + BEATS], Req::Read { txn: 1, lba: 4 });
    // The second ReadBlock follows beat 31 of the first block, and nothing interleaves.
    assert!(
        matches!(
            w.log[BEATS],
            Req::Beat {
                txn: 31,
                addr: 0x11f0,
                ..
            }
        ),
        "{:?}",
        w.log[BEATS]
    );
    let mut concatenated = pattern(3);
    concatenated.extend(pattern(4));
    assert_eq!(w.ram[0x1000..0x1400], concatenated[..]);
    // The largest case: every block of the controller into the top of the aperture.
    let w = read_and_check(0, 0x3000, 16, false);
    assert_eq!(w.log.len(), 16 * (1 + BEATS));
    let all: Vec<u8> = (0..16).flat_map(pattern).collect();
    assert_eq!(w.ram[0x3000..0x5000], all[..]);
}

#[test]
fn the_buffer_holds_the_block_exactly_from_data_through_beat_31() {
    let mut w = World::new();
    w.command(READ, 7, 0x1400, 2);
    // Accepted: `Issue` at block 0, beat 0, empty buffer, the wake pending.
    let v = view(&w.c);
    assert_eq!((v.engine, v.block, v.beat, v.buffer.len()), (1, 0, 0, 0));
    let mut positions = Vec::new();
    while w.step() {
        let v = view(&w.c);
        if v.busy {
            positions.push((v.engine, v.block, v.beat, v.buffer.len()));
        }
        match (v.engine, v.buffer.len()) {
            (2, len) => assert_eq!(len, 0, "waiting for ReadBlock"),
            (3, len) => {
                assert_eq!(len, 512, "waiting for a beat");
                assert_eq!(v.buffer, pattern(7 + v.block as usize));
            }
            (1, 0) => assert_eq!(v.beat, 0, "issuing ReadBlock"),
            (1, 512) => assert_eq!(v.buffer, pattern(7 + v.block as usize)),
            (0, 0) => assert!(v.done),
            other => panic!("unreachable position {other:?}"),
        }
    }
    // The exact sequence of positions of block 0, then the start of block 1.
    let mut expected = vec![(2, 0, 0, 0), (1, 0, 0, 512)];
    for j in 0..32u8 {
        expected.push((3, 0, j, 512));
        if j < 31 {
            expected.push((1, 0, j + 1, 512));
        }
    }
    expected.extend([(1, 1, 0, 0), (2, 1, 0, 0), (1, 1, 0, 512)]);
    assert_eq!(positions[..expected.len()], expected[..]);
    // Completion drops everything.
    let v = view(&w.c);
    assert_eq!(
        (v.engine, v.txn, v.block, v.beat, v.buffer.len(), v.latched),
        (0, None, 0, 0, 0, None)
    );
}

#[test]
fn inspect_shows_the_engine_position() {
    let mut w = World::new();
    w.command(READ, 1, 0x1000, 2);
    let at = |w: &World| {
        let f = w.c.inspect().fields;
        let get = |n: &str| f.iter().find(|(k, _)| *k == n).unwrap().1.clone();
        (get("engine"), get("block"), get("beat"))
    };
    assert_eq!(
        at(&w),
        (Value::Str("issue".into()), Value::U64(0), Value::U64(0))
    );
    w.step();
    assert_eq!(
        at(&w),
        (
            Value::Str("wait_media".into()),
            Value::U64(0),
            Value::U64(0)
        )
    );
    for _ in 0..2 + 2 * 20 {
        w.step();
    }
    assert_eq!(
        at(&w),
        (
            Value::Str("wait_beat".into()),
            Value::U64(0),
            Value::U64(20)
        )
    );
    for _ in 0..2 * 12 + 1 {
        w.step();
    }
    assert_eq!(
        at(&w),
        (Value::Str("issue".into()), Value::U64(1), Value::U64(0))
    );
    w.run();
    assert_eq!(
        at(&w),
        (Value::Str("idle".into()), Value::U64(0), Value::U64(0))
    );
}

// --- Completion and the interrupt ---

#[test]
fn completion_sets_done_with_error_0_and_follows_irq_enable() {
    // Disabled: DONE, no level; enabling afterwards asserts it.
    let mut w = read_and_check(0, 0x1000, 1, false);
    assert_eq!(w.read_reg(0x18), 1);
    assert_eq!(w.write_reg(R_IRQ_ENABLE, 1), WriteOutcome::Done);
    assert_eq!(w.levels, [true]);
    // Enabled: exactly one assertion at completion, one deassertion at ACK.
    let mut w = read_and_check(2, 0x1800, 3, true);
    assert_eq!(w.write_reg(R_ACK, 1), WriteOutcome::Done);
    assert_eq!(w.levels, [true, false]);
    assert_eq!(w.status(), 0);
    // Nothing is asserted while the engine runs.
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, 1);
    w.command(READ, 0, 0x1000, 2);
    while w.view_busy() {
        assert!(w.levels.is_empty());
        w.step();
    }
    assert_eq!(w.levels, [true]);
    // A second READ after ACK completes and asserts again.
    w.write_reg(R_ACK, 1);
    w.command(READ, 4, 0x2000, 1);
    w.run();
    assert_eq!(w.levels, [true, false, true]);
    assert_eq!(w.ram[0x2000..0x2200], pattern(4)[..]);
}

impl World {
    fn view_busy(&self) -> bool {
        view(&self.c).busy
    }
}

#[test]
fn busy_holds_at_every_stage_and_a_second_command_is_rejected() {
    let mut w = World::new();
    w.write_reg(R_IRQ_ENABLE, 1);
    let ram = w.ram.clone();
    w.command(READ, 6, 0x2400, 2);
    let mut n = 0;
    while w.view_busy() {
        let rejected = if n > 0 { REJECTED } else { 0 };
        assert_eq!(w.status(), BUSY | rejected);
        assert_eq!(w.read_reg(0x18), 0);
        if n % 5 == 0 {
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
    let (expected, final_ram) = oracle(&w.media, &ram, 6, 0x2400, 2);
    assert_eq!(ops(&w.log), expected);
    assert!(w.ram == final_ram);
    assert_eq!(w.status(), DONE | REJECTED);
    assert_eq!(w.levels, [true]);
}

// --- The descriptor latch ---

#[test]
fn the_transfer_uses_the_latched_descriptor_not_the_registers() {
    let mut w = World::new();
    let ram = w.ram.clone();
    w.command(READ, 1, 0x1200, 3);
    // Rewrite the registers at once, and again mid-transfer.
    w.command_registers(9, 0x3000, 1);
    for _ in 0..40 {
        w.step();
    }
    w.command_registers(12, 0x4000, 2);
    for _ in 0..70 {
        w.step();
    }
    w.command_registers(0, 0x1000, 4);
    w.run();
    let (expected, final_ram) = oracle(&w.media, &ram, 1, 0x1200, 3);
    assert_eq!(ops(&w.log), expected);
    assert!(w.ram == final_ram);
    assert_eq!(
        (
            w.read_reg(R_LBA),
            w.read_reg(R_MEM_ADDR),
            w.read_reg(R_BLOCK_COUNT)
        ),
        (0, 0x1000, 4)
    );
    assert!(w.ram[0x3000..0x5000].iter().all(|&b| b == 0));
}

impl World {
    fn command_registers(&mut self, lba: u32, addr: u32, count: u32) {
        for (offset, value) in [(R_LBA, lba), (R_MEM_ADDR, addr), (R_BLOCK_COUNT, count)] {
            assert_eq!(self.write_reg(offset, value), WriteOutcome::Done);
        }
        assert!(self.levels.is_empty() || self.levels == [true]);
    }
}

// --- Transaction identity ---

#[test]
fn every_request_gets_a_fresh_txn_from_its_ports_counter() {
    let mut w = read_and_check(0, 0x1000, 2, false);
    let blk: Vec<u64> = w
        .log
        .iter()
        .filter_map(|r| match r {
            Req::Read { txn, .. } => Some(*txn),
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
    w.command(READ, 9, 0x2000, 1);
    w.run();
    assert_eq!(w.log[66], Req::Read { txn: 2, lba: 9 });
    let later: Vec<u64> = w.log[67..]
        .iter()
        .map(|r| match r {
            Req::Beat { txn, .. } => *txn,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(later, (64..96).collect::<Vec<_>>());
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

fn read_result(txn: u64, len: usize) -> Message {
    BlockMsg::ReadResult {
        txn: TxnId(txn),
        outcome: BlockReadOutcome::Data {
            data: vec![0x77; len],
        },
    }
    .into()
}

fn write_resp(txn: u64) -> Message {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
    .into()
}

#[test]
fn a_result_must_match_the_one_outstanding_request() {
    let mut w = World::new();
    // Advance the counters so stale and future txns exist on both ports.
    w.command(READ, 0, 0x1000, 1);
    w.run();
    w.write_reg(R_ACK, 1);
    w.command(READ, 1, 0x1200, 2);
    // `Issue`, wake pending: nothing is outstanding.
    for (port, msg) in [
        (BLK_PORT, read_result(1, 512)),
        (DMA_PORT, write_resp(32)),
        (DMA_PORT, write_resp(31)),
    ] {
        assert_violation(&mut w, port, msg, Phase::Complete);
    }
    w.step();
    // `WaitMedia { txn: 1 }`.
    assert_eq!(w.outstanding, Some(Req::Read { txn: 1, lba: 1 }));
    let wrong: Vec<(PortId, Message, Phase)> = vec![
        // Stale, future, and far-off txns.
        (BLK_PORT, read_result(0, 512), Phase::Complete),
        (BLK_PORT, read_result(2, 512), Phase::Complete),
        (BLK_PORT, read_result(u64::MAX, 512), Phase::Complete),
        // Not 512 bytes.
        (BLK_PORT, read_result(1, 0), Phase::Complete),
        (BLK_PORT, read_result(1, 511), Phase::Complete),
        (BLK_PORT, read_result(1, 513), Phase::Complete),
        (BLK_PORT, read_result(1, 1024), Phase::Complete),
        // Outside `Complete`.
        (BLK_PORT, read_result(1, 512), Phase::Request),
        (BLK_PORT, read_result(1, 512), Phase::Transfer),
        (BLK_PORT, read_result(1, 512), Phase::Commit),
        // The wrong kind.
        (
            BLK_PORT,
            BlockMsg::WriteResult {
                txn: TxnId(1),
                outcome: BlockWriteOutcome::Done,
            }
            .into(),
            Phase::Complete,
        ),
        (
            BLK_PORT,
            BlockMsg::ReadBlock {
                txn: TxnId(1),
                lba: 0,
            }
            .into(),
            Phase::Complete,
        ),
        // The wrong port or protocol.
        (DMA_PORT, write_resp(32), Phase::Complete),
        (DMA_PORT, read_result(1, 512), Phase::Complete),
        (BLK_PORT, write_resp(1), Phase::Complete),
        (IRQ_PORT, read_result(1, 512), Phase::Complete),
        (
            DMA_PORT,
            IrqMsg::Level { asserted: true }.into(),
            Phase::Complete,
        ),
    ];
    for (port, msg, phase) in wrong {
        assert_violation(&mut w, port, msg, phase);
    }
    // A second wake while a request is outstanding.
    let mut ctx = MockCtx::new(Phase::Request);
    let before = snapshot_of(&w.c);
    let result =
        w.c.handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx);
    assert!(matches!(result, Err(SimError::ComponentFault(_))));
    assert!(ctx.order.is_empty());
    assert_eq!(snapshot_of(&w.c), before);
    w.step();
    w.step();
    // `WaitBeat { txn: 32 }`.
    assert!(matches!(w.outstanding, Some(Req::Beat { txn: 32, .. })));
    let wrong: Vec<(PortId, Message, Phase)> = vec![
        (DMA_PORT, write_resp(31), Phase::Complete),
        (DMA_PORT, write_resp(33), Phase::Complete),
        (DMA_PORT, write_resp(0), Phase::Complete),
        (DMA_PORT, write_resp(32), Phase::Request),
        (DMA_PORT, write_resp(32), Phase::Transfer),
        (DMA_PORT, write_resp(32), Phase::Commit),
        (
            DMA_PORT,
            MemMsg::ReadResp {
                txn: TxnId(32),
                outcome: ReadOutcome::Data { data: vec![0; 16] },
            }
            .into(),
            Phase::Complete,
        ),
        (
            DMA_PORT,
            MemMsg::WriteReq {
                txn: TxnId(32),
                addr: 0x1200,
                data: vec![0; 16],
            }
            .into(),
            Phase::Complete,
        ),
        (BLK_PORT, read_result(1, 512), Phase::Complete),
        (BLK_PORT, read_result(2, 512), Phase::Complete),
        (MEM_PORT, write_resp(32), Phase::Complete),
    ];
    for (port, msg, phase) in wrong {
        assert_violation(&mut w, port, msg, phase);
    }
    // The run then completes normally, and a late duplicate faults.
    w.run();
    assert_eq!(w.status(), DONE);
    assert_violation(&mut w, DMA_PORT, write_resp(95), Phase::Complete);
    assert_violation(&mut w, BLK_PORT, read_result(2, 512), Phase::Complete);
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

#[test]
fn txn_counters_never_wrap() {
    // `blk`: the last allocatable txn is u64::MAX - 1; the next ReadBlock faults.
    let mut w = World::with(idle_with_counters(0, u64::MAX - 1));
    w.command(READ, 0, 0x1000, 1);
    w.step();
    assert_eq!(
        w.outstanding,
        Some(Req::Read {
            txn: u64::MAX - 1,
            lba: 0
        })
    );
    w.run();
    assert_eq!(w.status(), DONE);
    assert_eq!(view(&w.c).blk_txn, u64::MAX);
    w.write_reg(R_ACK, 1);
    w.command(READ, 1, 0x1000, 1);
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
    // `dma`: 32 beats end exactly at u64::MAX - 1; the next beat faults.
    let mut w = World::with(idle_with_counters(u64::MAX - 32, 0));
    w.command(READ, 0, 0x1000, 1);
    w.run();
    assert!(matches!(w.log.last(), Some(Req::Beat { txn, .. }) if *txn == u64::MAX - 1));
    assert_eq!(view(&w.c).dma_txn, u64::MAX);
    w.write_reg(R_ACK, 1);
    w.command(READ, 1, 0x1000, 1);
    w.step();
    w.step();
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
    assert_eq!(snapshot_of(&w.c), before);
    let v = view(&w.c);
    assert_eq!((v.engine, v.beat, v.buffer.len()), (1, 0, 512));
}

// --- Failure paths: see `dma_faults.rs`; here only shown not to panic ---

#[test]
fn engine_failure_results_do_not_panic() {
    let mut w = World::new();
    w.command(READ, 0, 0x1000, 2);
    w.step();
    let mut ctx = MockCtx::new(Phase::Complete);
    let error = BlockMsg::ReadResult {
        txn: TxnId(0),
        outcome: BlockReadOutcome::Error {
            error: MediaError::BadBlock,
        },
    };
    assert!(ctx.deliver_msg(&mut w.c, BLK_PORT, error.into()).is_ok());
    assert!(!view(&w.c).busy);
    let mut w = World::new();
    w.command(READ, 0, 0x1000, 2);
    for _ in 0..5 {
        w.step();
    }
    let Some(Req::Beat { txn, .. }) = w.outstanding.clone() else {
        panic!()
    };
    let fault = MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    let mut ctx = MockCtx::new(Phase::Complete);
    assert!(ctx.deliver_msg(&mut w.c, DMA_PORT, fault.into()).is_ok());
    assert!(!view(&w.c).busy);
}

// --- Checkpoints (mock) ---

/// Named positions of a three-block READ, each identified from the snapshot.
/// A named position and its test on the snapshot.
type Point = (&'static str, fn(&View) -> bool);

fn named_points() -> Vec<Point> {
    vec![
        ("accepted, before the first issue", |v| {
            v.busy && v.engine == 1 && v.block == 0 && v.buffer.is_empty()
        }),
        ("ReadBlock outstanding", |v| v.engine == 2 && v.block == 0),
        ("Data received, buffer full", |v| {
            v.engine == 1 && v.block == 0 && v.beat == 0 && v.buffer.len() == 512
        }),
        ("beat 0 outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 0
        }),
        ("mid-block beat outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 15
        }),
        ("mid-block, issuing", |v| {
            v.engine == 1 && v.block == 0 && v.beat == 16
        }),
        ("beat 31 outstanding", |v| {
            v.engine == 3 && v.block == 0 && v.beat == 31
        }),
        ("between blocks", |v| {
            v.engine == 1 && v.block == 1 && v.buffer.is_empty()
        }),
        ("second ReadBlock outstanding", |v| {
            v.engine == 2 && v.block == 1
        }),
        ("final beat outstanding", |v| {
            v.engine == 3 && v.block == 2 && v.beat == 31
        }),
        ("completed", |v| v.done),
    ]
}

fn three_block_read(w: &mut World) {
    w.write_reg(R_IRQ_ENABLE, 1);
    w.command(READ, 10, 0x2200, 3);
}

#[test]
fn named_checkpoints_resume_identically() {
    let mut reference = World::new();
    three_block_read(&mut reference);
    reference.run();
    for (name, at) in named_points() {
        let mut w = World::new();
        three_block_read(&mut w);
        while !at(&view(&w.c)) {
            assert!(w.step(), "never reached {name}");
        }
        w.checkpoint();
        w.run();
        assert_eq!(w.log, reference.log, "{name}");
        assert!(w.ram == reference.ram, "{name}");
        assert_eq!(w.levels, reference.levels, "{name}");
        assert_eq!(w.traced, reference.traced, "{name}");
        assert_eq!(snapshot_of(&w.c), snapshot_of(&reference.c), "{name}");
    }
}

#[test]
fn a_checkpoint_after_every_event_resumes_identically() {
    let mut reference = World::new();
    three_block_read(&mut reference);
    reference.run();
    let mut w = World::new();
    w.checkpoint_every_event = true;
    three_block_read(&mut w);
    w.checkpoint();
    w.run();
    assert_eq!(w.log, reference.log);
    assert!(w.ram == reference.ram);
    assert_eq!(w.levels, reference.levels);
    assert_eq!(w.traced, reference.traced);
    assert_eq!(snapshot_of(&w.c), snapshot_of(&reference.c));
}

// --- Independent oracle, property ---

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn reads_match_the_oracle(
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
        // Random block contents.
        let mut x = seed | 1;
        for block in &mut w.media {
            for b in block.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
        }
        w.write_reg(R_IRQ_ENABLE, u32::from(irq));
        let ram = w.ram.clone();
        w.command(READ, lba, addr, count);
        let mut steps = 0;
        let mut max_outstanding = 0;
        loop {
            if Some(steps) == checkpoint_at {
                w.checkpoint();
            }
            max_outstanding = max_outstanding.max(usize::from(w.outstanding.is_some()));
            if !w.step() {
                break;
            }
            steps += 1;
        }
        let (expected, final_ram) = oracle(&w.media, &ram, lba, addr, count);
        prop_assert_eq!(ops(&w.log), expected);
        prop_assert!(w.ram == final_ram);
        prop_assert_eq!(w.log.len(), count as usize * 33);
        prop_assert_eq!(steps, count as usize * 33 * 2);
        prop_assert_eq!(max_outstanding, 1);
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

fn media_image() -> Vec<u8> {
    media().concat()
}

/// A host and the controller's `dma` as the two masters of a bus with the RAM at 0 and
/// the controller's window at [`MMIO`]; the controller's `blk` to a `SimpleBlockMedia`
/// holding [`media`], its `irq` to a sink. Every link is one cycle.
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
    let bytes = media_image();
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

/// The READ of `count` blocks from `lba` to `addr` with the interrupt enabled, the
/// registers rewritten while it runs, a rejected second command, then at cycle
/// `readback` a `STATUS` read, 16-byte reads of `[addr - 16, addr + 512 count + 16)`, an
/// ACK, and a final `STATUS` read. Every request has its own txn.
fn workload(lba: u32, addr: u32, count: u32, readback: u64) -> Vec<(u64, MemMsg)> {
    let w32 = |offset: u64, value: u32| write(0, MMIO + offset, &value.to_le_bytes());
    let mut requests = vec![
        (0, w32(R_LBA, lba)),
        (1, w32(R_MEM_ADDR, addr)),
        (2, w32(R_BLOCK_COUNT, count)),
        (3, w32(R_IRQ_ENABLE, 1)),
        (4, w32(R_COMMAND, READ)),
        (6, w32(R_LBA, 0)),
        (7, w32(R_MEM_ADDR, BASE as u32)),
        (8, w32(R_BLOCK_COUNT, 5)),
        (10, w32(R_COMMAND, READ)),
        (readback, read(0, MMIO + R_STATUS, 4)),
    ];
    let start = u64::from(addr) - 16;
    let end = u64::from(addr) + u64::from(count) * 512 + 16;
    for (k, at) in (start..end).step_by(16).enumerate() {
        requests.push((readback + 1 + k as u64, read(0, at, 16)));
    }
    let after = readback + 2 + (end - start) / 16;
    requests.push((after, w32(R_ACK, 1)));
    requests.push((after + 1, read(0, MMIO + R_STATUS, 4)));
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
fn a_runtime_read_puts_the_media_bytes_in_ram() {
    let (lba, addr, count, readback) = (2, 0x1400, 3, 3000);
    let requests = workload(lba, addr, count, readback);
    let (mut rt, ids) = build(&requests);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_all(&mut rt);
    // Host responses, by txn.
    let responses: BTreeMap<u64, MemMsg> = events
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
    let data = |txn: u64| match &responses[&txn] {
        MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } => data.clone(),
        other => panic!("{other:?}"),
    };
    // STATUS at read-back: DONE and REJECTED; after ACK: REJECTED only.
    assert_eq!(data(9), (DONE | REJECTED).to_le_bytes());
    let last = requests.len() as u64 - 1;
    assert_eq!(data(last), REJECTED.to_le_bytes());
    // RAM: zeros, blocks 2, 3, 4 exactly, zeros.
    let ram: Vec<u8> = (10..last - 1).flat_map(data).collect();
    let mut expected = vec![0; 16];
    for b in lba..lba + count {
        expected.extend(pattern(b as usize));
    }
    expected.extend([0; 16]);
    assert_eq!(ram.len(), expected.len());
    assert!(ram == expected, "RAM differs from the media blocks");
    // The media saw exactly the three ReadBlocks, in order.
    let media_reads: Vec<(u64, u64)> = events
        .iter()
        .filter(|e| e.target == ids.media)
        .map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::Block(BlockMsg::ReadBlock { txn, lba }),
                ..
            } => (txn.0, *lba),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(media_reads, [(0, 2), (1, 3), (2, 4)]);
    // The controller's engine events strictly alternate wake, result: one outstanding.
    let engine: Vec<&Dispatched> = events
        .iter()
        .filter(|e| e.target == ids.ctl)
        .filter(|e| !matches!(e.delivery, Delivered::Message { port: MEM_PORT, .. }))
        .collect();
    assert_eq!(engine.len(), 2 * 3 * 33);
    for pair in engine.chunks(2) {
        assert!(matches!(pair[0].delivery, Delivered::Wake { token: ISSUE }));
        assert_eq!(pair[0].key.phase, Phase::Request);
        assert!(matches!(
            pair[1].delivery,
            Delivered::Message {
                port: DMA_PORT | BLK_PORT,
                ..
            }
        ));
        assert_eq!(pair[1].key.phase, Phase::Complete);
    }
    // Each wake is one cycle after the result before it.
    for w in engine[1..].windows(2).step_by(2) {
        assert_eq!(cycle(w[1]), cycle(w[0]) + 1);
    }
    // One assertion at completion, one deassertion at ACK.
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
    assert_eq!(levels, [true, false]);
    // Trace: the command, the rejection, the completion, all before the read-back.
    let trace = rt.take_trace().unwrap();
    let records: Vec<(u64, Traced)> = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == ids.ctl)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0 / TICKS_PER_CYCLE, (r.kind, r.fields.clone()))
        })
        .collect();
    let (at, traced): (Vec<u64>, Vec<Traced>) = records.into_iter().unzip();
    assert_eq!(
        traced,
        [
            command_record(1, 2, 0x1400, 3, true),
            (REJECTED_KIND, vec![]),
            done_record(0),
        ]
    );
    assert!(at[2] < readback);
    // Timing: the first wake one cycle after the accepting MMIO write, the ReadBlock one
    // link cycle after it.
    assert_eq!(cycle(engine[0]), at[0] + 1);
    let first_media = events.iter().find(|e| e.target == ids.media).unwrap();
    assert_eq!(cycle(first_media), cycle(engine[0]) + 1);
    assert_eq!(first_media.key.phase, Phase::Request);
}

fn checkpoint_workload() -> Vec<(u64, MemMsg)> {
    workload(12, 0x4c00, 2, 1200)
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
    // The workload's DMA must be finished before its read-back.
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
fn runtime_reads_are_deterministic() {
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
