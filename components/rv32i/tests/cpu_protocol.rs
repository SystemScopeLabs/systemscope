//! `Rv32iCpu` driven directly (`docs/m1-design.md` §5.3, §5.6): transaction correlation,
//! protocol violations that must fault the session rather than trap, and snapshot
//! restore validation.
//!
//! A real bus and RAM never send most of these responses, so a mock context delivers
//! them by hand.

mod common;

use std::num::NonZeroU64;

use common::MockCtx;
use common::asm::*;
use systemscope_contracts::component::{Component, Delivered, PortId};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_rv32i::cpu::{COMMIT, FETCH, MEMORY, SNAPSHOT_SCHEMA, TRAP_KIND};
use systemscope_rv32i::{CpuConfigError, Rv32iConfig, Rv32iCpu};

const ENTRY: u32 = 0x8000_0000;
const CLOCK: ClockDomainId = ClockDomainId(0);
const LIMIT: u64 = 100;

fn config() -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(LIMIT).unwrap(),
    }
}

/// A CPU after `init`, with the first fetch's wake taken.
fn start() -> (Rv32iCpu, MockCtx) {
    let mut cpu = Rv32iCpu::new(config()).unwrap();
    let mut ctx = MockCtx::new();
    cpu.init(&mut ctx).unwrap();
    let wake = ctx.take_wake();
    assert_eq!(wake.token, FETCH);
    assert_eq!(wake.phase, Phase::Request);
    assert_eq!(
        wake.when,
        ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 0
        }
    );
    (cpu, ctx)
}

fn data(txn: TxnId, bytes: &[u8]) -> MemMsg {
    MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Data {
            data: bytes.to_vec(),
        },
    }
}

fn word(txn: TxnId, w: u32) -> MemMsg {
    data(txn, &w.to_le_bytes())
}

fn done(txn: TxnId) -> MemMsg {
    MemMsg::WriteResp {
        txn,
        outcome: WriteOutcome::Done,
    }
}

fn read_fault(txn: TxnId) -> MemMsg {
    MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    }
}

fn txn_of(msg: &MemMsg) -> TxnId {
    match msg {
        MemMsg::ReadReq { txn, .. }
        | MemMsg::WriteReq { txn, .. }
        | MemMsg::ReadResp { txn, .. }
        | MemMsg::WriteResp { txn, .. } => *txn,
    }
}

/// Sends the pending fetch and returns its txn.
fn fetch(cpu: &mut Rv32iCpu, ctx: &mut MockCtx) -> TxnId {
    ctx.wake(cpu, FETCH, Phase::Request).unwrap();
    let msg = ctx.take_sent();
    let MemMsg::ReadReq { txn, addr, len: 4 } = msg else {
        panic!("not a fetch: {msg:?}");
    };
    assert_eq!(addr, u64::from(cpu.pc()));
    txn
}

/// Fetches `insn` and delivers it; returns the wake it scheduled.
fn fetch_word(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> u64 {
    let txn = fetch(cpu, ctx);
    ctx.respond(cpu, word(txn, insn)).unwrap();
    ctx.take_wake().token
}

/// Runs `insn`, which must not touch memory, to its commit.
fn retire(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) {
    assert_eq!(fetch_word(cpu, ctx, insn), COMMIT);
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    let next = ctx.take_wake();
    assert_eq!(next.token, FETCH);
    assert_eq!(
        next.when,
        ScheduleWhen::Cycles {
            domain: CLOCK,
            k: 1
        }
    );
    ctx.traced.clear();
}

/// Fetches the memory instruction `insn` and sends its data request.
fn issue(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> MemMsg {
    assert_eq!(fetch_word(cpu, ctx, insn), MEMORY);
    ctx.wake(cpu, MEMORY, Phase::Request).unwrap();
    ctx.take_sent()
}

/// A CPU with x10 = `ENTRY + 0x1000` and x1 = 0x1234, about to fetch.
fn ready() -> (Rv32iCpu, MockCtx) {
    let (mut cpu, mut ctx) = start();
    retire(&mut cpu, &mut ctx, lui(10, (ENTRY + 0x1000) >> 12));
    retire(&mut cpu, &mut ctx, addi(1, 0, 0x234));
    retire(&mut cpu, &mut ctx, addi(1, 1, 0x7ff));
    retire(&mut cpu, &mut ctx, addi(1, 1, 0x7ff));
    retire(&mut cpu, &mut ctx, addi(1, 1, 2));
    assert_eq!(
        cpu.registers()
            .read(systemscope_rv32i::Reg::new(1).unwrap()),
        0x1234
    );
    (cpu, ctx)
}

/// Asserts `result` is a session fault that left the CPU exactly as it was.
fn assert_fault(cpu: &Rv32iCpu, ctx: &MockCtx, before: &StateView, result: Result<(), SimError>) {
    assert!(
        matches!(result, Err(SimError::ComponentFault(_))),
        "expected a component fault: {result:?}"
    );
    assert_eq!(&cpu.inspect(), before, "a fault changes nothing");
    assert!(ctx.sent.is_empty() && ctx.woke.is_empty() && ctx.traced.is_empty());
    assert_eq!(cpu.halt(), None, "a protocol fault is not a trap");
}

// ---------------------------------------------------------------------------------------
// Correlation.

#[test]
fn a_response_for_another_txn_faults() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    for wrong in [TxnId(txn.0 + 1), TxnId(u64::MAX)] {
        let result = ctx.respond(&mut cpu, word(wrong, addi(1, 0, 1)));
        assert_fault(&cpu, &ctx, &before, result);
    }
    // The right one is still accepted.
    ctx.respond(&mut cpu, word(txn, addi(1, 0, 1))).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
}

#[test]
fn a_stale_response_faults() {
    let (mut cpu, mut ctx) = start();
    let first = fetch(&mut cpu, &mut ctx);
    ctx.respond(&mut cpu, word(first, addi(1, 0, 1))).unwrap();
    ctx.take_wake();
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    ctx.take_wake();
    ctx.traced.clear();
    let second = fetch(&mut cpu, &mut ctx);
    assert_ne!(first, second);
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, word(first, addi(2, 0, 2)));
    assert_fault(&cpu, &ctx, &before, result);
}

#[test]
fn a_stale_data_response_faults() {
    let (mut cpu, mut ctx) = ready();
    let store = issue(&mut cpu, &mut ctx, sw(1, 10, 0));
    ctx.respond(&mut cpu, done(txn_of(&store))).unwrap();
    ctx.take_wake();
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    ctx.take_wake();
    ctx.traced.clear();
    let load = issue(&mut cpu, &mut ctx, lw(2, 10, 0));
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, data(txn_of(&store), &[0; 4]));
    assert_fault(&cpu, &ctx, &before, result);
    ctx.respond(&mut cpu, data(txn_of(&load), &[1, 0, 0, 0]))
        .unwrap();
}

/// A duplicate response faults, both before and after the instruction commits, and the
/// instruction retires once.
#[test]
fn a_duplicate_response_never_commits_twice() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    ctx.respond(&mut cpu, word(txn, addi(1, 0, 1))).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, word(txn, addi(1, 0, 1)));
    assert_fault(&cpu, &ctx, &before, result);
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    ctx.take_wake();
    ctx.traced.clear();
    assert_eq!(cpu.instret(), 1);
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, word(txn, addi(1, 0, 1)));
    assert_fault(&cpu, &ctx, &before, result);
    // A second commit wake is not accepted either.
    let result = ctx.wake(&mut cpu, COMMIT, Phase::Commit);
    assert_fault(&cpu, &ctx, &before, result);
    assert_eq!(cpu.instret(), 1);
}

#[test]
fn a_duplicate_store_response_never_commits_twice() {
    let (mut cpu, mut ctx) = ready();
    let store = issue(&mut cpu, &mut ctx, sw(1, 10, 0));
    ctx.respond(&mut cpu, done(txn_of(&store))).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, done(txn_of(&store)));
    assert_fault(&cpu, &ctx, &before, result);
}

// ---------------------------------------------------------------------------------------
// Protocol violations.

#[test]
fn a_read_response_to_a_store_faults() {
    let (mut cpu, mut ctx) = ready();
    let store = issue(&mut cpu, &mut ctx, sw(1, 10, 0));
    assert!(matches!(store, MemMsg::WriteReq { .. }));
    let before = cpu.inspect();
    for response in [data(txn_of(&store), &[0; 4]), read_fault(txn_of(&store))] {
        let result = ctx.respond(&mut cpu, response);
        assert_fault(&cpu, &ctx, &before, result);
    }
}

#[test]
fn a_write_response_to_a_load_faults() {
    let (mut cpu, mut ctx) = ready();
    let load = issue(&mut cpu, &mut ctx, lw(2, 10, 0));
    assert!(matches!(load, MemMsg::ReadReq { len: 4, .. }));
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, done(txn_of(&load)));
    assert_fault(&cpu, &ctx, &before, result);
}

#[test]
fn a_write_response_to_a_fetch_faults() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, done(txn));
    assert_fault(&cpu, &ctx, &before, result);
}

#[test]
fn a_fetch_of_the_wrong_length_faults() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    for len in [0, 1, 3, 5, 8] {
        let result = ctx.respond(&mut cpu, data(txn, &vec![0x13; len]));
        assert_fault(&cpu, &ctx, &before, result);
    }
}

/// A load answered with the wrong number of bytes is a model bug, not an access fault.
#[test]
fn load_data_of_the_wrong_length_faults() {
    for (insn, len) in [
        (lw(2, 10, 0), 2),
        (lw(2, 10, 0), 8),
        (lh(2, 10, 0), 1),
        (lbu(2, 10, 0), 0),
    ] {
        let (mut cpu, mut ctx) = ready();
        let load = issue(&mut cpu, &mut ctx, insn);
        let before = cpu.inspect();
        let result = ctx.respond(&mut cpu, data(txn_of(&load), &vec![0xaa; len]));
        assert_fault(&cpu, &ctx, &before, result);
    }
}

#[test]
fn a_response_with_nothing_outstanding_faults() {
    // Before the first fetch.
    let (mut cpu, mut ctx) = start();
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, word(TxnId(0), addi(1, 0, 1)));
    assert_fault(&cpu, &ctx, &before, result);

    // While a load waits to be sent.
    let (mut cpu, mut ctx) = ready();
    assert_eq!(fetch_word(&mut cpu, &mut ctx, lw(2, 10, 0)), MEMORY);
    let before = cpu.inspect();
    let next = TxnId(5 + 1);
    for response in [data(next, &[0; 4]), done(next)] {
        let result = ctx.respond(&mut cpu, response);
        assert_fault(&cpu, &ctx, &before, result);
    }

    // After halting.
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    ctx.respond(&mut cpu, word(txn, EBREAK)).unwrap();
    ctx.take_wake();
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    assert!(ctx.woke.is_empty(), "a halted CPU schedules nothing");
    ctx.traced.clear();
    let before = cpu.inspect();
    let result = ctx.respond(&mut cpu, word(txn, EBREAK));
    let halted = cpu.halt();
    assert!(matches!(result, Err(SimError::ComponentFault(_))));
    assert_eq!(cpu.inspect(), before);
    assert_eq!(cpu.halt(), halted);
}

#[test]
fn a_request_on_the_initiator_port_faults() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    let requests = [
        MemMsg::ReadReq {
            txn,
            addr: 0,
            len: 4,
        },
        MemMsg::WriteReq {
            txn,
            addr: 0,
            data: vec![1],
        },
    ];
    for request in requests {
        let result = ctx.respond(&mut cpu, request);
        assert_fault(&cpu, &ctx, &before, result);
    }
}

#[test]
fn a_response_outside_complete_faults() {
    let (mut cpu, mut ctx) = start();
    let txn = fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    for phase in [
        Phase::Request,
        Phase::Transfer,
        Phase::Commit,
        Phase::Observe,
    ] {
        ctx.phase = phase;
        let ev = Delivered::Message {
            port: PortId(0),
            msg: word(txn, addi(1, 0, 1)).into(),
        };
        let result = cpu.handle_event(&ev, &mut ctx);
        assert_fault(&cpu, &ctx, &before, result);
    }
}

#[test]
fn a_wake_that_does_not_match_the_state_faults() {
    let (mut cpu, mut ctx) = start();
    let before = cpu.inspect();
    for token in [MEMORY, COMMIT, 3, u64::MAX] {
        let result = ctx.wake(&mut cpu, token, Phase::Request);
        assert_fault(&cpu, &ctx, &before, result);
    }
    fetch(&mut cpu, &mut ctx);
    let before = cpu.inspect();
    for token in [FETCH, MEMORY, COMMIT] {
        let result = ctx.wake(&mut cpu, token, Phase::Commit);
        assert_fault(&cpu, &ctx, &before, result);
    }
}

// ---------------------------------------------------------------------------------------
// Architectural outcomes of memory responses.

/// A memory `Fault` is architectural: it traps with the access's address.
#[test]
fn memory_faults_trap_rather_than_fault_the_session() {
    let (mut cpu, mut ctx) = ready();
    let load = issue(&mut cpu, &mut ctx, lw(2, 10, 4));
    ctx.respond(&mut cpu, read_fault(txn_of(&load))).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    // Nothing changes until the commit, and then only the halt.
    let before = cpu.inspect();
    assert_eq!(
        before.get("state"),
        Some(&Value::Str("commit_pending".to_owned()))
    );
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    assert!(ctx.woke.is_empty());
    assert_eq!(ctx.traced.len(), 1);
    assert_eq!(ctx.traced[0].0, TRAP_KIND);
    assert_eq!(cpu.instret(), 5);
    assert_eq!(
        cpu.registers()
            .read(systemscope_rv32i::Reg::new(2).unwrap()),
        0
    );
    assert!(matches!(
        cpu.halt(),
        Some(systemscope_rv32i::Halt::Trap(t)) if t.tval == ENTRY + 0x1004 && t.pc == ENTRY + 20
    ));
}

/// Neither `pc`, the registers, nor `instret` move before the commit.
#[test]
fn a_load_changes_nothing_before_its_commit() {
    let (mut cpu, mut ctx) = ready();
    let arch = |cpu: &Rv32iCpu| (cpu.pc(), cpu.registers().clone(), cpu.instret());
    let before = arch(&cpu);
    let load = issue(&mut cpu, &mut ctx, lw(2, 10, 0));
    assert_eq!(arch(&cpu), before);
    ctx.respond(&mut cpu, data(txn_of(&load), &[4, 3, 2, 1]))
        .unwrap();
    ctx.take_wake();
    assert_eq!(arch(&cpu), before);
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    assert_eq!(
        cpu.registers()
            .read(systemscope_rv32i::Reg::new(2).unwrap()),
        0x0102_0304
    );
    assert_eq!(cpu.pc(), before.0 + 4);
    assert_eq!(cpu.instret(), before.2 + 1);
}

// ---------------------------------------------------------------------------------------
// Configuration and snapshots.

#[test]
fn a_misaligned_entry_is_rejected() {
    for entry in [1, 2, 3, ENTRY + 2] {
        let config = Rv32iConfig { entry, ..config() };
        assert_eq!(
            Rv32iCpu::new(config).err(),
            Some(CpuConfigError::MisalignedEntry(entry))
        );
    }
}

fn snapshot_of(cpu: &Rv32iCpu) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    cpu.snapshot(&mut w);
    w.into_bytes()
}

fn restore(config: Rv32iConfig, bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    let mut cpu = Rv32iCpu::new(config).unwrap();
    let mut r = SnapshotReader::new(bytes);
    cpu.restore(&mut r, SNAPSHOT_SCHEMA)?;
    r.finish().map_err(RestoreError::Decode)?;
    Ok(cpu)
}

/// Round-trips `cpu` and checks the copy is identical.
fn round_trip(cpu: &Rv32iCpu) -> Rv32iCpu {
    let bytes = snapshot_of(cpu);
    let copy = restore(config(), &bytes).unwrap();
    assert_eq!(snapshot_of(&copy), bytes);
    assert_eq!(copy.inspect(), cpu.inspect());
    copy
}

/// The snapshot layout, byte for byte: configuration, `pc`, `x1`…`x31`, `instret`, next
/// txn, then the state. A pending data access is stored as its instruction word only.
#[test]
fn snapshot_layout_is_stable() {
    let (mut cpu, mut ctx) = ready();
    let insn = sw(1, 10, 0);
    issue(&mut cpu, &mut ctx, insn);
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(LIMIT);
    w.u32(ENTRY + 20);
    for i in 1..32 {
        w.u32(match i {
            1 => 0x1234,
            10 => ENTRY + 0x1000,
            _ => 0,
        });
    }
    w.u64(5);
    w.u64(7);
    w.u8(3);
    w.u64(6);
    w.u32(insn);
    assert_eq!(snapshot_of(&cpu), w.into_bytes());
}

/// A CPU restored in any state continues exactly as the original, and restoring sends
/// nothing (restore has no context to send with).
#[test]
fn restored_cpus_continue_identically_from_every_state() {
    // Drive a store and a load, cloning the CPU through a snapshot at every step and
    // replaying the rest of the drive on both.
    type Step = fn(&mut Rv32iCpu, &mut MockCtx) -> Result<(), SimError>;
    let steps: Vec<Step> = vec![
        |c, x| x.wake(c, FETCH, Phase::Request),
        |c, x| x.respond(c, word(TxnId(5), sw(1, 10, 0))),
        |c, x| x.wake(c, MEMORY, Phase::Request),
        |c, x| x.respond(c, done(TxnId(6))),
        |c, x| x.wake(c, COMMIT, Phase::Commit),
        |c, x| x.wake(c, FETCH, Phase::Request),
        |c, x| x.respond(c, word(TxnId(7), lw(2, 10, 0))),
        |c, x| x.wake(c, MEMORY, Phase::Request),
        |c, x| x.respond(c, data(TxnId(8), &[0x34, 0x12, 0, 0])),
        |c, x| x.wake(c, COMMIT, Phase::Commit),
        |c, x| x.wake(c, FETCH, Phase::Request),
        |c, x| x.respond(c, read_fault(TxnId(9))),
        |c, x| x.wake(c, COMMIT, Phase::Commit),
    ];
    for split in 0..=steps.len() {
        let (mut original, mut ctx) = ready();
        for step in &steps[..split] {
            step(&mut original, &mut ctx).unwrap();
        }
        let mut copy = round_trip(&original);
        let mut copy_ctx = MockCtx::new();
        ctx = MockCtx::new();
        for step in &steps[split..] {
            step(&mut original, &mut ctx).unwrap();
            step(&mut copy, &mut copy_ctx).unwrap();
            assert_eq!(copy.inspect(), original.inspect());
        }
        assert_eq!(copy_ctx.sent, ctx.sent, "split {split}");
        assert_eq!(copy_ctx.woke, ctx.woke);
        assert_eq!(copy_ctx.traced, ctx.traced);
        assert_eq!(snapshot_of(&copy), snapshot_of(&original));
        assert_eq!(
            copy.registers()
                .read(systemscope_rv32i::Reg::new(2).unwrap()),
            0x1234
        );
        assert!(matches!(
            copy.halt(),
            Some(systemscope_rv32i::Halt::Trap(_))
        ));
    }
}

/// A CPU restored while its store is outstanding sends nothing and accepts only the
/// outstanding txn's response.
#[test]
fn a_restored_pending_store_keeps_its_txn_and_is_not_reissued() {
    let (mut cpu, mut ctx) = ready();
    let store = issue(&mut cpu, &mut ctx, sw(1, 10, 0));
    let txn = txn_of(&store);
    let mut copy = round_trip(&cpu);
    let mut ctx = MockCtx::new();
    // Every wake is refused: nothing can make it send again.
    let before = copy.inspect();
    for token in [FETCH, MEMORY, COMMIT] {
        let result = ctx.wake(&mut copy, token, Phase::Request);
        assert_fault(&copy, &ctx, &before, result);
    }
    let result = ctx.respond(&mut copy, done(TxnId(txn.0 - 1)));
    assert_fault(&copy, &ctx, &before, result);
    ctx.respond(&mut copy, done(txn)).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    ctx.wake(&mut copy, COMMIT, Phase::Commit).unwrap();
    assert!(ctx.sent.is_empty());
    assert_eq!(copy.instret(), 6);
}

/// Builds a snapshot of the standard configuration from its parts.
fn forge(
    pc: u32,
    x10: u32,
    instret: u64,
    next_txn: u64,
    state: impl FnOnce(&mut SnapshotWriter),
) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(LIMIT);
    w.u32(pc);
    for i in 1..32 {
        w.u32(if i == 10 { x10 } else { 0 });
    }
    w.u64(instret);
    w.u64(next_txn);
    state(&mut w);
    w.into_bytes()
}

fn assert_rejected(bytes: &[u8]) {
    assert!(
        matches!(restore(config(), bytes), Err(RestoreError::InvalidState(_))),
        "{:?}",
        restore(config(), bytes).err()
    );
}

#[test]
fn restore_accepts_forged_snapshots_that_are_consistent() {
    let base = ENTRY + 0x1000;
    // Waiting for a load of x10 + 0.
    let ok = forge(ENTRY, base, 0, 1, |w| {
        w.u8(3);
        w.u64(0);
        w.u32(lw(2, 10, 0));
    });
    restore(config(), &ok).unwrap();
    // A committed load value is not checked; its register and next pc are.
    let ok = forge(ENTRY, base, 0, 1, |w| {
        w.u8(4);
        w.u8(1);
        w.u32(lw(2, 10, 0));
        w.u8(0);
        w.u8(1);
        w.u8(2);
        w.u32(0xdead_beef);
        w.u32(ENTRY + 4);
    });
    restore(config(), &ok).unwrap();
}

#[test]
fn restore_rejects_a_different_configuration() {
    let (cpu, _) = ready();
    let bytes = snapshot_of(&cpu);
    for other in [
        Rv32iConfig {
            entry: ENTRY + 4,
            ..config()
        },
        Rv32iConfig {
            max_instructions: NonZeroU64::new(LIMIT + 1).unwrap(),
            ..config()
        },
        Rv32iConfig {
            clock: ClockDomainId(1),
            ..config()
        },
    ] {
        assert!(matches!(
            restore(other, &bytes),
            Err(RestoreError::InvalidState(_))
        ));
    }
}

#[test]
fn restore_rejects_inconsistent_state() {
    let base = ENTRY + 0x1000;
    let cases = [
        // Misaligned pc.
        forge(ENTRY + 2, base, 0, 0, |w| w.u8(0)),
        // Outstanding fetch txn that is not the latest issued.
        forge(ENTRY, base, 0, 3, |w| {
            w.u8(1);
            w.u64(1);
        }),
        forge(ENTRY, base, 0, 0, |w| {
            w.u8(1);
            w.u64(0);
        }),
        // Pending data access whose word is not an aligned load or store.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(2);
            w.u32(addi(1, 0, 1));
        }),
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(3);
            w.u64(0);
            w.u32(lw(2, 10, 2));
        }),
        // Pending outcome that is not its instruction's.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(4);
            w.u8(1);
            w.u32(addi(1, 0, 1));
            w.u8(0);
            w.u8(1);
            w.u8(1);
            w.u32(2);
            w.u32(ENTRY + 4);
        }),
        // A load committing into the wrong register.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(4);
            w.u8(1);
            w.u32(lw(2, 10, 0));
            w.u8(0);
            w.u8(1);
            w.u8(3);
            w.u32(0);
            w.u32(ENTRY + 4);
        }),
        // A load fault at the wrong address.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(4);
            w.u8(1);
            w.u32(lw(2, 10, 0));
            w.u8(1);
            w.u8(6);
            w.u32(base + 4);
        }),
        // A fetch fault with the wrong trap value.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(4);
            w.u8(0);
            w.u8(1);
            w.u8(1);
            w.u32(ENTRY + 4);
        }),
        // Halted on a trap at another pc.
        forge(ENTRY, base, 0, 1, |w| {
            w.u8(5);
            w.u8(0);
            w.u8(3);
            w.u32(ENTRY + 4);
            w.u32(ENTRY + 4);
        }),
        // Running at or past the limit, or halted on the limit before reaching it.
        forge(ENTRY, base, LIMIT, 0, |w| w.u8(0)),
        forge(ENTRY, base, LIMIT - 1, 0, |w| {
            w.u8(5);
            w.u8(1);
        }),
    ];
    for bytes in cases {
        assert_rejected(&bytes);
    }
}

#[test]
fn restore_rejects_unknown_tags() {
    let cases = [
        forge(ENTRY, 0, 0, 0, |w| w.u8(6)),
        forge(ENTRY, 0, 0, 1, |w| {
            w.u8(4);
            w.u8(2);
        }),
        forge(ENTRY, 0, 0, 1, |w| {
            w.u8(4);
            w.u8(0);
            w.u8(2);
        }),
        forge(ENTRY, 0, 0, 1, |w| {
            w.u8(4);
            w.u8(0);
            w.u8(1);
            w.u8(9);
            w.u32(0);
        }),
        forge(ENTRY, 0, 0, 0, |w| {
            w.u8(5);
            w.u8(2);
        }),
    ];
    for bytes in cases {
        assert!(matches!(
            restore(config(), &bytes),
            Err(RestoreError::Decode(_))
        ));
    }
}
