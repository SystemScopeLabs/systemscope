//! `SimpleBlockMedia` (`docs/m2-design.md` §8.2): configuration, ports, identity, reads,
//! writes, errors, session faults, canonical storage, snapshots, restore rejection,
//! inspect, and trace, driven directly through a mock context and checked against an
//! independent sparse-media model; then in a real runtime with a scripted `block.v0`
//! initiator, for timing and every-event checkpoints.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{BlockSent, MockCtx, SendKind, restore_into, snapshot_of};
use proptest::prelude::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{
    self, BLOCK_SIZE, BlockMsg, BlockReadOutcome, BlockWriteOutcome, MediaError, TxnId,
};
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::media::{PORT, READ_KIND, SNAPSHOT_SCHEMA, WRITE_KIND};
use systemscope_platform::{BlockMediaConfig, BlockMediaConfigError, MediaImage, SimpleBlockMedia};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

const CLOCK: ClockDomainId = ClockDomainId(3);

const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: CLOCK,
    k: 2,
};

/// Well past 2^32 blocks, so any dense allocation would be impossible.
const HUGE: u64 = 1 << 40;

fn hash(bytes: &[u8]) -> [u8; 32] {
    blake3::hash(bytes).into()
}

/// An image of `bytes` with the hash a builder would compute.
fn image(bytes: &[u8]) -> MediaImage {
    MediaImage {
        image_hash: hash(bytes),
        bytes: bytes.to_vec(),
    }
}

fn config(capacity_blocks: u64) -> BlockMediaConfig {
    BlockMediaConfig {
        capacity_blocks,
        latency: LATENCY,
        bad_blocks: BTreeSet::new(),
    }
}

fn media(capacity_blocks: u64) -> SimpleBlockMedia {
    SimpleBlockMedia::new(config(capacity_blocks), &image(&[])).unwrap()
}

fn media_with(capacity_blocks: u64, bytes: &[u8]) -> SimpleBlockMedia {
    SimpleBlockMedia::new(config(capacity_blocks), &image(bytes)).unwrap()
}

/// A block of `b` bytes.
fn fill(b: u8) -> Vec<u8> {
    vec![b; BLOCK_SIZE]
}

/// A block whose bytes all differ from their neighbours; never all zero.
fn pattern(seed: u8) -> Vec<u8> {
    (0..BLOCK_SIZE)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed) | 1)
        .collect()
}

/// Blocks, concatenated into a raw image.
fn raw(blocks: &[Vec<u8>]) -> Vec<u8> {
    blocks.concat()
}

fn read_req(txn: u64, lba: u64) -> BlockMsg {
    BlockMsg::ReadBlock {
        txn: TxnId(txn),
        lba,
    }
}

fn write_req(txn: u64, lba: u64, data: &[u8]) -> BlockMsg {
    BlockMsg::WriteBlock {
        txn: TxnId(txn),
        lba,
        data: data.to_vec(),
    }
}

/// Delivers `msg` in `Request` and returns the single result, checking its port, timing,
/// and phase.
fn serve(m: &mut SimpleBlockMedia, msg: BlockMsg) -> BlockMsg {
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver_msg(m, PORT, msg.into()).unwrap();
    assert_eq!(ctx.order, [SendKind::Block]);
    let sent = ctx.blocks.pop().unwrap();
    assert_eq!(
        (sent.port, sent.when, sent.phase),
        (
            PORT,
            ScheduleWhen::Cycles {
                domain: CLOCK,
                k: 2
            },
            Phase::Complete
        )
    );
    sent.msg
}

fn load(m: &mut SimpleBlockMedia, lba: u64) -> Vec<u8> {
    match serve(m, read_req(1, lba)) {
        BlockMsg::ReadResult {
            txn: TxnId(1),
            outcome: BlockReadOutcome::Data { data },
        } => data,
        other => panic!("{other:?}"),
    }
}

fn store(m: &mut SimpleBlockMedia, lba: u64, data: &[u8]) {
    assert_eq!(serve(m, write_req(2, lba, data)), done(2));
}

fn done(txn: u64) -> BlockMsg {
    BlockMsg::WriteResult {
        txn: TxnId(txn),
        outcome: BlockWriteOutcome::Done,
    }
}

fn read_error(txn: u64, error: MediaError) -> BlockMsg {
    BlockMsg::ReadResult {
        txn: TxnId(txn),
        outcome: BlockReadOutcome::Error { error },
    }
}

fn write_error(txn: u64, error: MediaError) -> BlockMsg {
    BlockMsg::WriteResult {
        txn: TxnId(txn),
        outcome: BlockWriteOutcome::Error { error },
    }
}

fn stored(m: &SimpleBlockMedia) -> u64 {
    match m.inspect().fields[2] {
        ("stored_blocks", Value::U64(n)) => n,
        ref other => panic!("{other:?}"),
    }
}

/// The canonical snapshot of a media with `config`, `image_hash`, and `blocks`, written
/// independently of the component.
fn canonical(
    config: &BlockMediaConfig,
    image_hash: [u8; 32],
    blocks: &BTreeMap<u64, Vec<u8>>,
) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u64(config.capacity_blocks);
    match config.latency {
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
    w.len(config.bad_blocks.len());
    for lba in &config.bad_blocks {
        w.u64(*lba);
    }
    w.raw(&image_hash);
    let nonzero: Vec<_> = blocks
        .iter()
        .filter(|(_, b)| b.iter().any(|&x| x != 0))
        .collect();
    w.len(nonzero.len());
    for (lba, block) in nonzero {
        w.u64(*lba);
        w.bytes(block);
    }
    w.into_bytes()
}

// --- Configuration, ports, identity ---

#[test]
fn the_configuration_is_checked() {
    let new = |cap, bytes: &[u8]| SimpleBlockMedia::new(config(cap), &image(bytes)).err();
    assert_eq!(new(0, &[]), Some(BlockMediaConfigError::ZeroCapacity));
    for len in [1, 511, 513, 1023] {
        assert_eq!(
            new(4, &vec![0; len]),
            Some(BlockMediaConfigError::PartialBlock(len))
        );
    }
    assert_eq!(
        new(2, &fill(1).repeat(3)),
        Some(BlockMediaConfigError::ImageTooLarge(3))
    );
    assert_eq!(new(1, &[]), None);
    assert_eq!(new(1, &fill(1)), None);
    assert_eq!(new(3, &fill(1).repeat(3)), None);
    assert_eq!(new(HUGE, &fill(1).repeat(3)), None);
    assert_eq!(
        BlockMediaConfigError::PartialBlock(7).to_string(),
        "media image of 7 bytes is not a whole number of blocks"
    );
}

#[test]
fn it_is_a_block_target_named_blk() {
    let m = media(4);
    assert_eq!(m.type_name(), "platform.disk");
    let ports = m.ports();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].name, "blk");
    assert_eq!(ports[0].protocol, block_v0::PROTOCOL);
    assert_eq!(ports[0].role, Role::Target);
    assert_eq!(PORT, PortId(0));
    assert_eq!(m.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(SNAPSHOT_SCHEMA, 1);
}

#[test]
fn init_sends_nothing() {
    let mut m = media(4);
    let mut ctx = MockCtx::new(Phase::Request);
    m.init(&mut ctx).unwrap();
    assert!(ctx.order.is_empty() && ctx.traced.is_empty());
}

#[test]
fn inspect_shows_capacity_hash_and_stored_count_only() {
    let bytes = raw(&[pattern(1), fill(0), fill(9)]);
    let mut m = media_with(8, &bytes);
    store(&mut m, 5, &pattern(3));
    assert_eq!(
        m.inspect().fields,
        vec![
            ("capacity_blocks", Value::U64(8)),
            ("image_hash", Value::Bytes(hash(&bytes).to_vec())),
            ("stored_blocks", Value::U64(3)),
        ]
    );
}

/// The same config and image bytes always give the same identity and state; a builder
/// hashing the same bytes gets the same hash.
#[test]
fn the_same_bytes_give_the_same_identity() {
    let bytes = raw(&[pattern(1), fill(0), pattern(2)]);
    let a = media_with(4, &bytes);
    let b = media_with(4, &bytes.clone());
    assert_eq!(a.inspect(), b.inspect());
    assert_eq!(snapshot_of(&a), snapshot_of(&b));
    let other = media_with(4, &raw(&[pattern(1), fill(0), pattern(3)]));
    assert_ne!(a.inspect().fields[1], other.inspect().fields[1]);
}

// --- Reads and writes ---

#[test]
fn the_image_is_copied_into_the_map_without_zero_blocks() {
    let mut m = media_with(8, &raw(&[pattern(1), fill(0), fill(0xff)]));
    assert_eq!(stored(&m), 2);
    assert_eq!(load(&mut m, 0), pattern(1));
    assert_eq!(load(&mut m, 1), fill(0));
    assert_eq!(load(&mut m, 2), fill(0xff));
    // Past the image's end: zero.
    assert_eq!(load(&mut m, 3), fill(0));
    assert_eq!(load(&mut m, 7), fill(0));
    assert_eq!(stored(&m), 2);
}

#[test]
fn an_all_zero_image_stores_nothing() {
    let m = media_with(4, &fill(0).repeat(4));
    assert_eq!(stored(&m), 0);
}

#[test]
fn results_are_sent_after_the_latency_in_complete_with_the_txn() {
    for latency in [
        LinkLatency::After(Duration::from_ns(7)),
        LinkLatency::Cycles {
            domain: ClockDomainId(9),
            k: 0,
        },
    ] {
        let mut m = SimpleBlockMedia::new(
            BlockMediaConfig {
                capacity_blocks: 4,
                latency,
                bad_blocks: BTreeSet::new(),
            },
            &image(&[]),
        )
        .unwrap();
        let expected = match latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        for (txn, msg) in [
            (0, write_req(0, 1, &pattern(1))),
            (u64::MAX, read_req(u64::MAX, 1)),
            (77, read_req(77, 4)),
            (0x1234_5678_9abc, write_req(0x1234_5678_9abc, 9, &fill(1))),
        ] {
            let mut ctx = MockCtx::new(Phase::Request);
            ctx.deliver_msg(&mut m, PORT, msg.into()).unwrap();
            let BlockSent {
                port,
                msg,
                when,
                phase,
            } = ctx.blocks.pop().unwrap();
            assert_eq!((port, when, phase), (PORT, expected, Phase::Complete));
            let got = match msg {
                BlockMsg::ReadResult { txn, .. } | BlockMsg::WriteResult { txn, .. } => txn,
                other => panic!("{other:?}"),
            };
            assert_eq!(got, TxnId(txn));
        }
    }
}

#[test]
fn writes_replace_whole_blocks() {
    let mut m = media(4);
    store(&mut m, 2, &pattern(1));
    assert_eq!(load(&mut m, 2), pattern(1));
    store(&mut m, 2, &pattern(2));
    assert_eq!(load(&mut m, 2), pattern(2));
    assert_eq!(load(&mut m, 1), fill(0));
    assert_eq!(stored(&m), 1);
}

#[test]
fn writing_zeros_removes_the_block() {
    let mut m = media(4);
    store(&mut m, 1, &pattern(1));
    store(&mut m, 1, &fill(0));
    assert_eq!(stored(&m), 0);
    assert_eq!(load(&mut m, 1), fill(0));
    // Zeros to an absent block allocate nothing.
    store(&mut m, 3, &fill(0));
    assert_eq!(stored(&m), 0);
    assert_eq!(snapshot_of(&m), snapshot_of(&media(4)));
}

/// No fallback to the image: a zeroed image block reads as zero.
#[test]
fn writing_zeros_over_an_image_block_leaves_it_zero() {
    let mut m = media_with(4, &raw(&[pattern(1), pattern(2)]));
    store(&mut m, 1, &fill(0));
    assert_eq!(load(&mut m, 1), fill(0));
    assert_eq!(load(&mut m, 0), pattern(1));
    assert_eq!(stored(&m), 1);
}

#[test]
fn reading_unwritten_blocks_does_not_grow_the_map() {
    let mut m = media(HUGE);
    let before = snapshot_of(&m);
    for lba in [0, 1, 2, HUGE / 2, HUGE - 1, 0, 1] {
        assert_eq!(load(&mut m, lba), fill(0));
    }
    assert_eq!(stored(&m), 0);
    assert_eq!(snapshot_of(&m), before);
}

// --- Capacity boundaries and errors ---

#[test]
fn a_one_block_media_has_only_lba_zero() {
    let mut m = media(1);
    store(&mut m, 0, &pattern(1));
    assert_eq!(load(&mut m, 0), pattern(1));
    assert_eq!(
        serve(&mut m, read_req(3, 1)),
        read_error(3, MediaError::OutOfRange)
    );
    assert_eq!(
        serve(&mut m, write_req(4, 1, &pattern(2))),
        write_error(4, MediaError::OutOfRange)
    );
}

#[test]
fn the_last_lba_is_capacity_minus_one() {
    for cap in [
        2,
        4096,
        (1 << 32) - 1,
        1 << 32,
        (1 << 32) + 1,
        HUGE,
        u64::MAX,
    ] {
        let mut m = media(cap);
        store(&mut m, cap - 1, &pattern(5));
        assert_eq!(load(&mut m, cap - 1), pattern(5));
        for lba in [cap, cap.saturating_add(1), u64::MAX] {
            assert_eq!(
                serve(&mut m, read_req(3, lba)),
                read_error(3, MediaError::OutOfRange)
            );
            assert_eq!(
                serve(&mut m, write_req(4, lba, &pattern(2))),
                write_error(4, MediaError::OutOfRange)
            );
        }
        assert_eq!(stored(&m), 1, "capacity {cap}");
    }
}

#[test]
fn errors_read_and_write_nothing() {
    let mut m = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: 8,
            latency: LATENCY,
            bad_blocks: [2, 5, 8, 100].into(),
        },
        &image(&raw(&[fill(0), fill(0), pattern(1)])),
    )
    .unwrap();
    let before = snapshot_of(&m);
    let cases = [
        (read_req(1, 2), read_error(1, MediaError::BadBlock)),
        (
            write_req(2, 2, &pattern(9)),
            write_error(2, MediaError::BadBlock),
        ),
        (
            write_req(3, 5, &pattern(9)),
            write_error(3, MediaError::BadBlock),
        ),
        (
            write_req(4, 5, &fill(0)),
            write_error(4, MediaError::BadBlock),
        ),
        // Out of range wins over a listed bad block.
        (read_req(5, 8), read_error(5, MediaError::OutOfRange)),
        (
            write_req(6, 100, &pattern(9)),
            write_error(6, MediaError::OutOfRange),
        ),
        (read_req(7, 9), read_error(7, MediaError::OutOfRange)),
    ];
    for (req, expected) in cases {
        assert_eq!(serve(&mut m, req), expected);
        assert_eq!(snapshot_of(&m), before);
    }
    // Neighbours of bad blocks work.
    store(&mut m, 3, &pattern(3));
    assert_eq!(load(&mut m, 3), pattern(3));
}

// --- Session faults ---

/// Delivers `ev` and requires a session fault that sent, traced, and changed nothing.
fn assert_faults(m: &mut SimpleBlockMedia, phase: Phase, ev: Delivered) {
    let before = snapshot_of(m);
    let inspect = m.inspect();
    let mut ctx = MockCtx::new(phase);
    let result = m.handle_event(&ev, &mut ctx);
    assert!(
        matches!(result, Err(SimError::ComponentFault(_))),
        "{ev:?}: {result:?}"
    );
    assert!(ctx.order.is_empty(), "{ev:?}");
    assert!(ctx.traced.is_empty(), "{ev:?}");
    assert_eq!(snapshot_of(m), before, "{ev:?}");
    assert_eq!(m.inspect(), inspect, "{ev:?}");
}

fn on_blk(msg: impl Into<Message>) -> Delivered {
    Delivered::Message {
        port: PORT,
        msg: msg.into(),
    }
}

/// Only exactly 512 bytes is a write; every other length faults, whatever the LBA, and
/// is never answered as a `BadBlock` or any other media error.
#[test]
fn write_data_must_be_exactly_one_block() {
    let mut m = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: 4,
            latency: LATENCY,
            bad_blocks: [3].into(),
        },
        &image(&pattern(1)),
    )
    .unwrap();
    for len in [0, 1, 511, 513, 1024] {
        for lba in [0, 1, 3, 4, u64::MAX] {
            let data = vec![0xa5; len];
            assert_faults(&mut m, Phase::Request, on_blk(write_req(1, lba, &data)));
        }
    }
    store(&mut m, 1, &vec![0xa5; 512]);
    assert_eq!(load(&mut m, 1), fill(0xa5));
}

#[test]
fn protocol_violations_fault_the_session() {
    let mut m = media_with(4, &pattern(1));
    store(&mut m, 2, &pattern(2));
    let results = [
        BlockMsg::ReadResult {
            txn: TxnId(1),
            outcome: BlockReadOutcome::Data { data: fill(1) },
        },
        BlockMsg::WriteResult {
            txn: TxnId(1),
            outcome: BlockWriteOutcome::Done,
        },
        read_error(1, MediaError::BadBlock),
        write_error(1, MediaError::OutOfRange),
    ];
    for result in results {
        for phase in [Phase::Request, Phase::Complete] {
            assert_faults(&mut m, phase, on_blk(result.clone()));
        }
    }
    // Not block.v0.
    let foreign: [Message; 3] = [
        MemMsg::ReadReq {
            txn: TxnId(1),
            addr: 0,
            len: 4,
        }
        .into(),
        MemMsg::WriteReq {
            txn: TxnId(1),
            addr: 0,
            data: vec![1],
        }
        .into(),
        IrqMsg::Level { asserted: true }.into(),
    ];
    for msg in foreign {
        assert_faults(&mut m, Phase::Request, on_blk(msg));
    }
    // Requests outside `Request`.
    for phase in [Phase::Transfer, Phase::Complete] {
        assert_faults(&mut m, phase, on_blk(read_req(1, 0)));
        assert_faults(&mut m, phase, on_blk(write_req(1, 2, &pattern(9))));
    }
    // An unknown port, and a wake: the media schedules none.
    assert_faults(
        &mut m,
        Phase::Request,
        Delivered::Message {
            port: PortId(1),
            msg: write_req(1, 2, &pattern(9)).into(),
        },
    );
    assert_faults(&mut m, Phase::Request, Delivered::Wake { token: 0 });
}

// --- Trace ---

#[test]
fn accepted_requests_are_traced_at_acceptance() {
    let mut m = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: 4,
            latency: LATENCY,
            bad_blocks: [1].into(),
        },
        &image(&[]),
    )
    .unwrap();
    let mut ctx = MockCtx::new(Phase::Request);
    for msg in [
        read_req(1, 0),
        write_req(2, 3, &pattern(1)),
        read_req(3, 1),
        write_req(4, 1, &pattern(1)),
        read_req(5, 4),
        write_req(6, 9, &fill(0)),
    ] {
        ctx.deliver_msg(&mut m, PORT, msg.into()).unwrap();
    }
    let s = |v: &str| Value::Str(v.to_string());
    let record = |kind, lba, outcome| {
        (
            kind,
            vec![("lba", Value::U64(lba)), ("outcome", s(outcome))],
        )
    };
    assert_eq!(
        ctx.traced,
        vec![
            record(READ_KIND, 0, "ok"),
            record(WRITE_KIND, 3, "ok"),
            record(READ_KIND, 1, "bad_block"),
            record(WRITE_KIND, 1, "bad_block"),
            record(READ_KIND, 4, "out_of_range"),
            record(WRITE_KIND, 9, "out_of_range"),
        ]
    );
    assert_eq!(READ_KIND, "platform.disk.read");
    assert_eq!(WRITE_KIND, "platform.disk.write");
    assert_eq!(ctx.blocks.len(), 6);
}

// --- Snapshots ---

#[test]
fn snapshot_layout_is_pinned() {
    let bytes = raw(&[fill(0), pattern(1)]);
    let mut m = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: 0x1_0000_0001,
            latency: LinkLatency::After(Duration::from_ns(2)),
            bad_blocks: [9, 4].into(),
        },
        &image(&bytes),
    )
    .unwrap();
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver_msg(
        &mut m,
        PORT,
        write_req(1, 0x1_0000_0000, &fill(0xdd)).into(),
    )
    .unwrap();
    let mut w = SnapshotWriter::new();
    w.u64(0x1_0000_0001);
    w.u8(0);
    w.u128(2_000_000);
    w.len(2);
    w.u64(4);
    w.u64(9);
    w.raw(&hash(&bytes));
    w.len(2);
    w.u64(1);
    w.bytes(&pattern(1));
    w.u64(0x1_0000_0000);
    w.bytes(&fill(0xdd));
    assert_eq!(snapshot_of(&m), w.into_bytes());
}

#[test]
fn different_write_orders_with_the_same_contents_snapshot_identically() {
    let run = |order: [u64; 3]| {
        let mut m = media(16);
        for lba in order {
            store(&mut m, lba, &pattern(lba as u8));
        }
        snapshot_of(&m)
    };
    let a = run([9, 2, 5]);
    assert_eq!(a, run([5, 9, 2]));
    assert_eq!(a, run([2, 5, 9]));
    // And a history that wrote, then zeroed, extra blocks.
    let mut m = media(16);
    for lba in [1, 5, 9, 12, 2] {
        store(&mut m, lba, &pattern(lba as u8 ^ 0x40));
    }
    for lba in [1, 12] {
        store(&mut m, lba, &fill(0));
    }
    for lba in [9, 2, 5] {
        store(&mut m, lba, &pattern(lba as u8));
    }
    assert_eq!(snapshot_of(&m), a);
}

/// A fresh media with `m`'s configuration and image: the target of a restore.
type Fresh = fn() -> SimpleBlockMedia;

/// Snapshot → restore into a fresh media → snapshot is byte-identical, and the restored
/// media inspects and reads the same.
fn assert_round_trips(mut m: SimpleBlockMedia, fresh: Fresh, probes: &[u64]) {
    let bytes = snapshot_of(&m);
    let mut restored = fresh();
    restore_into(&mut restored, &bytes).unwrap();
    assert_eq!(snapshot_of(&restored), bytes);
    assert_eq!(restored.inspect(), m.inspect());
    for &lba in probes {
        assert_eq!(
            serve(&mut restored, read_req(1, lba)),
            serve(&mut m, read_req(1, lba)),
            "lba {lba}"
        );
    }
    // Restoring into an already-used media gives the same state too.
    let mut used = fresh();
    store_any(&mut used, 0, &pattern(0x77));
    restore_into(&mut used, &bytes).unwrap();
    assert_eq!(snapshot_of(&used), bytes);
}

/// Delivers a write whatever its outcome, for media with bad blocks.
fn store_any(m: &mut SimpleBlockMedia, lba: u64, data: &[u8]) {
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver_msg(m, PORT, write_req(9, lba, data).into())
        .unwrap();
}

fn image_blocks() -> Vec<u8> {
    raw(&[pattern(1), fill(0), pattern(2), fill(3)])
}

#[test]
fn round_trips_of_an_empty_media() {
    assert_round_trips(media(8), || media(8), &[0, 7, 8]);
}

#[test]
fn round_trips_of_an_image_only_media() {
    fn fresh() -> SimpleBlockMedia {
        media_with(8, &image_blocks())
    }
    assert_round_trips(fresh(), fresh, &[0, 1, 2, 3, 4, 7]);
}

#[test]
fn round_trips_after_zero_backed_reads() {
    let mut m = media(8);
    for lba in 0..8 {
        load(&mut m, lba);
    }
    assert_round_trips(m, || media(8), &[0, 3]);
}

#[test]
fn round_trips_after_one_write() {
    let mut m = media(8);
    store(&mut m, 3, &pattern(1));
    assert_round_trips(m, || media(8), &[2, 3, 4]);
}

#[test]
fn round_trips_after_many_writes() {
    let mut m = media(64);
    for lba in (0..64).rev().step_by(3) {
        store(&mut m, lba, &pattern(lba as u8));
    }
    assert_round_trips(m, || media(64), &(0..64).collect::<Vec<_>>());
}

#[test]
fn round_trips_after_an_overwrite() {
    let mut m = media(8);
    store(&mut m, 3, &pattern(1));
    store(&mut m, 3, &pattern(2));
    assert_round_trips(m, || media(8), &[3]);
}

#[test]
fn round_trips_after_writes_over_the_image() {
    fn fresh() -> SimpleBlockMedia {
        media_with(8, &image_blocks())
    }
    let mut m = fresh();
    store(&mut m, 0, &pattern(9));
    store(&mut m, 2, &fill(0));
    store(&mut m, 1, &pattern(8));
    assert_round_trips(m, fresh, &[0, 1, 2, 3, 4]);
}

#[test]
fn round_trips_with_the_highest_lba_written() {
    let mut m = media(8);
    store(&mut m, 7, &pattern(1));
    assert_round_trips(m, || media(8), &[6, 7, 8]);
}

#[test]
fn round_trips_of_a_large_media() {
    let mut m = media(1 << 20);
    store(&mut m, 0, &pattern(1));
    store(&mut m, (1 << 20) - 1, &pattern(2));
    assert_round_trips(m, || media(1 << 20), &[0, 1 << 19, (1 << 20) - 1]);
}

#[test]
fn round_trips_of_a_media_past_four_billion_blocks() {
    let mut m = media(HUGE);
    store(&mut m, 1 << 32, &pattern(1));
    store(&mut m, HUGE - 1, &pattern(2));
    store(&mut m, 5, &pattern(3));
    assert_round_trips(m, || media(HUGE), &[5, 1 << 32, HUGE - 1, HUGE]);
}

#[test]
fn round_trips_with_a_nonzero_latency() {
    fn fresh() -> SimpleBlockMedia {
        SimpleBlockMedia::new(
            BlockMediaConfig {
                capacity_blocks: 8,
                latency: LinkLatency::After(Duration::from_ns(123)),
                bad_blocks: [4].into(),
            },
            &image(&pattern(1)),
        )
        .unwrap()
    }
    let mut m = fresh();
    store_any(&mut m, 2, &pattern(5));
    let bytes = snapshot_of(&m);
    let mut restored = fresh();
    restore_into(&mut restored, &bytes).unwrap();
    assert_eq!(snapshot_of(&restored), bytes);
}

/// Required regression (`docs/m2-design.md` §8.2): restore never re-applies the image.
#[test]
fn restore_does_not_reapply_the_initial_image() {
    let a = raw(&[fill(0), pattern(0xa)]);
    let mut m = media_with(4, &a);
    store(&mut m, 1, &pattern(0xb));
    let bytes = snapshot_of(&m);
    let mut restored = media_with(4, &a);
    restore_into(&mut restored, &bytes).unwrap();
    assert_eq!(load(&mut restored, 1), pattern(0xb));
    assert_eq!(snapshot_of(&restored), bytes);
}

#[test]
fn a_zeroed_image_block_stays_zero_after_restore() {
    let a = raw(&[pattern(1), pattern(2)]);
    let mut m = media_with(4, &a);
    store(&mut m, 1, &fill(0));
    let bytes = snapshot_of(&m);
    let mut restored = media_with(4, &a);
    restore_into(&mut restored, &bytes).unwrap();
    assert_eq!(load(&mut restored, 1), fill(0));
    assert_eq!(load(&mut restored, 0), pattern(1));
    assert_eq!(stored(&restored), 1);
}

/// Requires `bytes` to be rejected by `target`, changing nothing.
fn assert_rejected(target: &mut SimpleBlockMedia, bytes: &[u8]) {
    let before = snapshot_of(target);
    let result = restore_into(target, bytes);
    assert!(result.is_err(), "accepted");
    assert_eq!(snapshot_of(target), before);
}

#[test]
fn restore_rejects_a_different_image() {
    // Same length, capacity, and latency; different content.
    let a = raw(&[pattern(1), pattern(2)]);
    let b = raw(&[pattern(1), pattern(3)]);
    let mut from_a = media_with(4, &a);
    store(&mut from_a, 3, &pattern(7));
    let bytes = snapshot_of(&from_a);
    let mut from_b = media_with(4, &b);
    store(&mut from_b, 0, &pattern(8));
    let result = restore_into(&mut from_b, &bytes);
    assert_eq!(
        result,
        Err(RestoreError::InvalidState(
            "disk: snapshot has a different image hash"
        ))
    );
    assert_rejected(&mut from_b, &bytes);
    assert_eq!(load(&mut from_b, 0), pattern(8));
    assert_eq!(load(&mut from_b, 1), pattern(3));
}

/// The empty image has its own identity: an image of zero blocks, with identical (empty)
/// contents, is a different image.
#[test]
fn the_empty_image_has_an_identity() {
    let empty = snapshot_of(&media(4));
    let mut zeros = media_with(4, &fill(0));
    assert_eq!(stored(&zeros), 0);
    assert_rejected(&mut zeros, &empty);
    let zeros_bytes = snapshot_of(&zeros);
    assert_rejected(&mut media(4), &zeros_bytes);
    let mut again = media(4);
    restore_into(&mut again, &empty).unwrap();
    assert_eq!(snapshot_of(&again), empty);
}

#[test]
fn restore_rejects_a_different_configuration() {
    let base = || media_with(8, &pattern(1));
    let bytes = snapshot_of(&base());
    let variants = [
        SimpleBlockMedia::new(config(9), &image(&pattern(1))).unwrap(),
        SimpleBlockMedia::new(
            BlockMediaConfig {
                latency: LinkLatency::Cycles {
                    domain: CLOCK,
                    k: 3,
                },
                ..config(8)
            },
            &image(&pattern(1)),
        )
        .unwrap(),
        SimpleBlockMedia::new(
            BlockMediaConfig {
                latency: LinkLatency::Cycles {
                    domain: ClockDomainId(4),
                    k: 2,
                },
                ..config(8)
            },
            &image(&pattern(1)),
        )
        .unwrap(),
        SimpleBlockMedia::new(
            BlockMediaConfig {
                bad_blocks: [3].into(),
                ..config(8)
            },
            &image(&pattern(1)),
        )
        .unwrap(),
    ];
    for mut other in variants {
        assert_rejected(&mut other, &bytes);
        let theirs = snapshot_of(&other);
        assert_rejected(&mut base(), &theirs);
    }
}

#[test]
fn restore_rejects_non_canonical_block_lists() {
    let cfg = config(8);
    let h = hash(&[]);
    let header = |w: &mut SnapshotWriter| {
        let bytes = canonical(&cfg, h, &BTreeMap::new());
        w.raw(&bytes[..bytes.len() - 4]);
    };
    let with_blocks = |blocks: &[(u64, Vec<u8>)]| {
        let mut w = SnapshotWriter::new();
        header(&mut w);
        w.len(blocks.len());
        for (lba, data) in blocks {
            w.u64(*lba);
            w.bytes(data);
        }
        w.into_bytes()
    };
    let mut target = media(8);
    store(&mut target, 6, &pattern(6));
    // The canonical form is accepted.
    let good = with_blocks(&[(1, pattern(1)), (7, pattern(7))]);
    restore_into(&mut media(8), &good).unwrap();
    for bad in [
        with_blocks(&[(7, pattern(7)), (1, pattern(1))]),
        with_blocks(&[(1, pattern(1)), (1, pattern(2))]),
        with_blocks(&[(8, pattern(1))]),
        with_blocks(&[(u64::MAX, pattern(1))]),
        with_blocks(&[(1, fill(0))]),
        with_blocks(&[(1, vec![1; 511])]),
        with_blocks(&[(1, vec![1; 513])]),
        with_blocks(&[(1, pattern(1)), (2, fill(0))]),
        good[..good.len() - 1].to_vec(),
    ] {
        assert_rejected(&mut target, &bad);
    }
    // Trailing bytes are rejected by the reader's `finish`, after the component's part.
    let mut long = good.clone();
    long.push(0);
    assert!(restore_into(&mut media(8), &long).is_err());
    // Bad blocks must be encoded ascending, like the component writes them.
    let listed = BlockMediaConfig {
        bad_blocks: [2, 5].into(),
        ..config(8)
    };
    let mut swapped = SnapshotWriter::new();
    swapped.raw(&canonical(&cfg, h, &BTreeMap::new())[..8 + 13]);
    swapped.len(2);
    swapped.u64(5);
    swapped.u64(2);
    swapped.raw(&h);
    swapped.len(0);
    let mut listed_media = SimpleBlockMedia::new(listed, &image(&[])).unwrap();
    assert_rejected(&mut listed_media, &swapped.into_bytes());
    let mut r = SnapshotReader::new(&[]);
    assert!(listed_media.restore(&mut r, 1).is_err());
}

/// Splitting any operation sequence with a snapshot and restore changes no result and no
/// final state.
#[test]
fn restored_media_continue_like_uninterrupted_ones() {
    let bytes = raw(&[pattern(1), fill(0), pattern(2)]);
    let fresh = || {
        SimpleBlockMedia::new(
            BlockMediaConfig {
                capacity_blocks: 6,
                latency: LATENCY,
                bad_blocks: [4].into(),
            },
            &image(&bytes),
        )
        .unwrap()
    };
    let ops = vec![
        read_req(1, 0),
        write_req(2, 0, &pattern(9)),
        read_req(3, 0),
        write_req(4, 2, &fill(0)),
        read_req(5, 2),
        write_req(6, 5, &pattern(5)),
        read_req(7, 4),
        write_req(8, 6, &pattern(5)),
        write_req(9, 5, &pattern(6)),
        read_req(10, 5),
        read_req(11, 1),
    ];
    let serve_all = |m: &mut SimpleBlockMedia, ops: &[BlockMsg]| -> Vec<BlockMsg> {
        let mut ctx = MockCtx::new(Phase::Request);
        for op in ops {
            ctx.deliver_msg(m, PORT, op.clone().into()).unwrap();
        }
        ctx.blocks.into_iter().map(|s| s.msg).collect()
    };
    let mut whole = fresh();
    let expected = serve_all(&mut whole, &ops);
    for k in 0..=ops.len() {
        let mut first = fresh();
        let head = serve_all(&mut first, &ops[..k]);
        let mut second = fresh();
        restore_into(&mut second, &snapshot_of(&first)).unwrap();
        let tail = serve_all(&mut second, &ops[k..]);
        assert_eq!([head, tail].concat(), expected, "split at {k}");
        assert_eq!(snapshot_of(&second), snapshot_of(&whole), "split at {k}");
    }
}

// --- Independent model ---

/// The sparse-media model of `docs/m2-design.md` §8.2, written without the component's
/// code: image non-zero blocks at construction; a read is the entry or zeros; a
/// non-zero write inserts, a zero write removes.
struct Model {
    capacity: u64,
    bad: BTreeSet<u64>,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl Model {
    fn new(capacity: u64, bad: BTreeSet<u64>, image: &[u8]) -> Model {
        let mut blocks = BTreeMap::new();
        for (i, chunk) in image.chunks(BLOCK_SIZE).enumerate() {
            if chunk.iter().any(|&b| b != 0) {
                blocks.insert(i as u64, chunk.to_vec());
            }
        }
        Model {
            capacity,
            bad,
            blocks,
        }
    }

    fn error(&self, lba: u64) -> Option<MediaError> {
        if lba >= self.capacity {
            Some(MediaError::OutOfRange)
        } else if self.bad.contains(&lba) {
            Some(MediaError::BadBlock)
        } else {
            None
        }
    }

    fn serve(&mut self, msg: &BlockMsg) -> BlockMsg {
        match msg {
            BlockMsg::ReadBlock { txn, lba } => BlockMsg::ReadResult {
                txn: *txn,
                outcome: match self.error(*lba) {
                    Some(error) => BlockReadOutcome::Error { error },
                    None => BlockReadOutcome::Data {
                        data: self.blocks.get(lba).cloned().unwrap_or(fill(0)),
                    },
                },
            },
            BlockMsg::WriteBlock { txn, lba, data } => BlockMsg::WriteResult {
                txn: *txn,
                outcome: match self.error(*lba) {
                    Some(error) => BlockWriteOutcome::Error { error },
                    None => {
                        if data.iter().all(|&b| b == 0) {
                            self.blocks.remove(lba);
                        } else {
                            self.blocks.insert(*lba, data.clone());
                        }
                        BlockWriteOutcome::Done
                    }
                },
            },
            other => panic!("{other:?}"),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Read(u64),
    Write(u64, u8),
    /// A write of this many bytes, never 512: a session fault.
    Malformed(u64, usize),
}

/// Block contents by kind: 0 is all zero, the rest distinct patterns.
fn data_of(kind: u8) -> Vec<u8> {
    if kind == 0 { fill(0) } else { pattern(kind) }
}

fn lba_in(capacity: u64) -> impl Strategy<Value = u64> {
    prop_oneof![
        0..capacity.saturating_add(2),
        Just(0),
        Just(capacity - 1),
        Just(capacity),
        Just(u64::MAX),
    ]
}

fn op_in(capacity: u64) -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => lba_in(capacity).prop_map(Op::Read),
        6 => (lba_in(capacity), 0..4u8).prop_map(|(lba, k)| Op::Write(lba, k)),
        1 => (
            lba_in(capacity),
            prop_oneof![Just(0usize), Just(1), Just(511), Just(513), 2..600usize]
        )
            .prop_filter("not a block", |(_, len)| *len != BLOCK_SIZE)
            .prop_map(|(lba, len)| Op::Malformed(lba, len)),
    ]
}

fn scenario() -> impl Strategy<Value = (u64, BTreeSet<u64>, Vec<u8>, Vec<Op>)> {
    prop_oneof![1..12u64, Just(HUGE)].prop_flat_map(|capacity| {
        let small = capacity.min(12);
        (
            Just(capacity),
            prop::collection::btree_set(lba_in(capacity), 0..3),
            prop::collection::vec(0..4u8, 0..=small as usize)
                .prop_map(|kinds| kinds.into_iter().flat_map(data_of).collect::<Vec<u8>>()),
            prop::collection::vec(op_in(capacity), 1..48),
        )
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn media_matches_the_sparse_model((capacity, bad, bytes, ops) in scenario()) {
        let cfg = BlockMediaConfig { capacity_blocks: capacity, latency: LATENCY, bad_blocks: bad.clone() };
        let mut m = SimpleBlockMedia::new(cfg.clone(), &image(&bytes)).unwrap();
        let mut model = Model::new(capacity, bad, &bytes);
        prop_assert_eq!(snapshot_of(&m), canonical(&cfg, hash(&bytes), &model.blocks));
        for (i, op) in ops.iter().enumerate() {
            let txn = i as u64 ^ 0x5a5a;
            let msg = match op {
                Op::Read(lba) => read_req(txn, *lba),
                Op::Write(lba, k) => write_req(txn, *lba, &data_of(*k)),
                Op::Malformed(lba, len) => write_req(txn, *lba, &vec![0xc3; *len]),
            };
            let before = snapshot_of(&m);
            let mut ctx = MockCtx::new(Phase::Request);
            let result = ctx.deliver_msg(&mut m, PORT, msg.clone().into());
            if let Op::Malformed(..) = op {
                prop_assert!(matches!(result, Err(SimError::ComponentFault(_))));
                prop_assert!(ctx.order.is_empty());
                prop_assert_eq!(snapshot_of(&m), before);
                continue;
            }
            result.unwrap();
            prop_assert_eq!(ctx.blocks.len(), 1);
            prop_assert_eq!(&ctx.blocks[0].msg, &model.serve(&msg));
            prop_assert_eq!(stored(&m), model.blocks.len() as u64);
        }
        let bytes_now = snapshot_of(&m);
        prop_assert_eq!(&bytes_now, &canonical(&cfg, hash(&bytes), &model.blocks));
        // And the final state round-trips through a fresh media.
        let mut fresh = SimpleBlockMedia::new(cfg, &image(&bytes)).unwrap();
        restore_into(&mut fresh, &bytes_now).unwrap();
        prop_assert_eq!(snapshot_of(&fresh), bytes_now);
    }
}

// --- Runtime ---

const TICKS_PER_CYCLE: u64 = 1000;

/// A stateless `block.v0` initiator that sends `requests[i].1` at cycle `requests[i].0`
/// of `clock`, in `Request`, all scheduled during `init`.
struct BlockScript {
    clock: ClockDomainId,
    requests: Vec<(u64, BlockMsg)>,
}

impl Component for BlockScript {
    fn type_name(&self) -> &'static str {
        "test.block_script"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "blk",
            protocol: block_v0::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for (k, msg) in &self.requests {
            let when = ScheduleWhen::Cycles {
                domain: self.clock,
                k: *k,
            };
            ctx.send(PortId(0), msg.clone().into(), when, Phase::Request)?;
        }
        Ok(())
    }

    /// Results are recorded by the runtime's dispatch records; nothing to do.
    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Ok(())
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    /// Stateless: every pending request is in the runtime's queue.
    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// The script → the media over a `Cycles { 1 }` link, the media answering after
/// `media_cycles`.
fn build(requests: &[(u64, BlockMsg)], media_cycles: u64) -> (Runtime, ComponentId, ComponentId) {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let script = t.add_component(
        "soc.host",
        Box::new(BlockScript {
            clock,
            requests: requests.to_vec(),
        }),
    );
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: 8,
            latency: LinkLatency::Cycles {
                domain: clock,
                k: media_cycles,
            },
            bad_blocks: [6].into(),
        },
        &image(&raw(&[pattern(1), fill(0), pattern(2)])),
    )
    .unwrap();
    let disk = t.add_component("soc.disk", Box::new(disk));
    t.connect(
        (script, "blk"),
        (disk, "blk"),
        Some(LinkLatency::Cycles {
            domain: clock,
            k: 1,
        }),
    );
    (t.elaborate(SessionConfig::default()).unwrap(), script, disk)
}

/// Reads an unwritten block, writes it, reads it back, reads out of range, writes again
/// (over the image, then zeros), and hits a bad block.
fn workload() -> Vec<(u64, BlockMsg)> {
    vec![
        (0, read_req(10, 3)),
        (1, write_req(11, 3, &pattern(7))),
        (2, read_req(12, 3)),
        (2, read_req(13, 8)),
        (3, write_req(14, 3, &pattern(8))),
        (3, write_req(15, 0, &fill(0))),
        (4, read_req(16, 0)),
        (5, write_req(17, 6, &pattern(9))),
        (5, read_req(18, 2)),
        (6, read_req(19, 3)),
    ]
}

fn result_of(e: &Dispatched) -> BlockMsg {
    match &e.delivery {
        Delivered::Message {
            msg: Message::Block(m),
            ..
        } => m.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_runtime_workload_gives_the_modelled_results() {
    let (mut rt, host, disk) = build(&workload(), 2);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    let mut model = Model::new(8, [6].into(), &raw(&[pattern(1), fill(0), pattern(2)]));
    let expected: Vec<BlockMsg> = workload().iter().map(|(_, m)| model.serve(m)).collect();
    let results: Vec<(u64, Phase, BlockMsg)> = events
        .iter()
        .filter(|e| e.target == host)
        .map(|e| (e.key.tick.0 / TICKS_PER_CYCLE, e.key.phase, result_of(e)))
        .collect();
    assert_eq!(
        results.iter().map(|r| r.2.clone()).collect::<Vec<_>>(),
        expected
    );
    // Request at cycle k → accepted at k + 1 → result at k + 1 + 2 + 1, in `Complete`.
    let cycles: Vec<u64> = results.iter().map(|r| r.0).collect();
    assert_eq!(cycles, [4, 5, 6, 6, 7, 7, 8, 9, 9, 10]);
    assert!(results.iter().all(|r| r.1 == Phase::Complete));
    // Traced at acceptance, in `Request`, one record per request.
    let trace = rt.take_trace().unwrap();
    let records: Vec<(u64, Phase, &str)> = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == disk)
        .map(|r| {
            let TraceAt::Event(key) = r.at else {
                panic!("{r:?}")
            };
            (key.tick.0 / TICKS_PER_CYCLE, key.phase, r.kind)
        })
        .collect();
    assert_eq!(records.len(), 10);
    assert!(records.iter().all(|r| r.1 == Phase::Request));
    assert_eq!(
        records.iter().map(|r| r.0).collect::<Vec<_>>(),
        [1, 2, 3, 3, 4, 4, 5, 6, 6, 7]
    );
    // The final state is the model's.
    let cfg = BlockMediaConfig {
        capacity_blocks: 8,
        latency: LinkLatency::Cycles {
            domain: ClockDomainId(0),
            k: 2,
        },
        bad_blocks: [6].into(),
    };
    let snap = rt.snapshot().unwrap();
    let expected_disk = canonical(
        &cfg,
        hash(&raw(&[pattern(1), fill(0), pattern(2)])),
        &model.blocks,
    );
    assert!(
        snap.windows(expected_disk.len())
            .any(|w| w == expected_disk),
        "the runtime snapshot holds the model's disk state"
    );
}

/// A write is visible from its acceptance: a read accepted after it, but long before the
/// write's result is delivered, returns the written data.
#[test]
fn writes_take_effect_at_acceptance_not_at_result_delivery() {
    let requests = vec![(0, write_req(1, 4, &pattern(4))), (1, read_req(2, 4))];
    let (mut rt, host, disk) = build(&requests, 20);
    rt.init().unwrap();
    let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    let accepted: Vec<u64> = events
        .iter()
        .filter(|e| e.target == disk)
        .map(|e| e.key.tick.0 / TICKS_PER_CYCLE)
        .collect();
    assert_eq!(accepted, [1, 2]);
    let results: Vec<(u64, BlockMsg)> = events
        .iter()
        .filter(|e| e.target == host)
        .map(|e| (e.key.tick.0 / TICKS_PER_CYCLE, result_of(e)))
        .collect();
    assert_eq!(
        results,
        [
            (22, done(1)),
            (
                23,
                BlockMsg::ReadResult {
                    txn: TxnId(2),
                    outcome: BlockReadOutcome::Data { data: pattern(4) },
                }
            ),
        ]
    );
    // The read was accepted at cycle 2, twenty cycles before the write's result.
    assert!(accepted[1] < results[0].0);
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let requests = workload();
    let reference = {
        let (mut rt, _, _) = build(&requests, 2);
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
        let (mut rt, _, _) = build(&requests, 2);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let prefix = rt.take_trace().unwrap();
        let (mut fresh, _, _) = build(&requests, 2);
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
        assert_eq!(
            fresh.snapshot().unwrap(),
            reference.2,
            "final snapshot after a checkpoint at {k}"
        );
        assert_eq!(
            fresh.take_trace().unwrap(),
            reference.1,
            "trace after a checkpoint at {k}"
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
        let (mut rt, _, _) = build(&workload(), 2);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
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
