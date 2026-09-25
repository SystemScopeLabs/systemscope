//! `DmaBlockController` engine failures (`docs/m2-design.md` §9.7, §9.8; M2.7c): a media
//! `Error` and a beat `Fault` during READ and WRITE end the command with DONE and
//! `ERROR` 7 (MEDIA_ERROR) or 6 (DMA_FAULT), with the partial effects of §9.7 and no
//! rollback: a READ keeps the earlier blocks and the failing block's earlier beats in
//! RAM; a WRITE keeps the earlier blocks on the media and never writes the failing one.
//! Nothing is issued after the failure, the engine is left canonical, `REJECTED` is
//! untouched, and the interrupt follows `DONE && IRQ_ENABLE`. A response that is not the
//! outstanding request's result stays a session fault whatever its outcome.
//!
//! [`World`] is a scripted RAM and media that answer the controller's one outstanding
//! request and fail exactly where a [`Fault`] says; an independent oracle predicts the
//! requests, the RAM, the media, and the code. Then real runtimes: a bus whose RAM
//! region ends inside the DMA aperture, so a beat past it gets the bus's real
//! `AccessFault` (the negative topology of §9.7), and a `SimpleBlockMedia` with
//! `bad_blocks` for media errors; with every-event checkpoints.

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
    BLK_PORT, COMMAND_KIND, DMA_FAULT, DMA_PORT, DONE_KIND, IRQ_PORT, ISSUE, MEDIA_ERROR, MEM_PORT,
    REJECTED_KIND,
};
use systemscope_platform::ram::{RamConfig, RamImage, Segment};
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

/// The execution codes of §9.2 and §9.7.
const E_DMA_FAULT: u8 = 6;
const E_MEDIA_ERROR: u8 = 7;

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

/// The RAM before a command: every byte differs from [`media_block`]'s.
fn old_ram() -> Vec<u8> {
    (0..RAM_SIZE)
        .map(|a| ((a * 7 + a / BEAT * 3) % 253) as u8 | 0x80)
        .collect()
}

/// Media block `b` before a command: every beat distinct, never equal to the RAM's.
fn media_block(b: usize) -> Vec<u8> {
    (0..BLOCK)
        .map(|k| ((b * 31 + k * 5 + k / BEAT * 11) % 127) as u8)
        .collect()
}

fn old_media() -> Vec<Vec<u8>> {
    (0..CAPACITY as usize).map(media_block).collect()
}

/// Where the scripted RAM or media fails, by block index *i* within the command and beat
/// index *j*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// The media answers block *i*'s `ReadBlock` or `WriteBlock` with `Error`.
    Media(u32),
    /// The RAM answers beat *j* of block *i* with `Fault`.
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

/// A request without its `txn`, as the oracle predicts it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    ReadBlock(u64),
    WriteBlock(u64, Vec<u8>),
    Store(u64, Vec<u8>),
    Load(u64),
}

impl Req {
    fn op(&self) -> Op {
        match self {
            Req::ReadBlock { lba, .. } => Op::ReadBlock(*lba),
            Req::WriteBlock { lba, data, .. } => Op::WriteBlock(*lba, data.clone()),
            Req::Store { addr, data, .. } => Op::Store(*addr, data.clone()),
            Req::Load { addr, .. } => Op::Load(*addr),
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

/// The controller, a scripted RAM and media that fail where `fault` says, and the one
/// pending wake or outstanding request the runtime's queue would hold.
struct World {
    c: DmaBlockController,
    ram: Vec<u8>,
    media: Vec<Vec<u8>>,
    fault: Fault,
    /// The command's LBA and address, to place each request in the transfer.
    cmd: (u64, u64),
    wake: bool,
    outstanding: Option<Req>,
    log: Vec<Req>,
    levels: Vec<bool>,
    traced: Vec<Traced>,
    checkpoint_every_event: bool,
}

impl World {
    fn new(fault: Fault) -> World {
        World {
            c: DmaBlockController::new(config()).unwrap(),
            ram: old_ram(),
            media: old_media(),
            fault,
            cmd: (0, 0),
            wake: false,
            outstanding: None,
            log: Vec::new(),
            levels: Vec::new(),
            traced: Vec::new(),
            checkpoint_every_event: false,
        }
    }

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

    fn write_reg(&mut self, offset: u64, value: u32) {
        let mut ctx = MockCtx::new(Phase::Transfer);
        ctx.deliver(
            &mut self.c,
            MEM_PORT,
            write(9, offset, &value.to_le_bytes()),
        )
        .unwrap();
        assert!(ctx.blocks.is_empty());
        let sent = ctx.take_one();
        assert_eq!(
            sent.msg,
            MemMsg::WriteResp {
                txn: TxnId(9),
                outcome: WriteOutcome::Done
            }
        );
        self.absorb(&ctx);
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
    }

    /// Block *i* and beat *j* of a beat address within the command.
    fn position(&self, addr: u64) -> (u32, u8) {
        let delta = addr - self.cmd.1;
        ((delta / 512) as u32, (delta % 512 / 16) as u8)
    }

    /// The scripted result of `req`: the fault if this is where it strikes, else success
    /// with its effect applied.
    fn result_of(&mut self, req: &Req) -> (PortId, Message) {
        match req {
            Req::ReadBlock { txn, lba } => {
                let block = (lba - self.cmd.0) as u32;
                let outcome = if self.fault == Fault::Media(block) {
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
                let block = (lba - self.cmd.0) as u32;
                let outcome = if self.fault == Fault::Media(block) {
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

    /// Delivers the outstanding request's result in `Complete`: nothing is sent, and the
    /// controller either schedules its next wake or completes, never both.
    fn respond(&mut self) {
        let req = self.outstanding.take().unwrap();
        let (port, msg) = self.result_of(&req);
        let mut ctx = MockCtx::new(Phase::Complete);
        ctx.deliver_msg(&mut self.c, port, msg).unwrap();
        assert!(
            ctx.sent.is_empty() && ctx.blocks.is_empty(),
            "sent after a result"
        );
        self.absorb(&ctx);
        let completed = ctx.traced.iter().any(|(kind, _)| *kind == DONE_KIND);
        assert!(self.wake != completed, "{:?}", ctx.traced);
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

    fn checkpoint(&mut self) {
        let bytes = snapshot_of(&self.c);
        let mut fresh = DmaBlockController::new(config()).unwrap();
        restore_into(&mut fresh, &bytes).unwrap();
        assert_eq!(snapshot_of(&fresh), bytes);
        assert_eq!(fresh.inspect(), self.c.inspect());
        self.c = fresh;
    }

    fn ops(&self) -> Vec<Op> {
        self.log.iter().map(Req::op).collect()
    }
}

/// What a command leaves, per §9.5–§9.7 written from the text.
struct Expected {
    ops: Vec<Op>,
    ram: Vec<u8>,
    media: Vec<Vec<u8>>,
    error: u8,
}

/// The READ of `count` blocks from `lba` to `addr` failing at `fault`.
fn read_oracle(
    ram: &[u8],
    media: &[Vec<u8>],
    (lba, addr, count): (u32, u32, u32),
    fault: Fault,
) -> Expected {
    let mut e = Expected {
        ops: Vec::new(),
        ram: ram.to_vec(),
        media: media.to_vec(),
        error: 0,
    };
    for i in 0..count {
        let block = u64::from(lba + i);
        e.ops.push(Op::ReadBlock(block));
        if fault == Fault::Media(i) {
            e.error = E_MEDIA_ERROR;
            return e;
        }
        let data = &media[block as usize];
        for j in 0..BEATS {
            let at = addr as usize + i as usize * BLOCK + j * BEAT;
            let bytes = data[j * BEAT..][..BEAT].to_vec();
            e.ops.push(Op::Store(at as u64, bytes.clone()));
            if fault == Fault::Beat(i, j as u8) {
                e.error = E_DMA_FAULT;
                return e;
            }
            e.ram[at..at + BEAT].copy_from_slice(&bytes);
        }
    }
    e
}

/// The WRITE of `count` blocks from `addr` to `lba` failing at `fault`.
fn write_oracle(
    ram: &[u8],
    media: &[Vec<u8>],
    (lba, addr, count): (u32, u32, u32),
    fault: Fault,
) -> Expected {
    let mut e = Expected {
        ops: Vec::new(),
        ram: ram.to_vec(),
        media: media.to_vec(),
        error: 0,
    };
    for i in 0..count {
        let start = addr as usize + i as usize * BLOCK;
        for j in 0..BEATS {
            e.ops.push(Op::Load((start + j * BEAT) as u64));
            if fault == Fault::Beat(i, j as u8) {
                e.error = E_DMA_FAULT;
                return e;
            }
        }
        let block = (lba + i) as usize;
        e.ops.push(Op::WriteBlock(
            block as u64,
            ram[start..start + BLOCK].to_vec(),
        ));
        if fault == Fault::Media(i) {
            e.error = E_MEDIA_ERROR;
            return e;
        }
        e.media[block] = ram[start..start + BLOCK].to_vec();
    }
    e
}

fn command_record(op: u32, lba: u32, addr: u32, count: u32) -> Traced {
    (
        COMMAND_KIND,
        vec![
            ("op", Value::U64(op.into())),
            ("lba", Value::U64(lba.into())),
            ("addr", Value::U64(addr.into())),
            ("count", Value::U64(count.into())),
            ("accepted", Value::Bool(true)),
        ],
    )
}

fn done_record(error: u8) -> Traced {
    (DONE_KIND, vec![("error", Value::U64(error.into()))])
}

/// The canonical DONE state after a command with `error`, whatever the engine was doing.
fn assert_terminal(w: &World, error: u8) {
    let v = view(&w.c);
    assert_eq!(
        (v.busy, v.done, v.error, v.latched),
        (false, true, error, None)
    );
    assert_eq!(
        (v.engine, v.txn, v.block, v.beat, v.buffer.len()),
        (0, None, 0, 0, 0),
        "engine canonical"
    );
    assert!(!w.wake && w.outstanding.is_none(), "nothing pending");
    let f = w.c.inspect().fields;
    let get = |n: &str| f.iter().find(|(k, _)| *k == n).unwrap().1.clone();
    assert_eq!(get("state"), Value::Str("done".into()));
    assert_eq!(get("engine"), Value::Str("idle".into()));
    assert_eq!(get("error"), Value::U64(error.into()));
    assert_eq!(get("command"), Value::Str("none".into()));
}

/// Runs `op` over `(lba, addr, count)` with `fault` in a fresh world, checks it against
/// the oracle and the canonical terminal state, and returns the world.
fn run_and_check(op: u32, desc: (u32, u32, u32), fault: Fault, irq: bool) -> World {
    let mut w = World::new(fault);
    w.write_reg(R_IRQ_ENABLE, u32::from(irq));
    let (ram, media) = (w.ram.clone(), w.media.clone());
    w.command(op, desc.0, desc.1, desc.2);
    w.run();
    let e = if op == READ {
        read_oracle(&ram, &media, desc, fault)
    } else {
        write_oracle(&ram, &media, desc, fault)
    };
    assert_eq!(w.ops(), e.ops, "requests stop exactly at the failure");
    assert!(w.ram == e.ram, "RAM differs from the oracle");
    assert!(w.media == e.media, "media differs from the oracle");
    assert_eq!(w.status(), DONE | u32::from(e.error) << 8);
    assert_eq!(w.levels, if irq { vec![true] } else { vec![] });
    assert_eq!(
        w.traced,
        [
            command_record(op, desc.0, desc.1, desc.2),
            done_record(e.error)
        ]
    );
    assert_terminal(&w, e.error);
    w
}

// --- Codes ---

#[test]
fn execution_codes_are_6_and_7() {
    assert_eq!((DMA_FAULT, MEDIA_ERROR), (E_DMA_FAULT, E_MEDIA_ERROR));
    let mut w = run_and_check(READ, (0, 0x1000, 1), Fault::Beat(0, 3), false);
    assert_eq!(w.status(), 0x0602);
    let mut w2 = run_and_check(READ, (0, 0x1000, 1), Fault::Media(0), false);
    assert_eq!(w2.status(), 0x0702);
    let mut w3 = run_and_check(WRITE, (0, 0x1000, 1), Fault::Beat(0, 3), false);
    assert_eq!(w3.status(), 0x0602);
    let mut w4 = run_and_check(WRITE, (0, 0x1000, 1), Fault::Media(0), false);
    assert_eq!(w4.status(), 0x0702);
    // ACK clears DONE and ERROR, like a successful completion's.
    for w in [&mut w, &mut w2, &mut w3, &mut w4] {
        w.write_reg(R_ACK, 1);
        assert_eq!(w.status(), 0);
    }
}

// --- READ ---

#[test]
fn a_read_media_error_writes_nothing_of_its_block() {
    // Block 0 fails: RAM untouched, nothing after the ReadBlock.
    let w = run_and_check(READ, (4, 0x1400, 3), Fault::Media(0), true);
    assert_eq!(w.ops(), [Op::ReadBlock(4)]);
    assert!(w.ram == old_ram());
    // Block 1 fails: block 0 stays in RAM, blocks 1 and 2 untouched, block 2 never read.
    let w = run_and_check(READ, (4, 0x1400, 3), Fault::Media(1), true);
    assert_eq!(w.log.len(), 1 + BEATS + 1);
    assert_eq!(w.ops().last(), Some(&Op::ReadBlock(5)));
    assert_eq!(w.ram[0x1400..0x1600], media_block(4)[..]);
    assert_eq!(w.ram[0x1600..0x1a00], old_ram()[0x1600..0x1a00]);
    assert!(!w.ops().contains(&Op::ReadBlock(6)));
    // The last block fails: every earlier block stays.
    let w = run_and_check(READ, (4, 0x1400, 3), Fault::Media(2), false);
    assert_eq!(
        w.ram[0x1400..0x1800],
        [media_block(4), media_block(5)].concat()[..]
    );
    assert_eq!(w.ram[0x1800..0x1a00], old_ram()[0x1800..0x1a00]);
}

#[test]
fn a_read_beat_fault_keeps_the_earlier_beats_and_is_not_rolled_back() {
    for j in [0u8, 1, 15, 31] {
        let w = run_and_check(READ, (3, 0x2000, 1), Fault::Beat(0, j), false);
        let cut = 0x2000 + 16 * j as usize;
        assert_eq!(
            w.ram[0x2000..cut],
            media_block(3)[..16 * j as usize],
            "j={j}"
        );
        assert_eq!(w.ram[cut..0x2200], old_ram()[cut..0x2200], "j={j}");
        // Beats 0 … j were issued, j being the faulting one; nothing after it.
        assert_eq!(w.log.len(), 1 + j as usize + 1, "j={j}");
        assert!(matches!(w.log.last(), Some(Req::Store { addr, .. }) if *addr == cut as u64));
        // The rest of RAM never changed.
        assert!(w.ram[..0x2000] == old_ram()[..0x2000] && w.ram[0x2200..] == old_ram()[0x2200..]);
    }
    // j = 0: the whole block stays old; j = 31: 496 bytes new, the last 16 old.
    let w = run_and_check(READ, (3, 0x2000, 1), Fault::Beat(0, 0), false);
    assert!(w.ram == old_ram());
    let w = run_and_check(READ, (3, 0x2000, 1), Fault::Beat(0, 31), false);
    assert_eq!(w.ram[0x2000..0x21f0], media_block(3)[..496]);
    assert_eq!(w.ram[0x21f0..0x2200], old_ram()[0x21f0..0x2200]);
}

#[test]
fn a_read_fault_in_a_later_block_keeps_the_earlier_blocks() {
    for j in [0u8, 9, 31] {
        let w = run_and_check(READ, (8, 0x1800, 3), Fault::Beat(1, j), true);
        let cut = 0x1a00 + 16 * j as usize;
        assert_eq!(
            w.ram[0x1800..0x1a00],
            media_block(8)[..],
            "block 0 committed"
        );
        assert_eq!(
            w.ram[0x1a00..cut],
            media_block(9)[..16 * j as usize],
            "prefix"
        );
        assert_eq!(w.ram[cut..0x1c00], old_ram()[cut..0x1c00], "suffix");
        assert_eq!(w.ram[0x1c00..0x1e00], old_ram()[0x1c00..0x1e00], "block 2");
        assert!(
            !w.ops().contains(&Op::ReadBlock(10)),
            "block 2 never starts"
        );
        assert_eq!(w.log.len(), 33 + 1 + j as usize + 1);
    }
}

// --- WRITE ---

#[test]
fn a_write_beat_fault_never_writes_its_block() {
    for j in [0u8, 1, 15, 31] {
        let mut w = World::new(Fault::Beat(0, j));
        w.command(WRITE, 6, 0x2400, 1);
        // Up to the faulting beat, the buffer holds the 16j bytes read.
        while !(view(&w.c).engine == 3 && view(&w.c).beat == j) {
            assert!(w.step());
        }
        assert_eq!(view(&w.c).buffer.len(), 16 * j as usize);
        w.run();
        assert!(w.media == old_media(), "j={j}: no partial WriteBlock");
        assert!(
            !w.log.iter().any(|r| matches!(r, Req::WriteBlock { .. })),
            "j={j}"
        );
        assert_eq!(w.log.len(), j as usize + 1);
        assert_eq!(w.status(), DONE | 6 << 8);
        assert_terminal(&w, E_DMA_FAULT);
        // Also through the oracle.
        run_and_check(WRITE, (6, 0x2400, 1), Fault::Beat(0, j), true);
    }
}

#[test]
fn a_write_fault_keeps_the_earlier_blocks_committed() {
    for j in [0u8, 15, 31] {
        let w = run_and_check(WRITE, (2, 0x1000, 3), Fault::Beat(1, j), false);
        assert_eq!(
            w.media[2][..],
            old_ram()[0x1000..0x1200],
            "block 0 committed"
        );
        assert_eq!(w.media[3], media_block(3), "block 1 not written");
        assert_eq!(w.media[4], media_block(4), "block 2 untouched");
        let writes: Vec<u64> = w
            .log
            .iter()
            .filter_map(|r| match r {
                Req::WriteBlock { lba, .. } => Some(*lba),
                _ => None,
            })
            .collect();
        assert_eq!(writes, [2]);
        assert_eq!(w.log.len(), 33 + j as usize + 1);
    }
}

#[test]
fn a_write_media_error_stops_after_its_block() {
    // Block 0 fails after all 32 beats: nothing committed, nothing after it.
    let w = run_and_check(WRITE, (5, 0x3000, 3), Fault::Media(0), true);
    assert_eq!(w.log.len(), 33);
    assert!(w.media == old_media());
    // Block 1 fails: block 0 committed, block 1 not, block 2 never starts.
    let w = run_and_check(WRITE, (5, 0x3000, 3), Fault::Media(1), true);
    assert_eq!(w.log.len(), 66);
    assert!(matches!(w.log.last(), Some(Req::WriteBlock { lba: 6, .. })));
    assert_eq!(w.media[5][..], old_ram()[0x3000..0x3200]);
    assert_eq!(w.media[6], media_block(6));
    assert_eq!(w.media[7], media_block(7));
    assert!(!w.log.iter().any(|r| match r {
        Req::Load { addr, .. } => *addr >= 0x3400,
        _ => false,
    }));
    // While the failing WriteBlock was outstanding the buffer was empty at beat 32; the
    // error drops the engine to its canonical DONE state without rebuilding it.
    let mut w = World::new(Fault::Media(1));
    w.command(WRITE, 5, 0x3000, 3);
    while !(view(&w.c).engine == 2 && view(&w.c).block == 1) {
        assert!(w.step());
    }
    let v = view(&w.c);
    assert_eq!((v.beat, v.buffer.len()), (32, 0));
    w.run();
    assert_terminal(&w, E_MEDIA_ERROR);
}

// --- Completion ---

#[test]
fn a_failure_completes_like_success_for_irq_and_ack() {
    for (op, fault, code) in [
        (READ, Fault::Beat(0, 5), E_DMA_FAULT),
        (READ, Fault::Media(1), E_MEDIA_ERROR),
        (WRITE, Fault::Beat(1, 20), E_DMA_FAULT),
        (WRITE, Fault::Media(0), E_MEDIA_ERROR),
    ] {
        // IRQ_ENABLE = 0: DONE + ERROR, no level; enabling it asserts, ACK deasserts.
        let mut w = run_and_check(op, (0, 0x1000, 2), fault, false);
        assert_eq!(w.read_reg(0x18), 1);
        w.write_reg(R_IRQ_ENABLE, 1);
        assert_eq!(w.levels, [true]);
        w.write_reg(R_ACK, 1);
        assert_eq!(w.levels, [true, false]);
        assert_eq!(w.status(), 0);
        // IRQ_ENABLE = 1: one assertion at the failure, one deassertion at ACK.
        let mut w = run_and_check(op, (0, 0x1000, 2), fault, true);
        assert!(view(&w.c).irq);
        w.write_reg(R_ACK, 1);
        w.write_reg(R_ACK, 1);
        assert_eq!(w.levels, [true, false], "{op} {fault:?} {code}");
        assert_eq!((w.status(), w.read_reg(0x18)), (0, 0));
    }
}

#[test]
fn a_failure_leaves_rejected_as_it_was() {
    for (op, fault, code) in [
        (READ, Fault::Beat(0, 7), E_DMA_FAULT),
        (WRITE, Fault::Media(0), E_MEDIA_ERROR),
    ] {
        // Not set before: not set by the failure.
        let mut w = World::new(fault);
        w.command(op, 0, 0x1000, 1);
        w.run();
        assert_eq!(w.status(), DONE | u32::from(code) << 8);
        // Set by a rejected command mid-transfer: survives the failure and ACK, until its
        // own W1C.
        let mut w = World::new(fault);
        w.command(op, 0, 0x1000, 1);
        w.step();
        w.write_reg(R_COMMAND, READ);
        assert_eq!(w.status(), BUSY | REJECTED);
        w.run();
        assert_eq!(w.status(), DONE | REJECTED | u32::from(code) << 8);
        assert!(view(&w.c).rejected);
        w.write_reg(R_ACK, 1);
        assert_eq!(w.status(), REJECTED);
        w.write_reg(R_STATUS, REJECTED);
        assert_eq!(w.status(), 0);
        assert_eq!(
            w.traced,
            [
                command_record(op, 0, 0x1000, 1),
                (REJECTED_KIND, vec![]),
                done_record(code),
            ]
        );
    }
}

#[test]
fn nothing_is_issued_after_a_failure_and_the_next_command_runs_normally() {
    for (op, fault) in [
        (READ, Fault::Beat(0, 4)),
        (READ, Fault::Media(0)),
        (WRITE, Fault::Beat(0, 4)),
        (WRITE, Fault::Media(0)),
    ] {
        let mut w = World::new(fault);
        w.command(op, 1, 0x1000, 2);
        let before = w.log.len();
        // Up to and including the failing result, then nothing is pending (`respond`
        // checked that the result sent nothing and scheduled no wake).
        w.run();
        assert!(!w.wake && w.outstanding.is_none());
        assert!(w.log.len() > before);
        // No wake can be pending: a stray one would find nothing to issue, and a repeat
        // of the failing result is no longer outstanding. Both fault the session and
        // change nothing.
        let snap = snapshot_of(&w.c);
        let mut ctx = MockCtx::new(Phase::Request);
        let result =
            w.c.handle_event(&Delivered::Wake { token: ISSUE }, &mut ctx);
        assert!(matches!(result, Err(SimError::ComponentFault(_))));
        assert!(ctx.order.is_empty());
        assert_eq!(snapshot_of(&w.c), snap);
        let last = w.log.last().unwrap().clone();
        let (port, msg) = w.result_of(&last);
        let mut ctx = MockCtx::new(Phase::Complete);
        assert!(matches!(
            ctx.deliver_msg(&mut w.c, port, msg),
            Err(SimError::ComponentFault(_))
        ));
        assert_eq!(snapshot_of(&w.c), snap);
        // After ACK, a new command runs to success with the counters carried on.
        let (dma, blk) = (view(&w.c).dma_txn, view(&w.c).blk_txn);
        w.write_reg(R_ACK, 1);
        w.fault = Fault::None;
        let (ram, media) = (w.ram.clone(), w.media.clone());
        let log = w.log.len();
        w.command(op, 9, 0x2000, 1);
        w.run();
        let e = if op == READ {
            read_oracle(&ram, &media, (9, 0x2000, 1), Fault::None)
        } else {
            write_oracle(&ram, &media, (9, 0x2000, 1), Fault::None)
        };
        assert_eq!(w.ops()[log..], e.ops[..]);
        assert!(w.ram == e.ram && w.media == e.media);
        assert_eq!(w.status(), DONE);
        let first = match &w.log[log] {
            Req::ReadBlock { txn, .. } => (None, Some(*txn)),
            Req::Load { txn, .. } => (Some(*txn), None),
            other => panic!("{other:?}"),
        };
        assert!(first == (None, Some(blk)) || first == (Some(dma), None));
    }
}

// --- Protocol violations stay session faults ---

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

fn media_error(read: bool, txn: u64) -> Message {
    let (txn, error) = (TxnId(txn), MediaError::BadBlock);
    if read {
        BlockMsg::ReadResult {
            txn,
            outcome: BlockReadOutcome::Error { error },
        }
        .into()
    } else {
        BlockMsg::WriteResult {
            txn,
            outcome: BlockWriteOutcome::Error { error },
        }
        .into()
    }
}

/// A `Fault` response to a READ's `WriteReq` (`store`) or a WRITE's `ReadReq`.
fn beat_fault(store: bool, txn: u64) -> Message {
    let (txn, fault) = (TxnId(txn), MemFault::AccessFault);
    if store {
        MemMsg::WriteResp {
            txn,
            outcome: WriteOutcome::Fault { fault },
        }
        .into()
    } else {
        MemMsg::ReadResp {
            txn,
            outcome: ReadOutcome::Fault { fault },
        }
        .into()
    }
}

#[test]
fn failure_outcomes_that_are_not_the_outstanding_result_fault_the_session() {
    for op in [READ, WRITE] {
        let is_read = op == READ;
        let mut w = World::new(Fault::None);
        // Used counters, so stale txns exist on both ports.
        w.command(op, 0, 0x1000, 1);
        w.run();
        w.write_reg(R_ACK, 1);
        let (dma, blk) = (view(&w.c).dma_txn, view(&w.c).blk_txn);
        w.command(op, 2, 0x1400, 2);
        // Nothing outstanding yet: any failure outcome is a violation.
        for (port, msg) in [
            (BLK_PORT, media_error(is_read, blk)),
            (BLK_PORT, media_error(!is_read, blk)),
            (DMA_PORT, beat_fault(is_read, dma)),
            (DMA_PORT, beat_fault(!is_read, dma)),
        ] {
            assert_violation(&mut w, port, msg, Phase::Complete);
        }
        // Step to the first beat outstanding (a READ first gets its block).
        while !matches!(w.outstanding, Some(Req::Store { .. } | Req::Load { .. })) {
            w.step();
        }
        let txn = match w.outstanding {
            Some(Req::Store { txn, .. } | Req::Load { txn, .. }) => txn,
            _ => unreachable!(),
        };
        for (port, msg, phase) in [
            (DMA_PORT, beat_fault(is_read, txn - 1), Phase::Complete),
            (DMA_PORT, beat_fault(is_read, txn + 1), Phase::Complete),
            (DMA_PORT, beat_fault(is_read, txn), Phase::Request),
            (DMA_PORT, beat_fault(is_read, txn), Phase::Transfer),
            (DMA_PORT, beat_fault(is_read, txn), Phase::Commit),
            // The other engine's response kind.
            (DMA_PORT, beat_fault(!is_read, txn), Phase::Complete),
            // The wrong port or protocol.
            (BLK_PORT, beat_fault(is_read, txn), Phase::Complete),
            (MEM_PORT, beat_fault(is_read, txn), Phase::Complete),
            (BLK_PORT, media_error(is_read, blk), Phase::Complete),
            (DMA_PORT, media_error(is_read, blk), Phase::Complete),
        ] {
            assert_violation(&mut w, port, msg, phase);
        }
        // Step to the block request outstanding (READ: the next block's ReadBlock).
        while !matches!(
            w.outstanding,
            Some(Req::ReadBlock { .. } | Req::WriteBlock { .. })
        ) {
            w.step();
        }
        let txn = match w.outstanding {
            Some(Req::ReadBlock { txn, .. } | Req::WriteBlock { txn, .. }) => txn,
            _ => unreachable!(),
        };
        for (port, msg, phase) in [
            (BLK_PORT, media_error(is_read, txn - 1), Phase::Complete),
            (BLK_PORT, media_error(is_read, txn + 1), Phase::Complete),
            (BLK_PORT, media_error(is_read, txn), Phase::Request),
            (BLK_PORT, media_error(is_read, txn), Phase::Transfer),
            (BLK_PORT, media_error(is_read, txn), Phase::Commit),
            (BLK_PORT, media_error(!is_read, txn), Phase::Complete),
            (DMA_PORT, media_error(is_read, txn), Phase::Complete),
            (MEM_PORT, media_error(is_read, txn), Phase::Complete),
            (DMA_PORT, beat_fault(is_read, dma), Phase::Complete),
        ] {
            assert_violation(&mut w, port, msg, phase);
        }
        // The genuine failure is a device error: DONE + 7, not a session fault.
        let mut ctx = MockCtx::new(Phase::Complete);
        ctx.deliver_msg(&mut w.c, BLK_PORT, media_error(is_read, txn))
            .unwrap();
        w.outstanding = None;
        w.absorb(&ctx);
        assert_eq!(w.status(), DONE | 7 << 8);
        assert_terminal(&w, E_MEDIA_ERROR);
    }
}

#[test]
fn a_genuine_beat_fault_is_a_device_error() {
    for op in [READ, WRITE] {
        let mut w = World::new(Fault::None);
        w.command(op, 0, 0x1000, 1);
        while !matches!(w.outstanding, Some(Req::Store { .. } | Req::Load { .. })) {
            w.step();
        }
        let Some(Req::Store { txn, .. } | Req::Load { txn, .. }) = w.outstanding.take() else {
            unreachable!()
        };
        let mut ctx = MockCtx::new(Phase::Complete);
        assert!(
            ctx.deliver_msg(&mut w.c, DMA_PORT, beat_fault(op == READ, txn))
                .is_ok()
        );
        assert!(ctx.sent.is_empty() && ctx.blocks.is_empty() && ctx.wakes.is_empty());
        w.absorb(&ctx);
        assert_terminal(&w, E_DMA_FAULT);
    }
}

// --- Checkpoints at the failure boundary ---

/// Whether the engine has reached a position.
type At = fn(&View) -> bool;

#[test]
fn a_checkpoint_just_before_a_failure_resumes_identically() {
    let cases: Vec<(u32, Fault, At)> = vec![
        // READ: the failing ReadBlock outstanding; beat 12 of block 1 outstanding.
        (READ, Fault::Media(1), |v| v.engine == 2 && v.block == 1),
        (READ, Fault::Beat(1, 12), |v| {
            v.engine == 3 && v.block == 1 && v.beat == 12
        }),
        // WRITE: beat 12 of block 1 outstanding; the failing WriteBlock outstanding.
        (WRITE, Fault::Beat(1, 12), |v| {
            v.engine == 3 && v.block == 1 && v.beat == 12
        }),
        (WRITE, Fault::Media(1), |v| v.engine == 2 && v.block == 1),
        // And one step earlier: issuing the failing request.
        (READ, Fault::Beat(1, 12), |v| {
            v.engine == 1 && v.block == 1 && v.beat == 12
        }),
        (WRITE, Fault::Media(1), |v| {
            v.engine == 1 && v.block == 1 && v.beat == 32
        }),
    ];
    for (op, fault, at) in cases {
        let desc = (3, 0x1800, 3);
        let reference = run_and_check(op, desc, fault, true);
        let mut w = World::new(fault);
        w.write_reg(R_IRQ_ENABLE, 1);
        w.command(op, desc.0, desc.1, desc.2);
        while !at(&view(&w.c)) {
            assert!(w.step(), "{op} {fault:?}: position never reached");
        }
        w.checkpoint();
        w.run();
        assert_eq!(w.log, reference.log, "{op} {fault:?}");
        assert!(w.ram == reference.ram && w.media == reference.media);
        assert_eq!(w.levels, reference.levels);
        assert_eq!(w.traced, reference.traced);
        assert_eq!(
            snapshot_of(&w.c),
            snapshot_of(&reference.c),
            "{op} {fault:?}"
        );
    }
}

#[test]
fn a_checkpoint_after_every_event_of_a_failing_run_resumes_identically() {
    for (op, fault) in [
        (READ, Fault::Beat(2, 30)),
        (READ, Fault::Media(2)),
        (WRITE, Fault::Beat(2, 30)),
        (WRITE, Fault::Media(2)),
    ] {
        let reference = run_and_check(op, (1, 0x2000, 4), fault, true);
        let mut w = World::new(fault);
        w.checkpoint_every_event = true;
        w.write_reg(R_IRQ_ENABLE, 1);
        w.command(op, 1, 0x2000, 4);
        w.checkpoint();
        w.run();
        assert_eq!(w.log, reference.log);
        assert!(w.ram == reference.ram && w.media == reference.media);
        assert_eq!(
            (w.levels.clone(), w.traced.clone()),
            (reference.levels.clone(), reference.traced.clone())
        );
        assert_eq!(snapshot_of(&w.c), snapshot_of(&reference.c));
    }
}

// --- Independent oracle, properties ---

/// A transfer of 1 to 4 blocks at a valid LBA and an aligned address inside the aperture.
fn transfer() -> impl Strategy<Value = (u32, u32, u32)> {
    (1u32..=4)
        .prop_flat_map(|n| {
            (
                0..=(CAPACITY as u32 - n),
                0u32..(APERTURE as u32 / 16),
                Just(n),
            )
        })
        .prop_map(|(lba, slot, n)| {
            let slots = (APERTURE as u32 - n * 512) / 16 + 1;
            (lba, BASE as u32 + (slot % slots) * 16, n)
        })
}

/// A failure inside a transfer of `count` blocks.
fn fault_in(count: u32) -> impl Strategy<Value = Fault> {
    prop_oneof![
        (0..count).prop_map(Fault::Media),
        (0..count, 0u8..32).prop_map(|(i, j)| Fault::Beat(i, j)),
    ]
}

fn case() -> impl Strategy<Value = ((u32, u32, u32), Fault, bool, Option<usize>)> {
    transfer().prop_flat_map(|desc| {
        (
            Just(desc),
            fault_in(desc.2),
            any::<bool>(),
            prop::option::of(0usize..300),
        )
    })
}

fn check_property(
    op: u32,
    (desc, fault, irq, checkpoint_at): ((u32, u32, u32), Fault, bool, Option<usize>),
) -> Result<(), TestCaseError> {
    let (lba, addr, count) = desc;
    let mut w = World::new(fault);
    w.write_reg(R_IRQ_ENABLE, u32::from(irq));
    let (ram, media) = (w.ram.clone(), w.media.clone());
    w.command(op, lba, addr, count);
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
    let e = if op == READ {
        read_oracle(&ram, &media, desc, fault)
    } else {
        write_oracle(&ram, &media, desc, fault)
    };
    prop_assert_eq!(w.ops(), e.ops);
    prop_assert!(w.ram == e.ram);
    prop_assert!(w.media == e.media);
    prop_assert_eq!(w.status(), DONE | u32::from(e.error) << 8);
    prop_assert_eq!(w.levels.clone(), if irq { vec![true] } else { vec![] });
    // The failing block's range, spelled out: earlier blocks done, later ones untouched.
    let (i, prefix) = match fault {
        Fault::Media(i) => (i, 0),
        Fault::Beat(i, j) => (i, 16 * j as usize),
        Fault::None => unreachable!(),
    };
    let start = addr as usize + i as usize * BLOCK;
    let end = addr as usize + count as usize * BLOCK;
    if op == READ {
        for k in 0..i as usize {
            let at = addr as usize + k * BLOCK;
            prop_assert!(w.ram[at..at + BLOCK] == media[lba as usize + k][..]);
        }
        prop_assert!(w.ram[start..start + prefix] == media[(lba + i) as usize][..prefix]);
        prop_assert!(w.ram[start + prefix..end] == ram[start + prefix..end]);
    } else {
        for k in 0..i as usize {
            let at = addr as usize + k * BLOCK;
            prop_assert!(w.media[lba as usize + k][..] == ram[at..at + BLOCK]);
        }
        for k in i..count {
            prop_assert!(w.media[(lba + k) as usize] == media[(lba + k) as usize]);
        }
        let failed = (lba + i) as u64;
        let sent_failed = w
            .log
            .iter()
            .any(|r| matches!(r, Req::WriteBlock { lba, .. } if *lba == failed));
        prop_assert_eq!(sent_failed, matches!(fault, Fault::Media(_)));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn read_failures_match_the_oracle(c in case()) {
        check_property(READ, c)?;
    }

    #[test]
    fn write_failures_match_the_oracle(c in case()) {
        check_property(WRITE, c)?;
    }
}

// --- Runtime ---

const TICKS_PER_CYCLE: u64 = 1000;
const MMIO: u64 = 0x2000_0000;
/// The bus's RAM region ends 15 beats into 0x2000: the aperture contains a hole.
const RAM_END: u64 = 0x20f0;
/// A block of the runtime media that answers `Error { BadBlock }`.
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

struct Ids {
    host: ComponentId,
    ctl: ComponentId,
    media: ComponentId,
    sink: ComponentId,
}

/// A host and the controller's `dma` as the masters of a bus with a RAM of [`old_ram`]
/// bytes over `[0, RAM_END)` and the controller's window at [`MMIO`], and nothing in
/// `[RAM_END, 0x2000_0000)`, although the DMA aperture is `[0x1000, 0x5000)`. The
/// controller's `blk` goes to a `SimpleBlockMedia` of [`old_media`] with block [`BAD`]
/// bad, its `irq` to a sink.
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
            ctl,
            media,
            sink,
        },
    )
}

fn w32(offset: u64, value: u32) -> MemMsg {
    write(0, MMIO + offset, &value.to_le_bytes())
}

/// Numbers every request with its own txn.
fn numbered(mut requests: Vec<(u64, MemMsg)>) -> Vec<(u64, MemMsg)> {
    for (i, (_, msg)) in requests.iter_mut().enumerate() {
        match msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => *txn = TxnId(i as u64),
            _ => unreachable!(),
        }
    }
    requests
}

/// One command every `gap` cycles from `start`, each followed by a `STATUS` read and
/// an ACK: `(op, lba, addr, count)`.
fn commands(start: u64, gap: u64, list: &[(u32, u32, u32, u32)]) -> Vec<(u64, MemMsg)> {
    let mut requests = vec![(start, w32(R_IRQ_ENABLE, 1))];
    for (n, &(op, lba, addr, count)) in list.iter().enumerate() {
        let t = start + 1 + n as u64 * gap;
        requests.extend([
            (t, w32(R_LBA, lba)),
            (t + 1, w32(R_MEM_ADDR, addr)),
            (t + 2, w32(R_BLOCK_COUNT, count)),
            (t + 3, w32(R_COMMAND, op)),
            (t + gap - 3, read(0, MMIO + R_STATUS, 4)),
            (t + gap - 2, w32(R_ACK, 1)),
        ]);
    }
    requests
}

/// 16-byte host reads of `[from, to)` from cycle `at`.
fn reads(at: u64, from: u64, to: u64) -> Vec<(u64, MemMsg)> {
    (from..to)
        .step_by(BEAT)
        .enumerate()
        .map(|(k, a)| (at + k as u64, read(0, a, 16)))
        .collect()
}

/// The READ workload: blocks 4, 5, 6 to 0x1e00, whose block 1 runs into the hole at beat
/// 15 (DMA_FAULT); then blocks 8, 9, 10 to 0x1000, whose block 1 is bad (MEDIA_ERROR);
/// then the RAM read back.
fn read_workload() -> Vec<(u64, MemMsg)> {
    let mut requests = commands(0, 500, &[(READ, 4, 0x1e00, 3), (READ, 8, 0x1000, 3)]);
    requests.extend(reads(1100, 0x1000, RAM_END));
    numbered(requests)
}

/// The WRITE workload: from 0x1e00 to blocks 12, 13, 14, whose block 1 runs into the
/// hole at beat 15 (DMA_FAULT); from 0x1000 to blocks 8, 9, 10, whose block 1 is bad
/// (MEDIA_ERROR); then READs of blocks 12, 13 to 0x1400 and 8 to 0x1800 and the RAM
/// read back.
fn write_workload() -> Vec<(u64, MemMsg)> {
    let mut requests = commands(
        0,
        500,
        &[
            (WRITE, 12, 0x1e00, 3),
            (WRITE, 8, 0x1000, 3),
            (READ, 12, 0x1400, 2),
            (READ, 8, 0x1800, 1),
        ],
    );
    requests.extend(reads(2100, 0x1400, 0x1a00));
    numbered(requests)
}

fn run_all(rt: &mut Runtime) -> Vec<Dispatched> {
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None, "device errors never fault the session");
    events
}

fn responses(events: &[Dispatched], host: ComponentId) -> Vec<(u64, MemMsg)> {
    let mut out: Vec<(u64, MemMsg)> = events
        .iter()
        .filter(|e| e.target == host)
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
    out.sort_by_key(|(txn, _)| *txn);
    out
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

/// The controller's trace records, with the cycle of each.
fn traced(rt: &mut Runtime, ctl: ComponentId) -> Vec<(u64, Traced)> {
    rt.take_trace()
        .unwrap()
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == ctl)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0 / TICKS_PER_CYCLE, (r.kind, r.fields.clone()))
        })
        .collect()
}

fn levels(events: &[Dispatched], sink: ComponentId) -> Vec<bool> {
    events
        .iter()
        .filter(|e| e.target == sink)
        .map(|e| match e.delivery {
            Delivered::Message {
                msg: Message::Irq(IrqMsg::Level { asserted }),
                ..
            } => asserted,
            _ => panic!("{e:?}"),
        })
        .collect()
}

#[test]
fn a_runtime_read_keeps_partial_progress_through_a_bus_fault_and_a_media_error() {
    let requests = read_workload();
    let (mut rt, ids) = build(&requests);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_all(&mut rt);
    let r = responses(&events, ids.host);
    assert_eq!(r.len(), requests.len());
    // STATUS after each command: DONE with 6, then DONE with 7.
    assert_eq!(data_of(&r[5].1), (DONE | 6 << 8).to_le_bytes());
    assert_eq!(data_of(&r[11].1), (DONE | 7 << 8).to_le_bytes());
    // RAM [0x1000, RAM_END): block 8 (the media error's block 0), then old bytes, then
    // block 4 at 0x1e00 and block 5's first 15 beats at 0x2000 up to the hole.
    let ram: Vec<u8> = r[13..].iter().flat_map(|(_, m)| data_of(m)).collect();
    let mut expected = old_ram()[0x1000..RAM_END as usize].to_vec();
    expected[..0x200].copy_from_slice(&media_block(8));
    expected[0xe00..0x1000].copy_from_slice(&media_block(4));
    expected[0x1000..0x10f0].copy_from_slice(&media_block(5)[..0xf0]);
    assert!(ram == expected, "RAM differs");
    // The media saw ReadBlock 4, 5 (never 6), then 8, 9 (never 10).
    let media: Vec<u64> = events
        .iter()
        .filter(|e| e.target == ids.media)
        .map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::Block(BlockMsg::ReadBlock { lba, .. }),
                ..
            } => *lba,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(media, [4, 5, 8, 9]);
    // The faulting beat was the controller's last dma traffic of the first command: its
    // `WriteReq` to 0x20f0 got the bus's AccessFault.
    let dma_results: Vec<&Dispatched> = events
        .iter()
        .filter(|e| {
            e.target == ids.ctl && matches!(e.delivery, Delivered::Message { port: DMA_PORT, .. })
        })
        .collect();
    assert_eq!(dma_results.len(), 32 + 16 + 32);
    assert!(matches!(
        &dma_results[47].delivery,
        Delivered::Message {
            msg: Message::MemV1(MemMsg::WriteResp {
                outcome: WriteOutcome::Fault { .. },
                ..
            }),
            ..
        }
    ));
    // Two completions, each asserting, each ACKed.
    assert_eq!(levels(&events, ids.sink), [true, false, true, false]);
    let t = traced(&mut rt, ids.ctl);
    let kinds: Vec<Traced> = t.into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        kinds,
        [
            command_record(READ, 4, 0x1e00, 3),
            done_record(6),
            command_record(READ, 8, 0x1000, 3),
            done_record(7),
        ]
    );
}

#[test]
fn a_runtime_write_never_commits_a_failing_block() {
    let requests = write_workload();
    let (mut rt, ids) = build(&requests);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events = run_all(&mut rt);
    let r = responses(&events, ids.host);
    assert_eq!(r.len(), requests.len());
    let status: Vec<Vec<u8>> = [5, 11, 17, 23].iter().map(|&k| data_of(&r[k].1)).collect();
    assert_eq!(
        status,
        [
            (DONE | 6 << 8).to_le_bytes(),
            (DONE | 7 << 8).to_le_bytes(),
            DONE.to_le_bytes(),
            DONE.to_le_bytes(),
        ]
    );
    // The media received WriteBlock 12 (block 13 hit the hole before its WriteBlock),
    // then WriteBlock 8 and the failing WriteBlock 9 (never 10), then the READs.
    let media: Vec<BlockMsg> = events
        .iter()
        .filter(|e| e.target == ids.media)
        .map(|e| match &e.delivery {
            Delivered::Message {
                msg: Message::Block(m),
                ..
            } => m.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    let summary: Vec<(&str, u64)> = media
        .iter()
        .map(|m| match m {
            BlockMsg::WriteBlock { lba, .. } => ("write", *lba),
            BlockMsg::ReadBlock { lba, .. } => ("read", *lba),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("write", 12),
            ("write", 8),
            ("write", 9),
            ("read", 12),
            ("read", 13),
            ("read", 8)
        ]
    );
    let ram = old_ram();
    let BlockMsg::WriteBlock { data, .. } = &media[0] else {
        unreachable!()
    };
    assert!(data[..] == ram[0x1e00..0x2000]);
    // Read back: block 12 is the RAM at 0x1e00, block 13 is still old, block 8 is the
    // RAM at 0x1000.
    let back: Vec<u8> = r[25..].iter().flat_map(|(_, m)| data_of(m)).collect();
    let expected = [
        ram[0x1e00..0x2000].to_vec(),
        media_block(13),
        ram[0x1000..0x1200].to_vec(),
    ]
    .concat();
    assert!(back == expected, "media differs");
    assert_eq!(
        levels(&events, ids.sink),
        [true, false, true, false, true, false, true, false]
    );
    let kinds: Vec<Traced> = traced(&mut rt, ids.ctl)
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    assert_eq!(
        kinds,
        [
            command_record(WRITE, 12, 0x1e00, 3),
            done_record(6),
            command_record(WRITE, 8, 0x1000, 3),
            done_record(7),
            command_record(READ, 12, 0x1400, 2),
            done_record(0),
            command_record(READ, 8, 0x1800, 1),
            done_record(0),
        ]
    );
}

fn resumes_at_every_event(requests: &[(u64, MemMsg)]) {
    let reference = {
        let (mut rt, _) = build(requests);
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
    for k in 0..=reference.0.len() {
        let (mut rt, _) = build(requests);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let prefix = rt.take_trace().unwrap();
        let (mut fresh, _) = build(requests);
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
fn runtime_read_failures_resume_from_every_event_boundary() {
    let requests = numbered(commands(
        0,
        500,
        &[(READ, 4, 0x1e00, 2), (READ, 8, 0x1000, 2)],
    ));
    resumes_at_every_event(&requests);
}

#[test]
fn runtime_write_failures_resume_from_every_event_boundary() {
    let requests = numbered(commands(
        0,
        500,
        &[(WRITE, 12, 0x1e00, 2), (WRITE, 8, 0x1000, 2)],
    ));
    resumes_at_every_event(&requests);
}
