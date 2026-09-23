//! `AddressBus` configuration, routing, faults, protocol checks, and snapshots
//! (`docs/m1-design.md` §7.1), driven directly through a mock context.
//!
//! The routing oracle below works in `u128`, where `[base, base + size)` and
//! `[addr, addr + len)` cannot overflow, so it shares none of the bus's checked arithmetic.

mod common;

use common::{MockCtx, Sent, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{Component, PortId, Role};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotWriter};
use systemscope_contracts::trace::Value;
use systemscope_platform::bus::{CPU_PORT, FAULT_KIND, region_port};
use systemscope_platform::{AddressBus, BusConfigError, Region};

const RAM: Region = Region {
    name: "ram",
    base: 0x8000_0000,
    size: 0x1000,
};

const UART: Region = Region {
    name: "uart",
    base: 0x1000_0000,
    size: 8,
};

/// Directly after `RAM`, to check that the bus never splits a request.
const NEXT: Region = Region {
    name: "next",
    base: 0x8000_1000,
    size: 0x1000,
};

/// Ends exactly at the top of the address space.
const TOP: Region = Region {
    name: "top",
    base: u64::MAX - 0xf,
    size: 0x10,
};

fn region(name: &'static str, base: u64, size: u64) -> Region {
    Region { name, base, size }
}

fn bus(regions: &[Region]) -> AddressBus {
    AddressBus::new(regions.to_vec()).unwrap()
}

fn platform() -> AddressBus {
    bus(&[RAM, UART, NEXT, TOP])
}

fn outstanding(bus: &AddressBus) -> Value {
    bus.inspect().get("outstanding").unwrap().clone()
}

/// Delivers `msg` on the `cpu` port in `Request` and returns what the bus sent.
fn request(bus: &mut AddressBus, msg: MemMsg) -> (Sent, MockCtx) {
    let mut ctx = MockCtx::new(Phase::Request);
    ctx.deliver(bus, CPU_PORT, msg).unwrap();
    (ctx.take_one(), ctx)
}

fn forwarded(port: PortId, msg: MemMsg) -> Sent {
    Sent {
        port,
        msg,
        when: ScheduleWhen::Now,
        phase: Phase::Transfer,
    }
}

fn read_fault(txn: u64) -> Sent {
    Sent {
        port: CPU_PORT,
        msg: MemMsg::ReadResp {
            txn: TxnId(txn),
            outcome: ReadOutcome::Fault {
                fault: MemFault::AccessFault,
            },
        },
        when: ScheduleWhen::Now,
        phase: Phase::Complete,
    }
}

fn write_fault(txn: u64) -> Sent {
    Sent {
        port: CPU_PORT,
        msg: MemMsg::WriteResp {
            txn: TxnId(txn),
            outcome: WriteOutcome::Fault {
                fault: MemFault::AccessFault,
            },
        },
        when: ScheduleWhen::Now,
        phase: Phase::Complete,
    }
}

fn fault_record(txn: u64, addr: u64, len: u64) -> common::Traced {
    (
        FAULT_KIND,
        vec![
            ("txn", Value::U64(txn)),
            ("addr", Value::U64(addr)),
            ("len", Value::U64(len)),
        ],
    )
}

fn data(txn: u64, bytes: &[u8]) -> MemMsg {
    MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Data {
            data: bytes.to_vec(),
        },
    }
}

fn done(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
}

/// Delivers `msg` on `port` in `Complete`, returning the error it causes, if any.
fn respond(bus: &mut AddressBus, port: PortId, msg: MemMsg) -> Result<MockCtx, SimError> {
    let mut ctx = MockCtx::new(Phase::Complete);
    ctx.deliver(bus, port, msg)?;
    Ok(ctx)
}

fn component_fault(result: Result<impl Sized, SimError>) -> &'static str {
    match result {
        Err(SimError::ComponentFault(what)) => what,
        Err(other) => panic!("expected a component fault, got {other:?}"),
        Ok(_) => panic!("expected a component fault, got success"),
    }
}

// ---------------------------------------------------------------------------------------
// Configuration.

#[test]
fn ports_are_cpu_then_one_initiator_per_region_in_order() {
    let ports = platform().ports();
    let names: Vec<_> = ports.iter().map(|p| p.name).collect();
    assert_eq!(names, ["cpu", "ram", "uart", "next", "top"]);
    assert_eq!(ports[0].role, Role::Target);
    assert!(ports[1..].iter().all(|p| p.role == Role::Initiator));
    assert!(
        ports
            .iter()
            .all(|p| p.protocol.name == "mem" && p.protocol.version == 1)
    );
    assert_eq!(region_port(0), PortId(1));
    assert_eq!(platform().regions(), [RAM, UART, NEXT, TOP]);
}

#[test]
fn zero_size_region_is_rejected() {
    let err = AddressBus::new(vec![RAM, region("empty", 0x100, 0)]);
    assert_eq!(err.err(), Some(BusConfigError::ZeroSize("empty")));
}

#[test]
fn region_past_the_end_of_u64_is_rejected() {
    for (base, size) in [(u64::MAX, 2), (2, u64::MAX), (u64::MAX - 1, 3)] {
        let err = AddressBus::new(vec![region("wrap", base, size)]);
        assert_eq!(
            err.err(),
            Some(BusConfigError::Wraps("wrap")),
            "{base:#x}+{size:#x}"
        );
    }
    // Ending exactly at u64::MAX is fine, as is the largest region there is.
    for (base, size) in [
        (u64::MAX, 1),
        (u64::MAX - 0xf, 0x10),
        (0, u64::MAX),
        (1, u64::MAX),
    ] {
        assert!(AddressBus::new(vec![region("top", base, size)]).is_ok());
    }
}

#[test]
fn overlapping_regions_are_rejected() {
    let a = region("a", 0x1000, 0x1000);
    for b in [
        region("b", 0x1000, 0x1000),
        region("b", 0x1fff, 1),
        region("b", 0x0fff, 2),
        region("b", 0x1800, 0x10),
        region("b", 0, 0x10_0000),
    ] {
        assert_eq!(
            AddressBus::new(vec![a, b]).err(),
            Some(BusConfigError::Overlap("a", "b")),
            "{b:?}"
        );
    }
    // Adjacent regions share no byte.
    assert!(AddressBus::new(vec![a, region("b", 0x2000, 1), region("c", 0x0fff, 1)]).is_ok());
}

#[test]
fn duplicate_and_reserved_names_are_rejected() {
    let err = AddressBus::new(vec![RAM, region("ram", 0x100, 1)]);
    assert_eq!(err.err(), Some(BusConfigError::DuplicateName("ram")));
    let err = AddressBus::new(vec![region("cpu", 0x100, 1)]);
    assert_eq!(err.err(), Some(BusConfigError::ReservedName("cpu")));
}

// ---------------------------------------------------------------------------------------
// Routing.

#[test]
fn requests_reach_their_region_as_offsets_with_the_same_txn() {
    let mut b = platform();
    let (sent, ctx) = request(&mut b, read(7, 0x8000_0010, 4));
    assert_eq!(sent, forwarded(region_port(0), read(7, 0x10, 4)));
    assert!(ctx.traced.is_empty());

    let (sent, _) = request(&mut b, write(8, 0x1000_0004, &[0xAA]));
    assert_eq!(sent, forwarded(region_port(1), write(8, 4, &[0xAA])));

    let (sent, _) = request(&mut b, write(9, 0x8000_1ffc, &[1, 2, 3, 4]));
    assert_eq!(
        sent,
        forwarded(region_port(2), write(9, 0xffc, &[1, 2, 3, 4]))
    );
    assert_eq!(outstanding(&b), Value::U64(3));
}

#[test]
fn region_boundaries_are_half_open() {
    let mut b = bus(&[RAM]);
    let at = |b: &mut AddressBus, txn, addr, len| request(b, read(txn, addr, len)).0;
    // First byte, last byte, whole region.
    assert_eq!(
        at(&mut b, 1, 0x8000_0000, 1),
        forwarded(region_port(0), read(1, 0, 1))
    );
    assert_eq!(
        at(&mut b, 2, 0x8000_0fff, 1),
        forwarded(region_port(0), read(2, 0xfff, 1))
    );
    assert_eq!(
        at(&mut b, 3, 0x8000_0000, 0x1000),
        forwarded(region_port(0), read(3, 0, 0x1000))
    );
    // One byte outside on either side, and one byte too long.
    assert_eq!(at(&mut b, 4, 0x7fff_ffff, 1), read_fault(4));
    assert_eq!(at(&mut b, 5, 0x8000_1000, 1), read_fault(5));
    assert_eq!(at(&mut b, 6, 0x8000_0ffd, 4), read_fault(6));
    assert_eq!(at(&mut b, 7, 0x7fff_ffff, 2), read_fault(7));
    assert_eq!(at(&mut b, 8, 0x8000_0000, 0x1001), read_fault(8));
}

#[test]
fn requests_crossing_into_an_adjacent_region_are_not_split() {
    let mut b = platform();
    let (sent, ctx) = request(&mut b, write(1, 0x8000_0fff, &[1, 2]));
    assert_eq!(sent, write_fault(1));
    assert_eq!(ctx.traced, [fault_record(1, 0x8000_0fff, 2)]);
    assert_eq!(outstanding(&b), Value::U64(0));
}

#[test]
fn unmapped_addresses_fault() {
    let mut b = platform();
    for (txn, addr) in [(1, 0), (2, 0x1000_0008), (3, 0x7fff_fffc), (4, 0x8000_2000)] {
        let (sent, ctx) = request(&mut b, read(txn, addr, 4));
        assert_eq!(sent, read_fault(txn));
        assert_eq!(ctx.traced, [fault_record(txn, addr, 4)]);
    }
}

#[test]
fn the_last_byte_of_the_address_space_is_addressable() {
    let mut b = platform();
    let (sent, _) = request(&mut b, read(1, u64::MAX, 1));
    assert_eq!(sent, forwarded(region_port(3), read(1, 0xf, 1)));
    let (sent, _) = request(&mut b, read(2, u64::MAX - 3, 4));
    assert_eq!(sent, forwarded(region_port(3), read(2, 0xc, 4)));
    // Without a region there, it is an ordinary unmapped address.
    let (sent, _) = request(&mut bus(&[RAM]), read(3, u64::MAX, 1));
    assert_eq!(sent, read_fault(3));
}

#[test]
fn ranges_past_the_end_of_the_address_space_fault() {
    let mut b = platform();
    for (txn, addr, len) in [
        (1, u64::MAX, 2),
        (2, u64::MAX - 2, 4),
        (3, u64::MAX, u32::MAX),
    ] {
        let (sent, ctx) = request(&mut b, read(txn, addr, len));
        assert_eq!(sent, read_fault(txn));
        assert_eq!(ctx.traced, [fault_record(txn, addr, u64::from(len))]);
    }
    let (sent, ctx) = request(&mut b, write(4, u64::MAX, &[1, 2]));
    assert_eq!(sent, write_fault(4));
    assert_eq!(ctx.traced, [fault_record(4, u64::MAX, 2)]);
}

#[test]
fn a_faulted_request_leaves_nothing_outstanding() {
    let mut b = platform();
    request(&mut b, read(1, 0, 4));
    assert_eq!(outstanding(&b), Value::U64(0));
    // The txn is free again at once.
    let (sent, _) = request(&mut b, read(1, 0x8000_0000, 4));
    assert_eq!(sent, forwarded(region_port(0), read(1, 0, 4)));
}

#[test]
fn responses_are_relayed_unchanged_in_the_phase_they_arrive() {
    let mut b = platform();
    request(&mut b, read(1, 0x8000_0000, 2));
    request(&mut b, write(2, 0x1000_0000, &[9]));
    let target_fault = MemMsg::WriteResp {
        txn: TxnId(2),
        outcome: WriteOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    for (port, resp, phase) in [
        (region_port(0), data(1, &[5, 6]), Phase::Complete),
        (region_port(1), target_fault, Phase::Commit),
    ] {
        let mut ctx = MockCtx::new(phase);
        ctx.deliver(&mut b, port, resp.clone()).unwrap();
        let sent = ctx.take_one();
        assert_eq!(
            sent,
            Sent {
                port: CPU_PORT,
                msg: resp,
                when: ScheduleWhen::Now,
                phase,
            }
        );
        // The bus traces only its own faults.
        assert!(ctx.traced.is_empty());
    }
    assert_eq!(outstanding(&b), Value::U64(0));
}

// ---------------------------------------------------------------------------------------
// Protocol violations.

#[test]
fn zero_length_requests_fault_the_session() {
    for msg in [
        read(1, 0x8000_0000, 0),
        write(1, 0x8000_0000, &[]),
        read(1, 0, 0),
        read(1, u64::MAX, 0),
        write(1, u64::MAX, &[]),
    ] {
        let mut b = platform();
        let mut ctx = MockCtx::new(Phase::Request);
        let what = component_fault(ctx.deliver(&mut b, CPU_PORT, msg.clone()));
        assert_eq!(what, "address bus: zero-length request", "{msg:?}");
        assert!(ctx.sent.is_empty() && ctx.traced.is_empty());
    }
}

#[test]
fn a_duplicate_outstanding_txn_faults_the_session() {
    let mut b = platform();
    request(&mut b, read(5, 0x8000_0000, 4));
    let mut ctx = MockCtx::new(Phase::Request);
    // Whether the second request would be routed or answered with a fault.
    for msg in [read(5, 0x8000_0010, 4), write(5, 0, &[1])] {
        let what = component_fault(ctx.deliver(&mut b, CPU_PORT, msg));
        assert_eq!(what, "address bus: request reuses an outstanding txn");
    }
    // Once the response has gone back, the txn may be reused.
    respond(&mut b, region_port(0), data(5, &[0; 4])).unwrap();
    let (sent, _) = request(&mut b, read(5, 0x8000_0010, 4));
    assert_eq!(sent, forwarded(region_port(0), read(5, 0x10, 4)));
}

#[test]
fn unknown_and_duplicate_responses_fault_the_session() {
    let mut b = platform();
    let what = component_fault(respond(&mut b, region_port(0), data(1, &[0])));
    assert_eq!(what, "address bus: response for unknown txn");

    request(&mut b, read(2, 0x8000_0000, 1));
    respond(&mut b, region_port(0), data(2, &[0])).unwrap();
    let what = component_fault(respond(&mut b, region_port(0), data(2, &[0])));
    assert_eq!(what, "address bus: response for unknown txn");
}

#[test]
fn a_response_on_the_wrong_port_faults_the_session() {
    let mut b = platform();
    request(&mut b, read(1, 0x8000_0000, 1));
    let what = component_fault(respond(&mut b, region_port(1), data(1, &[0])));
    assert_eq!(what, "address bus: response on the wrong region port");
    // The correct response is still accepted.
    respond(&mut b, region_port(0), data(1, &[0])).unwrap();
}

#[test]
fn a_response_of_the_wrong_kind_faults_the_session() {
    let mut b = platform();
    request(&mut b, read(1, 0x8000_0000, 1));
    request(&mut b, write(2, 0x8000_0000, &[1]));
    let what = component_fault(respond(&mut b, region_port(0), done(1)));
    assert_eq!(what, "address bus: response kind mismatch");
    let what = component_fault(respond(&mut b, region_port(0), data(2, &[0])));
    assert_eq!(what, "address bus: response kind mismatch");
}

#[test]
fn messages_in_the_wrong_direction_fault_the_session() {
    let mut b = platform();
    let what = component_fault(respond(&mut b, CPU_PORT, data(1, &[0])));
    assert_eq!(what, "address bus: response on the cpu port");
    let what = component_fault(respond(&mut b, region_port(0), read(1, 0, 1)));
    assert_eq!(what, "address bus: request on a region port");
}

#[test]
fn requests_must_arrive_in_request() {
    for phase in [Phase::Transfer, Phase::Complete, Phase::Commit] {
        let mut b = platform();
        let mut ctx = MockCtx::new(phase);
        let what = component_fault(ctx.deliver(&mut b, CPU_PORT, read(1, 0x8000_0000, 4)));
        assert_eq!(what, "address bus: request arrived after REQUEST");
        assert!(ctx.sent.is_empty());
    }
}

// ---------------------------------------------------------------------------------------
// Snapshots and inspect.

#[test]
fn inspect_shows_counts_only() {
    let mut b = platform();
    request(&mut b, read(1, 0x8000_0000, 4));
    assert_eq!(
        b.inspect().fields,
        [("regions", Value::U64(4)), ("outstanding", Value::U64(1))]
    );
}

#[test]
fn snapshot_layout_is_pinned() {
    let mut b = bus(&[region("a", 0x10, 0x20)]);
    request(&mut b, write(9, 0x10, &[1]));
    request(&mut b, read(3, 0x11, 1));
    let mut w = SnapshotWriter::new();
    w.len(1);
    w.str("a");
    w.u64(0x10);
    w.u64(0x20);
    // Outstanding transactions by ascending txn, whatever the arrival order.
    w.len(2);
    w.u64(3);
    w.u16(0);
    w.bool(false);
    w.u64(9);
    w.u16(0);
    w.bool(true);
    assert_eq!(snapshot_of(&b), w.into_bytes());
}

#[test]
fn restore_resumes_outstanding_transactions() {
    let mut b = platform();
    request(&mut b, read(1, 0x8000_0000, 2));
    request(&mut b, write(2, u64::MAX, &[7]));
    let bytes = snapshot_of(&b);

    let mut fresh = platform();
    restore_into(&mut fresh, &bytes).unwrap();
    assert_eq!(snapshot_of(&fresh), bytes);
    let ctx = respond(&mut fresh, region_port(3), done(2)).unwrap();
    assert_eq!(ctx.sent[0].msg, done(2));
    let what = component_fault(respond(&mut fresh, region_port(1), data(1, &[0, 0])));
    assert_eq!(what, "address bus: response on the wrong region port");
    respond(&mut fresh, region_port(0), data(1, &[0, 0])).unwrap();
}

#[test]
fn restore_rejects_a_different_memory_map() {
    let bytes = snapshot_of(&platform());
    for regions in [
        vec![RAM, UART, NEXT],
        vec![UART, RAM, NEXT, TOP],
        vec![
            region("ram", 0x8000_0000, 0x2000),
            UART,
            region("next", 0x9000_0000, 1),
            TOP,
        ],
        vec![region("dram", 0x8000_0000, 0x1000), UART, NEXT, TOP],
        vec![RAM, UART, NEXT, TOP, region("extra", 0, 1)],
    ] {
        let err = restore_into(&mut bus(&regions), &bytes);
        assert_eq!(
            err,
            Err(RestoreError::InvalidState(
                "address bus: snapshot was taken with a different memory map"
            )),
            "{regions:?}"
        );
    }
}

#[test]
fn restore_rejects_non_canonical_outstanding_maps() {
    let b = bus(&[RAM]);
    let config = snapshot_of(&b);
    let config = &config[..config.len() - 4];
    let with = |entries: &[(u64, u16, bool)]| {
        let mut w = SnapshotWriter::new();
        w.raw(config);
        w.len(entries.len());
        for &(txn, region, write) in entries {
            w.u64(txn);
            w.u16(region);
            w.bool(write);
        }
        w.into_bytes()
    };
    assert!(restore_into(&mut bus(&[RAM]), &with(&[(1, 0, false), (2, 0, true)])).is_ok());
    for (entries, what) in [
        (
            vec![(2, 0, false), (1, 0, false)],
            "address bus: outstanding txns out of order or duplicated",
        ),
        (
            vec![(1, 0, false), (1, 0, true)],
            "address bus: outstanding txns out of order or duplicated",
        ),
        (
            vec![(1, 1, false)],
            "address bus: outstanding txn on an unknown region",
        ),
    ] {
        let err = restore_into(&mut bus(&[RAM]), &with(&entries));
        assert_eq!(err, Err(RestoreError::InvalidState(what)), "{entries:?}");
    }
}

// ---------------------------------------------------------------------------------------
// Properties.

/// The index of the one region holding all of `[addr, addr + len)`, computed in `u128`.
fn oracle(regions: &[Region], addr: u64, len: u64) -> Option<usize> {
    let (start, end) = (u128::from(addr), u128::from(addr) + u128::from(len));
    let hits: Vec<usize> = (0..regions.len())
        .filter(|&i| {
            let base = u128::from(regions[i].base);
            base <= start && end <= base + u128::from(regions[i].size)
        })
        .collect();
    assert!(hits.len() <= 1, "regions overlap");
    hits.first().copied()
}

/// Up to six disjoint regions, in a shuffled declaration order, sometimes ending at the
/// top of the address space.
fn regions() -> impl Strategy<Value = Vec<Region>> {
    const NAMES: [&str; 6] = ["r0", "r1", "r2", "r3", "r4", "r5"];
    let layout = (
        prop_oneof![Just(0u64), any::<u64>(), Just(u64::MAX - 0x40_0000)],
        prop::collection::vec((0u64..0x1_0000, 1u64..0x1_0000), 1..=6),
        any::<bool>(),
    );
    layout
        .prop_map(|(start, spans, to_top)| {
            let mut out = Vec::new();
            let mut at = start;
            for (i, &(gap, size)) in spans.iter().enumerate() {
                let Some(base) = at.checked_add(gap) else {
                    break;
                };
                let Some(last) = base.checked_add(size - 1) else {
                    break;
                };
                out.push(region(NAMES[i], base, size));
                let Some(next) = last.checked_add(1) else {
                    break;
                };
                at = next;
            }
            if to_top && let Some(last) = out.last_mut() {
                last.size = u64::MAX - last.base + 1;
            }
            if out.is_empty() {
                out.push(region(NAMES[0], 0, 1));
            }
            out
        })
        .prop_shuffle()
}

/// An address near a region edge, or anywhere.
fn address(regions: Vec<Region>) -> impl Strategy<Value = (Vec<Region>, u64)> {
    let edges: Vec<u64> = regions
        .iter()
        .flat_map(|r| [r.base, r.base.wrapping_add(r.size)])
        .collect();
    let near = (prop::sample::select(edges), -4i64..=4).prop_map(|(e, d)| e.wrapping_add_signed(d));
    let addr = prop_oneof![3 => near, 1 => any::<u64>(), 1 => Just(u64::MAX)];
    (Just(regions), addr)
}

fn length() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => 1u32..=8,
        1 => prop::sample::select(vec![1, 2, 4, 0x1000, 0xffff, u32::MAX]),
        1 => any::<u32>().prop_map(|l| l.max(1)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Every non-empty request reaches exactly the region the oracle names, at the right
    /// offset with the same txn and payload, or gets an access fault and reaches nothing.
    #[test]
    fn requests_route_as_the_oracle_says(
        (regions, addr) in regions().prop_flat_map(address),
        len in length(),
        is_write in any::<bool>(),
        txn in any::<u64>(),
    ) {
        let mut b = AddressBus::new(regions.clone()).unwrap();
        // Keep writes small; the payload's length is what matters.
        let len = if is_write { len.min(64) } else { len };
        let payload: Vec<u8> = if is_write {
            (0..len).map(|i| i as u8 ^ 0x5a).collect()
        } else {
            Vec::new()
        };
        let msg = if is_write { write(txn, addr, &payload) } else { read(txn, addr, len) };
        let (sent, ctx) = request(&mut b, msg);
        match oracle(&regions, addr, u64::from(len)) {
            Some(i) => {
                let offset = addr - regions[i].base;
                let expected = if is_write {
                    write(txn, offset, &payload)
                } else {
                    read(txn, offset, len)
                };
                let port = region_port(u16::try_from(i).unwrap());
                prop_assert_eq!(sent, forwarded(port, expected));
                prop_assert!(ctx.traced.is_empty());
                prop_assert_eq!(outstanding(&b), Value::U64(1));
            }
            None => {
                prop_assert_eq!(sent, if is_write { write_fault(txn) } else { read_fault(txn) });
                prop_assert_eq!(ctx.traced, vec![fault_record(txn, addr, u64::from(len))]);
                prop_assert_eq!(outstanding(&b), Value::U64(0));
            }
        }
    }

    /// A zero-length request is never answered: it always faults the session.
    #[test]
    fn zero_length_requests_never_route(
        (regions, addr) in regions().prop_flat_map(address),
        is_write in any::<bool>(),
    ) {
        let mut b = AddressBus::new(regions).unwrap();
        let msg = if is_write { write(1, addr, &[]) } else { read(1, addr, 0) };
        let mut ctx = MockCtx::new(Phase::Request);
        let result = ctx.deliver(&mut b, CPU_PORT, msg);
        prop_assert_eq!(result, Err(SimError::ComponentFault("address bus: zero-length request")));
        prop_assert!(ctx.sent.is_empty());
    }
}
