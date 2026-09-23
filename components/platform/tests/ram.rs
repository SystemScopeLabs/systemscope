//! `Ram` construction, sparse canonical storage, faults, and snapshots
//! (`docs/m1-design.md` §7.2), driven directly through a mock context.
//!
//! The model below is a flat `Vec<u8>`, and the expected canonical snapshot is built from
//! it by scanning 4096-byte chunks, independently of the RAM's page map.

mod common;

use common::{MockCtx, Sent, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::Component;
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Duration};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_platform::ram::{MAX_SIZE, PAGE_SIZE, PORT};
use systemscope_platform::{Ram, RamConfig, RamConfigError, RamImage, Segment};

const P: u64 = PAGE_SIZE as u64;
const HASH: [u8; 32] = [0x11; 32];
const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: ClockDomainId(0),
    k: 3,
};

fn config(size: u64) -> RamConfig {
    RamConfig {
        size,
        latency: LATENCY,
    }
}

fn image(segments: &[(u64, &[u8])]) -> RamImage {
    RamImage {
        image_hash: HASH,
        segments: segments
            .iter()
            .map(|&(offset, bytes)| Segment {
                offset,
                bytes: bytes.to_vec(),
            })
            .collect(),
    }
}

fn ram(size: u64) -> Ram {
    Ram::new(config(size), &image(&[])).unwrap()
}

fn pages(ram: &Ram) -> Value {
    ram.inspect().get("non_zero_pages").unwrap().clone()
}

/// Delivers a request in `Transfer` and returns the response the RAM sends.
fn serve(ram: &mut Ram, msg: MemMsg) -> MemMsg {
    let mut ctx = MockCtx::new(Phase::Transfer);
    ctx.deliver(ram, PORT, msg).unwrap();
    let sent = ctx.take_one();
    assert_eq!((sent.port, sent.phase), (PORT, Phase::Complete));
    assert!(ctx.traced.is_empty(), "the RAM traces nothing");
    sent.msg
}

fn load(ram: &mut Ram, offset: u64, len: u32) -> Vec<u8> {
    match serve(ram, read(0, offset, len)) {
        MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } => data,
        other => panic!("expected data, got {other:?}"),
    }
}

fn store(ram: &mut Ram, offset: u64, data: &[u8]) {
    assert_eq!(serve(ram, write(0, offset, data)), done(0));
}

fn done(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
}

fn read_fault(txn: u64) -> MemMsg {
    MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    }
}

fn write_fault(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    }
}

/// The canonical snapshot of a RAM with `size`, `LATENCY`, `hash`, and these contents.
fn canonical(size: u64, hash: [u8; 32], contents: &[u8]) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u64(size);
    w.raw(&hash);
    w.u8(1);
    w.u32(0);
    w.u64(3);
    let mut chunks = Vec::new();
    for (index, chunk) in contents.chunks(PAGE_SIZE).enumerate() {
        if chunk.iter().any(|&b| b != 0) {
            let mut page = chunk.to_vec();
            page.resize(PAGE_SIZE, 0);
            chunks.push((u32::try_from(index).unwrap(), page));
        }
    }
    w.len(chunks.len());
    for (index, page) in chunks {
        w.u32(index);
        w.bytes(&page);
    }
    w.into_bytes()
}

// ---------------------------------------------------------------------------------------
// Construction.

#[test]
fn sizes_must_be_in_range() {
    let err = |size| Ram::new(config(size), &image(&[])).err();
    assert_eq!(err(0), Some(RamConfigError::ZeroSize));
    assert_eq!(
        err(MAX_SIZE + 1),
        Some(RamConfigError::TooLarge(MAX_SIZE + 1))
    );
    // Sparse: even the largest RAM allocates nothing up front.
    let largest = ram(MAX_SIZE);
    assert_eq!(pages(&largest), Value::U64(0));
    assert_eq!(err(1), None);
}

#[test]
fn image_segments_must_fit_and_not_overlap() {
    let err = |segments: &[(u64, &[u8])]| Ram::new(config(0x100), &image(segments)).err();
    assert_eq!(
        err(&[(0xff, &[1, 2])]),
        Some(RamConfigError::SegmentOutOfRange(0))
    );
    assert_eq!(
        err(&[(0, &[1]), (0x100, &[1])]),
        Some(RamConfigError::SegmentOutOfRange(1))
    );
    assert_eq!(
        err(&[(u64::MAX, &[1])]),
        Some(RamConfigError::SegmentOutOfRange(0))
    );
    assert_eq!(
        err(&[(0x101, &[])]),
        Some(RamConfigError::SegmentOutOfRange(0))
    );
    assert_eq!(
        err(&[(0x10, &[1, 2, 3]), (0x0, &[1]), (0x12, &[4])]),
        Some(RamConfigError::SegmentsOverlap(0, 2))
    );
    // Adjacent segments, a segment ending exactly at `size`, and empty segments are fine.
    assert_eq!(
        err(&[
            (0x10, &[1, 2]),
            (0x12, &[3]),
            (0xff, &[9]),
            (0x100, &[]),
            (0x10, &[])
        ]),
        None
    );
}

#[test]
fn the_image_is_placed_at_offsets_without_zero_pages() {
    let mut r = Ram::new(
        config(4 * P),
        &image(&[
            (P - 2, &[1, 2, 3, 4]),
            (3 * P, &[0; 64]),
            (2 * P + 7, &[0, 0, 9]),
        ]),
    )
    .unwrap();
    // Pages 0, 1, and 2 hold non-zero bytes; the zero segment on page 3 allocates nothing.
    assert_eq!(pages(&r), Value::U64(3));
    assert_eq!(load(&mut r, P - 3, 6), [0, 1, 2, 3, 4, 0]);
    assert_eq!(load(&mut r, 2 * P + 7, 3), [0, 0, 9]);
    assert_eq!(load(&mut r, 3 * P, 64), [0; 64]);
}

// ---------------------------------------------------------------------------------------
// Reads, writes, and faults.

#[test]
fn unmapped_pages_read_as_zero() {
    let mut r = ram(4 * P);
    assert_eq!(load(&mut r, 0, 8), [0; 8]);
    assert_eq!(load(&mut r, 4 * P - 1, 1), [0]);
    assert_eq!(load(&mut r, 0, 4 * 4096), vec![0; 4 * PAGE_SIZE]);
    assert_eq!(pages(&r), Value::U64(0));
}

#[test]
fn accesses_may_cross_pages() {
    let mut r = ram(4 * P);
    store(&mut r, P - 1, &[0xa, 0xb, 0xc, 0xd]);
    assert_eq!(pages(&r), Value::U64(2));
    assert_eq!(load(&mut r, P - 1, 4), [0xa, 0xb, 0xc, 0xd]);
    assert_eq!(load(&mut r, P - 2, 6), [0, 0xa, 0xb, 0xc, 0xd, 0]);
    // A write spanning three pages, and a read across an absent page.
    let long: Vec<u8> = (0..(PAGE_SIZE + 4)).map(|i| (i % 251) as u8 + 1).collect();
    store(&mut r, 2 * P - 2, &long);
    assert_eq!(pages(&r), Value::U64(4));
    assert_eq!(load(&mut r, 2 * P - 2, long.len() as u32), long);
}

#[test]
fn responses_follow_the_configured_latency_in_complete() {
    for latency in [
        LATENCY,
        LinkLatency::After(Duration::from_ns(7)),
        LinkLatency::Cycles {
            domain: ClockDomainId(2),
            k: 0,
        },
    ] {
        let mut r = Ram::new(RamConfig { size: 16, latency }, &image(&[])).unwrap();
        let when = match latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        for msg in [read(4, 0, 2), write(5, 2, &[1]), read(6, 16, 1)] {
            let mut ctx = MockCtx::new(Phase::Request);
            ctx.deliver(&mut r, PORT, msg).unwrap();
            let Sent {
                port,
                when: w,
                phase,
                ..
            } = ctx.take_one();
            assert_eq!((port, w, phase), (PORT, when, Phase::Complete));
        }
    }
}

#[test]
fn requests_past_the_end_fault_and_change_nothing() {
    let size = 2 * P + 5;
    let mut r = ram(size);
    store(&mut r, size - 1, &[7]);
    let before = snapshot_of(&r);
    assert_eq!(serve(&mut r, read(1, size - 1, 2)), read_fault(1));
    assert_eq!(serve(&mut r, read(2, size, 1)), read_fault(2));
    assert_eq!(serve(&mut r, write(3, size - 1, &[1, 2])), write_fault(3));
    assert_eq!(serve(&mut r, write(4, size, &[1])), write_fault(4));
    // Past the end of the address space.
    assert_eq!(serve(&mut r, read(5, u64::MAX, 2)), read_fault(5));
    assert_eq!(serve(&mut r, write(6, u64::MAX, &[1, 1])), write_fault(6));
    assert_eq!(snapshot_of(&r), before);
    assert_eq!(load(&mut r, size - 1, 1), [7]);
}

#[test]
fn protocol_violations_fault_the_session() {
    let mut r = ram(16);
    let mut ctx = MockCtx::new(Phase::Transfer);
    for (msg, what) in [
        (read(1, 0, 0), "ram: zero-length request"),
        (write(1, 0, &[]), "ram: zero-length request"),
        (read(1, 99, 0), "ram: zero-length request"),
        (done(1), "ram: response on the target port"),
        (read_fault(1), "ram: response on the target port"),
    ] {
        assert_eq!(
            ctx.deliver(&mut r, PORT, msg),
            Err(SimError::ComponentFault(what))
        );
    }
    assert!(ctx.sent.is_empty());
}

// ---------------------------------------------------------------------------------------
// Canonical storage.

#[test]
fn zeroing_the_last_non_zero_byte_removes_the_page() {
    let mut r = ram(2 * P);
    store(&mut r, 10, &[1, 2]);
    assert_eq!(pages(&r), Value::U64(1));
    store(&mut r, 10, &[0]);
    assert_eq!(pages(&r), Value::U64(1));
    store(&mut r, 11, &[0]);
    assert_eq!(pages(&r), Value::U64(0));
    // Writing zeros to an absent page allocates nothing.
    store(&mut r, P, &[0; 32]);
    assert_eq!(pages(&r), Value::U64(0));
    assert_eq!(snapshot_of(&r), snapshot_of(&ram(2 * P)));
}

#[test]
fn different_histories_with_the_same_contents_snapshot_identically() {
    let mut a = ram(3 * P);
    store(&mut a, P - 4, &[9; 8]);
    store(&mut a, 2 * P, &[5]);
    store(&mut a, P - 4, &[0; 8]);
    store(&mut a, 100, &[3]);
    let mut b = ram(3 * P);
    store(&mut b, 2 * P, &[5]);
    store(&mut b, 100, &[3]);
    assert_eq!(snapshot_of(&a), snapshot_of(&b));

    let mut contents = vec![0; 3 * PAGE_SIZE];
    contents[100] = 3;
    contents[2 * PAGE_SIZE] = 5;
    assert_eq!(snapshot_of(&a), canonical(3 * P, HASH, &contents));
}

#[test]
fn inspect_shows_size_hash_and_page_count_only() {
    let mut r = ram(3 * P);
    store(&mut r, 5, &[1]);
    store(&mut r, 2 * P, &[1]);
    assert_eq!(
        r.inspect().fields,
        [
            ("size", Value::U64(3 * P)),
            ("image_hash", Value::Bytes(HASH.to_vec())),
            ("non_zero_pages", Value::U64(2)),
        ]
    );
}

// ---------------------------------------------------------------------------------------
// Snapshots.

#[test]
fn snapshot_layout_is_pinned() {
    let mut r = Ram::new(
        RamConfig {
            size: 2 * P,
            latency: LinkLatency::After(Duration::from_ns(2)),
        },
        &image(&[(P + 1, &[0xee])]),
    )
    .unwrap();
    store(&mut r, 0, &[0xdd]);
    let mut w = SnapshotWriter::new();
    w.u64(2 * P);
    w.raw(&HASH);
    w.u8(0);
    w.u128(2_000_000);
    w.len(2);
    for (index, at, byte) in [(0u32, 0usize, 0xdd), (1, 1, 0xee)] {
        let mut page = vec![0; PAGE_SIZE];
        page[at] = byte;
        w.u32(index);
        w.bytes(&page);
    }
    assert_eq!(snapshot_of(&r), w.into_bytes());
}

/// Required regression (`docs/m1-design.md` §7.2): an omitted page means "all zero now",
/// never "as in the initial image".
#[test]
fn restore_clears_pages_loaded_from_the_initial_image() {
    let program = image(&[(P, &[0xab; 64]), (0, &[1])]);
    let mut r = Ram::new(config(3 * P), &program).unwrap();
    assert_eq!(pages(&r), Value::U64(2));
    // The simulation zeroes every image byte of page 1, so the snapshot omits it.
    store(&mut r, P, &[0; 64]);
    assert_eq!(pages(&r), Value::U64(1));
    let bytes = snapshot_of(&r);

    // A fresh RAM starts from the image again, with page 1 non-zero...
    let mut fresh = Ram::new(config(3 * P), &program).unwrap();
    assert_eq!(load(&mut fresh, P, 64), [0xab; 64]);
    let mut fresh = Ram::new(config(3 * P), &program).unwrap();
    assert_eq!(pages(&fresh), Value::U64(2));
    // ...until the snapshot replaces its whole memory.
    restore_into(&mut fresh, &bytes).unwrap();
    assert_eq!(load(&mut fresh, P, 64), [0; 64]);
    assert_eq!(load(&mut fresh, 0, 1), [1]);
    assert_eq!(pages(&fresh), Value::U64(1));
    assert_eq!(snapshot_of(&fresh), bytes);
}

#[test]
fn restore_rejects_a_different_configuration() {
    let bytes = snapshot_of(&ram(2 * P));
    let cases = [
        (config(3 * P), HASH, "ram: snapshot has a different size"),
        (
            config(2 * P),
            [0x22; 32],
            "ram: snapshot has a different image hash",
        ),
        (
            RamConfig {
                size: 2 * P,
                latency: LinkLatency::Cycles {
                    domain: ClockDomainId(0),
                    k: 4,
                },
            },
            HASH,
            "ram: snapshot has a different latency",
        ),
    ];
    for (config, image_hash, what) in cases {
        // Same contents (none), different identity.
        let mut other = Ram::new(
            config,
            &RamImage {
                image_hash,
                segments: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            restore_into(&mut other, &bytes),
            Err(RestoreError::InvalidState(what))
        );
    }
}

#[test]
fn restore_rejects_non_canonical_pages() {
    let size = 2 * P + 10;
    let header = {
        let bytes = snapshot_of(&ram(size));
        bytes[..bytes.len() - 4].to_vec()
    };
    let with = |pages: &[(u32, Vec<u8>)]| {
        let mut w = SnapshotWriter::new();
        w.raw(&header);
        w.len(pages.len());
        for (index, page) in pages {
            w.u32(*index);
            w.bytes(page);
        }
        w.into_bytes()
    };
    let page = |at: usize| {
        let mut p = vec![0; PAGE_SIZE];
        p[at] = 1;
        p
    };
    assert!(restore_into(&mut ram(size), &with(&[(0, page(0)), (2, page(9))])).is_ok());
    for (pages, what) in [
        (
            vec![(1, page(0)), (0, page(0))],
            "ram: pages out of order or duplicated",
        ),
        (
            vec![(1, page(0)), (1, page(1))],
            "ram: pages out of order or duplicated",
        ),
        (vec![(3, page(0))], "ram: page past the end"),
        (vec![(0, vec![0; PAGE_SIZE])], "ram: all-zero page"),
        (
            vec![(0, vec![1; PAGE_SIZE - 1])],
            "ram: page is not 4096 bytes",
        ),
        (
            vec![(0, vec![1; PAGE_SIZE + 1])],
            "ram: page is not 4096 bytes",
        ),
        (vec![(2, page(10))], "ram: non-zero bytes past the end"),
    ] {
        let mut r = ram(size);
        assert_eq!(
            restore_into(&mut r, &with(&pages)),
            Err(RestoreError::InvalidState(what))
        );
    }
}

// ---------------------------------------------------------------------------------------
// Properties.

/// Three and a bit pages, so the last page is partial.
const SIZE: u64 = 3 * P + 100;

#[derive(Clone, Debug)]
enum Op {
    Read(u64, u32),
    Write(u64, Vec<u8>),
    /// A write of zeros, which must remove pages it empties.
    Zero(u64, u32),
}

/// Offsets near page boundaries and the end, or anywhere, sometimes out of range.
fn offset() -> impl Strategy<Value = u64> {
    let near = (0..=4u64, -3i64..=3).prop_map(|(page, d)| (page * P).saturating_add_signed(d));
    prop_oneof![
        3 => near,
        2 => 0..SIZE,
        1 => (SIZE - 8)..(SIZE + 8),
        1 => Just(u64::MAX),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let len = prop_oneof![3 => 1u32..=8, 1 => 1u32..=300, 1 => Just(P as u32 + 1)];
    prop_oneof![
        (offset(), len.clone()).prop_map(|(o, l)| Op::Read(o, l)),
        (offset(), prop::collection::vec(any::<u8>(), 1..=16)).prop_map(|(o, d)| Op::Write(o, d)),
        (
            offset(),
            prop::collection::vec(prop_oneof![3 => Just(0u8), 1 => any::<u8>()], 1..=16)
        )
            .prop_map(|(o, d)| Op::Write(o, d)),
        (offset(), len).prop_map(|(o, l)| Op::Zero(o, l)),
    ]
}

/// Whether `[offset, offset + len)` lies inside the model, in `u128`.
fn fits(offset: u64, len: usize) -> bool {
    u128::from(offset) + len as u128 <= u128::from(SIZE)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    /// Any sequence of reads, writes, and zero writes behaves like a flat byte array, and
    /// the snapshot always equals the canonical encoding of the array's contents.
    #[test]
    fn ram_matches_a_flat_model(ops in prop::collection::vec(op(), 1..64)) {
        let mut r = ram(SIZE);
        let mut model = vec![0u8; usize::try_from(SIZE).unwrap()];
        for (i, op) in ops.into_iter().enumerate() {
            let txn = i as u64;
            match op {
                Op::Read(offset, len) => {
                    let resp = serve(&mut r, read(txn, offset, len));
                    let expected = if fits(offset, len as usize) {
                        let at = usize::try_from(offset).unwrap();
                        let data = model[at..at + len as usize].to_vec();
                        MemMsg::ReadResp { txn: TxnId(txn), outcome: ReadOutcome::Data { data } }
                    } else {
                        read_fault(txn)
                    };
                    prop_assert_eq!(resp, expected);
                }
                Op::Write(offset, data) => {
                    let resp = serve(&mut r, write(txn, offset, &data));
                    if fits(offset, data.len()) {
                        let at = usize::try_from(offset).unwrap();
                        model[at..at + data.len()].copy_from_slice(&data);
                        prop_assert_eq!(resp, done(txn));
                    } else {
                        prop_assert_eq!(resp, write_fault(txn));
                    }
                }
                Op::Zero(offset, len) => {
                    let data = vec![0; len as usize];
                    let resp = serve(&mut r, write(txn, offset, &data));
                    if fits(offset, data.len()) {
                        let at = usize::try_from(offset).unwrap();
                        model[at..at + data.len()].fill(0);
                        prop_assert_eq!(resp, done(txn));
                    } else {
                        prop_assert_eq!(resp, write_fault(txn));
                    }
                }
            }
            prop_assert_eq!(snapshot_of(&r), canonical(SIZE, HASH, &model));
        }
        let non_zero = model.chunks(PAGE_SIZE).filter(|c| c.iter().any(|&b| b != 0)).count();
        prop_assert_eq!(pages(&r), Value::U64(non_zero as u64));

        // The snapshot restores into a fresh RAM exactly, whatever its image held.
        let bytes = snapshot_of(&r);
        let mut fresh = Ram::new(config(SIZE), &image(&[(P - 1, &[0xff; 3])])).unwrap();
        restore_into(&mut fresh, &bytes).unwrap();
        prop_assert_eq!(snapshot_of(&fresh), bytes);
        prop_assert_eq!(load(&mut fresh, 0, SIZE as u32), model);
    }
}
