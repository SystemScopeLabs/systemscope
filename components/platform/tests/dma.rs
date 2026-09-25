//! `DmaBlockController` control plane (`docs/m2-design.md` §9.1–§9.4, §9.8, §9.9; M2.6):
//! configuration, ports, the register map and access discipline, the lifecycle,
//! validation and its precedence, `REJECTED`, the interrupt line, snapshots, restore
//! rejection, inspect, and trace, driven through a mock context and checked against an
//! independent register/lifecycle model; then in a real runtime with a scripted MMIO
//! initiator, an interrupt sink, and tie-offs on `dma` and `blk` that fault on any
//! message, for timing and every-event checkpoints.
//!
//! Every mock-context delivery also checks that nothing is sent on `dma` or `blk`: the
//! engine is M2.7.

mod common;

use common::{MockCtx, Script, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{self, BlockMsg, BlockReadOutcome};
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::dma::{
    ACK, ADDRESS_LIMIT, BAD_COMMAND, BAD_COUNT, BLK_PORT, BLOCK_COUNT, COMMAND, COMMAND_KIND,
    DMA_ALIGN, DMA_PORT, DMA_RANGE, DONE_KIND, IRQ_ENABLE, IRQ_PORT, IRQ_STATUS, LBA, LBA_RANGE,
    MAX_CAPACITY_BLOCKS, MEM_ADDR, MEM_PORT, REJECTED_KIND, SIZE, SNAPSHOT_SCHEMA, STATUS,
};
use systemscope_platform::{
    DmaBlockController, DmaBlockControllerConfig, DmaBlockControllerConfigError,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

const CLOCK: ClockDomainId = ClockDomainId(3);

const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: CLOCK,
    k: 2,
};

/// A small aperture: 16 blocks at 0x1000.
const BASE: u64 = 0x1000;
const APERTURE: u64 = 0x2000;
const CAPACITY: u64 = 16;

/// The frozen register values, written out independently of the crate's constants.
const R_COMMAND: u64 = 0x00;
const R_STATUS: u64 = 0x04;
const R_LBA: u64 = 0x08;
const R_MEM_ADDR: u64 = 0x0C;
const R_BLOCK_COUNT: u64 = 0x10;
const R_IRQ_ENABLE: u64 = 0x14;
const R_IRQ_STATUS: u64 = 0x18;
const R_RESERVED: u64 = 0x1C;

const BUSY: u32 = 1;
const DONE: u32 = 2;
const REJECTED: u32 = 4;

fn config(capacity_blocks: u64, dma_base: u64, dma_size: u64) -> DmaBlockControllerConfig {
    DmaBlockControllerConfig {
        clock: CLOCK,
        latency: LATENCY,
        capacity_blocks,
        dma_base,
        dma_size,
    }
}

fn controller_with(capacity_blocks: u64, dma_base: u64, dma_size: u64) -> DmaBlockController {
    DmaBlockController::new(config(capacity_blocks, dma_base, dma_size)).unwrap()
}

fn controller() -> DmaBlockController {
    controller_with(CAPACITY, BASE, APERTURE)
}

/// Delivers one MMIO request on `mem` and returns the context, checking that exactly one
/// response went back on `mem` after the latency in `Complete`, that every `Level` went
/// on `irq` (`Now`, `Complete`), and that nothing was sent on `dma` or `blk`.
fn mmio(c: &mut DmaBlockController, msg: MemMsg) -> (MemMsg, MockCtx) {
    let mut ctx = MockCtx::new(Phase::Transfer);
    ctx.deliver(c, MEM_PORT, msg).unwrap();
    assert!(ctx.blocks.is_empty(), "block.v0 sent: {:?}", ctx.blocks);
    assert!(ctx.wakes.is_empty(), "wake scheduled: {:?}", ctx.wakes);
    assert_eq!(ctx.sent.len(), 1, "mem.v1 sends: {:?}", ctx.sent);
    let sent = ctx.sent.pop().unwrap();
    assert_eq!(
        (sent.port, sent.when, sent.phase),
        (
            MEM_PORT,
            ScheduleWhen::Cycles {
                domain: CLOCK,
                k: 2
            },
            Phase::Complete
        )
    );
    for irq in &ctx.irqs {
        assert_eq!(
            (irq.port, irq.when, irq.phase),
            (IRQ_PORT, ScheduleWhen::Now, Phase::Complete)
        );
    }
    (sent.msg, ctx)
}

/// Writes `value` to `offset`; the outcome and the levels sent.
fn wr(c: &mut DmaBlockController, offset: u64, value: u32) -> (WriteOutcome, Vec<bool>) {
    let (resp, ctx) = mmio(c, write(7, offset, &value.to_le_bytes()));
    let MemMsg::WriteResp {
        txn: TxnId(7),
        outcome,
    } = resp
    else {
        panic!("{resp:?}")
    };
    (outcome, ctx.irqs.iter().map(|i| i.asserted).collect())
}

/// Writes a register that must accept the write, sending no level.
fn set(c: &mut DmaBlockController, offset: u64, value: u32) {
    assert_eq!(wr(c, offset, value), (WriteOutcome::Done, vec![]));
}

/// Reads `offset`: the value, or `None` for a fault.
fn rd(c: &mut DmaBlockController, offset: u64) -> Option<u32> {
    let (resp, ctx) = mmio(c, read(8, offset, 4));
    assert!(ctx.irqs.is_empty() && ctx.traced.is_empty());
    match resp {
        MemMsg::ReadResp {
            txn: TxnId(8),
            outcome: ReadOutcome::Data { data },
        } => Some(u32::from_le_bytes(data.try_into().unwrap())),
        MemMsg::ReadResp {
            txn: TxnId(8),
            outcome:
                ReadOutcome::Fault {
                    fault: MemFault::AccessFault,
                },
        } => None,
        other => panic!("{other:?}"),
    }
}

fn status(c: &mut DmaBlockController) -> u32 {
    rd(c, R_STATUS).unwrap()
}

/// Programs the three command registers.
fn program(c: &mut DmaBlockController, lba: u32, addr: u32, count: u32) {
    set(c, R_LBA, lba);
    set(c, R_MEM_ADDR, addr);
    set(c, R_BLOCK_COUNT, count);
}

/// Programs and submits a command, which must be written in IDLE, and returns the
/// resulting `ERROR` code, or 0 if it was accepted (BUSY).
fn submit(c: &mut DmaBlockController, command: u32, lba: u32, addr: u32, count: u32) -> u8 {
    program(c, lba, addr, count);
    assert_eq!(wr(c, R_COMMAND, command).0, WriteOutcome::Done);
    let s = status(c);
    if s & BUSY != 0 {
        assert_eq!(s & !REJECTED, BUSY);
        0
    } else {
        assert_eq!(s & (DONE | 0xff00 | BUSY), DONE | (s & 0xff00));
        let code = (s >> 8) as u8;
        assert_ne!(code, 0);
        code
    }
}

fn field(c: &DmaBlockController, name: &str) -> Value {
    c.inspect()
        .fields
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no field {name}"))
        .1
}

// --- Configuration, ports, identity ---

#[test]
fn the_configuration_is_checked() {
    let new = |cap, base, size| DmaBlockController::new(config(cap, base, size)).err();
    assert_eq!(
        new(0, 0, 16),
        Some(DmaBlockControllerConfigError::Capacity(0))
    );
    assert_eq!(new(1, 0, 16), None);
    assert_eq!(new(1 << 32, 0, 16), None);
    assert_eq!(
        new((1 << 32) + 1, 0, 16),
        Some(DmaBlockControllerConfigError::Capacity((1 << 32) + 1))
    );
    assert_eq!(
        new(u64::MAX, 0, 16),
        Some(DmaBlockControllerConfigError::Capacity(u64::MAX))
    );
    assert_eq!(
        new(1, 0, 0),
        Some(DmaBlockControllerConfigError::EmptyAperture)
    );
    assert_eq!(new(1, 0, 1 << 32), None);
    assert_eq!(new(1, 0xffff_fff0, 0x10), None);
    assert_eq!(
        new(1, 0xffff_fff0, 0x11),
        Some(DmaBlockControllerConfigError::ApertureOutOfRange)
    );
    assert_eq!(
        new(1, u64::MAX, 2),
        Some(DmaBlockControllerConfigError::ApertureOutOfRange)
    );
    assert_eq!(
        new(1, 1 << 32, 1),
        Some(DmaBlockControllerConfigError::ApertureOutOfRange)
    );
    // An aperture is not required to be RAM, aligned, or block-sized.
    assert_eq!(new(1, 3, 5), None);
    assert_eq!(MAX_CAPACITY_BLOCKS, 1 << 32);
    assert_eq!(ADDRESS_LIMIT, 1 << 32);
    assert_eq!(
        DmaBlockControllerConfigError::Capacity(0).to_string(),
        "controller capacity 0 is not in 1..=2^32 blocks"
    );
}

#[test]
fn it_has_the_four_frozen_ports() {
    let c = controller();
    assert_eq!(c.type_name(), "platform.blk");
    let ports: Vec<_> = c
        .ports()
        .into_iter()
        .map(|p| (p.name, p.protocol, p.role))
        .collect();
    assert_eq!(
        ports,
        [
            ("mem", mem_v1::PROTOCOL, Role::Target),
            ("dma", mem_v1::PROTOCOL, Role::Initiator),
            ("blk", block_v0::PROTOCOL, Role::Initiator),
            ("irq", irq_v0::PROTOCOL, Role::Initiator),
        ]
    );
    assert_eq!(
        (MEM_PORT, DMA_PORT, BLK_PORT, IRQ_PORT),
        (PortId(0), PortId(1), PortId(2), PortId(3))
    );
    assert_eq!(c.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(SNAPSHOT_SCHEMA, 1);
}

#[test]
fn the_register_map_is_the_frozen_one() {
    assert_eq!(
        [
            COMMAND,
            STATUS,
            LBA,
            MEM_ADDR,
            BLOCK_COUNT,
            IRQ_ENABLE,
            IRQ_STATUS,
            ACK,
            SIZE
        ],
        [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x18, 0x20]
    );
}

#[test]
fn reset_is_idle_with_every_register_zero() {
    let mut c = controller();
    let mut ctx = MockCtx::new(Phase::Request);
    c.init(&mut ctx).unwrap();
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
    for offset in [
        R_STATUS,
        R_LBA,
        R_MEM_ADDR,
        R_BLOCK_COUNT,
        R_IRQ_ENABLE,
        R_IRQ_STATUS,
    ] {
        assert_eq!(rd(&mut c, offset), Some(0), "offset {offset:#x}");
    }
    assert_eq!(
        c.inspect().fields,
        vec![
            ("lba", Value::U64(0)),
            ("mem_addr", Value::U64(0)),
            ("block_count", Value::U64(0)),
            ("irq_enable", Value::U64(0)),
            ("status", Value::U64(0)),
            ("state", Value::Str("idle".into())),
            ("error", Value::U64(0)),
            ("rejected", Value::Bool(false)),
            ("command", Value::Str("none".into())),
            ("engine", Value::Str("idle".into())),
            ("block", Value::U64(0)),
            ("beat", Value::U64(0)),
            ("irq", Value::Bool(false)),
        ]
    );
}

// --- Register access ---

#[test]
fn programming_registers_hold_full_32_bit_values() {
    let mut c = controller();
    for (offset, value) in [
        (R_LBA, 0xffff_ffff),
        (R_MEM_ADDR, 0x8000_0010),
        (R_BLOCK_COUNT, 0xdead_beef),
    ] {
        set(&mut c, offset, value);
        assert_eq!(rd(&mut c, offset), Some(value));
    }
    // IRQ_ENABLE keeps bit 0 only.
    set(&mut c, R_IRQ_ENABLE, 0xffff_fffe);
    assert_eq!(rd(&mut c, R_IRQ_ENABLE), Some(0));
    set(&mut c, R_IRQ_ENABLE, 0xffff_ffff);
    assert_eq!(rd(&mut c, R_IRQ_ENABLE), Some(1));
    // STATUS and IRQ_STATUS ignore every bit but their W1C bit in IDLE.
    set(&mut c, R_STATUS, 0xffff_fffb);
    set(&mut c, R_IRQ_STATUS, 0xffff_ffff);
    assert_eq!(status(&mut c), 0);
}

/// Requires `msg` to get an `AccessFault` and change nothing.
fn assert_access_faults(c: &mut DmaBlockController, msg: MemMsg) {
    let before = snapshot_of(c);
    let (resp, ctx) = mmio(c, msg.clone());
    let faulted = matches!(
        resp,
        MemMsg::ReadResp {
            outcome: ReadOutcome::Fault {
                fault: MemFault::AccessFault
            },
            ..
        } | MemMsg::WriteResp {
            outcome: WriteOutcome::Fault {
                fault: MemFault::AccessFault
            },
            ..
        }
    );
    assert!(faulted, "{msg:?}: {resp:?}");
    assert!(ctx.irqs.is_empty() && ctx.traced.is_empty(), "{msg:?}");
    assert_eq!(snapshot_of(c), before, "{msg:?}");
}

#[test]
fn bad_accesses_fault_and_change_nothing() {
    // A DONE state with the line asserted, so a stray ACK or enable write would show.
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    program(&mut c, 0, 0x1000, 0);
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![true]));
    for offset in (0..0x20).step_by(4) {
        for len in [1u32, 2, 3, 5, 8] {
            assert_access_faults(&mut c, read(1, offset, len));
            assert_access_faults(&mut c, write(1, offset, &vec![0xff; len as usize]));
        }
    }
    for offset in [1, 2, 3, 0x05, 0x0a, 0x17, 0x19, 0x1e] {
        assert_access_faults(&mut c, read(1, offset, 4));
        assert_access_faults(&mut c, write(1, offset, &[0xff; 4]));
    }
    // COMMAND cannot be read; the reserved word cannot be read or written.
    assert_access_faults(&mut c, read(1, R_COMMAND, 4));
    assert_access_faults(&mut c, read(1, R_RESERVED, 4));
    assert_access_faults(&mut c, write(1, R_RESERVED, &[0xff; 4]));
    // Past or across the window, and at the top of the address space.
    for offset in [0x20, 0x24, 0x100, 0xffff_fffc, u64::MAX - 3] {
        assert_access_faults(&mut c, read(1, offset, 4));
        assert_access_faults(&mut c, write(1, offset, &[0xff; 4]));
    }
    assert_access_faults(&mut c, read(1, u64::MAX, 4));
    assert_access_faults(&mut c, write(1, u64::MAX - 1, &[1; 4]));
    assert_eq!(status(&mut c), DONE | u32::from(BAD_COUNT) << 8);
}

#[test]
fn protocol_violations_fault_the_session() {
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    submit(&mut c, 1, 0, 0x1000, 1);
    let before = snapshot_of(&c);
    let cases: Vec<(PortId, Message)> = vec![
        (MEM_PORT, read(1, 0, 0).into()),
        (MEM_PORT, write(1, 4, &[]).into()),
        (
            MEM_PORT,
            MemMsg::ReadResp {
                txn: TxnId(1),
                outcome: ReadOutcome::Data { data: vec![0; 4] },
            }
            .into(),
        ),
        (
            MEM_PORT,
            MemMsg::WriteResp {
                txn: TxnId(1),
                outcome: WriteOutcome::Done,
            }
            .into(),
        ),
        (MEM_PORT, IrqMsg::Level { asserted: true }.into()),
        (
            MEM_PORT,
            BlockMsg::ReadBlock {
                txn: TxnId(0),
                lba: 0,
            }
            .into(),
        ),
        // No engine request is ever outstanding.
        (
            DMA_PORT,
            MemMsg::WriteResp {
                txn: TxnId(0),
                outcome: WriteOutcome::Done,
            }
            .into(),
        ),
        (
            DMA_PORT,
            MemMsg::ReadResp {
                txn: TxnId(0),
                outcome: ReadOutcome::Data { data: vec![0; 16] },
            }
            .into(),
        ),
        (
            BLK_PORT,
            BlockMsg::ReadResult {
                txn: TxnId(0),
                outcome: BlockReadOutcome::Data { data: vec![0; 512] },
            }
            .into(),
        ),
        (IRQ_PORT, IrqMsg::Level { asserted: false }.into()),
        (PortId(4), read(1, 0x04, 4).into()),
    ];
    for (port, msg) in cases {
        for phase in [Phase::Request, Phase::Transfer, Phase::Complete] {
            let mut ctx = MockCtx::new(phase);
            let result = c.handle_event(
                &Delivered::Message {
                    port,
                    msg: msg.clone(),
                },
                &mut ctx,
            );
            assert!(
                matches!(result, Err(SimError::ComponentFault(_))),
                "{port:?} {msg:?}: {result:?}"
            );
            assert!(ctx.order.is_empty() && ctx.traced.is_empty());
            assert_eq!(snapshot_of(&c), before);
        }
    }
    let mut ctx = MockCtx::new(Phase::Request);
    let result = c.handle_event(&Delivered::Wake { token: 0 }, &mut ctx);
    assert!(matches!(result, Err(SimError::ComponentFault(_))));
    assert_eq!(snapshot_of(&c), before);
}

// --- Validation ---

#[test]
fn each_check_has_its_code() {
    assert_eq!(
        [BAD_COMMAND, BAD_COUNT, LBA_RANGE, DMA_ALIGN, DMA_RANGE],
        [1, 2, 3, 4, 5]
    );
    let cases = [
        (0, 0, 0x1000, 1, BAD_COMMAND),
        (3, 0, 0x1000, 1, BAD_COMMAND),
        (0xffff_ffff, 0, 0x1000, 1, BAD_COMMAND),
        (1, 0, 0x1000, 0, BAD_COUNT),
        (2, 16, 0x1000, 1, LBA_RANGE),
        (1, 0, 0x1008, 1, DMA_ALIGN),
        (2, 0, 0x0ff0, 1, DMA_RANGE),
        (1, 0, 0x1000, 1, 0),
        (2, 15, 0x2e00, 1, 0),
    ];
    for (command, lba, addr, count, code) in cases {
        let mut c = controller();
        assert_eq!(
            submit(&mut c, command, lba, addr, count),
            code,
            "COMMAND {command} LBA {lba} MEM_ADDR {addr:#x} COUNT {count}"
        );
    }
}

/// The first failing check in the frozen order sets the code, whatever else is wrong.
#[test]
fn validation_order_is_command_count_lba_alignment_range() {
    let cases = [
        // Everything wrong: the command wins.
        (7, 99, 0x0003, 0, BAD_COMMAND),
        (0, 16, 0x0ff8, 0, BAD_COMMAND),
        // A good command: the count wins over everything after it.
        (1, 99, 0x0003, 0, BAD_COUNT),
        (2, 0xffff_ffff, 0xffff_ffff, 0, BAD_COUNT),
        // Command and count good: the LBA range wins over alignment and range.
        (1, 16, 0x0003, 1, LBA_RANGE),
        (2, 10, 0x0ff8, 7, LBA_RANGE),
        (1, 0xffff_ffff, 0xffff_fff1, 0xffff_ffff, LBA_RANGE),
        // Up to the LBA good: alignment wins over range.
        (1, 0, 0x0003, 1, DMA_ALIGN),
        (2, 0, 0xffff_fff8, 16, DMA_ALIGN),
        // Only the range is wrong.
        (1, 0, 0x0ff0, 1, DMA_RANGE),
        (2, 0, 0x2ff0, 1, DMA_RANGE),
    ];
    for (command, lba, addr, count, code) in cases {
        let mut c = controller();
        assert_eq!(
            submit(&mut c, command, lba, addr, count),
            code,
            "COMMAND {command} LBA {lba} MEM_ADDR {addr:#x} COUNT {count}"
        );
        // The failure is exactly DONE + ERROR: nothing else set.
        assert_eq!(status(&mut c), DONE | u32::from(code) << 8);
    }
}

#[test]
fn the_lba_check_covers_the_whole_transfer() {
    // A large aperture so the range check never interferes.
    let big = |cap| controller_with(cap, 0, 1 << 32);
    let cases: [(u64, u32, u32, u8); 10] = [
        (1, 0, 1, 0),
        (1, 1, 1, LBA_RANGE),
        (1, 0, 2, LBA_RANGE),
        (10, 9, 1, 0),
        (10, 10, 1, LBA_RANGE),
        (10, 8, 2, 0),
        (10, 8, 3, LBA_RANGE),
        (1 << 32, 0xffff_ffff, 1, 0),
        (1 << 32, 0xffff_ffff, 2, LBA_RANGE),
        (1 << 32, 0xffff_ffff, 0xffff_ffff, LBA_RANGE),
    ];
    for (cap, lba, count, code) in cases {
        let mut c = big(cap);
        assert_eq!(
            submit(&mut c, 1, lba, 0, count),
            code,
            "capacity {cap} LBA {lba} COUNT {count}"
        );
    }
    // The whole controller-visible range passes the LBA check; the byte count then fails
    // the DMA range (5), proving check 3 passed.
    let mut c = big(1 << 32);
    assert_eq!(submit(&mut c, 2, 0, 0, 0xffff_ffff), DMA_RANGE);
    // LBAs are zero-extended: 0x8000_0000 is block 2^31, not a negative number.
    let mut c = big(1 << 31);
    assert_eq!(submit(&mut c, 1, 0x8000_0000, 0, 1), LBA_RANGE);
    let mut c = big((1 << 31) + 1);
    assert_eq!(submit(&mut c, 1, 0x8000_0000, 0, 1), 0);
}

#[test]
fn mem_addr_must_be_16_byte_aligned() {
    for offset in [1, 2, 4, 8, 12, 15] {
        let mut c = controller();
        assert_eq!(submit(&mut c, 1, 0, 0x1000 + offset, 1), DMA_ALIGN);
    }
    for offset in [0x10, 0x20, 0x1f0] {
        let mut c = controller();
        assert_eq!(submit(&mut c, 1, 0, 0x1000 + offset, 1), 0);
    }
}

#[test]
fn the_range_check_covers_the_whole_transfer() {
    let cases: [(u64, u64, u32, u32, u8); 10] = [
        // Aperture [0x1000, 0x3000): 16 blocks.
        (BASE, APERTURE, 0x1000, 1, 0),
        (BASE, APERTURE, 0x1000, 16, 0),
        (BASE, APERTURE, 0x1000, 17, LBA_RANGE),
        (BASE, APERTURE, 0x2e00, 1, 0),
        (BASE, APERTURE, 0x2e10, 1, DMA_RANGE),
        (BASE, APERTURE, 0x0ff0, 1, DMA_RANGE),
        (BASE, APERTURE, 0x3000, 1, DMA_RANGE),
        // At the top of the 32-bit space: the end is exactly 2^32, or past it.
        (0xffff_f000, 0x1000, 0xffff_fe00, 1, 0),
        (0xffff_f000, 0x1000, 0xffff_fff0, 1, DMA_RANGE),
        // An aperture smaller than a block admits nothing.
        (BASE, 0x100, 0x1000, 1, DMA_RANGE),
    ];
    for (base, size, addr, count, code) in cases {
        let mut c = controller_with(CAPACITY, base, size);
        assert_eq!(
            submit(&mut c, 2, 0, addr, count),
            code,
            "aperture {base:#x}+{size:#x} MEM_ADDR {addr:#x} COUNT {count}"
        );
    }
    // The byte count of the largest command is 2^41: no u32 wrap in the arithmetic.
    let mut c = controller_with(1 << 32, 0, 1 << 32);
    assert_eq!(submit(&mut c, 1, 0, 0, 0x0080_0001), DMA_RANGE);
    let mut c = controller_with(1 << 32, 0, 1 << 32);
    assert_eq!(submit(&mut c, 1, 0, 0, 0x0080_0000), 0);
}

// --- Lifecycle ---

#[test]
fn a_valid_command_goes_busy_and_latches_its_registers() {
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    program(&mut c, 3, 0x1400, 2);
    let (resp, ctx) = mmio(&mut c, write(1, R_COMMAND, &1u32.to_le_bytes()));
    assert_eq!(
        resp,
        MemMsg::WriteResp {
            txn: TxnId(1),
            outcome: WriteOutcome::Done
        }
    );
    // No level, no completion, no engine traffic (checked by `mmio`).
    assert!(ctx.irqs.is_empty());
    assert_eq!(ctx.traced.len(), 1);
    assert_eq!(status(&mut c), BUSY);
    assert_eq!(rd(&mut c, R_IRQ_STATUS), Some(0));
    assert_eq!(
        field(&c, "command"),
        Value::Str("read lba=0x3 addr=0x1400 count=2".into())
    );
    assert_eq!(field(&c, "engine"), Value::Str("issue".into()));
    let latched = snapshot_of(&c);
    // The registers can be rewritten at any time; the latched command does not change.
    program(&mut c, 9, 0x2000, 5);
    assert_eq!(rd(&mut c, R_LBA), Some(9));
    assert_eq!(rd(&mut c, R_MEM_ADDR), Some(0x2000));
    assert_eq!(rd(&mut c, R_BLOCK_COUNT), Some(5));
    assert_eq!(
        field(&c, "command"),
        Value::Str("read lba=0x3 addr=0x1400 count=2".into())
    );
    assert_eq!(status(&mut c), BUSY);
    // Only the three register words of the snapshot differ.
    let now = snapshot_of(&c);
    let registers = header_len()..header_len() + 12;
    assert_eq!(latched[..registers.start], now[..registers.start]);
    assert_eq!(latched[registers.end..], now[registers.end..]);
    // An accepted command never completes on its own.
    assert_eq!(wr(&mut c, R_IRQ_STATUS, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(status(&mut c), BUSY);
}

#[test]
fn a_command_while_busy_is_rejected_and_changes_nothing_else() {
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    assert_eq!(submit(&mut c, 2, 1, 0x1000, 4), 0);
    let before = field(&c, "command");
    for command in [1u32, 2, 0, 99] {
        let (resp, ctx) = mmio(&mut c, write(5, R_COMMAND, &command.to_le_bytes()));
        assert_eq!(
            resp,
            MemMsg::WriteResp {
                txn: TxnId(5),
                outcome: WriteOutcome::Done
            }
        );
        assert!(ctx.irqs.is_empty());
        assert_eq!(ctx.traced, vec![(REJECTED_KIND, vec![])]);
        assert_eq!(status(&mut c), BUSY | REJECTED);
        assert_eq!(field(&c, "command"), before);
        assert_eq!(field(&c, "irq"), Value::Bool(false));
    }
}

#[test]
fn a_command_while_done_is_rejected_until_acknowledged() {
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    program(&mut c, 0, 0x1000, 0);
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![true]));
    let done = DONE | u32::from(BAD_COUNT) << 8;
    assert_eq!(status(&mut c), done);
    // A now-valid command is still rejected: the completion is not lost.
    program(&mut c, 0, 0x1000, 1);
    let (resp, ctx) = mmio(&mut c, write(5, R_COMMAND, &1u32.to_le_bytes()));
    assert_eq!(
        resp,
        MemMsg::WriteResp {
            txn: TxnId(5),
            outcome: WriteOutcome::Done
        }
    );
    assert!(ctx.irqs.is_empty());
    assert_eq!(ctx.traced, vec![(REJECTED_KIND, vec![])]);
    assert_eq!(status(&mut c), done | REJECTED);
    assert_eq!(rd(&mut c, R_IRQ_STATUS), Some(1));
    // ACK returns to IDLE and deasserts the line; REJECTED stays.
    assert_eq!(wr(&mut c, ACK, 1), (WriteOutcome::Done, vec![false]));
    assert_eq!(status(&mut c), REJECTED);
    // Now a command is accepted.
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(status(&mut c), BUSY | REJECTED);
}

#[test]
fn rejected_is_sticky_until_written_one_in_status_bit_2() {
    let mut c = controller();
    submit(&mut c, 1, 0, 0x1000, 0);
    wr(&mut c, R_COMMAND, 1);
    let done = DONE | u32::from(BAD_COUNT) << 8;
    assert_eq!(status(&mut c), done | REJECTED);
    // Writes without bit 2 leave it, and change nothing else.
    for value in [0, 1, 2, 3, 0xffff_fffb, 0x0000_ff00] {
        set(&mut c, R_STATUS, value);
        assert_eq!(status(&mut c), done | REJECTED, "STATUS write {value:#x}");
    }
    // ACK clears DONE and ERROR but not REJECTED.
    set(&mut c, ACK, 1);
    assert_eq!(status(&mut c), REJECTED);
    // An accepted command does not clear it.
    assert_eq!(submit(&mut c, 1, 0, 0x1000, 1), 0);
    assert_eq!(status(&mut c), BUSY | REJECTED);
    // Programming writes and IRQ_ENABLE writes do not clear it.
    program(&mut c, 1, 0x1010, 2);
    set(&mut c, R_IRQ_ENABLE, 1);
    assert_eq!(status(&mut c), BUSY | REJECTED);
    // Writing bit 2 clears only REJECTED.
    set(&mut c, R_STATUS, REJECTED);
    assert_eq!(status(&mut c), BUSY);
    // And clearing it when it is clear does nothing.
    set(&mut c, R_STATUS, 0xffff_ffff);
    assert_eq!(status(&mut c), BUSY);
}

#[test]
fn ack_acts_only_in_done_and_only_on_bit_0() {
    let mut c = controller();
    // IDLE: nothing.
    set(&mut c, ACK, 1);
    assert_eq!(status(&mut c), 0);
    // DONE: bit 0 clear does nothing; other bits are ignored.
    submit(&mut c, 9, 0, 0x1000, 1);
    set(&mut c, ACK, 0xffff_fffe);
    assert_eq!(status(&mut c), DONE | u32::from(BAD_COMMAND) << 8);
    set(&mut c, ACK, 0xffff_ffff);
    assert_eq!(status(&mut c), 0);
    // BUSY: nothing.
    assert_eq!(submit(&mut c, 1, 0, 0x1000, 1), 0);
    set(&mut c, ACK, 1);
    assert_eq!(status(&mut c), BUSY);
}

// --- Interrupt line ---

#[test]
fn the_line_follows_done_and_irq_enable() {
    let mut c = controller();
    // A failure with the interrupt disabled asserts nothing.
    program(&mut c, 0, 0x1003, 1);
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(rd(&mut c, R_IRQ_STATUS), Some(1));
    // Enabling while DONE asserts; the same value again sends nothing.
    assert_eq!(
        wr(&mut c, R_IRQ_ENABLE, 1),
        (WriteOutcome::Done, vec![true])
    );
    assert_eq!(wr(&mut c, R_IRQ_ENABLE, 3), (WriteOutcome::Done, vec![]));
    // Disabling deasserts, re-enabling asserts again.
    assert_eq!(
        wr(&mut c, R_IRQ_ENABLE, 0),
        (WriteOutcome::Done, vec![false])
    );
    assert_eq!(wr(&mut c, R_IRQ_ENABLE, 2), (WriteOutcome::Done, vec![]));
    assert_eq!(
        wr(&mut c, R_IRQ_ENABLE, 1),
        (WriteOutcome::Done, vec![true])
    );
    // ACK deasserts once; a repeated ACK sends nothing.
    assert_eq!(wr(&mut c, ACK, 1), (WriteOutcome::Done, vec![false]));
    assert_eq!(wr(&mut c, ACK, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(rd(&mut c, R_IRQ_STATUS), Some(0));
    // A failure with the interrupt enabled asserts at once.
    program(&mut c, 0, 0x1000, 0);
    assert_eq!(wr(&mut c, R_COMMAND, 2), (WriteOutcome::Done, vec![true]));
    // A rejection while DONE and enabled sends nothing.
    assert_eq!(wr(&mut c, R_COMMAND, 2), (WriteOutcome::Done, vec![]));
    assert_eq!(wr(&mut c, ACK, 1), (WriteOutcome::Done, vec![false]));
    // Accepting, and rejecting while BUSY, send nothing.
    program(&mut c, 0, 0x1000, 1);
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(wr(&mut c, R_COMMAND, 1), (WriteOutcome::Done, vec![]));
    assert_eq!(wr(&mut c, R_IRQ_ENABLE, 0), (WriteOutcome::Done, vec![]));
    assert_eq!(wr(&mut c, R_IRQ_ENABLE, 1), (WriteOutcome::Done, vec![]));
}

// --- Trace ---

#[test]
fn commands_completions_and_rejections_are_traced() {
    let mut c = controller();
    program(&mut c, 5, 0x1234, 3);
    let (_, ctx) = mmio(&mut c, write(1, R_COMMAND, &1u32.to_le_bytes()));
    let command = |op, lba, addr, count, accepted| {
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
    };
    assert_eq!(
        ctx.traced,
        vec![
            command(1, 5, 0x1234, 3, false),
            (DONE_KIND, vec![("error", Value::U64(u64::from(DMA_ALIGN)))]),
        ]
    );
    let (_, ctx) = mmio(&mut c, write(1, R_COMMAND, &2u32.to_le_bytes()));
    assert_eq!(ctx.traced, vec![(REJECTED_KIND, vec![])]);
    set(&mut c, ACK, 1);
    program(&mut c, 5, 0x1230, 3);
    let (_, ctx) = mmio(&mut c, write(1, R_COMMAND, &2u32.to_le_bytes()));
    assert_eq!(ctx.traced, vec![command(2, 5, 0x1230, 3, true)]);
    assert_eq!(
        [COMMAND_KIND, DONE_KIND, REJECTED_KIND],
        [
            "platform.blk.command",
            "platform.blk.done",
            "platform.blk.rejected"
        ]
    );
}

// --- Snapshots ---

/// The config header of `controller()`'s snapshot, written independently.
fn header() -> Vec<u8> {
    header_of(&config(CAPACITY, BASE, APERTURE))
}

fn header_of(cfg: &DmaBlockControllerConfig) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u32(cfg.clock.0);
    match cfg.latency {
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
    w.u64(cfg.capacity_blocks);
    w.u64(cfg.dma_base);
    w.u64(cfg.dma_size);
    w.into_bytes()
}

fn header_len() -> usize {
    header().len()
}

/// Every field of a schema-1 snapshot after the config, for writing valid and invalid
/// snapshots by hand.
#[derive(Clone, Debug)]
struct Raw {
    registers: [u32; 4],
    busy: u8,
    done: u8,
    rejected: u8,
    error: u8,
    latched: Option<(u8, u32, u32, u32)>,
    engine: u8,
    txn: Option<u64>,
    block: u32,
    beat: u8,
    buffer: Vec<u8>,
    dma_txn: u64,
    blk_txn: u64,
    irq: u8,
}

impl Raw {
    fn idle() -> Raw {
        Raw {
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
            buffer: vec![],
            dma_txn: 0,
            blk_txn: 0,
            irq: 0,
        }
    }

    fn busy() -> Raw {
        Raw {
            registers: [2, 0x1100, 3, 1],
            busy: 1,
            latched: Some((1, 2, 0x1100, 3)),
            engine: 1,
            ..Raw::idle()
        }
    }

    fn done(error: u8, irq_enable: bool) -> Raw {
        Raw {
            registers: [0, 0, 0, u32::from(irq_enable)],
            done: 1,
            error,
            irq: u8::from(irq_enable),
            ..Raw::idle()
        }
    }

    fn encode_with(&self, header: &[u8]) -> Vec<u8> {
        let mut w = SnapshotWriter::new();
        w.raw(header);
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

    fn encode(&self) -> Vec<u8> {
        self.encode_with(&header())
    }
}

#[test]
fn snapshot_layout_is_pinned() {
    let mut c = controller();
    assert_eq!(snapshot_of(&c), Raw::idle().encode());
    set(&mut c, R_IRQ_ENABLE, 1);
    assert_eq!(submit(&mut c, 1, 2, 0x1100, 3), 0);
    assert_eq!(snapshot_of(&c), Raw::busy().encode());
    // After a failed command and a rejection, with an After latency.
    let cfg = DmaBlockControllerConfig {
        latency: LinkLatency::After(Duration::from_ns(5)),
        ..config(1 << 32, 0x8000_0000, 0x0100_0000)
    };
    let mut c = DmaBlockController::new(cfg).unwrap();
    let mut ctx = MockCtx::new(Phase::Transfer);
    for (offset, value) in [
        (R_IRQ_ENABLE, 1),
        (R_LBA, 0xffff_ffff),
        (R_MEM_ADDR, 0x8000_0000),
        (R_BLOCK_COUNT, 2),
        (R_COMMAND, 2),
        (R_COMMAND, 1),
    ] {
        ctx.deliver(&mut c, MEM_PORT, write(0, offset, &u32::to_le_bytes(value)))
            .unwrap();
    }
    let expected = Raw {
        registers: [0xffff_ffff, 0x8000_0000, 2, 1],
        rejected: 1,
        ..Raw::done(LBA_RANGE, true)
    };
    assert_eq!(snapshot_of(&c), expected.encode_with(&header_of(&cfg)));
}

/// Named states for round trips and continuations, each built through MMIO only.
fn states() -> Vec<(&'static str, DmaBlockController)> {
    let mut out = vec![("reset", controller())];
    let mut c = controller();
    program(&mut c, 7, 0x1230, 9);
    set(&mut c, R_IRQ_ENABLE, 1);
    out.push(("programmed idle", c));
    let mut c = controller();
    submit(&mut c, 0, 0, 0, 0);
    wr(&mut c, R_COMMAND, 1);
    set(&mut c, ACK, 1);
    out.push(("rejected sticky in idle", c));
    let mut c = controller();
    submit(&mut c, 1, 0, 0x1008, 1);
    out.push(("error, irq disabled", c));
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    submit(&mut c, 2, 0, 0x0ff0, 1);
    out.push(("error, irq enabled", c));
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    submit(&mut c, 2, 16, 0x1000, 1);
    assert_eq!(wr(&mut c, ACK, 1), (WriteOutcome::Done, vec![false]));
    out.push(("acknowledged", c));
    let mut c = controller();
    set(&mut c, R_IRQ_ENABLE, 1);
    submit(&mut c, 2, 12, 0x2800, 4);
    out.push(("busy", c));
    let mut c = controller();
    submit(&mut c, 1, 0, 0x1000, 16);
    wr(&mut c, R_COMMAND, 2);
    program(&mut c, 1, 2, 3);
    out.push(("busy, rejected, reprogrammed", c));
    out
}

/// A script touching every register and path, applied after a checkpoint.
fn continuation() -> Vec<(u64, u32)> {
    vec![
        (R_COMMAND, 1),
        (R_IRQ_ENABLE, 1),
        (ACK, 1),
        (R_STATUS, REJECTED),
        (R_LBA, 1),
        (R_MEM_ADDR, 0x1000),
        (R_BLOCK_COUNT, 2),
        (R_COMMAND, 2),
        (R_COMMAND, 2),
        (R_IRQ_ENABLE, 0),
        (ACK, 1),
    ]
}

fn run_script(c: &mut DmaBlockController, script: &[(u64, u32)]) -> Vec<(MemMsg, Vec<bool>)> {
    let mut out = Vec::new();
    for &(offset, value) in script {
        let (resp, ctx) = mmio(c, write(1, offset, &value.to_le_bytes()));
        out.push((resp, ctx.irqs.iter().map(|i| i.asserted).collect()));
        let (resp, _) = mmio(c, read(2, R_STATUS, 4));
        out.push((resp, vec![]));
    }
    out
}

#[test]
fn every_state_round_trips_and_continues_identically() {
    for (name, mut original) in states() {
        let bytes = snapshot_of(&original);
        let mut restored = controller();
        restore_into(&mut restored, &bytes).unwrap();
        assert_eq!(snapshot_of(&restored), bytes, "{name}");
        assert_eq!(restored.inspect(), original.inspect(), "{name}");
        // Into a used controller too.
        let mut used = controller();
        submit(&mut used, 1, 0, 0x1000, 1);
        restore_into(&mut used, &bytes).unwrap();
        assert_eq!(snapshot_of(&used), bytes, "{name}");
        // Continuation.
        let a = run_script(&mut original, &continuation());
        let b = run_script(&mut restored, &continuation());
        assert_eq!(a, b, "{name}");
        assert_eq!(snapshot_of(&original), snapshot_of(&restored), "{name}");
    }
}

/// Requires `bytes` to be rejected, leaving every state unchanged.
fn assert_rejected(bytes: &[u8], why: &str) {
    for (name, mut target) in states() {
        let before = snapshot_of(&target);
        let result = restore_into(&mut target, bytes);
        assert!(result.is_err(), "accepted {why} into {name}");
        assert_eq!(snapshot_of(&target), before, "{why} changed {name}");
    }
}

#[test]
fn hand_written_valid_snapshots_restore() {
    for raw in [
        Raw::idle(),
        Raw::busy(),
        Raw::done(BAD_COMMAND, false),
        Raw::done(DMA_RANGE, true),
        Raw {
            rejected: 1,
            ..Raw::busy()
        },
    ] {
        let bytes = raw.encode();
        let mut c = controller();
        restore_into(&mut c, &bytes).unwrap();
        assert_eq!(snapshot_of(&c), bytes, "{raw:?}");
    }
}

#[test]
fn restore_rejects_impossible_states() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "different capacity",
            Raw::idle().encode_with(&header_of(&config(17, BASE, APERTURE))),
        ),
        (
            "different aperture",
            Raw::idle().encode_with(&header_of(&config(CAPACITY, BASE, 0x3000))),
        ),
        (
            "different clock",
            Raw::idle().encode_with(&header_of(&DmaBlockControllerConfig {
                clock: ClockDomainId(4),
                ..config(CAPACITY, BASE, APERTURE)
            })),
        ),
        (
            "different latency",
            Raw::idle().encode_with(&header_of(&DmaBlockControllerConfig {
                latency: LinkLatency::Cycles {
                    domain: CLOCK,
                    k: 1,
                },
                ..config(CAPACITY, BASE, APERTURE)
            })),
        ),
        (
            "IRQ_ENABLE bit 1",
            Raw {
                registers: [0, 0, 0, 2],
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "bool 2",
            Raw {
                rejected: 2,
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "busy and done",
            Raw {
                done: 1,
                error: 1,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "error in idle",
            Raw {
                error: 1,
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "error while busy",
            Raw {
                error: 2,
                ..Raw::busy()
            }
            .encode(),
        ),
        ("done without an error", Raw::done(0, false).encode()),
        ("DMA_FAULT without an engine", Raw::done(6, false).encode()),
        (
            "MEDIA_ERROR without an engine",
            Raw::done(7, false).encode(),
        ),
        ("unused code", Raw::done(8, false).encode()),
        (
            "busy without a latched command",
            Raw {
                latched: None,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "latched command in idle",
            Raw {
                latched: Some((1, 0, 0x1000, 1)),
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "latched unknown operation",
            Raw {
                latched: Some((3, 0, 0x1000, 1)),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "latched zero count",
            Raw {
                latched: Some((1, 0, 0x1000, 0)),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "latched LBA out of range",
            Raw {
                latched: Some((2, 16, 0x1000, 1)),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "latched misaligned address",
            Raw {
                latched: Some((2, 0, 0x1004, 1)),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "latched address outside the aperture",
            Raw {
                latched: Some((2, 0, 0x0e00, 1)),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "idle engine while busy",
            Raw {
                engine: 0,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "issuing engine while idle",
            Raw {
                engine: 1,
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "waiting for media",
            Raw {
                engine: 2,
                txn: Some(0),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "waiting for a beat",
            Raw {
                engine: 3,
                txn: Some(0),
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "unknown engine state",
            Raw {
                engine: 9,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "block progress",
            Raw {
                block: 1,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "beat progress",
            Raw {
                beat: 1,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "buffered data",
            Raw {
                buffer: vec![0; 16],
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "used dma txn",
            Raw {
                dma_txn: 1,
                ..Raw::busy()
            }
            .encode(),
        ),
        (
            "used blk txn",
            Raw {
                blk_txn: 1,
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "level asserted while idle",
            Raw {
                registers: [0, 0, 0, 1],
                irq: 1,
                ..Raw::idle()
            }
            .encode(),
        ),
        (
            "level asserted while disabled",
            Raw {
                irq: 1,
                ..Raw::done(BAD_COUNT, false)
            }
            .encode(),
        ),
        (
            "level deasserted while done and enabled",
            Raw {
                irq: 0,
                ..Raw::done(BAD_COUNT, true)
            }
            .encode(),
        ),
        ("empty", vec![]),
    ];
    for (why, bytes) in cases {
        assert_rejected(&bytes, why);
    }
    let good = Raw::busy().encode();
    assert_rejected(&good[..good.len() - 1], "truncated");
    let mut long = good.clone();
    long.push(0);
    // Trailing bytes: rejected by the reader's `finish`.
    assert!(restore_into(&mut controller(), &long).is_err());
    let mut r = SnapshotReader::new(&[]);
    assert!(controller().restore(&mut r, 1).is_err());
}

// --- Independent model ---

/// The register and lifecycle model of `docs/m2-design.md` §9.2–§9.4, written from the
/// tables without the component's code.
#[derive(Clone, Debug, PartialEq)]
struct Model {
    capacity: u64,
    base: u64,
    size: u64,
    lba: u32,
    addr: u32,
    count: u32,
    enable: bool,
    busy: bool,
    done: bool,
    rejected: bool,
    error: u8,
    latched: Option<(u8, u32, u32, u32)>,
    line: bool,
}

impl Model {
    fn new(capacity: u64, base: u64, size: u64) -> Model {
        Model {
            capacity,
            base,
            size,
            lba: 0,
            addr: 0,
            count: 0,
            enable: false,
            busy: false,
            done: false,
            rejected: false,
            error: 0,
            latched: None,
            line: false,
        }
    }

    /// The first failing check's code, or 0; in 128-bit arithmetic.
    fn check(&self, command: u32) -> u8 {
        let (lba, addr, count) = (self.lba as u128, self.addr as u128, self.count as u128);
        let failures = [
            command != 1 && command != 2,
            count == 0,
            lba + count > self.capacity as u128,
            addr % 16 != 0,
            addr < self.base as u128 || addr + count * 512 > (self.base + self.size) as u128,
        ];
        failures.iter().position(|&f| f).map_or(0, |i| i as u8 + 1)
    }

    /// The levels a change of state sends.
    fn relevel(&mut self) -> Vec<bool> {
        let line = self.done && self.enable;
        if line == self.line {
            vec![]
        } else {
            self.line = line;
            vec![line]
        }
    }

    fn status(&self) -> u32 {
        (self.busy as u32)
            | (self.done as u32) << 1
            | (self.rejected as u32) << 2
            | (self.error as u32) << 8
    }

    fn read(&self, offset: u64) -> Option<u32> {
        Some(match offset {
            R_STATUS => self.status(),
            R_LBA => self.lba,
            R_MEM_ADDR => self.addr,
            R_BLOCK_COUNT => self.count,
            R_IRQ_ENABLE => self.enable as u32,
            R_IRQ_STATUS => self.done as u32,
            _ => return None,
        })
    }

    /// A 4-byte aligned write: whether it was accepted, and the levels sent.
    fn write(&mut self, offset: u64, value: u32) -> (bool, Vec<bool>) {
        match offset {
            R_COMMAND => {
                if self.busy || self.done {
                    self.rejected = true;
                    return (true, vec![]);
                }
                let code = self.check(value);
                if code == 0 {
                    self.busy = true;
                    self.latched = Some((value as u8, self.lba, self.addr, self.count));
                    (true, vec![])
                } else {
                    self.done = true;
                    self.error = code;
                    (true, self.relevel())
                }
            }
            R_STATUS => {
                if value & 4 != 0 {
                    self.rejected = false;
                }
                (true, vec![])
            }
            R_LBA => {
                self.lba = value;
                (true, vec![])
            }
            R_MEM_ADDR => {
                self.addr = value;
                (true, vec![])
            }
            R_BLOCK_COUNT => {
                self.count = value;
                (true, vec![])
            }
            R_IRQ_ENABLE => {
                self.enable = value & 1 == 1;
                (true, self.relevel())
            }
            R_IRQ_STATUS => {
                if value & 1 == 1 && self.done {
                    self.done = false;
                    self.error = 0;
                    return (true, self.relevel());
                }
                (true, vec![])
            }
            _ => (false, vec![]),
        }
    }

    /// The canonical snapshot of this state.
    fn snapshot(&self, header: &[u8]) -> Vec<u8> {
        Raw {
            registers: [self.lba, self.addr, self.count, self.enable as u32],
            busy: self.busy as u8,
            done: self.done as u8,
            rejected: self.rejected as u8,
            error: self.error,
            latched: self.latched,
            engine: self.busy as u8,
            txn: None,
            block: 0,
            beat: 0,
            buffer: vec![],
            dma_txn: 0,
            blk_txn: 0,
            irq: self.line as u8,
        }
        .encode_with(header)
    }
}

#[derive(Clone, Debug)]
enum Op {
    Write(u64, u32),
    Read(u64),
    /// Any other access: a width other than 4 or a misaligned or out-of-window offset.
    Odd(MemMsg),
}

fn offset() -> impl Strategy<Value = u64> {
    prop_oneof![
        8 => (0..8u64).prop_map(|i| i * 4),
        1 => 0..0x40u64,
        1 => Just(u64::MAX - 3),
    ]
}

/// Values that exercise every check near its boundary for a capacity-16 controller with
/// the aperture at 0x1000.
fn value() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => 0..4u32,
        3 => 0..20u32,
        3 => (0x0fu32..0x31).prop_map(|b| b << 8),
        2 => (0xfe0u32..0x3020).prop_map(|a| a & !3),
        1 => Just(4u32),
        1 => Just(0xffff_ffff),
        1 => any::<u32>(),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (offset(), value()).prop_map(|(o, v)| Op::Write(o, v)),
        4 => offset().prop_map(Op::Read),
        1 => (offset(), prop_oneof![Just(1u32), Just(2), Just(3), Just(8)])
            .prop_map(|(o, len)| Op::Odd(read(3, o, len))),
        1 => (offset(), prop_oneof![Just(1usize), Just(2), Just(8)])
            .prop_map(|(o, len)| Op::Odd(write(3, o, &vec![0xff; len]))),
    ]
}

/// Whether `offset` is an aligned 4-byte access inside the window.
fn word(offset: u64) -> bool {
    offset.is_multiple_of(4) && offset < 0x20
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn the_controller_matches_the_model(
        ops in prop::collection::vec(op(), 1..64),
        capacity in prop_oneof![Just(CAPACITY), Just(1u64), Just(1u64 << 32)],
    ) {
        let cfg = config(capacity, BASE, APERTURE);
        let mut c = DmaBlockController::new(cfg).unwrap();
        let mut m = Model::new(capacity, BASE, APERTURE);
        let header = header_of(&cfg);
        for op in &ops {
            let (resp, ctx) = match op {
                Op::Write(o, v) => mmio(&mut c, write(1, *o, &v.to_le_bytes())),
                Op::Read(o) => mmio(&mut c, read(1, *o, 4)),
                Op::Odd(msg) => mmio(&mut c, msg.clone()),
            };
            let levels: Vec<bool> = ctx.irqs.iter().map(|i| i.asserted).collect();
            let fault = MemFault::AccessFault;
            match op {
                Op::Write(o, v) => {
                    let (ok, expected) = if word(*o) { m.write(*o, *v) } else { (false, vec![]) };
                    let outcome = if ok { WriteOutcome::Done } else { WriteOutcome::Fault { fault } };
                    prop_assert_eq!(resp, MemMsg::WriteResp { txn: TxnId(1), outcome });
                    prop_assert_eq!(levels, expected);
                }
                Op::Read(o) => {
                    let outcome = match m.read(*o).filter(|_| word(*o)) {
                        Some(v) => ReadOutcome::Data { data: v.to_le_bytes().to_vec() },
                        None => ReadOutcome::Fault { fault },
                    };
                    prop_assert_eq!(resp, MemMsg::ReadResp { txn: TxnId(1), outcome });
                    prop_assert!(levels.is_empty());
                }
                Op::Odd(_) => {
                    let faulted = matches!(
                        resp,
                        MemMsg::ReadResp { outcome: ReadOutcome::Fault { .. }, .. }
                            | MemMsg::WriteResp { outcome: WriteOutcome::Fault { .. }, .. }
                    );
                    prop_assert!(faulted);
                    prop_assert!(levels.is_empty());
                }
            }
            prop_assert_eq!(snapshot_of(&c), m.snapshot(&header));
        }
        // And the final state round-trips.
        let bytes = snapshot_of(&c);
        let mut fresh = DmaBlockController::new(cfg).unwrap();
        restore_into(&mut fresh, &bytes).unwrap();
        prop_assert_eq!(snapshot_of(&fresh), bytes);
    }
}

// --- Runtime ---

const TICKS_PER_CYCLE: u64 = 1000;

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

/// A target that faults the session on any message: proves `dma` and `blk` stay silent.
struct TieOff(&'static str, systemscope_contracts::protocol::ProtocolId);

impl Component for TieOff {
    fn type_name(&self) -> &'static str {
        "test.tie_off"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: self.0,
            protocol: self.1,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Err(SimError::ComponentFault(
            "tie-off: the controller sent engine traffic",
        ))
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
    sink: ComponentId,
}

/// A scripted MMIO initiator → the controller (`Cycles { 1 }`, the controller answering
/// after `Cycles { 2 }`), its `irq` → a sink, and `dma` and `blk` → tie-offs.
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
    let ctl = DmaBlockController::new(DmaBlockControllerConfig {
        clock,
        latency: cycles(2),
        capacity_blocks: CAPACITY,
        dma_base: BASE,
        dma_size: APERTURE,
    })
    .unwrap();
    let ctl = t.add_component("soc.blk", Box::new(ctl));
    let sink = t.add_component("soc.irqc", Box::new(IrqSink));
    let ram = t.add_component("soc.ram", Box::new(TieOff("mem", mem_v1::PROTOCOL)));
    let disk = t.add_component("soc.disk", Box::new(TieOff("blk", block_v0::PROTOCOL)));
    t.connect((host, "mem"), (ctl, "mem"), Some(cycles(1)));
    t.connect((ctl, "irq"), (sink, "irq"), None);
    t.connect((ctl, "dma"), (ram, "mem"), Some(cycles(1)));
    t.connect((ctl, "blk"), (disk, "blk"), Some(cycles(1)));
    (
        t.elaborate(SessionConfig::default()).unwrap(),
        Ids { host, ctl, sink },
    )
}

fn w32(offset: u64, value: u32) -> MemMsg {
    write(0, offset, &value.to_le_bytes())
}

/// Programming, a validation failure with the interrupt enabled, a rejection in DONE,
/// ACK, clearing REJECTED, a valid command, a rejection in BUSY, reprogramming during
/// BUSY, reads, and bad accesses. Each request has its own txn.
fn workload() -> Vec<(u64, MemMsg)> {
    let mut requests = vec![
        (0, w32(R_LBA, 2)),
        (0, w32(R_MEM_ADDR, 0x1000)),
        (1, w32(R_BLOCK_COUNT, 0)),
        (2, w32(R_IRQ_ENABLE, 1)),
        (3, w32(R_COMMAND, 1)),
        (4, read(0, R_STATUS, 4)),
        (5, w32(R_COMMAND, 2)),
        (6, w32(ACK, 1)),
        (6, read(0, R_IRQ_STATUS, 4)),
        (7, w32(R_STATUS, REJECTED)),
        (8, w32(R_BLOCK_COUNT, 4)),
        (9, w32(R_COMMAND, 1)),
        (10, w32(R_COMMAND, 1)),
        (11, w32(R_LBA, 7)),
        (12, read(0, R_STATUS, 4)),
        (12, read(0, R_LBA, 4)),
        (13, read(0, R_COMMAND, 4)),
        (14, w32(R_RESERVED, 1)),
        (15, read(0, R_BLOCK_COUNT, 2)),
    ];
    for (i, (_, msg)) in requests.iter_mut().enumerate() {
        match msg {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => *txn = TxnId(i as u64),
            _ => unreachable!(),
        }
    }
    requests
}

fn outcome_of(e: &Dispatched) -> MemMsg {
    match &e.delivery {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => m.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_runtime_workload_matches_the_model_and_the_frozen_timing() {
    let (mut rt, ids) = build(&workload());
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    // Responses: the model's, each with its request's txn, `Complete`, at request + 1
    // (link) + 2 (latency) + 1 (link) cycles.
    let mut m = Model::new(CAPACITY, BASE, APERTURE);
    let mut expected_levels = Vec::new();
    let expected: Vec<(u64, MemMsg)> = workload()
        .into_iter()
        .map(|(k, msg)| {
            let resp = match msg {
                MemMsg::WriteReq { txn, addr, data } => {
                    let value = u32::from_le_bytes(data.try_into().unwrap());
                    let (ok, levels) = m.write(addr, value);
                    expected_levels.extend(levels.into_iter().map(|l| (k + 1, l)));
                    MemMsg::WriteResp {
                        txn,
                        outcome: if ok {
                            WriteOutcome::Done
                        } else {
                            WriteOutcome::Fault {
                                fault: MemFault::AccessFault,
                            }
                        },
                    }
                }
                MemMsg::ReadReq { txn, addr, len } => MemMsg::ReadResp {
                    txn,
                    outcome: match m.read(addr).filter(|_| len == 4) {
                        Some(v) => ReadOutcome::Data {
                            data: v.to_le_bytes().to_vec(),
                        },
                        None => ReadOutcome::Fault {
                            fault: MemFault::AccessFault,
                        },
                    },
                },
                _ => unreachable!(),
            };
            (k + 4, resp)
        })
        .collect();
    let responses: Vec<(u64, MemMsg)> = events
        .iter()
        .filter(|e| e.target == ids.host)
        .map(|e| {
            assert_eq!(e.key.phase, Phase::Complete);
            (e.key.tick.0 / TICKS_PER_CYCLE, outcome_of(e))
        })
        .collect();
    assert_eq!(responses, expected);
    // Levels arrive in the same tick as the write that caused them, in `Complete`: the
    // effect is at acceptance, two cycles before the write's response.
    let levels: Vec<(u64, bool)> = events
        .iter()
        .filter(|e| e.target == ids.sink)
        .map(|e| {
            assert_eq!(e.key.phase, Phase::Complete);
            let Delivered::Message {
                msg: Message::Irq(IrqMsg::Level { asserted }),
                ..
            } = e.delivery
            else {
                panic!("{e:?}")
            };
            (e.key.tick.0 / TICKS_PER_CYCLE, asserted)
        })
        .collect();
    assert_eq!(levels, [(4, true), (7, false)]);
    assert_eq!(levels, expected_levels);
    // Traces: the failed command, its completion, two rejections, the accepted command.
    let trace = rt.take_trace().unwrap();
    let kinds: Vec<(u64, &str)> = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == ids.ctl)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0 / TICKS_PER_CYCLE, r.kind)
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (4, COMMAND_KIND),
            (4, DONE_KIND),
            (6, REJECTED_KIND),
            (10, COMMAND_KIND),
            (11, REJECTED_KIND),
        ]
    );
    // The controller ends BUSY with the command latched at cycle 10, and nothing else
    // ever happened: no engine traffic reached a tie-off (it would have faulted).
    let snap = rt.snapshot().unwrap();
    let ours = m.snapshot(&header_of(&DmaBlockControllerConfig {
        clock: ClockDomainId(0),
        latency: LinkLatency::Cycles {
            domain: ClockDomainId(0),
            k: 2,
        },
        ..config(CAPACITY, BASE, APERTURE)
    }));
    assert!(snap.windows(ours.len()).any(|w| w == ours));
    assert!(m.busy && m.latched == Some((1, 2, 0x1000, 4)));
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let requests = workload();
    let reference = {
        let (mut rt, _) = build(&requests);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
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
        let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(fresh.snapshot().unwrap(), reference.2, "checkpoint at {k}");
        assert_eq!(
            fresh.take_trace().unwrap(),
            reference.1,
            "checkpoint at {k}"
        );
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.3, reference.4),
            "checkpoint after {k} events"
        );
    }
}

#[test]
fn runs_are_deterministic() {
    let run = || {
        let (mut rt, ids) = build(&workload());
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        let _ = ids;
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
