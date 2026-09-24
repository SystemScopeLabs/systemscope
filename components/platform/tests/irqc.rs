//! `SimpleIrqController` (`docs/m2-design.md` §7.2): configuration, ports, level
//! aggregation, the `PENDING`/`ENABLE` window, snapshots, inspect, and trace, driven
//! directly through a mock context and checked against an independent model; then behind
//! an `AddressBus` in a real runtime, with scripted `irq.v0` sources and a sink.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{IrqSent, MockCtx, Script, SendKind, Traced, read, restore_into, snapshot_of, write};
use proptest::prelude::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::protocol::{Message, mem};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::irqc::{
    ENABLE, LEVEL_KIND, MAX_SOURCES, MEIP_KIND, PENDING, SIZE, SNAPSHOT_SCHEMA, valid_mask,
};
use systemscope_platform::{
    AddressBus, IrqControllerConfig, IrqControllerConfigError, Region, SimpleIrqController,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;

const CLOCK: ClockDomainId = ClockDomainId(0);

const LATENCY: LinkLatency = LinkLatency::Cycles {
    domain: CLOCK,
    k: 2,
};

fn config(sources: u8) -> IrqControllerConfig {
    IrqControllerConfig {
        sources,
        latency: LATENCY,
    }
}

fn ctrl(sources: u8) -> SimpleIrqController {
    SimpleIrqController::new(config(sources)).unwrap()
}

/// The source mask, written independently of the crate: a 64-bit shift never overflows.
fn mask(sources: u8) -> u32 {
    u32::try_from((1u64 << sources) - 1).unwrap()
}

/// What one input did: the levels sent on `cpu` and the trace records.
#[derive(Debug, PartialEq, Eq)]
struct Effect {
    levels: Vec<bool>,
    traced: Vec<Traced>,
}

/// The levels sent on `cpu`, each checked to be `Now`, `Complete`.
fn cpu_levels(c: &SimpleIrqController, irqs: &[IrqSent]) -> Vec<bool> {
    irqs.iter()
        .map(|s| {
            assert_eq!(
                (s.port, s.when, s.phase),
                (c.cpu_port(), ScheduleWhen::Now, Phase::Complete)
            );
            s.asserted
        })
        .collect()
}

/// Delivers `Level { asserted }` on source `index`, in `Complete`.
fn level(c: &mut SimpleIrqController, index: u8, asserted: bool) -> Effect {
    let mut ctx = MockCtx::new(Phase::Complete);
    let port = c.src_port(index);
    ctx.deliver_msg(c, port, IrqMsg::Level { asserted }.into())
        .unwrap();
    assert!(ctx.sent.is_empty(), "a level is never answered");
    Effect {
        levels: cpu_levels(c, &ctx.irqs),
        traced: ctx.traced,
    }
}

/// Delivers a request on `mem` in `Transfer`; returns its response, which must be the one
/// `mem.v1` send, after the latency, in `Complete`, and what else the request did, with
/// the order of the sends.
fn serve(c: &mut SimpleIrqController, msg: MemMsg) -> (MemMsg, Effect, Vec<SendKind>) {
    let mut ctx = MockCtx::new(Phase::Transfer);
    let port = c.mem_port();
    ctx.deliver(c, port, msg).unwrap();
    let sent = ctx.take_one();
    assert_eq!(
        (sent.port, sent.when, sent.phase),
        (
            c.mem_port(),
            ScheduleWhen::Cycles {
                domain: CLOCK,
                k: 2
            },
            Phase::Complete
        )
    );
    let effect = Effect {
        levels: cpu_levels(c, &ctx.irqs),
        traced: ctx.traced,
    };
    (sent.msg, effect, ctx.order)
}

fn read_reg(c: &mut SimpleIrqController, offset: u64) -> u32 {
    let (resp, effect, _) = serve(c, read(7, offset, 4));
    assert_eq!(effect, quiet());
    match resp {
        MemMsg::ReadResp {
            txn: TxnId(7),
            outcome: ReadOutcome::Data { data },
        } => u32::from_le_bytes(data.try_into().unwrap()),
        other => panic!("{other:?}"),
    }
}

fn write_enable(c: &mut SimpleIrqController, value: u32) -> Effect {
    let (resp, effect, _) = serve(c, write(8, ENABLE, &value.to_le_bytes()));
    assert_eq!(resp, done(8));
    effect
}

fn done(txn: u64) -> MemMsg {
    MemMsg::WriteResp {
        txn: TxnId(txn),
        outcome: WriteOutcome::Done,
    }
}

fn quiet() -> Effect {
    Effect {
        levels: vec![],
        traced: vec![],
    }
}

fn level_trace(source: u8, asserted: bool) -> Traced {
    (
        LEVEL_KIND,
        vec![
            ("source", Value::U64(u64::from(source))),
            ("asserted", Value::Bool(asserted)),
        ],
    )
}

fn meip_trace(asserted: bool) -> Traced {
    (MEIP_KIND, vec![("asserted", Value::Bool(asserted))])
}

/// `pending`, `enable`, `out`.
fn state(c: &SimpleIrqController) -> (u32, u32, bool) {
    (c.pending(), c.enable(), c.out())
}

// ---------------------------------------------------------------------------------------
// Configuration and ports.

#[test]
fn only_1_to_32_sources_are_accepted() {
    assert_eq!(
        SimpleIrqController::new(config(0)).err(),
        Some(IrqControllerConfigError::NoSources)
    );
    for n in [33, 64, u8::MAX] {
        assert_eq!(
            SimpleIrqController::new(config(n)).err(),
            Some(IrqControllerConfigError::TooManySources(n))
        );
    }
    for n in 1..=MAX_SOURCES {
        let c = ctrl(n);
        assert_eq!(state(&c), (0, 0, false));
    }
    assert_eq!(MAX_SOURCES, 32);
}

#[test]
fn ports_are_the_sources_then_cpu_then_mem() {
    for n in [1u8, 2, 31, 32] {
        let c = ctrl(n);
        let ports = c.ports();
        assert_eq!(ports.len(), usize::from(n) + 2);
        for i in 0..n {
            assert_eq!(
                ports[usize::from(i)],
                PortSpec {
                    name: Box::leak(format!("src{i}").into_boxed_str()),
                    protocol: irq_v0::PROTOCOL,
                    role: Role::Target,
                }
            );
            assert_eq!(c.src_port(i), PortId(u16::from(i)));
        }
        assert_eq!(
            ports[usize::from(n)],
            PortSpec {
                name: "cpu",
                protocol: irq_v0::PROTOCOL,
                role: Role::Initiator,
            }
        );
        assert_eq!(
            ports[usize::from(n) + 1],
            PortSpec {
                name: "mem",
                protocol: mem_v1::PROTOCOL,
                role: Role::Target,
            }
        );
        assert_eq!(c.cpu_port(), PortId(u16::from(n)));
        assert_eq!(c.mem_port(), PortId(u16::from(n) + 1));
    }
}

#[test]
fn valid_mask_is_the_frozen_definition() {
    for n in 1..=MAX_SOURCES {
        assert_eq!(valid_mask(n), mask(n), "N = {n}");
    }
    assert_eq!(valid_mask(1), 0b1);
    assert_eq!(valid_mask(2), 0b11);
    assert_eq!(valid_mask(31), 0x7fff_ffff);
    assert_eq!(valid_mask(32), u32::MAX);
}

#[test]
fn reset_is_all_low_and_init_sends_nothing() {
    let mut c = ctrl(4);
    let mut ctx = MockCtx::new(Phase::Request);
    c.init(&mut ctx).unwrap();
    assert!(ctx.sent.is_empty() && ctx.irqs.is_empty() && ctx.traced.is_empty());
    assert_eq!(state(&c), (0, 0, false));
    assert_eq!(read_reg(&mut c, PENDING), 0);
    assert_eq!(read_reg(&mut c, ENABLE), 0);
}

// ---------------------------------------------------------------------------------------
// Source levels.

#[test]
fn each_source_sets_and_clears_its_own_pending_bit() {
    for n in [1u8, 2, 31, 32] {
        let mut c = ctrl(n);
        for i in 0..n {
            assert_eq!(
                level(&mut c, i, true),
                Effect {
                    levels: vec![],
                    traced: vec![level_trace(i, true)]
                }
            );
            assert_eq!(c.pending(), 1u32 << i);
            assert_eq!(read_reg(&mut c, PENDING), 1u32 << i);
            assert_eq!(
                level(&mut c, i, false),
                Effect {
                    levels: vec![],
                    traced: vec![level_trace(i, false)]
                }
            );
            assert_eq!(c.pending(), 0);
        }
        // All asserted at once: `pending` holds them all.
        for i in 0..n {
            level(&mut c, i, true);
        }
        assert_eq!(read_reg(&mut c, PENDING), mask(n));
    }
}

#[test]
fn repeated_levels_change_nothing_and_are_silent() {
    let mut c = ctrl(2);
    // A deassert at reset is already the stored level.
    assert_eq!(level(&mut c, 0, false), quiet());
    write_enable(&mut c, 0b11);
    assert_eq!(
        level(&mut c, 0, true),
        Effect {
            levels: vec![true],
            traced: vec![level_trace(0, true), meip_trace(true)]
        }
    );
    let before = snapshot_of(&c);
    for _ in 0..3 {
        assert_eq!(level(&mut c, 0, true), quiet());
    }
    assert_eq!(snapshot_of(&c), before);
    level(&mut c, 0, false);
    assert_eq!(level(&mut c, 0, false), quiet());
    assert_eq!(state(&c), (0, 0b11, false));
}

// ---------------------------------------------------------------------------------------
// Aggregation: meip = (pending & enable) != 0, sent only on change.

#[test]
fn directed_aggregation() {
    let rise = |i| Effect {
        levels: vec![true],
        traced: vec![level_trace(i, true), meip_trace(true)],
    };
    let asserted = |i| Effect {
        levels: vec![],
        traced: vec![level_trace(i, true)],
    };
    let deasserted = |i| Effect {
        levels: vec![],
        traced: vec![level_trace(i, false)],
    };
    let fall_on = |i| Effect {
        levels: vec![false],
        traced: vec![level_trace(i, false), meip_trace(false)],
    };
    let meip = |asserted| Effect {
        levels: vec![asserted],
        traced: vec![meip_trace(asserted)],
    };

    // A source asserted while disabled: no MEIP.
    let mut c = ctrl(2);
    assert_eq!(level(&mut c, 0, true), asserted(0));
    assert!(!c.out());
    // Enabling the asserted source: MEIP rises, in the ENABLE write.
    assert_eq!(write_enable(&mut c, 0b01), meip(true));
    assert!(c.out());
    // Disabling the only asserted source: MEIP falls.
    assert_eq!(write_enable(&mut c, 0b00), meip(false));
    assert_eq!(state(&c), (0b01, 0, false));

    // Two asserted, enabled sources: MEIP high, sent once.
    let mut c = ctrl(2);
    write_enable(&mut c, 0b11);
    assert_eq!(level(&mut c, 0, true), rise(0));
    assert_eq!(level(&mut c, 1, true), asserted(1));
    // Deasserting one of them: MEIP stays high, nothing sent.
    assert_eq!(level(&mut c, 0, false), deasserted(0));
    assert!(c.out());
    // Deasserting the last enabled one: MEIP falls.
    assert_eq!(level(&mut c, 1, false), fall_on(1));
    assert_eq!(state(&c), (0, 0b11, false));

    // A disabled asserted source and an enabled deasserted one: MEIP low.
    let mut c = ctrl(2);
    write_enable(&mut c, 0b10);
    assert_eq!(level(&mut c, 0, true), asserted(0));
    assert_eq!(state(&c), (0b01, 0b10, false));
    // An ENABLE write that leaves MEIP low sends nothing.
    assert_eq!(write_enable(&mut c, 0b10), quiet());
}

#[test]
fn enable_writes_that_keep_the_output_send_nothing() {
    let mut c = ctrl(3);
    level(&mut c, 0, true);
    level(&mut c, 2, true);
    assert_eq!(write_enable(&mut c, 0b001).levels, [true]);
    // Still high through each of these.
    for v in [0b101, 0b100, 0b111, 0b101] {
        assert_eq!(write_enable(&mut c, v), quiet(), "{v:#b}");
        assert!(c.out());
    }
    assert_eq!(write_enable(&mut c, 0b010).levels, [false]);
    for v in [0b010, 0, 0b010] {
        assert_eq!(write_enable(&mut c, v), quiet(), "{v:#b}");
    }
}

/// `pending` is not a latch: a pulse leaves nothing behind.
#[test]
fn pending_is_the_current_level_not_a_latch() {
    let mut c = ctrl(1);
    level(&mut c, 0, true);
    level(&mut c, 0, false);
    assert_eq!(write_enable(&mut c, 1), quiet());
    assert_eq!(read_reg(&mut c, PENDING), 0);
}

// ---------------------------------------------------------------------------------------
// MMIO.

#[test]
fn an_enable_write_takes_effect_at_acceptance() {
    let mut c = ctrl(2);
    level(&mut c, 1, true);
    let (resp, effect, order) = serve(&mut c, write(3, ENABLE, &0b10u32.to_le_bytes()));
    // In the same handler: `enable` changed, MEIP recomputed, and the level sent `Now`,
    // before the response, which is still scheduled after the latency.
    assert_eq!(state(&c), (0b10, 0b10, true));
    assert_eq!(effect.levels, [true]);
    assert_eq!(effect.traced, [meip_trace(true)]);
    assert_eq!(order, [SendKind::Irq, SendKind::Mem]);
    assert_eq!(resp, done(3));
    // Reads see it at once too.
    assert_eq!(read_reg(&mut c, ENABLE), 0b10);
}

#[test]
fn enable_writes_are_masked_to_the_sources() {
    for n in [1u8, 2, 3, 31, 32] {
        let mut c = ctrl(n);
        for v in [u32::MAX, 0xa5a5_a5a5, 0x8000_0000, 1, 0] {
            write_enable(&mut c, v);
            assert_eq!(c.enable(), v & mask(n), "N = {n}, {v:#x}");
            assert_eq!(read_reg(&mut c, ENABLE), v & mask(n));
        }
    }
}

fn fault_for(msg: &MemMsg) -> MemMsg {
    let fault = MemFault::AccessFault;
    match msg {
        MemMsg::ReadReq { txn, .. } => MemMsg::ReadResp {
            txn: *txn,
            outcome: ReadOutcome::Fault { fault },
        },
        MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp {
            txn: *txn,
            outcome: WriteOutcome::Fault { fault },
        },
        _ => unreachable!(),
    }
}

/// A controller at N = 32 with every register non-trivial and the output high.
fn busy() -> SimpleIrqController {
    let mut c = ctrl(32);
    for i in [0, 5, 31] {
        level(&mut c, i, true);
    }
    write_enable(&mut c, 0x8000_0021);
    assert_eq!(state(&c), (0x8000_0021, 0x8000_0021, true));
    c
}

#[test]
fn every_other_access_faults_and_changes_nothing() {
    let mut requests = Vec::new();
    // Widths other than 4 at either register, reads and writes.
    for offset in [PENDING, ENABLE] {
        for len in [1u32, 2, 3, 8] {
            requests.push(read(1, offset, len));
            requests.push(write(2, offset, &vec![0xff; len as usize]));
        }
    }
    // PENDING is read-only.
    requests.push(write(3, PENDING, &u32::MAX.to_le_bytes()));
    requests.push(write(3, PENDING, &0u32.to_le_bytes()));
    // Misaligned 4-byte accesses, other offsets, and requests crossing the window.
    for offset in [1u64, 2, 3, 5, 6, 7, 8, 0xc, 0x10, 0x1000] {
        requests.push(read(4, offset, 4));
        requests.push(write(5, offset, &u32::MAX.to_le_bytes()));
    }
    requests.push(read(6, 6, 4));
    requests.push(read(6, 4, 8));
    requests.push(read(6, 0, 8));
    requests.push(write(6, 7, &[0xff, 0xff]));
    // Past u64::MAX, and ending exactly there.
    requests.push(read(7, u64::MAX, 4));
    requests.push(write(7, u64::MAX - 2, &u32::MAX.to_le_bytes()));
    requests.push(read(7, u64::MAX - 3, 4));
    for msg in requests {
        let mut c = busy();
        let before = snapshot_of(&c);
        let (resp, effect, _) = serve(&mut c, msg.clone());
        assert_eq!(resp, fault_for(&msg), "{msg:?}");
        assert_eq!(effect, quiet(), "{msg:?}");
        assert_eq!(snapshot_of(&c), before, "{msg:?}");
    }
}

#[test]
fn the_two_registers_read_and_write_as_mapped() {
    let mut c = busy();
    assert_eq!(read_reg(&mut c, PENDING), 0x8000_0021);
    assert_eq!(read_reg(&mut c, ENABLE), 0x8000_0021);
    write_enable(&mut c, 0x0000_0020);
    assert_eq!(read_reg(&mut c, ENABLE), 0x20);
    assert_eq!(read_reg(&mut c, PENDING), 0x8000_0021);
    assert_eq!(SIZE, 8);
    assert_eq!((PENDING, ENABLE), (0, 4));
}

#[test]
fn protocol_violations_fault_the_session() {
    let n = 2;
    let fresh = || {
        let mut c = ctrl(n);
        level(&mut c, 0, true);
        write_enable(&mut c, 1);
        c
    };
    let irq = |asserted| Message::from(IrqMsg::Level { asserted });
    let mut cases: Vec<(Phase, PortId, Message)> = Vec::new();
    // A level outside COMPLETE, on any source, either value.
    for phase in [
        Phase::Request,
        Phase::Transfer,
        Phase::Commit,
        Phase::Observe,
    ] {
        for (i, asserted) in [(0, false), (1, true)] {
            cases.push((phase, PortId(i), irq(asserted)));
        }
    }
    // mem.v1 or mem.v0 on a source; irq.v0 on mem; anything on cpu; an unknown port.
    cases.push((Phase::Complete, PortId(1), read(1, PENDING, 4).into()));
    cases.push((
        Phase::Complete,
        PortId(0),
        mem::MemMsg::ReadReq {
            txn: mem::TxnId(1),
            addr: 0,
            len: 4,
        }
        .into(),
    ));
    cases.push((Phase::Complete, PortId(3), irq(true)));
    cases.push((Phase::Complete, PortId(2), irq(false)));
    cases.push((Phase::Transfer, PortId(2), read(1, PENDING, 4).into()));
    cases.push((Phase::Complete, PortId(4), irq(true)));
    cases.push((Phase::Transfer, PortId(4), read(1, PENDING, 4).into()));
    // Responses on mem, and zero-length requests.
    cases.push((Phase::Complete, PortId(3), done(1).into()));
    cases.push((
        Phase::Complete,
        PortId(3),
        MemMsg::ReadResp {
            txn: TxnId(1),
            outcome: ReadOutcome::Data { data: vec![0; 4] },
        }
        .into(),
    ));
    cases.push((Phase::Transfer, PortId(3), read(1, PENDING, 0).into()));
    cases.push((Phase::Transfer, PortId(3), write(1, ENABLE, &[]).into()));
    for (phase, port, msg) in cases {
        let mut c = fresh();
        let before = snapshot_of(&c);
        let mut ctx = MockCtx::new(phase);
        let result = ctx.deliver_msg(&mut c, port, msg.clone());
        assert!(
            matches!(result, Err(SimError::ComponentFault(_))),
            "{phase:?} {port:?} {msg:?}: {result:?}"
        );
        assert!(ctx.sent.is_empty() && ctx.irqs.is_empty() && ctx.traced.is_empty());
        assert_eq!(snapshot_of(&c), before);
    }
    // It never wakes itself.
    let mut c = fresh();
    let mut ctx = MockCtx::new(Phase::Request);
    assert!(matches!(
        c.handle_event(&Delivered::Wake { token: 0 }, &mut ctx),
        Err(SimError::ComponentFault(_))
    ));
}

#[test]
fn the_latency_is_the_configured_one() {
    let latency = LinkLatency::After(Duration::from_fs(3_000_000));
    let mut c = SimpleIrqController::new(IrqControllerConfig {
        sources: 1,
        latency,
    })
    .unwrap();
    let mut ctx = MockCtx::new(Phase::Request);
    let port = c.mem_port();
    ctx.deliver(&mut c, port, read(1, ENABLE, 4)).unwrap();
    let sent = ctx.take_one();
    assert_eq!(
        (sent.when, sent.phase),
        (
            ScheduleWhen::After(Duration::from_fs(3_000_000)),
            Phase::Complete
        )
    );
}

// ---------------------------------------------------------------------------------------
// N = 32.

#[test]
fn source_31_works_at_32_sources() {
    let mut c = ctrl(32);
    assert_eq!(valid_mask(32), u32::MAX);
    assert_eq!(
        level(&mut c, 31, true),
        Effect {
            levels: vec![],
            traced: vec![level_trace(31, true)]
        }
    );
    assert_eq!(read_reg(&mut c, PENDING), 0x8000_0000);
    assert_eq!(
        write_enable(&mut c, 0x8000_0000),
        Effect {
            levels: vec![true],
            traced: vec![meip_trace(true)]
        }
    );
    assert_eq!(read_reg(&mut c, ENABLE), 0x8000_0000);
    let bytes = snapshot_of(&c);
    let mut copy = ctrl(32);
    restore_into(&mut copy, &bytes).unwrap();
    assert_eq!(state(&copy), (0x8000_0000, 0x8000_0000, true));
    assert_eq!(snapshot_of(&copy), bytes);
    assert_eq!(level(&mut copy, 31, false).levels, [false]);
    write_enable(&mut copy, u32::MAX);
    assert_eq!(copy.enable(), u32::MAX);
}

// ---------------------------------------------------------------------------------------
// Inspect.

fn field(view: &StateView, name: &str) -> Value {
    view.get(name).unwrap_or_else(|| panic!("{name}")).clone()
}

#[test]
fn inspect_shows_the_sources_and_the_three_registers() {
    let mut c = ctrl(5);
    level(&mut c, 3, true);
    write_enable(&mut c, 0b11000);
    let view = c.inspect();
    let names: Vec<&str> = view.fields.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["sources", "pending", "enable", "out"]);
    assert_eq!(field(&view, "sources"), Value::U64(5));
    assert_eq!(field(&view, "pending"), Value::U64(0b01000));
    assert_eq!(field(&view, "enable"), Value::U64(0b11000));
    assert_eq!(field(&view, "out"), Value::Bool(true));
}

// ---------------------------------------------------------------------------------------
// Snapshots.

fn latency_bytes(w: &mut SnapshotWriter, latency: LinkLatency) {
    match latency {
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
}

/// Schema 1 bytes, written from §7.2.
fn forge(sources: u8, latency: LinkLatency, pending: u32, enable: u32, out: bool) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u8(sources);
    latency_bytes(&mut w, latency);
    w.u32(pending);
    w.u32(enable);
    w.bool(out);
    w.into_bytes()
}

#[test]
fn the_snapshot_is_the_config_then_pending_enable_out() {
    let c = busy();
    assert_eq!(c.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(SNAPSHOT_SCHEMA, 1);
    assert_eq!(
        snapshot_of(&c),
        forge(32, LATENCY, 0x8000_0021, 0x8000_0021, true)
    );
    assert_eq!(snapshot_of(&ctrl(1)), forge(1, LATENCY, 0, 0, false));
}

#[test]
fn snapshots_round_trip_byte_for_byte() {
    type Build = fn(&mut SimpleIrqController);
    let cases: [(&str, u8, Build); 8] = [
        ("reset", 4, |_| {}),
        ("asserted but disabled", 4, |c| {
            level(c, 2, true);
        }),
        ("asserted and enabled", 4, |c| {
            level(c, 2, true);
            write_enable(c, 0b0100);
        }),
        ("several sources", 4, |c| {
            for i in [0, 1, 3] {
                level(c, i, true);
            }
            write_enable(c, 0b1010);
        }),
        ("one deasserted, another still asserted", 4, |c| {
            write_enable(c, 0b0011);
            level(c, 0, true);
            level(c, 1, true);
            level(c, 0, false);
        }),
        ("N = 1", 1, |c| {
            level(c, 0, true);
            write_enable(c, 1);
        }),
        ("N = 32", 32, |c| {
            level(c, 31, true);
            level(c, 0, true);
            write_enable(c, 0x8000_0000);
        }),
        ("N = 32, all", 32, |c| {
            for i in 0..32 {
                level(c, i, true);
            }
            write_enable(c, u32::MAX);
        }),
    ];
    for (name, n, build) in cases {
        let mut c = ctrl(n);
        build(&mut c);
        let bytes = snapshot_of(&c);
        let mut copy = ctrl(n);
        restore_into(&mut copy, &bytes).unwrap();
        assert_eq!(state(&copy), state(&c), "{name}");
        assert_eq!(snapshot_of(&copy), bytes, "{name}");
        assert_eq!(copy.inspect(), c.inspect(), "{name}");
    }
}

#[test]
fn restore_rejects_invalid_snapshots_and_changes_nothing() {
    let other_latency = LinkLatency::Cycles {
        domain: CLOCK,
        k: 3,
    };
    let rejected: [(&str, Vec<u8>); 11] = [
        (
            "pending outside the sources",
            forge(3, LATENCY, 0b1000, 0, false),
        ),
        (
            "pending bit 31 at N = 3",
            forge(3, LATENCY, 0x8000_0000, 0, false),
        ),
        (
            "enable outside the sources",
            forge(3, LATENCY, 0, 0b1000, false),
        ),
        (
            "out high with nothing enabled",
            forge(3, LATENCY, 0b001, 0b010, true),
        ),
        ("out high at reset", forge(3, LATENCY, 0, 0, true)),
        (
            "out low with a source active",
            forge(3, LATENCY, 0b001, 0b001, false),
        ),
        ("a different source count", forge(4, LATENCY, 0, 0, false)),
        ("a smaller source count", forge(2, LATENCY, 0, 0, false)),
        ("a different latency", forge(3, other_latency, 0, 0, false)),
        (
            "a physical latency",
            forge(
                3,
                LinkLatency::After(Duration::from_fs(2_000_000)),
                0,
                0,
                false,
            ),
        ),
        ("truncated", forge(3, LATENCY, 0, 0, false)[..10].to_vec()),
    ];
    for (name, bytes) in rejected {
        let mut c = ctrl(3);
        level(&mut c, 1, true);
        let before = snapshot_of(&c);
        assert!(restore_into(&mut c, &bytes).is_err(), "{name}");
        assert_eq!(snapshot_of(&c), before, "{name}");
    }
    // A trailing byte: the component reads a valid state, and the reader's `finish`
    // rejects the rest, as for every component.
    let mut bytes = forge(3, LATENCY, 0, 0, false);
    bytes.push(0);
    assert!(restore_into(&mut ctrl(3), &bytes).is_err());
    // `out` must be a canonical bool.
    let mut bytes = forge(3, LATENCY, 0, 0, false);
    *bytes.last_mut().unwrap() = 2;
    let mut c = ctrl(3);
    assert!(matches!(
        restore_into(&mut c, &bytes),
        Err(RestoreError::Decode(_))
    ));
    // The schema's own reader rejects a bit outside the mask only through the mask:
    // at N = 32 every bit is valid.
    let mut c = ctrl(32);
    restore_into(&mut c, &forge(32, LATENCY, u32::MAX, u32::MAX, true)).unwrap();
    assert_eq!(state(&c), (u32::MAX, u32::MAX, true));
}

/// A restored controller computes its next output from the restored `out`: it sends the
/// fall, and never repeats the rise.
#[test]
fn a_restored_controller_continues_from_its_output() {
    let mut c = ctrl(2);
    restore_into(&mut c, &forge(2, LATENCY, 0b01, 0b01, true)).unwrap();
    assert_eq!(level(&mut c, 1, true).levels, Vec::<bool>::new());
    assert_eq!(write_enable(&mut c, 0b11), quiet());
    assert_eq!(level(&mut c, 0, false).levels, Vec::<bool>::new());
    assert_eq!(level(&mut c, 1, false).levels, [false]);
}

// ---------------------------------------------------------------------------------------
// An independent model (§7.2) and random sequences.

#[derive(Clone, Copy, Debug)]
enum Op {
    Source { index: u8, asserted: bool },
    EnableWrite(u32),
    PendingRead,
    EnableRead,
}

/// The controller, from §7.2 alone: no code from the crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Model {
    sources: u8,
    pending: u32,
    enable: u32,
    out: bool,
}

/// What the model says one operation does.
#[derive(Debug, PartialEq, Eq)]
struct Expected {
    read: Option<u32>,
    level_changed: bool,
    sent: Option<bool>,
}

impl Model {
    fn new(sources: u8) -> Model {
        Model {
            sources,
            pending: 0,
            enable: 0,
            out: false,
        }
    }

    fn apply(&mut self, op: Op) -> Expected {
        let mut read = None;
        let mut level_changed = false;
        match op {
            Op::Source { index, asserted } => {
                let before = self.pending;
                if asserted {
                    self.pending |= 1 << index;
                } else {
                    self.pending &= !(1 << index);
                }
                level_changed = self.pending != before;
            }
            Op::EnableWrite(v) => self.enable = v & mask(self.sources),
            Op::PendingRead => read = Some(self.pending & mask(self.sources)),
            Op::EnableRead => read = Some(self.enable),
        }
        let meip = self.pending & self.enable != 0;
        let sent = (meip != self.out).then_some(meip);
        self.out = meip;
        Expected {
            read,
            level_changed,
            sent,
        }
    }
}

/// Runs `op` on the controller and reports it the way the model does.
fn run(c: &mut SimpleIrqController, op: Op) -> Expected {
    let (read, effect) = match op {
        Op::Source { index, asserted } => (None, level(c, index, asserted)),
        Op::EnableWrite(v) => (None, write_enable(c, v)),
        Op::PendingRead => (Some(read_reg(c, PENDING)), quiet()),
        Op::EnableRead => (Some(read_reg(c, ENABLE)), quiet()),
    };
    let level_changed = effect.traced.iter().any(|t| t.0 == LEVEL_KIND);
    assert!(effect.levels.len() <= 1, "{effect:?}");
    let sent = effect.levels.first().copied();
    // Each output change is traced once, after the level change that caused it.
    let mut expected_trace = Vec::new();
    if let Op::Source { index, asserted } = op
        && level_changed
    {
        expected_trace.push(level_trace(index, asserted));
    }
    if let Some(asserted) = sent {
        expected_trace.push(meip_trace(asserted));
    }
    assert_eq!(effect.traced, expected_trace);
    Expected {
        read,
        level_changed,
        sent,
    }
}

fn sources() -> impl Strategy<Value = u8> {
    prop_oneof![Just(1u8), Just(2), Just(31), Just(32), 1u8..=32]
}

fn op(sources: u8) -> impl Strategy<Value = Op> {
    let value = prop_oneof![
        any::<u32>(),
        0u32..4,
        Just(0),
        Just(u32::MAX),
        (0u32..32).prop_map(|b| 1 << b),
    ];
    prop_oneof![
        5 => (0..sources, any::<bool>())
            .prop_map(|(index, asserted)| Op::Source { index, asserted }),
        3 => value.prop_map(Op::EnableWrite),
        1 => Just(Op::PendingRead),
        1 => Just(Op::EnableRead),
    ]
}

fn scenario() -> impl Strategy<Value = (u8, Vec<Op>)> {
    sources().prop_flat_map(|n| (Just(n), prop::collection::vec(op(n), 0..64)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// After every operation of any sequence, the controller's state, reads, level
    /// traces, and sent levels are the model's.
    #[test]
    fn every_sequence_matches_the_model((n, ops) in scenario()) {
        let mut c = ctrl(n);
        let mut model = Model::new(n);
        for op in ops {
            let expected = model.apply(op);
            prop_assert_eq!(run(&mut c, op), expected, "{:?}", op);
            prop_assert_eq!(state(&c), (model.pending, model.enable, model.out));
        }
        // And the state snapshots and restores canonically.
        let bytes = snapshot_of(&c);
        let mut copy = ctrl(n);
        restore_into(&mut copy, &bytes).unwrap();
        prop_assert_eq!(snapshot_of(&copy), bytes);
    }

    /// Levels of distinct sources delivered in any order leave the same state: it depends
    /// only on the final levels and `ENABLE`.
    #[test]
    fn the_order_of_distinct_source_levels_does_not_matter(
        (n, levels, enable, order) in sources().prop_flat_map(|n| {
            let levels = prop::collection::vec(any::<bool>(), usize::from(n));
            (Just(n), levels, any::<u32>(), Just(()))
        }).prop_flat_map(|(n, levels, enable, ())| {
            let order = Just((0..n).collect::<Vec<u8>>()).prop_shuffle();
            (Just(n), Just(levels), Just(enable), order)
        })
    ) {
        let mut forward = ctrl(n);
        let mut shuffled = ctrl(n);
        write_enable(&mut forward, enable);
        write_enable(&mut shuffled, enable);
        for i in 0..n {
            level(&mut forward, i, levels[usize::from(i)]);
        }
        for &i in &order {
            level(&mut shuffled, i, levels[usize::from(i)]);
        }
        prop_assert_eq!(snapshot_of(&forward), snapshot_of(&shuffled));
    }
}

// ---------------------------------------------------------------------------------------
// In a real runtime: Script → bus → controller, scripted sources, a sink on `cpu`.

const IRQC_BASE: u64 = 0x1000_1000;
const TICKS_PER_CYCLE: u64 = 1000;

/// A stateless `irq.v0` source: sends `levels[i].1` at cycle `levels[i].0`, in
/// `Complete`, all scheduled during `init`.
struct LevelScript {
    clock: ClockDomainId,
    levels: Vec<(u64, bool)>,
}

impl Component for LevelScript {
    fn type_name(&self) -> &'static str {
        "test.irq_script"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for &(k, asserted) in &self.levels {
            let when = ScheduleWhen::Cycles {
                domain: self.clock,
                k,
            };
            ctx.send(
                PortId(0),
                IrqMsg::Level { asserted }.into(),
                when,
                Phase::Complete,
            )?;
        }
        Ok(())
    }

    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Err(SimError::ComponentFault(
            "irq script: a source receives nothing",
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

/// A stateless `irq.v0` sink standing in for the CPU; deliveries show up in the runtime's
/// dispatch records.
struct Sink;

impl Component for Sink {
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
            _ => Err(SimError::ComponentFault(
                "irq sink: not a level in COMPLETE",
            )),
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

/// Keeps the last view of one component.
struct Watch(ComponentId, Rc<RefCell<Option<StateView>>>);

impl Observer for Watch {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        *self.1.borrow_mut() = world.inspect(self.0);
        Control::Continue
    }
}

/// Where each component lands, in declaration order.
struct Ids {
    script: ComponentId,
    irqc: ComponentId,
    sink: ComponentId,
}

/// Script → bus → controller (2 sources, responding `Cycles { 0 }`), sources on `src0`
/// and `src1`, the sink on `cpu`; every link `Cycles { 1 }` except `cpu`, which has
/// `cpu_link`. With `swap`, the two sources are declared in the other order.
fn build(
    requests: &[(u64, MemMsg)],
    src0: &[(u64, bool)],
    src1: &[(u64, bool)],
    cpu_link: Option<u64>,
    swap: bool,
) -> (Runtime, Ids) {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let script = t.add_component(
        "soc.script",
        Box::new(Script {
            clock,
            requests: requests.to_vec(),
        }),
    );
    let bus = AddressBus::new(vec![Region {
        name: "irqc",
        base: IRQC_BASE,
        size: SIZE,
    }])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let irqc = SimpleIrqController::new(IrqControllerConfig {
        sources: 2,
        latency: cycles(0),
    })
    .unwrap();
    let irqc = t.add_component("soc.irqc", Box::new(irqc));
    let sink = t.add_component("soc.sink", Box::new(Sink));
    let source = |levels: &[(u64, bool)]| {
        Box::new(LevelScript {
            clock,
            levels: levels.to_vec(),
        })
    };
    let (a, b) = if swap {
        let b = t.add_component("soc.src1", source(src1));
        let a = t.add_component("soc.src0", source(src0));
        (a, b)
    } else {
        let a = t.add_component("soc.src0", source(src0));
        let b = t.add_component("soc.src1", source(src1));
        (a, b)
    };
    t.connect((script, "mem"), (bus, "cpu"), Some(cycles(1)));
    t.connect((bus, "irqc"), (irqc, "mem"), Some(cycles(1)));
    t.connect((a, "irq"), (irqc, "src0"), Some(cycles(1)));
    t.connect((b, "irq"), (irqc, "src1"), Some(cycles(1)));
    t.connect((irqc, "cpu"), (sink, "irq"), cpu_link.map(cycles));
    let rt = t.elaborate(SessionConfig::default()).unwrap();
    (rt, Ids { script, irqc, sink })
}

fn level_of(ev: &Dispatched) -> Option<bool> {
    match ev.delivery {
        Delivered::Message {
            msg: Message::Irq(IrqMsg::Level { asserted }),
            ..
        } => Some(asserted),
        _ => None,
    }
}

fn is_request(ev: &Dispatched) -> bool {
    matches!(
        ev.delivery,
        Delivered::Message {
            msg: Message::MemV1(MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. }),
            ..
        }
    )
}

/// The traffic of the timing test: src0 rises while disabled, `ENABLE` selects it, both
/// registers are read, src0 falls; then src1 rises while enabled and falls.
/// Scripted requests, by cycle.
type Requests = Vec<(u64, MemMsg)>;
/// Scripted levels of one source, by cycle.
type Levels = Vec<(u64, bool)>;

fn timing_traffic() -> (Requests, Levels, Levels) {
    let requests = vec![
        (6, write(1, IRQC_BASE + ENABLE, &0b11u32.to_le_bytes())),
        (9, read(2, IRQC_BASE + PENDING, 4)),
        (10, read(3, IRQC_BASE + ENABLE, 4)),
        (11, write(4, IRQC_BASE + PENDING, &0u32.to_le_bytes())),
    ];
    (
        requests,
        vec![(2, true), (14, false)],
        vec![(20, true), (25, false)],
    )
}

#[test]
fn outputs_follow_levels_and_enable_with_send_and_delivery_times_apart() {
    for cpu_link in [Some(1), None] {
        let (requests, src0, src1) = timing_traffic();
        let (mut rt, ids) = build(&requests, &src0, &src1, cpu_link, false);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
        let trace = rt.take_trace().unwrap();

        // The controller's decisions: each output change is traced at the event that
        // caused it.
        let causes: Vec<(&Dispatched, bool)> = trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.component == ids.irqc)
            .filter(|r| r.kind == MEIP_KIND)
            .map(|r| {
                let TraceAt::Event(key) = r.at else {
                    panic!("{r:?}")
                };
                let ev = events.iter().find(|e| e.key == key).unwrap();
                (ev, r.fields[0].1 == Value::Bool(true))
            })
            .collect();
        assert_eq!(
            causes.iter().map(|c| c.1).collect::<Vec<_>>(),
            [true, false, true, false]
        );
        // The rise is the ENABLE write's acceptance; the others are source levels, in
        // COMPLETE.
        assert!(is_request(causes[0].0), "{:?}", causes[0].0);
        assert_eq!(causes[0].0.target, ids.irqc);
        for (ev, _) in &causes[1..] {
            assert_eq!((ev.target, ev.key.phase), (ids.irqc, Phase::Complete));
            assert!(level_of(ev).is_some());
        }
        // Each is delivered to the sink one link latency later, in COMPLETE.
        let delivered: Vec<&Dispatched> = events.iter().filter(|e| e.target == ids.sink).collect();
        assert_eq!(delivered.len(), 4);
        for ((cause, asserted), got) in causes.iter().zip(&delivered) {
            assert_eq!(level_of(got), Some(*asserted));
            assert_eq!(got.key.phase, Phase::Complete);
            let delay = cpu_link.unwrap_or(0) * TICKS_PER_CYCLE;
            assert_eq!(got.key.tick.0, cause.key.tick.0 + delay, "{cpu_link:?}");
        }
        // The level and ENABLE traces agree with the script, and the reads saw the state.
        let responses: Vec<&MemMsg> = events
            .iter()
            .filter(|e| e.target == ids.script)
            .map(|e| match &e.delivery {
                Delivered::Message {
                    msg: Message::MemV1(m),
                    ..
                } => m,
                other => panic!("{other:?}"),
            })
            .collect();
        let data = |txn, v: u32| MemMsg::ReadResp {
            txn: TxnId(txn),
            outcome: ReadOutcome::Data {
                data: v.to_le_bytes().to_vec(),
            },
        };
        assert_eq!(
            responses,
            [
                &done(1),
                &data(2, 0b01),
                &data(3, 0b11),
                &fault_for(&requests[3].1),
            ]
        );
        let levels: Vec<Traced> = trace
            .records
            .iter()
            .filter(|r| r.component == ids.irqc && r.kind == LEVEL_KIND)
            .map(|r| (LEVEL_KIND, r.fields.clone()))
            .collect();
        assert_eq!(
            levels,
            [
                level_trace(0, true),
                level_trace(0, false),
                level_trace(1, true),
                level_trace(1, false)
            ]
        );
    }
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let (requests, src0, src1) = timing_traffic();
    let reference = {
        let (mut rt, _) = build(&requests, &src0, &src1, Some(1), false);
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (events, rt.state_digest().unwrap(), rt.execution_digest())
    };
    for k in 0..=reference.0.len() {
        let (mut rt, _) = build(&requests, &src0, &src1, Some(1), false);
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let (mut fresh, _) = build(&requests, &src0, &src1, Some(1), false);
        fresh.restore(&bytes).unwrap();
        let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.1, reference.2),
            "checkpoint after {k} events"
        );
    }
}

/// Two sources rising in the same `(tick, Complete)`, dispatched in both orders, leave the
/// same controller state and send the same one rise.
#[test]
fn same_phase_levels_in_either_order_give_the_same_result() {
    let requests = vec![(0, write(1, IRQC_BASE + ENABLE, &0b11u32.to_le_bytes()))];
    let mut outcomes = Vec::new();
    for swap in [false, true] {
        let (mut rt, ids) = build(&requests, &[(5, true)], &[(5, true)], Some(1), swap);
        let last = Rc::new(RefCell::new(None));
        rt.add_observer(Box::new(Watch(ids.irqc, Rc::clone(&last))));
        rt.init().unwrap();
        let events: Vec<Dispatched> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
        let at_irqc: Vec<(u64, Phase, PortId)> = events
            .iter()
            .filter(|e| e.target == ids.irqc && level_of(e).is_some())
            .map(|e| match e.delivery {
                Delivered::Message { port, .. } => (e.key.tick.0, e.key.phase, port),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(at_irqc.len(), 2);
        assert_eq!((at_irqc[0].0, at_irqc[0].1), (at_irqc[1].0, at_irqc[1].1));
        let first = at_irqc[0].2;
        let sink: Vec<(u64, Option<bool>)> = events
            .iter()
            .filter(|e| e.target == ids.sink)
            .map(|e| (e.key.tick.0, level_of(e)))
            .collect();
        let view = last.borrow().clone().unwrap();
        outcomes.push((first, sink, view));
    }
    let (a, b) = (&outcomes[0], &outcomes[1]);
    assert_ne!(
        a.0, b.0,
        "the two runs dispatch the sources in opposite orders"
    );
    assert_eq!((&a.1, &a.2), (&b.1, &b.2));
    assert_eq!(a.1.len(), 1);
    assert_eq!(field(&a.2, "pending"), Value::U64(0b11));
    assert_eq!(field(&a.2, "out"), Value::Bool(true));
}
