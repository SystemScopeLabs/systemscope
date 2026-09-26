//! `ModeledKernel` driven directly through `MockCtx` (`docs/m3-design.md` §6.2, §6.3,
//! §6.8): the gate, the held entry and its single release, the Issue/Wait engine, every
//! session fault, inspect, trace, and the schema-1 snapshot — its round trip at every
//! step, that a restored engine never reissues, and every restore rejection, which
//! changes nothing.

mod common;

use common::layout::*;
use common::{Mem, MockCtx, Sent, Woke, oracle, restore_into, snapshot_of};
use proptest::prelude::*;
use systemscope_contracts::component::{Component, Delivered, PortId};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_os::kernel::{
    ENTER_KIND, GATE_PORT, GATE_SIZE, ISSUE, MEM_PORT, RELEASE_KIND, SHUTDOWN_KIND, SNAPSHOT_SCHEMA,
};
use systemscope_os::{KernelConfig, ModeledKernel, Window};

const CLOCK: ClockDomainId = ClockDomainId(3);
/// The downstream txn the bus gave the `ENTER` in these tests.
const HELD: TxnId = TxnId(77);

fn config() -> KernelConfig {
    common::layout::config(CLOCK)
}

fn kernel() -> ModeledKernel {
    ModeledKernel::new(config()).unwrap()
}

fn next_cycle() -> Woke {
    Woke {
        when: ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 1,
        },
        phase: Phase::Request,
        token: ISSUE,
    }
}

fn enter_msg(txn: TxnId, value: u32) -> MemMsg {
    MemMsg::WriteReq {
        txn,
        addr: 0,
        data: value.to_le_bytes().to_vec(),
    }
}

fn done(txn: TxnId) -> MemMsg {
    MemMsg::WriteResp {
        txn,
        outcome: WriteOutcome::Done,
    }
}

fn view(k: &ModeledKernel) -> Vec<(&'static str, Value)> {
    k.inspect().fields
}

fn field(k: &ModeledKernel, name: &str) -> Value {
    k.inspect().get(name).unwrap().clone()
}

fn str_field(k: &ModeledKernel, name: &str) -> String {
    match field(k, name) {
        Value::Str(s) => s,
        other => panic!("{other:?}"),
    }
}

fn u_field(k: &ModeledKernel, name: &str) -> u64 {
    match field(k, name) {
        Value::U64(v) => v,
        other => panic!("{other:?}"),
    }
}

/// Delivers `ENTER` of `value` with txn [`HELD`], requiring a hold and one wake.
fn enter(k: &mut ModeledKernel, ctx: &mut MockCtx, value: u32) {
    ctx.deliver(k, GATE_PORT, enter_msg(HELD, value), Phase::Request)
        .unwrap();
    assert_eq!(ctx.take_sent(), vec![], "ENTER is never answered at once");
    assert_eq!(ctx.take_woke(), vec![next_cycle()]);
    assert_eq!(k.held(), Some(HELD));
}

/// Wakes the engine, requiring exactly one request on `mem` with the expected txn.
fn issue(k: &mut ModeledKernel, ctx: &mut MockCtx, txn: u64) -> MemMsg {
    ctx.wake(k, ISSUE, Phase::Request).unwrap();
    let sent = ctx.take_sent();
    assert_eq!(sent.len(), 1, "one request per wake");
    assert_eq!(ctx.take_woke(), vec![]);
    let Sent {
        port,
        msg,
        when,
        phase,
    } = sent.into_iter().next().unwrap();
    assert_eq!(
        (port, when, phase),
        (MEM_PORT, ScheduleWhen::Now, Phase::Request)
    );
    let got = match &msg {
        MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => txn.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(got, txn);
    msg
}

/// Answers `req` from `mem`, returning the response.
fn answer(req: &MemMsg, mem: &mut Mem) -> MemMsg {
    match req {
        MemMsg::ReadReq { txn, addr, len } => MemMsg::ReadResp {
            txn: *txn,
            outcome: ReadOutcome::Data {
                data: (*addr..addr + u64::from(*len))
                    .map(|a| mem.get(&a).copied().unwrap_or(0))
                    .collect(),
            },
        },
        MemMsg::WriteReq { txn, addr, data } => {
            if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(addr) {
                for (a, b) in (*addr..).zip(data) {
                    mem.insert(a, *b);
                }
            }
            done(*txn)
        }
        other => panic!("{other:?}"),
    }
}

fn as_access(req: &MemMsg) -> oracle::Access {
    match req {
        MemMsg::ReadReq { addr, len, .. } => (false, *addr, vec![0; *len as usize]),
        MemMsg::WriteReq { addr, data, .. } => (true, *addr, data.clone()),
        other => panic!("{other:?}"),
    }
}

/// Runs a whole operation from `ENTER` to the release, starting at `mem` txn `first`,
/// and returns its accesses. Calls `each` with the kernel after every handler.
fn operation(
    k: &mut ModeledKernel,
    ctx: &mut MockCtx,
    value: u32,
    first: u64,
    mem: &mut Mem,
    mut each: impl FnMut(&ModeledKernel),
) -> Vec<oracle::Access> {
    enter(k, ctx, value);
    each(k);
    let mut accesses = Vec::new();
    for txn in first.. {
        let req = issue(k, ctx, txn);
        each(k);
        accesses.push(as_access(&req));
        let resp = answer(&req, mem);
        ctx.deliver(k, MEM_PORT, resp, Phase::Complete).unwrap();
        each(k);
        let sent = ctx.take_sent();
        let woke = ctx.take_woke();
        if sent.is_empty() {
            assert_eq!(woke, vec![next_cycle()]);
            assert_eq!(k.held(), Some(HELD));
        } else {
            assert_eq!(woke, vec![]);
            assert_eq!(
                sent,
                vec![Sent {
                    port: GATE_PORT,
                    msg: done(HELD),
                    when: ScheduleWhen::Now,
                    phase: Phase::Complete,
                }],
                "one release, for the held txn, in the last response's Complete"
            );
            assert_eq!(k.held(), None);
            return accesses;
        }
    }
    unreachable!()
}

fn probe_memory() -> Mem {
    let mut mem = Mem::new();
    for i in 0..0x98u64 {
        mem.insert(u64::from(TRAP_FRAME) + i, (i * 7) as u8);
    }
    mem
}

#[test]
fn an_enter_runs_the_scripted_operation_and_releases_once() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let mut mem = probe_memory();
    let mut want = mem.clone();
    let got = operation(&mut k, &mut ctx, TRAP_FRAME, 0, &mut mem, |_| {});
    assert_eq!(
        got,
        oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut want)
    );
    assert_eq!(mem, want);
    assert_eq!(u_field(&k, "next_txn"), 21);
    assert_eq!(str_field(&k, "phase"), "idle");
    // The trace: the entry, then the release, for the held txn.
    let kinds: Vec<&str> = ctx.traced.iter().map(|t| t.0).collect();
    assert_eq!(kinds, [ENTER_KIND, RELEASE_KIND]);
    assert_eq!(
        ctx.traced[0].1,
        vec![
            ("txn", Value::U64(HELD.0)),
            ("value", Value::U64(u64::from(TRAP_FRAME))),
            ("op", Value::Str("script".to_owned())),
        ]
    );
    assert_eq!(ctx.traced[1].1, vec![("txn", Value::U64(HELD.0))]);
    // A second entry continues the txn counter.
    let got = operation(&mut k, &mut ctx, TRAP_FRAME, 21, &mut mem, |_| {});
    assert_eq!(got.len(), 21);
}

#[test]
fn a_bad_value_runs_the_shutdown() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let mut mem = Mem::new();
    let mut want = Mem::new();
    let got = operation(&mut k, &mut ctx, 0xDEAD_BEEC, 0, &mut mem, |_| {});
    assert_eq!(got, oracle::shutdown(u64::from(TRAP_FRAME), &mut want));
    assert_eq!(mem, want);
    let kinds: Vec<&str> = ctx.traced.iter().map(|t| t.0).collect();
    assert_eq!(kinds, [ENTER_KIND, SHUTDOWN_KIND, RELEASE_KIND]);
    assert_eq!(ctx.traced[0].1[2].1, Value::Str("shutdown".to_owned()));
    assert_eq!(ctx.traced[1].1[0], ("reason", Value::U64(1)));
}

#[test]
fn inspect_shows_the_held_entry_and_the_outstanding_access() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let idle = view(&k);
    assert_eq!(
        idle,
        vec![
            ("phase", Value::Str("idle".to_owned())),
            ("op", Value::Str("none".to_owned())),
            ("step", Value::U64(0)),
            ("held_txn", Value::Str("none".to_owned())),
            ("mem_txn", Value::Str("none".to_owned())),
            ("next_txn", Value::U64(0)),
            ("pending_kind", Value::Str("none".to_owned())),
            ("pending_addr", Value::U64(0)),
            ("pending_len", Value::U64(0)),
        ]
    );
    enter(&mut k, &mut ctx, TRAP_FRAME);
    assert_eq!(str_field(&k, "phase"), "issue");
    assert_eq!(str_field(&k, "op"), "script");
    assert_eq!(str_field(&k, "held_txn"), "77");
    assert_eq!(str_field(&k, "mem_txn"), "none");
    assert_eq!(str_field(&k, "pending_kind"), "read");
    assert_eq!(u_field(&k, "pending_addr"), u64::from(TRAP_FRAME));
    assert_eq!(u_field(&k, "pending_len"), 16);
    issue(&mut k, &mut ctx, 0);
    assert_eq!(str_field(&k, "phase"), "wait");
    assert_eq!(str_field(&k, "mem_txn"), "0");
    assert_eq!(u_field(&k, "next_txn"), 1);
    assert_eq!(u_field(&k, "step"), 0);
}

#[test]
fn the_gate_refuses_everything_but_a_4_byte_enter_write() {
    let refused_write = |txn| MemMsg::WriteResp {
        txn,
        outcome: WriteOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    let refused_read = |txn| MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    let cases = [
        (
            MemMsg::WriteReq {
                txn: TxnId(1),
                addr: 0,
                data: vec![1],
            },
            refused_write(TxnId(1)),
        ),
        (
            MemMsg::WriteReq {
                txn: TxnId(2),
                addr: 0,
                data: vec![0; 8],
            },
            refused_write(TxnId(2)),
        ),
        (
            MemMsg::WriteReq {
                txn: TxnId(3),
                addr: 4,
                data: vec![0; 4],
            },
            refused_write(TxnId(3)),
        ),
        (
            MemMsg::WriteReq {
                txn: TxnId(4),
                addr: 2,
                data: vec![0; 4],
            },
            refused_write(TxnId(4)),
        ),
        (
            MemMsg::WriteReq {
                txn: TxnId(5),
                addr: u64::MAX,
                data: vec![0; 4],
            },
            refused_write(TxnId(5)),
        ),
        (
            MemMsg::ReadReq {
                txn: TxnId(6),
                addr: 0,
                len: 4,
            },
            refused_read(TxnId(6)),
        ),
        (
            MemMsg::ReadReq {
                txn: TxnId(7),
                addr: 4,
                len: 4,
            },
            refused_read(TxnId(7)),
        ),
    ];
    for held in [false, true] {
        for (req, resp) in cases.clone() {
            let (mut k, mut ctx) = (kernel(), MockCtx::new());
            if held {
                enter(&mut k, &mut ctx, TRAP_FRAME);
            }
            let before = snapshot_of(&k);
            ctx.deliver(&mut k, GATE_PORT, req.clone(), Phase::Request)
                .unwrap();
            assert_eq!(
                ctx.take_sent(),
                vec![Sent {
                    port: GATE_PORT,
                    msg: resp,
                    when: ScheduleWhen::Cycles {
                        domain: CLOCK,
                        k: 0
                    },
                    phase: Phase::Complete,
                }],
                "{req:?}"
            );
            assert_eq!(ctx.take_woke(), vec![]);
            assert_eq!(snapshot_of(&k), before, "a refused access changes nothing");
            assert!(ctx.traced.iter().all(|t| t.0 != ENTER_KIND) || held);
        }
    }
    // The latency is the configured one.
    let mut k = ModeledKernel::new(KernelConfig {
        latency: LinkLatency::Cycles {
            domain: CLOCK,
            k: 5,
        },
        ..config()
    })
    .unwrap();
    let mut ctx = MockCtx::new();
    ctx.deliver(&mut k, GATE_PORT, cases[0].0.clone(), Phase::Request)
        .unwrap();
    assert_eq!(
        ctx.take_sent()[0].when,
        ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 5
        }
    );
    assert_eq!(GATE_SIZE, 8);
}

fn fault_message(r: Result<(), SimError>) -> &'static str {
    match r {
        Err(SimError::ComponentFault(m)) => m,
        other => panic!("{other:?}"),
    }
}

#[test]
fn impossible_gate_messages_fault_the_session() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let empty_write = MemMsg::WriteReq {
        txn: TxnId(1),
        addr: 0,
        data: vec![],
    };
    let empty_read = MemMsg::ReadReq {
        txn: TxnId(1),
        addr: 0,
        len: 0,
    };
    for msg in [empty_write, empty_read, done(TxnId(1))] {
        let r = ctx.deliver(&mut k, GATE_PORT, msg, Phase::Request);
        fault_message(r);
    }
    assert_eq!(ctx.take_sent(), vec![]);
    // An unknown port, token, or protocol.
    let r = ctx.deliver(&mut k, PortId(2), done(TxnId(0)), Phase::Request);
    fault_message(r);
    let r = ctx.wake(&mut k, ISSUE + 1, Phase::Request);
    fault_message(r);
    let r = k.handle_event(
        &Delivered::Message {
            port: GATE_PORT,
            msg: Message::Irq(systemscope_contracts::protocol::irq_v0::IrqMsg::Level {
                asserted: true,
            }),
        },
        &mut ctx,
    );
    fault_message(r);
    assert_eq!(k.held(), None);
}

#[test]
fn a_second_enter_while_held_faults_the_session() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    enter(&mut k, &mut ctx, TRAP_FRAME);
    let before = snapshot_of(&k);
    let r = ctx.deliver(
        &mut k,
        GATE_PORT,
        enter_msg(TxnId(78), TRAP_FRAME),
        Phase::Request,
    );
    assert!(fault_message(r).contains("second ENTER"));
    assert_eq!(snapshot_of(&k), before);
    issue(&mut k, &mut ctx, 0);
    let before = snapshot_of(&k);
    let r = ctx.deliver(
        &mut k,
        GATE_PORT,
        enter_msg(TxnId(78), TRAP_FRAME),
        Phase::Request,
    );
    assert!(fault_message(r).contains("second ENTER"));
    assert_eq!(snapshot_of(&k), before);
    assert_eq!(ctx.take_sent(), vec![]);
}

#[test]
fn wakes_that_do_not_match_the_state_fault_the_session() {
    // Idle: nothing to issue.
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    fault_message(ctx.wake(&mut k, ISSUE, Phase::Request));
    // Wait: the access is outstanding and is never sent again.
    enter(&mut k, &mut ctx, TRAP_FRAME);
    issue(&mut k, &mut ctx, 0);
    let before = snapshot_of(&k);
    fault_message(ctx.wake(&mut k, ISSUE, Phase::Request));
    assert_eq!(ctx.take_sent(), vec![], "Wait never reissues");
    assert_eq!(snapshot_of(&k), before);
}

#[test]
fn bad_responses_on_mem_fault_the_session_and_change_nothing() {
    let read_resp = |txn, n| MemMsg::ReadResp {
        txn: TxnId(txn),
        outcome: ReadOutcome::Data { data: vec![0; n] },
    };
    // Nothing outstanding: Idle and Issue.
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    fault_message(ctx.deliver(&mut k, MEM_PORT, read_resp(0, 16), Phase::Complete));
    enter(&mut k, &mut ctx, TRAP_FRAME);
    fault_message(ctx.deliver(&mut k, MEM_PORT, read_resp(0, 16), Phase::Complete));
    issue(&mut k, &mut ctx, 0);
    let before = snapshot_of(&k);
    let cases = [
        (read_resp(1, 16), Phase::Complete, "not outstanding"),
        (read_resp(0, 16), Phase::Request, "outside COMPLETE"),
        (done(TxnId(0)), Phase::Complete, "kind mismatch"),
        (read_resp(0, 15), Phase::Complete, "wrong length"),
        (read_resp(0, 17), Phase::Complete, "wrong length"),
        (
            MemMsg::ReadResp {
                txn: TxnId(0),
                outcome: ReadOutcome::Fault {
                    fault: MemFault::AccessFault,
                },
            },
            Phase::Complete,
            "faulted",
        ),
        (
            MemMsg::ReadReq {
                txn: TxnId(0),
                addr: 0,
                len: 4,
            },
            Phase::Complete,
            "request on the mem port",
        ),
    ];
    for (msg, phase, why) in cases {
        let m = fault_message(ctx.deliver(&mut k, MEM_PORT, msg, phase));
        assert!(m.contains(why), "{m} / {why}");
        assert_eq!(snapshot_of(&k), before, "{why}");
        assert_eq!(ctx.take_sent(), vec![]);
        assert_eq!(ctx.take_woke(), vec![]);
    }
    // A faulted write, at the first write.
    for _ in 0..10 {
        let req = issue_or_answer(&mut k, &mut ctx);
        let _ = req;
    }
    let before = snapshot_of(&k);
    let m = fault_message(ctx.deliver(
        &mut k,
        MEM_PORT,
        MemMsg::WriteResp {
            txn: TxnId(10),
            outcome: WriteOutcome::Fault {
                fault: MemFault::AccessFault,
            },
        },
        Phase::Complete,
    ));
    assert!(m.contains("faulted"));
    assert_eq!(snapshot_of(&k), before);
    assert_eq!(k.held(), Some(HELD), "a faulted session never releases");
}

/// Answers the outstanding read with zeros and issues the next access.
fn issue_or_answer(k: &mut ModeledKernel, ctx: &mut MockCtx) -> MemMsg {
    let txn = u_field(k, "next_txn") - 1;
    let len = u_field(k, "pending_len") as usize;
    ctx.deliver(
        k,
        MEM_PORT,
        MemMsg::ReadResp {
            txn: TxnId(txn),
            outcome: ReadOutcome::Data { data: vec![0; len] },
        },
        Phase::Complete,
    )
    .unwrap();
    assert_eq!(ctx.take_woke(), vec![next_cycle()]);
    issue(k, ctx, txn + 1)
}

#[test]
fn the_whitelist_is_checked_before_sending() {
    // A platform built wrong: UART TX is in kgate. The frame accesses go out; the UART
    // byte is refused before anything is sent.
    let mut k = ModeledKernel::new(KernelConfig {
        uart_tx: KGATE_BASE,
        ..config()
    })
    .unwrap();
    let mut ctx = MockCtx::new();
    let mut mem = probe_memory();
    enter(&mut k, &mut ctx, TRAP_FRAME);
    for txn in 0..20 {
        let req = issue(&mut k, &mut ctx, txn);
        let resp = answer(&req, &mut mem);
        ctx.deliver(&mut k, MEM_PORT, resp, Phase::Complete)
            .unwrap();
        ctx.take_woke();
    }
    let before = snapshot_of(&k);
    let m = fault_message(ctx.wake(&mut k, ISSUE, Phase::Request));
    assert!(m.contains("whitelist"));
    assert_eq!(ctx.take_sent(), vec![], "nothing reaches kgate");
    assert_eq!(snapshot_of(&k), before, "no txn consumed");
    // kgate over the trap frame: the very first access is refused.
    let mut k = ModeledKernel::new(KernelConfig {
        gate: Window {
            base: u64::from(TRAP_FRAME) + 0x40,
            size: 8,
        },
        ..config()
    })
    .unwrap();
    let mut ctx = MockCtx::new();
    enter(&mut k, &mut ctx, TRAP_FRAME);
    // The first chunk (frame + 0..16) does not touch it; walk to the one that does.
    for txn in 0..4 {
        let req = issue(&mut k, &mut ctx, txn);
        let resp = answer(&req, &mut Mem::new());
        ctx.deliver(&mut k, MEM_PORT, resp, Phase::Complete)
            .unwrap();
        ctx.take_woke();
    }
    fault_message(ctx.wake(&mut k, ISSUE, Phase::Request));
    assert_eq!(ctx.take_sent(), vec![]);
}

// --- Snapshot ---

#[test]
fn the_snapshot_round_trips_at_every_step_and_resumes_by_waiting() {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let mut mem = probe_memory();
    let mut points = vec![snapshot_of(&k)];
    operation(&mut k, &mut ctx, TRAP_FRAME, 0, &mut mem, |k| {
        points.push(snapshot_of(k));
    });
    assert_eq!(points.len(), 1 + 1 + 2 * 21);
    for (i, bytes) in points.iter().enumerate() {
        let mut fresh = kernel();
        restore_into(&mut fresh, bytes).unwrap();
        assert_eq!(&snapshot_of(&fresh), bytes, "point {i}");
        assert_eq!(view(&fresh), {
            let mut again = kernel();
            restore_into(&mut again, bytes).unwrap();
            view(&again)
        });
        // A restore schedules and sends nothing: resumption is by what is queued.
        let mut ctx = MockCtx::new();
        match str_field(&fresh, "phase").as_str() {
            "wait" => {
                // Waiting: an ISSUE wake would be a reissue, and faults.
                let mut probe = kernel();
                restore_into(&mut probe, bytes).unwrap();
                fault_message(ctx.wake(&mut probe, ISSUE, Phase::Request));
                assert_eq!(ctx.take_sent(), vec![]);
            }
            "issue" => {
                // Issuing: the wake sends the same request as the original did.
                let txn = u_field(&fresh, "next_txn");
                issue(&mut fresh, &mut ctx, txn);
            }
            _ => {}
        }
    }
}

#[test]
fn a_restored_wait_takes_the_response_already_in_flight() {
    // Original: enter, issue txn 0, answer, and so on. Checkpoint in Wait at each step,
    // restore, deliver the same response: the rest is identical and nothing is resent.
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    let mut mem = probe_memory();
    enter(&mut k, &mut ctx, TRAP_FRAME);
    let mut txn = 0;
    loop {
        let req = issue(&mut k, &mut ctx, txn);
        let checkpoint = snapshot_of(&k);
        let resp = answer(&req, &mut mem.clone());
        let mut restored = kernel();
        restore_into(&mut restored, &checkpoint).unwrap();
        let mut rctx = MockCtx::new();
        rctx.deliver(&mut restored, MEM_PORT, resp.clone(), Phase::Complete)
            .unwrap();
        answer(&req, &mut mem);
        ctx.deliver(&mut k, MEM_PORT, resp, Phase::Complete)
            .unwrap();
        assert_eq!(rctx.take_sent(), ctx.sent.clone());
        assert_eq!(rctx.take_woke(), ctx.woke.clone());
        assert_eq!(snapshot_of(&restored), snapshot_of(&k));
        let released = !ctx.take_sent().is_empty();
        ctx.take_woke();
        if released {
            break;
        }
        txn += 1;
    }
    assert_eq!(txn, 20);
}

/// A schema-1 snapshot written from the design text, independently of the component.
#[derive(Clone, Debug)]
struct Raw {
    config: KernelConfig,
    state: u8,
    wait_txn: u64,
    op: u8,
    step: u32,
    data: Vec<u8>,
    held: Option<u64>,
    held_tag: u8,
    next_txn: u64,
}

impl Raw {
    fn idle() -> Raw {
        Raw {
            config: config(),
            state: 0,
            wait_txn: 0,
            op: 0,
            step: 0,
            data: vec![],
            held: None,
            held_tag: 0,
            next_txn: 0,
        }
    }

    /// Waiting on the third frame read (txn 7 after seven earlier accesses).
    fn waiting() -> Raw {
        Raw {
            state: 2,
            wait_txn: 7,
            op: 0,
            step: 2,
            data: vec![0xAB; 32],
            held: Some(HELD.0),
            held_tag: 1,
            next_txn: 8,
            ..Raw::idle()
        }
    }

    fn encode(&self) -> Vec<u8> {
        let c = &self.config;
        let mut w = SnapshotWriter::new();
        w.u32(c.clock.0);
        match c.latency {
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
        for win in [c.ram, c.gate] {
            w.u64(win.base);
            w.u64(win.size);
        }
        w.u32(c.trap_frame);
        for win in [c.staging, c.frame_pool, c.blk] {
            w.u64(win.base);
            w.u64(win.size);
        }
        w.u64(c.uart_tx);
        w.u8(self.state);
        if self.state == 2 {
            w.u64(self.wait_txn);
        }
        if self.state != 0 {
            w.u8(self.op);
            w.u32(self.step);
            w.bytes(&self.data);
        }
        w.u8(self.held_tag);
        if let Some(t) = self.held {
            w.u64(t);
        }
        w.u64(self.next_txn);
        w.into_bytes()
    }
}

#[test]
fn hand_written_snapshots_restore() {
    assert_eq!(kernel().snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(snapshot_of(&kernel()), Raw::idle().encode());
    let mut k = kernel();
    restore_into(&mut k, &Raw::waiting().encode()).unwrap();
    assert_eq!(str_field(&k, "phase"), "wait");
    assert_eq!(str_field(&k, "mem_txn"), "7");
    assert_eq!(k.held(), Some(HELD));
    assert_eq!(snapshot_of(&k), Raw::waiting().encode());
    // Issue at the UART step, nothing outstanding.
    let issuing = Raw {
        state: 1,
        step: 20,
        data: vec![],
        next_txn: 20,
        ..Raw::waiting()
    };
    restore_into(&mut k, &issuing.encode()).unwrap();
    assert_eq!(str_field(&k, "phase"), "issue");
    assert_eq!(str_field(&k, "pending_kind"), "write");
    assert_eq!(u_field(&k, "pending_addr"), UART_BASE);
    assert_eq!(u_field(&k, "pending_len"), 1);
}

/// Restores `bytes` into a kernel in a non-trivial state and requires a rejection that
/// changes nothing.
fn assert_rejected(bytes: &[u8], why: &str) {
    let (mut k, mut ctx) = (kernel(), MockCtx::new());
    enter(&mut k, &mut ctx, TRAP_FRAME);
    issue(&mut k, &mut ctx, 0);
    let before = snapshot_of(&k);
    let err = restore_into(&mut k, bytes);
    assert!(err.is_err(), "{why} restored");
    assert_eq!(
        snapshot_of(&k),
        before,
        "{why}: a failed restore changes nothing"
    );
}

#[test]
fn restore_rejects_every_impossible_state() {
    let bad = |f: &dyn Fn(&mut Raw)| {
        let mut r = Raw::waiting();
        f(&mut r);
        r.encode()
    };
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (bad(&|r| r.config.uart_tx += 1), "a different config"),
        (
            bad(&|r| r.config.clock = ClockDomainId(9)),
            "a different clock",
        ),
        (bad(&|r| r.state = 3), "an unknown state tag"),
        (bad(&|r| r.op = 2), "an unknown op tag"),
        (bad(&|r| r.held_tag = 2), "an unknown held tag"),
        (
            bad(&|r| {
                r.state = 0;
                r.held = Some(HELD.0);
                r.held_tag = 1;
            }),
            "Idle with a held ENTER",
        ),
        (
            bad(&|r| {
                r.held = None;
                r.held_tag = 0;
            }),
            "an operation without a held ENTER",
        ),
        (bad(&|r| r.step = 21), "a step past the last access"),
        (bad(&|r| r.data.push(0)), "working data of the wrong length"),
        (
            bad(&|r| {
                r.op = 1;
                r.step = 1;
                r.data = vec![];
            }),
            "a shutdown step past its last",
        ),
        (bad(&|r| r.wait_txn = 6), "a Wait txn older than the latest"),
        (bad(&|r| r.wait_txn = 8), "a Wait txn not yet issued"),
        (
            bad(&|r| {
                r.wait_txn = 0;
                r.next_txn = 0;
            }),
            "a Wait with nothing issued",
        ),
    ];
    for (bytes, why) in &cases {
        assert_rejected(bytes, why);
    }
    // Truncated and trailing.
    let good = Raw::waiting().encode();
    for n in 0..good.len() {
        assert_rejected(&good[..n], "truncated");
    }
    // Trailing bytes are the reader's `finish` to reject, after the component has read
    // its state; the runtime faults the session on any restore failure.
    let mut long = good.clone();
    long.push(0);
    assert!(restore_into(&mut kernel(), &long).is_err());
    // A pending access the whitelist refuses: only a kernel whose config refuses it.
    let wrong = KernelConfig {
        uart_tx: KGATE_BASE,
        ..config()
    };
    let raw = Raw {
        config: wrong,
        state: 1,
        step: 20,
        data: vec![],
        next_txn: 20,
        ..Raw::waiting()
    };
    let mut k = ModeledKernel::new(wrong).unwrap();
    let before = snapshot_of(&k);
    let err = restore_into(&mut k, &raw.encode()).unwrap_err();
    assert!(matches!(err, RestoreError::InvalidState(m) if m.contains("whitelist")));
    assert_eq!(snapshot_of(&k), before);
}

/// The component's own `restore` of `bytes`: `Ok(true)` if it read exactly all of them,
/// `Ok(false)` if it accepted a prefix (the reader's `finish` then rejects the rest).
fn own_restore(k: &mut ModeledKernel, bytes: &[u8]) -> Result<bool, RestoreError> {
    let mut r = systemscope_contracts::snapshot::SnapshotReader::new(bytes);
    k.restore(&mut r, SNAPSHOT_SCHEMA)?;
    Ok(r.finish().is_ok())
}

proptest! {
    /// Arbitrary bytes never panic, and a rejection changes nothing.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let (mut k, mut ctx) = (kernel(), MockCtx::new());
        enter(&mut k, &mut ctx, TRAP_FRAME);
        let before = snapshot_of(&k);
        if own_restore(&mut k, &bytes).is_err() {
            prop_assert_eq!(snapshot_of(&k), before);
        }
    }

    /// Mutating one byte of a valid snapshot either is rejected, changing nothing, or
    /// restores a state that snapshots back to exactly those bytes.
    #[test]
    fn mutated_snapshots_restore_exactly_or_not_at_all(
        which in 0usize..4,
        at in any::<prop::sample::Index>(),
        xor in 1u8..=255,
    ) {
        let raws = [
            Raw::idle(),
            Raw::waiting(),
            Raw { state: 1, step: 20, data: vec![], next_txn: 20, ..Raw::waiting() },
            Raw { op: 1, step: 0, data: vec![], state: 2, wait_txn: 0, next_txn: 1, ..Raw::waiting() },
        ];
        let mut bytes = raws[which].encode();
        let i = at.index(bytes.len());
        bytes[i] ^= xor;
        let (mut k, mut ctx) = (kernel(), MockCtx::new());
        enter(&mut k, &mut ctx, TRAP_FRAME);
        let before = snapshot_of(&k);
        match own_restore(&mut k, &bytes) {
            Ok(true) => prop_assert_eq!(snapshot_of(&k), bytes),
            Ok(false) => {}
            Err(_) => prop_assert_eq!(snapshot_of(&k), before),
        }
    }

    /// For any frame contents and any held txn, the component makes the oracle's
    /// accesses with consecutive txns and releases the held txn exactly once.
    #[test]
    fn the_component_matches_the_oracle(
        bytes in proptest::collection::vec(any::<u8>(), 0x98),
        first in 0u64..1 << 40,
        held in any::<u64>(),
    ) {
        let mut k = kernel();
        let mut ctx = MockCtx::new();
        let raw = Raw { next_txn: first, ..Raw::idle() };
        restore_into(&mut k, &raw.encode()).unwrap();
        let mut mem = Mem::new();
        for (i, b) in bytes.iter().enumerate() {
            mem.insert(u64::from(TRAP_FRAME) + i as u64, *b);
        }
        let mut want = mem.clone();
        ctx.deliver(&mut k, GATE_PORT, enter_msg(TxnId(held), TRAP_FRAME), Phase::Request).unwrap();
        ctx.take_woke();
        let mut accesses = Vec::new();
        let mut releases = Vec::new();
        for txn in first.. {
            let req = issue(&mut k, &mut ctx, txn);
            accesses.push(as_access(&req));
            let resp = answer(&req, &mut mem);
            ctx.deliver(&mut k, MEM_PORT, resp, Phase::Complete).unwrap();
            ctx.take_woke();
            releases.extend(ctx.take_sent());
            if !releases.is_empty() {
                break;
            }
        }
        prop_assert_eq!(accesses, oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut want));
        prop_assert_eq!(mem, want);
        prop_assert_eq!(releases.len(), 1);
        prop_assert_eq!(&releases[0].msg, &done(TxnId(held)));
        prop_assert_eq!(u_field(&k, "next_txn"), first + 21);
    }

    /// A checkpoint at any handler boundary of an operation, restored into a fresh
    /// kernel, continues with exactly the sends, wakes, and final state of the
    /// uninterrupted run: nothing reissued, nothing lost.
    #[test]
    fn a_checkpoint_anywhere_continues_identically(
        cut in 0usize..43,
        bytes in proptest::collection::vec(any::<u8>(), 0x98),
    ) {
        let mut mem = Mem::new();
        for (i, b) in bytes.iter().enumerate() {
            mem.insert(u64::from(TRAP_FRAME) + i as u64, *b);
        }
        // Handlers as a script: ENTER, then (wake, response) × 21.
        // The request in flight is the network's, not the kernel's: it crosses the cut
        // in `wire`, as the runtime's queue would carry it.
        let run = |k: &mut ModeledKernel, from: usize, to: usize, mem: &mut Mem, wire: &mut Option<MemMsg>| {
            let mut ctx = MockCtx::new();
            let mut log = Vec::new();
            for h in from..to {
                if h == 0 {
                    ctx.deliver(k, GATE_PORT, enter_msg(HELD, TRAP_FRAME), Phase::Request).unwrap();
                } else if h % 2 == 1 {
                    ctx.wake(k, ISSUE, Phase::Request).unwrap();
                } else {
                    let resp = answer(&wire.take().unwrap(), mem);
                    ctx.deliver(k, MEM_PORT, resp, Phase::Complete).unwrap();
                }
                let sent = ctx.take_sent();
                if let Some(s) = sent.iter().find(|s| s.port == MEM_PORT) {
                    *wire = Some(s.msg.clone());
                }
                log.extend(sent);
                let _ = ctx.take_woke();
            }
            log
        };
        let mut whole = kernel();
        let mut m1 = mem.clone();
        let all = run(&mut whole, 0, 43, &mut m1, &mut None);
        let mut first = kernel();
        let mut m2 = mem.clone();
        let mut wire = None;
        let mut got = run(&mut first, 0, cut, &mut m2, &mut wire);
        let mut resumed = kernel();
        restore_into(&mut resumed, &snapshot_of(&first)).unwrap();
        got.extend(run(&mut resumed, cut, 43, &mut m2, &mut wire));
        prop_assert_eq!(got, all);
        prop_assert_eq!(snapshot_of(&resumed), snapshot_of(&whole));
        prop_assert_eq!(m2, m1);
    }

    /// Refused gate accesses interleaved anywhere in an operation are each answered once
    /// with a fault and disturb nothing: the operation's accesses and its single release
    /// are unchanged.
    #[test]
    fn refused_gate_accesses_never_disturb_the_held_entry(
        strays in proptest::collection::vec((0usize..43, 1u64..8, any::<bool>()), 0..8),
    ) {
        let mut k = kernel();
        let mut ctx = MockCtx::new();
        let mut mem = probe_memory();
        let mut faults = 0;
        let mut releases = Vec::new();
        let mut accesses = Vec::new();
        let mut last = None;
        for h in 0..43 {
            for &(at, offset, read) in &strays {
                if at != h {
                    continue;
                }
                let req = if read {
                    MemMsg::ReadReq { txn: TxnId(1000 + offset), addr: offset % 8, len: 4 }
                } else {
                    MemMsg::WriteReq { txn: TxnId(1000 + offset), addr: offset, data: vec![0; 4] }
                };
                ctx.deliver(&mut k, GATE_PORT, req, Phase::Request).unwrap();
                let sent = ctx.take_sent();
                prop_assert_eq!(sent.len(), 1);
                prop_assert_eq!(sent[0].port, GATE_PORT);
                let refused = matches!(
                    sent[0].msg,
                    MemMsg::ReadResp { outcome: ReadOutcome::Fault { .. }, .. }
                        | MemMsg::WriteResp { outcome: WriteOutcome::Fault { .. }, .. }
                );
                prop_assert!(refused);
                faults += 1;
            }
            if h == 0 {
                ctx.deliver(&mut k, GATE_PORT, enter_msg(HELD, TRAP_FRAME), Phase::Request).unwrap();
            } else if h % 2 == 1 {
                ctx.wake(&mut k, ISSUE, Phase::Request).unwrap();
                let sent = ctx.take_sent();
                prop_assert_eq!(sent.len(), 1);
                accesses.push(as_access(&sent[0].msg));
                last = Some(sent[0].msg.clone());
            } else {
                let resp = answer(last.as_ref().unwrap(), &mut mem);
                ctx.deliver(&mut k, MEM_PORT, resp, Phase::Complete).unwrap();
                releases.extend(ctx.take_sent());
            }
            let _ = ctx.take_woke();
        }
        prop_assert_eq!(faults, strays.len());
        let mut want = probe_memory();
        prop_assert_eq!(accesses, oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut want));
        prop_assert_eq!(releases.len(), 1);
        prop_assert_eq!(&releases[0].msg, &done(HELD));
        prop_assert_eq!(k.held(), None);
    }
}
