//! The machine external interrupt of the `M2` profile, driven directly
//! (`docs/m2-design.md` §5, §6.2, §6.4, §6.5, §7.1, M2.2b): the `irq` port and its
//! `irq.v0` rules, the one sampling point after a retirement, the entry, the priority of
//! the instruction limit and of synchronous traps, CSR-enable and `MRET` re-entry,
//! pending levels across snapshots, and `rv32.interrupt`.
//!
//! Expected values come from the pure oracle [`common::mei`], written from §5 alone, and
//! from the design's own examples. Nothing here reads a value through the crate's CSR
//! helpers: CSRs are observed through `inspect` and snapshots.

mod common;

use std::num::NonZeroU64;

use common::asm::*;
use common::mei::{Boundary, MEI_CAUSE, MeiResult, boundary, take_mei};
use common::{MockCtx, Traced};
use proptest::prelude::*;
use systemscope_contracts::component::{Component, PortId, PortSpec, Role};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::{
    self, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_rv32i::cpu::{
    COMMIT, COMMIT_KIND, FETCH, HALT_KIND, INTERRUPT_KIND, MEMORY, SNAPSHOT_SCHEMA,
    SNAPSHOT_SCHEMA_M2, TRAP_KIND,
};
use systemscope_rv32i::{Halt, Rv32iConfig, Rv32iCpu, Rv32iProfile};

const ENTRY: u32 = 0x8000_0000;
const CLOCK: ClockDomainId = ClockDomainId(0);
const LIMIT: u64 = 1000;
/// Where the tests put the handler (`mtvec`).
const HANDLER: u32 = 0x8000_0800;
/// Where the tests put their data.
const DATA: u32 = 0x8000_1000;

const MSTATUS: u16 = 0x300;
const MIE: u16 = 0x304;
const MIP: u16 = 0x344;
const MRET: u32 = 0x3020_0073;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}
fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(1, rd, rs1, csr)
}
fn csrrs(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(2, rd, rs1, csr)
}
fn csrrc(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(3, rd, rs1, csr)
}
fn csrrsi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(6, rd, uimm, csr)
}
fn csrrci(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(7, rd, uimm, csr)
}

// ---------------------------------------------------------------------------------------
// Building CPUs.

fn config(profile: Rv32iProfile, limit: u64) -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(limit).unwrap(),
        profile,
    }
}

/// The CSR block of schema 2, as flags and values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Csrs {
    mie: bool,
    mpie: bool,
    meie: bool,
    mtvec: u32,
    mscratch: u32,
    mepc: u32,
    mcause: u32,
    mtval: u32,
    irq: bool,
}

/// Everything enabled but the line, with recognizable values in the other CSRs.
const ARMED: Csrs = Csrs {
    mie: true,
    mpie: false,
    meie: true,
    mtvec: HANDLER,
    mscratch: 0x5a5a_0001,
    mepc: 0x8000_0400,
    mcause: 7,
    mtval: 0x55,
    irq: false,
};

impl Csrs {
    fn mstatus(&self) -> u32 {
        0x1800 | u32::from(self.mie) << 3 | u32::from(self.mpie) << 7
    }
    fn mie_csr(&self) -> u32 {
        u32::from(self.meie) << 11
    }
    fn mip(&self) -> u32 {
        u32::from(self.irq) << 11
    }
}

/// A schema 2 snapshot of a CPU in `FetchIssue` at `pc`, from its parts, with the next
/// `TxnId` 0.
fn forge(limit: u64, pc: u32, regs: &[(u32, u32)], instret: u64, csrs: &Csrs) -> Vec<u8> {
    forge_txn(limit, pc, regs, instret, 0, csrs)
}

fn forge_txn(
    limit: u64,
    pc: u32,
    regs: &[(u32, u32)],
    instret: u64,
    next_txn: u64,
    csrs: &Csrs,
) -> Vec<u8> {
    let mut x = [0u32; 32];
    for &(i, v) in regs {
        x[i as usize] = v;
    }
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(limit);
    w.u32(pc);
    for v in &x[1..] {
        w.u32(*v);
    }
    w.u64(instret);
    w.u64(next_txn);
    w.u8(0); // FetchIssue
    w.u8(u8::from(csrs.mie));
    w.u8(u8::from(csrs.mpie));
    w.u8(u8::from(csrs.meie));
    for v in [
        csrs.mtvec,
        csrs.mscratch,
        csrs.mepc,
        csrs.mcause,
        csrs.mtval,
    ] {
        w.u32(v);
    }
    w.u8(u8::from(csrs.irq));
    w.into_bytes()
}

fn restore(limit: u64, bytes: &[u8]) -> Rv32iCpu {
    let mut cpu = Rv32iCpu::new(config(Rv32iProfile::M2, limit)).unwrap();
    let mut r = SnapshotReader::new(bytes);
    cpu.restore(&mut r, SNAPSHOT_SCHEMA_M2).unwrap();
    r.finish().unwrap();
    cpu
}

/// An M2 CPU in `FetchIssue` at `pc` with `regs` and `csrs`, and a fresh context.
fn cpu_at(pc: u32, regs: &[(u32, u32)], csrs: Csrs) -> (Rv32iCpu, MockCtx) {
    (
        restore(LIMIT, &forge(LIMIT, pc, regs, 0, &csrs)),
        MockCtx::new(),
    )
}

fn snapshot_of(cpu: &Rv32iCpu) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    cpu.snapshot(&mut w);
    w.into_bytes()
}

// ---------------------------------------------------------------------------------------
// Observing CPUs.

fn view_u(view: &StateView, name: &str) -> u32 {
    match view.get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn read(cpu: &Rv32iCpu, name: &str) -> u32 {
    view_u(&cpu.inspect(), name)
}

fn state(cpu: &Rv32iCpu) -> String {
    match cpu.inspect().get("state") {
        Some(Value::Str(s)) => s.clone(),
        other => panic!("state: {other:?}"),
    }
}

fn reg(cpu: &Rv32iCpu, i: u32) -> u32 {
    if i == 0 {
        0
    } else {
        read(cpu, &format!("x{i}"))
    }
}

/// The CSRs as software reads them, from `inspect`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    mstatus: u32,
    mie: u32,
    mip: u32,
    mtvec: u32,
    mscratch: u32,
    mepc: u32,
    mcause: u32,
    mtval: u32,
}

fn seen(cpu: &Rv32iCpu) -> Seen {
    let v = cpu.inspect();
    Seen {
        mstatus: view_u(&v, "mstatus"),
        mie: view_u(&v, "mie"),
        mip: view_u(&v, "mip"),
        mtvec: view_u(&v, "mtvec"),
        mscratch: view_u(&v, "mscratch"),
        mepc: view_u(&v, "mepc"),
        mcause: view_u(&v, "mcause"),
        mtval: view_u(&v, "mtval"),
    }
}

impl From<Csrs> for Seen {
    fn from(c: Csrs) -> Seen {
        Seen {
            mstatus: c.mstatus(),
            mie: c.mie_csr(),
            mip: c.mip(),
            mtvec: c.mtvec,
            mscratch: c.mscratch,
            mepc: c.mepc,
            mcause: c.mcause,
            mtval: c.mtval,
        }
    }
}

impl Seen {
    /// These CSRs after the oracle's answer.
    fn after(self, mei: &MeiResult) -> Seen {
        Seen {
            mstatus: mei.new_mstatus,
            mepc: mei.mepc.unwrap_or(self.mepc),
            mcause: mei.mcause.unwrap_or(self.mcause),
            mtval: mei.mtval.unwrap_or(self.mtval),
            ..self
        }
    }
}

fn u(v: u32) -> Value {
    Value::U64(u64::from(v))
}

fn field(record: &Traced, name: &str) -> u32 {
    let (_, fields) = record;
    match fields.iter().find(|f| f.0 == name) {
        Some((_, Value::U64(v))) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?} in {record:?}"),
    }
}

/// The `rv32.interrupt` record of an entry.
fn interrupt(mepc: u32, handler: u32) -> Traced {
    (
        INTERRUPT_KIND,
        vec![
            ("mepc", u(mepc)),
            ("mcause", u(MEI_CAUSE)),
            ("handler", u(handler)),
        ],
    )
}

/// Nothing sent, woken, or traced since the last check.
fn quiet(ctx: &MockCtx) {
    assert!(ctx.sent.is_empty(), "{:?}", ctx.sent);
    assert!(ctx.woke.is_empty(), "{:?}", ctx.woke);
    assert!(ctx.traced.is_empty(), "{:?}", ctx.traced);
}

// ---------------------------------------------------------------------------------------
// Driving CPUs.

/// Sends the fetch from `FetchIssue`; returns its `TxnId`.
fn issue_fetch(cpu: &mut Rv32iCpu, ctx: &mut MockCtx) -> TxnId {
    ctx.wake(cpu, FETCH, Phase::Request).unwrap();
    let MemMsg::ReadReq { txn, addr, len: 4 } = ctx.take_sent() else {
        panic!("not a fetch");
    };
    assert_eq!(addr, u64::from(cpu.pc()));
    assert!(ctx.woke.is_empty());
    txn
}

fn data(txn: TxnId, bytes: &[u8]) -> MemMsg {
    MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Data {
            data: bytes.to_vec(),
        },
    }
}

/// Delivers the fetched word; returns the wake the CPU scheduled.
fn deliver_word(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, txn: TxnId, insn: u32) -> u64 {
    ctx.respond(cpu, data(txn, &insn.to_le_bytes())).unwrap();
    ctx.take_wake().token
}

/// Commits; returns what the commit traced. If the CPU goes on, checks that exactly the
/// next fetch is scheduled, at the next cycle's `Request`, as after any retirement.
fn commit(cpu: &mut Rv32iCpu, ctx: &mut MockCtx) -> Vec<Traced> {
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    assert!(ctx.sent.is_empty());
    if cpu.halt().is_none() {
        let next = ctx.take_wake();
        assert_eq!(
            (next.token, next.when, next.phase),
            (
                FETCH,
                ScheduleWhen::Cycles {
                    domain: CLOCK,
                    k: 1
                },
                Phase::Request
            )
        );
        assert_eq!(state(cpu), "fetch_issue");
    } else {
        assert!(ctx.woke.is_empty());
    }
    std::mem::take(&mut ctx.traced)
}

/// Runs `insn`, which must not touch memory, from `FetchIssue` through its commit.
fn step(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<Traced> {
    let txn = issue_fetch(cpu, ctx);
    assert_eq!(deliver_word(cpu, ctx, txn, insn), COMMIT);
    commit(cpu, ctx)
}

/// Runs a load or store from `FetchIssue` through its commit, answering the access with
/// `response(txn)` and delivering `level` in every wait on the way.
fn step_memory(
    cpu: &mut Rv32iCpu,
    ctx: &mut MockCtx,
    insn: u32,
    level: bool,
    response: impl FnOnce(TxnId) -> MemMsg,
) -> Vec<Traced> {
    let deliver = |cpu: &mut Rv32iCpu, ctx: &mut MockCtx, want: &str| {
        let before = seen(cpu);
        ctx.level(cpu, level).unwrap();
        quiet(ctx);
        assert_eq!(state(cpu), want);
        assert_eq!(seen(cpu).mip, u32::from(level) << 11);
        assert_eq!(
            Seen {
                mip: before.mip,
                ..seen(cpu)
            },
            before
        );
    };
    let txn = issue_fetch(cpu, ctx);
    deliver(cpu, ctx, "fetch_wait");
    assert_eq!(deliver_word(cpu, ctx, txn, insn), MEMORY);
    deliver(cpu, ctx, "mem_issue");
    ctx.wake(cpu, MEMORY, Phase::Request).unwrap();
    let txn = match ctx.take_sent() {
        MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => txn,
        other => panic!("{other:?}"),
    };
    assert!(ctx.woke.is_empty());
    deliver(cpu, ctx, "mem_wait");
    ctx.respond(cpu, response(txn)).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    deliver(cpu, ctx, "commit_pending");
    commit(cpu, ctx)
}

// ---------------------------------------------------------------------------------------
// The irq port and irq.v0.

#[test]
fn only_the_m2_profile_has_the_irq_port() {
    let mem = PortSpec {
        name: "mem",
        protocol: mem_v1::PROTOCOL,
        role: Role::Initiator,
    };
    let irq = PortSpec {
        name: "irq",
        protocol: irq_v0::PROTOCOL,
        role: Role::Target,
    };
    let m1 = Rv32iCpu::new(config(Rv32iProfile::M1, LIMIT)).unwrap();
    let m2 = Rv32iCpu::new(config(Rv32iProfile::M2, LIMIT)).unwrap();
    assert_eq!(m1.ports(), vec![mem]);
    assert_eq!(m2.ports(), vec![mem, irq]);
    assert_eq!(m1.type_name(), m2.type_name());
}

/// A CPU parked in each execution state, with the interrupt armed but the line low.
fn parked() -> Vec<(&'static str, Rv32iCpu)> {
    let regs = [(5, DATA)];
    let mut out = Vec::new();
    let (cpu, _) = cpu_at(ENTRY, &regs, ARMED);
    out.push(("fetch_issue", cpu));
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, ARMED);
    issue_fetch(&mut cpu, &mut ctx);
    out.push(("fetch_wait", cpu));
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, ARMED);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    deliver_word(&mut cpu, &mut ctx, txn, lw(6, 5, 0));
    out.push(("mem_issue", cpu.clone_via_snapshot()));
    ctx.wake(&mut cpu, MEMORY, Phase::Request).unwrap();
    out.push(("mem_wait", cpu));
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, ARMED);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    deliver_word(&mut cpu, &mut ctx, txn, addi(6, 0, 1));
    out.push(("commit_pending", cpu));
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, ARMED);
    step(&mut cpu, &mut ctx, ECALL);
    out.push(("halted", cpu));
    let mut cpu = restore(1, &forge(1, ENTRY, &regs, 0, &ARMED));
    step(&mut cpu, &mut MockCtx::new(), addi(6, 0, 1));
    out.push(("halted", cpu));
    for (name, cpu) in &out {
        assert_eq!(state(cpu), *name);
    }
    out
}

trait CloneViaSnapshot {
    fn clone_via_snapshot(&self) -> Rv32iCpu;
}

impl CloneViaSnapshot for Rv32iCpu {
    fn clone_via_snapshot(&self) -> Rv32iCpu {
        let mut copy = Rv32iCpu::new(config(Rv32iProfile::M2, LIMIT)).unwrap();
        let bytes = snapshot_of(self);
        copy.restore(&mut SnapshotReader::new(&bytes), SNAPSHOT_SCHEMA_M2)
            .unwrap();
        copy
    }
}

/// §6.2, §7.1: in every state, `Halted` included, a level in `Complete` sets `mip.MEIP`
/// and nothing else: no send, wake, trace, entry, or other state change. A repeated
/// level is a no-op.
#[test]
fn levels_set_meip_in_every_state_and_nothing_else() {
    for (name, mut cpu) in parked() {
        let mut ctx = MockCtx::new();
        let start = snapshot_of(&cpu);
        let body = start.len() - 1;
        for level in [false, false, true, true, false, true, false] {
            ctx.level(&mut cpu, level).unwrap();
            quiet(&ctx);
            let now = snapshot_of(&cpu);
            // Only the last byte of schema 2, the irq level, ever changes.
            assert_eq!(now[..body], start[..body], "{name}");
            assert_eq!(now[body], u8::from(level), "{name}");
            assert_eq!(read(&cpu, "mip"), u32::from(level) << 11, "{name}");
            assert_eq!(state(&cpu), name);
        }
    }
}

#[test]
fn a_level_outside_complete_faults_and_changes_nothing() {
    for (name, mut cpu) in parked() {
        for phase in [Phase::Request, Phase::Transfer, Phase::Commit] {
            for level in [true, false] {
                let mut ctx = MockCtx::new();
                let before = snapshot_of(&cpu);
                let msg = Message::Irq(IrqMsg::Level { asserted: level });
                let result = ctx.deliver(&mut cpu, PortId(1), msg, phase);
                assert!(
                    matches!(result, Err(SimError::ComponentFault(_))),
                    "{name} {phase:?}: {result:?}"
                );
                quiet(&ctx);
                assert_eq!(snapshot_of(&cpu), before);
            }
        }
    }
}

#[test]
fn messages_on_the_wrong_port_or_protocol_fault() {
    let level = Message::Irq(IrqMsg::Level { asserted: true });
    let resp = |txn| Message::MemV1(data(TxnId(txn), &addi(1, 0, 1).to_le_bytes()));
    let req = Message::MemV1(MemMsg::ReadReq {
        txn: TxnId(0),
        addr: 0,
        len: 4,
    });
    // M2: irq.v0 only on irq, mem.v1 only on mem, nothing on a port it lacks.
    for (port, msg) in [
        (PortId(0), level.clone()),
        (PortId(1), resp(0)),
        (PortId(1), req.clone()),
        (PortId(2), level.clone()),
        (PortId(2), resp(0)),
    ] {
        for (name, mut cpu) in parked() {
            let mut ctx = MockCtx::new();
            let before = snapshot_of(&cpu);
            let result = ctx.deliver(&mut cpu, port, msg.clone(), Phase::Complete);
            assert!(
                matches!(result, Err(SimError::ComponentFault(_))),
                "{name} {port:?} {msg:?}: {result:?}"
            );
            quiet(&ctx);
            assert_eq!(snapshot_of(&cpu), before);
        }
    }
    // The fetch response still works on mem after all of that.
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], ARMED);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    ctx.deliver(&mut cpu, PortId(0), resp(txn.0), Phase::Complete)
        .unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    // M1 has no irq port: an irq.v0 message faults whatever the port.
    for port in [PortId(0), PortId(1)] {
        let mut m1 = Rv32iCpu::new(config(Rv32iProfile::M1, LIMIT)).unwrap();
        let mut ctx = MockCtx::new();
        let mut w = SnapshotWriter::new();
        m1.snapshot(&mut w);
        let before = w.into_bytes();
        let result = ctx.deliver(&mut m1, port, level.clone(), Phase::Complete);
        assert!(
            matches!(result, Err(SimError::ComponentFault(_))),
            "{result:?}"
        );
        quiet(&ctx);
        let mut w = SnapshotWriter::new();
        m1.snapshot(&mut w);
        assert_eq!(w.into_bytes(), before);
    }
}

// ---------------------------------------------------------------------------------------
// Entry.

/// §5.2 after an ordinary instruction: every effect of the table, and nothing else.
#[test]
fn entry_after_an_ordinary_instruction() {
    let regs: Vec<(u32, u32)> = (1..32).map(|i| (i, 0x100 * i)).collect();
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, ARMED);
    // A level while waiting to fetch is recorded, not taken.
    ctx.level(&mut cpu, true).unwrap();
    quiet(&ctx);
    let before = seen(&cpu);
    let records = step(&mut cpu, &mut ctx, addi(6, 5, 1));
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(field(&records[0], "pc"), ENTRY);
    assert_eq!(field(&records[0], "next_pc"), ENTRY + 4);
    assert_eq!(records[1], interrupt(ENTRY + 4, HANDLER));
    assert_eq!(cpu.pc(), HANDLER);
    assert_eq!(cpu.instret(), 1, "the entry does not retire");
    assert_eq!(
        seen(&cpu),
        Seen {
            mstatus: 0x1880,
            mepc: ENTRY + 4,
            mcause: 0x8000_000b,
            mtval: 0,
            ..before
        }
    );
    // mie, mip, mscratch, mtvec, and every register but the instruction's rd unchanged.
    assert_eq!(
        (seen(&cpu).mie, seen(&cpu).mip, seen(&cpu).mscratch),
        (0x800, 0x800, ARMED.mscratch)
    );
    for (i, v) in &regs {
        let want = if *i == 6 { 0x501 } else { *v };
        assert_eq!(reg(&cpu, *i), want, "x{i}");
    }
    // The handler runs next; with MIE clear, the held line is not taken again.
    let records = step(&mut cpu, &mut ctx, addi(7, 7, 1));
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0], "pc"), HANDLER);
    assert_eq!(cpu.pc(), HANDLER + 4);
    assert_eq!(cpu.instret(), 2);
}

/// §5.2: `mepc` is the retiring instruction's `next_pc`, never its own address.
#[test]
fn mepc_is_the_next_pc_of_the_retiring_instruction() {
    let armed = Csrs { irq: true, ..ARMED };
    let base = 0x8000_0200;
    for (insn, next) in [
        (addi(6, 0, 1), ENTRY + 4),
        (beq(0, 0, 16), ENTRY + 16),
        (bne(0, 0, 16), ENTRY + 4),
        (beq(0, 0, -8), ENTRY - 8),
        (jal(1, 0x100), ENTRY + 0x100),
        (jalr(2, 5, 8), base + 8),
        (lui(3, 0x12345), ENTRY + 4),
        (FENCE, ENTRY + 4),
        (csrrs(4, MIP, 0), ENTRY + 4),
    ] {
        let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, base)], armed);
        let records = step(&mut cpu, &mut ctx, insn);
        assert_eq!(field(&records[0], "next_pc"), next, "{insn:#010x}");
        assert_eq!(records[1..], [interrupt(next, HANDLER)], "{insn:#010x}");
        assert_eq!(read(&cpu, "mepc"), next);
        assert_eq!(cpu.pc(), HANDLER);
    }
}

/// §5.1: a level delivered in `Complete` before the commit's `Commit` is seen by the
/// commit, whether it is dispatched before or after the instruction's own response.
#[test]
fn a_level_in_the_same_tick_is_seen_by_the_commit() {
    for (initial, level) in [(false, true), (true, false)] {
        let mut ends = Vec::new();
        for level_first in [true, false] {
            let (mut cpu, mut ctx) = cpu_at(
                ENTRY,
                &[],
                Csrs {
                    irq: initial,
                    ..ARMED
                },
            );
            let txn = issue_fetch(&mut cpu, &mut ctx);
            if level_first {
                ctx.level(&mut cpu, level).unwrap();
            }
            ctx.respond(&mut cpu, data(txn, &addi(6, 0, 1).to_le_bytes()))
                .unwrap();
            assert_eq!(ctx.take_wake().token, COMMIT);
            if !level_first {
                ctx.level(&mut cpu, level).unwrap();
            }
            let records = commit(&mut cpu, &mut ctx);
            if level {
                assert_eq!(records[1..], [interrupt(ENTRY + 4, HANDLER)]);
            } else {
                assert_eq!(records.len(), 1);
                assert_eq!(cpu.pc(), ENTRY + 4);
            }
            ends.push((records, snapshot_of(&cpu)));
        }
        assert_eq!(
            ends[0], ends[1],
            "the order within Complete does not matter"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Priorities.

/// §5.1: an instruction that reaches the limit halts the CPU; the pending interrupt is
/// not taken. One instruction earlier, it is.
#[test]
fn the_instruction_limit_takes_priority() {
    let armed = Csrs { irq: true, ..ARMED };
    for (limit, instret) in [(1, 0), (5, 4), (2, 0)] {
        let mut cpu = restore(limit, &forge(limit, ENTRY, &[], instret, &armed));
        let mut ctx = MockCtx::new();
        let before = seen(&cpu);
        let records = step(&mut cpu, &mut ctx, addi(6, 0, 1));
        let expected = boundary(
            instret + 1,
            limit,
            ENTRY + 4,
            before.mstatus,
            before.mie,
            before.mip,
            before.mtvec,
        );
        assert_eq!(records[0].0, COMMIT_KIND);
        match expected {
            Boundary::InstructionLimit => {
                assert_eq!(
                    records[1..],
                    [(HALT_KIND, vec![("instret", Value::U64(limit))])]
                );
                assert_eq!(cpu.halt(), Some(Halt::InstructionLimit));
                assert_eq!(seen(&cpu), before);
                assert_eq!(cpu.pc(), ENTRY + 4);
            }
            Boundary::Fetch(mei) => {
                assert!(mei.taken);
                assert_eq!(records[1..], [interrupt(ENTRY + 4, HANDLER)]);
                assert_eq!(cpu.pc(), mei.new_pc);
                assert_eq!(seen(&cpu), before.after(&mei));
            }
        }
        assert_eq!(cpu.instret(), instret + 1);
    }
}

/// §5.6: a trapping instruction does not retire, so nothing is sampled: the CPU halts as
/// in M1 and no CSR changes, even with the interrupt pending and enabled.
#[test]
fn a_synchronous_trap_takes_priority() {
    let armed = Csrs { irq: true, ..ARMED };
    let fault = |txn| MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Fault {
            fault: MemFault::AccessFault,
        },
    };
    for insn in [
        ECALL,
        EBREAK,
        0xffff_ffff,
        0x1050_0073, // WFI
        csrrw(1, 0x7c0, 2),
        lw(1, 5, 1),
        jal(0, 2),
    ] {
        let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, DATA)], armed);
        let before = seen(&cpu);
        let records = step(&mut cpu, &mut ctx, insn);
        assert_eq!(records.len(), 1, "{insn:#010x}: {records:?}");
        assert_eq!(records[0].0, TRAP_KIND);
        assert!(matches!(cpu.halt(), Some(Halt::Trap(_))));
        assert_eq!(seen(&cpu), before);
        assert_eq!((cpu.pc(), cpu.instret()), (ENTRY, 0));
    }
    // A fetch fault and a load fault.
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], armed);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    ctx.respond(&mut cpu, fault(txn)).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    let records = commit(&mut cpu, &mut ctx);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, TRAP_KIND);
    assert_eq!(seen(&cpu), Seen::from(armed));
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, DATA)], armed);
    let records = step_memory(&mut cpu, &mut ctx, lw(6, 5, 0), true, fault);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, TRAP_KIND);
    assert_eq!(seen(&cpu), Seen::from(armed));
}

// ---------------------------------------------------------------------------------------
// No sampling outside the boundary.

/// §5.1: a level that rises while a load or store is in flight, in every wait, is only
/// recorded; the access completes, the instruction retires, and only then is the
/// interrupt taken.
#[test]
fn no_interrupt_while_a_load_or_store_is_in_flight() {
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, DATA)], ARMED);
    let records = step_memory(&mut cpu, &mut ctx, lw(6, 5, 4), true, |txn| {
        data(txn, &0xcafe_f00du32.to_le_bytes())
    });
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(field(&records[0], "addr"), DATA + 4);
    assert_eq!(records[1..], [interrupt(ENTRY + 4, HANDLER)]);
    assert_eq!(reg(&cpu, 6), 0xcafe_f00d, "the load retired first");
    assert_eq!(cpu.instret(), 1);

    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, DATA), (6, 0x1234)], ARMED);
    let records = step_memory(&mut cpu, &mut ctx, sw(6, 5, 8), true, |txn| {
        MemMsg::WriteResp {
            txn,
            outcome: WriteOutcome::Done,
        }
    });
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(field(&records[0], "value"), 0x1234);
    assert_eq!(records[1..], [interrupt(ENTRY + 4, HANDLER)]);
    assert_eq!(cpu.instret(), 1);

    // A level that falls again before the commit is not taken.
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[(5, DATA)], Csrs { irq: true, ..ARMED });
    let records = step_memory(&mut cpu, &mut ctx, lw(6, 5, 0), false, |txn| {
        data(txn, &[0; 4])
    });
    assert_eq!(records.len(), 1);
    assert_eq!(cpu.pc(), ENTRY + 4);
}

/// §5.1: a level that rises while the fetch is outstanding is taken only after the
/// fetched instruction retires.
#[test]
fn no_interrupt_while_the_fetch_is_outstanding() {
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], ARMED);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    ctx.level(&mut cpu, true).unwrap();
    quiet(&ctx);
    assert_eq!((state(&cpu), cpu.pc()), ("fetch_wait".to_owned(), ENTRY));
    assert_eq!(read(&cpu, "mip"), 0x800);
    assert_eq!(deliver_word(&mut cpu, &mut ctx, txn, addi(6, 0, 3)), COMMIT);
    assert_eq!(state(&cpu), "commit_pending");
    assert_eq!(cpu.instret(), 0);
    let records = commit(&mut cpu, &mut ctx);
    assert_eq!(field(&records[0], "pc"), ENTRY);
    assert_eq!(records[1..], [interrupt(ENTRY + 4, HANDLER)]);
    assert_eq!(reg(&cpu, 6), 3);
}

/// §6.2: a halted CPU records a level and does nothing else.
#[test]
fn a_halted_cpu_only_records_the_level() {
    for (name, mut cpu) in parked() {
        if name != "halted" {
            continue;
        }
        let mut ctx = MockCtx::new();
        let (pc, instret, halt) = (cpu.pc(), cpu.instret(), cpu.halt());
        for level in [true, false, true] {
            ctx.level(&mut cpu, level).unwrap();
            quiet(&ctx);
            assert_eq!((cpu.pc(), cpu.instret(), cpu.halt()), (pc, instret, halt));
            assert_eq!(read(&cpu, "mip"), u32::from(level) << 11);
            assert_eq!(read(&cpu, "mstatus"), ARMED.mstatus());
        }
    }
}

// ---------------------------------------------------------------------------------------
// CSR writes at the boundary.

/// §5.5: a CSR instruction that sets the last missing condition takes the interrupt at
/// its own boundary: `mepc` is its `pc + 4`, and the next instruction does not retire.
#[test]
fn a_csr_write_that_enables_takes_the_interrupt_at_its_own_boundary() {
    let pending = Csrs { irq: true, ..ARMED };
    let at = ENTRY + 0x20;
    for (csrs, regs, insn) in [
        // MEIE = 1, MEIP = 1, MIE = 0: csrsi mstatus, 8.
        (
            Csrs {
                mie: false,
                ..pending
            },
            vec![],
            csrrsi(0, MSTATUS, 8),
        ),
        // Through a register, with rd.
        (
            Csrs {
                mie: false,
                ..pending
            },
            vec![(2, 0x88)],
            csrrw(3, MSTATUS, 2),
        ),
        // MIE = 1, MEIP = 1, MEIE = 0: set mie.MEIE.
        (
            Csrs {
                meie: false,
                ..pending
            },
            vec![(2, 0x800)],
            csrrs(0, MIE, 2),
        ),
    ] {
        let (mut cpu, mut ctx) = cpu_at(at, &regs, csrs);
        let records = step(&mut cpu, &mut ctx, insn);
        assert_eq!(records.len(), 2, "{insn:#010x}: {records:?}");
        assert_eq!(records[0].0, COMMIT_KIND);
        assert!(records[0].1.iter().any(|f| f.0 == "csr_value"));
        assert_eq!(records[1], interrupt(at + 4, HANDLER));
        assert_eq!(read(&cpu, "mepc"), at + 4);
        assert_eq!(read(&cpu, "mstatus"), 0x1880, "MIE cleared, MPIE set");
        assert_eq!(cpu.instret(), 1);
        // The next fetch is the handler's, not at + 4.
        assert_eq!(issue_fetch(&mut cpu, &mut ctx), TxnId(1));
        assert_eq!(cpu.pc(), HANDLER);
    }
    // Writing mip cannot enable anything: the line is the only source of MEIP.
    let (mut cpu, mut ctx) = cpu_at(at, &[(2, 0x800)], ARMED);
    for insn in [csrrs(0, MIP, 2), csrrw(0, MIP, 2), csrrsi(0, MIP, 31)] {
        let records = step(&mut cpu, &mut ctx, insn);
        assert_eq!(records.len(), 1);
        assert_eq!(read(&cpu, "mip"), 0);
    }
}

/// §5.5: clearing MIE or MEIE takes effect at the same boundary: with the interrupt
/// pending and enabled before the instruction, it is not taken after it.
#[test]
fn a_csr_write_that_disables_is_seen_at_its_own_boundary() {
    let pending = Csrs { irq: true, ..ARMED };
    for (regs, insn, mstatus, mie) in [
        (vec![], csrrci(0, MSTATUS, 8), 0x1800, 0x800),
        (vec![], csrrw(0, MSTATUS, 0), 0x1800, 0x800),
        (vec![(2, 0x800)], csrrc(0, MIE, 2), 0x1808, 0),
        (vec![], csrrw(0, MIE, 0), 0x1808, 0),
    ] {
        let (mut cpu, mut ctx) = cpu_at(ENTRY, &regs, pending);
        let records = step(&mut cpu, &mut ctx, insn);
        assert_eq!(records.len(), 1, "{insn:#010x}: {records:?}");
        assert_eq!(cpu.pc(), ENTRY + 4);
        assert_eq!((read(&cpu, "mstatus"), read(&cpu, "mie")), (mstatus, mie));
        assert_eq!(read(&cpu, "mepc"), ARMED.mepc);
    }
}

// ---------------------------------------------------------------------------------------
// MRET.

/// Handler state: in the handler, MIE clear, MPIE set, `mepc` the interrupted `pc`.
const IN_HANDLER: Csrs = Csrs {
    mie: false,
    mpie: true,
    meie: true,
    mtvec: HANDLER,
    mscratch: 0,
    mepc: 0x8000_0100,
    mcause: 0x8000_000b,
    mtval: 0,
    irq: true,
};

/// §5.4: `MRET` with the line still asserted retires, then re-enters at once: no
/// instruction at `mepc` runs, `mepc` is rewritten with the same address, and `instret`
/// counts only the `MRET`. The state is restored from a snapshot (the pending level of
/// §6.4).
#[test]
fn mret_reenters_at_once_while_the_line_is_held() {
    let (mut cpu, mut ctx) = cpu_at(HANDLER + 8, &[], IN_HANDLER);
    let records = step(&mut cpu, &mut ctx, MRET);
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(
        records[0],
        (
            COMMIT_KIND,
            vec![
                ("pc", u(HANDLER + 8)),
                ("insn", u(MRET)),
                ("rd", u(0)),
                ("rd_value", u(0)),
                ("next_pc", u(0x8000_0100)),
            ]
        )
    );
    assert_eq!(records[1], interrupt(0x8000_0100, HANDLER));
    assert_eq!(cpu.pc(), HANDLER);
    assert_eq!(cpu.instret(), 1, "only the MRET retired");
    assert_eq!(read(&cpu, "mepc"), 0x8000_0100);
    assert_eq!(read(&cpu, "mstatus"), 0x1880);
    assert_eq!(read(&cpu, "mcause"), MEI_CAUSE);
    // The next fetch is the handler's again.
    issue_fetch(&mut cpu, &mut ctx);
    assert_eq!(cpu.pc(), HANDLER);
}

/// `MRET` with the line low returns normally: the instruction at `mepc` runs next.
#[test]
fn mret_returns_normally_when_the_line_is_low() {
    let (mut cpu, mut ctx) = cpu_at(
        HANDLER + 8,
        &[],
        Csrs {
            irq: false,
            ..IN_HANDLER
        },
    );
    let records = step(&mut cpu, &mut ctx, MRET);
    assert_eq!(records.len(), 1);
    assert_eq!(cpu.pc(), 0x8000_0100);
    assert_eq!(read(&cpu, "mstatus"), 0x1888);
    let records = step(&mut cpu, &mut ctx, addi(6, 0, 1));
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0], "pc"), 0x8000_0100);
    assert_eq!(cpu.instret(), 2);
}

/// A handler that deasserts the line before `MRET` (here, a `Level(false)` delivered in
/// the `Complete` of `MRET`'s own fetch, or earlier) is not re-entered.
#[test]
fn a_deassert_before_mret_prevents_reentry() {
    // In MRET's own tick.
    let (mut cpu, mut ctx) = cpu_at(HANDLER + 8, &[], IN_HANDLER);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    assert_eq!(deliver_word(&mut cpu, &mut ctx, txn, MRET), COMMIT);
    ctx.level(&mut cpu, false).unwrap();
    let records = commit(&mut cpu, &mut ctx);
    assert_eq!(records.len(), 1);
    assert_eq!(cpu.pc(), 0x8000_0100);
    assert_eq!(read(&cpu, "mstatus"), 0x1888);
    // While the fetch is outstanding.
    let (mut cpu, mut ctx) = cpu_at(HANDLER + 8, &[], IN_HANDLER);
    let txn = issue_fetch(&mut cpu, &mut ctx);
    ctx.level(&mut cpu, false).unwrap();
    assert_eq!(deliver_word(&mut cpu, &mut ctx, txn, MRET), COMMIT);
    assert_eq!(commit(&mut cpu, &mut ctx).len(), 1);
    assert_eq!(cpu.pc(), 0x8000_0100);
}

// ---------------------------------------------------------------------------------------
// Snapshots (§6.4): schema 2 unchanged, pending levels restored with their meaning.

/// The state after an entry is plain `FetchIssue` at `mtvec`: the schema 2 bytes of that
/// state, with no interrupt-related transient state.
#[test]
fn the_state_after_an_entry_is_plain_fetch_issue() {
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], Csrs { irq: true, ..ARMED });
    step(&mut cpu, &mut ctx, addi(6, 0, 9));
    let expected = forge_txn(
        LIMIT,
        HANDLER,
        &[(6, 9)],
        1,
        1,
        &Csrs {
            mie: false,
            mpie: true,
            mepc: ENTRY + 4,
            mcause: MEI_CAUSE,
            mtval: 0,
            irq: true,
            ..ARMED
        },
    );
    assert_eq!(snapshot_of(&cpu), expected);
}

/// §6.4 case A: a pending, enabled level restored in the middle of an instruction is not
/// taken on restore; the instruction finishes, and the interrupt is taken after it
/// retires, exactly as in the original.
#[test]
fn a_restored_pending_level_waits_for_the_retirement() {
    for (name, cpu) in parked() {
        if name == "halted" || name == "fetch_issue" {
            continue;
        }
        let mut original = cpu;
        let mut ctx = MockCtx::new();
        ctx.level(&mut original, true).unwrap();
        let mut copy = original.clone_via_snapshot();
        assert_eq!(copy.inspect(), original.inspect(), "{name}");
        assert_eq!(state(&copy), name);
        let finish = |cpu: &mut Rv32iCpu| {
            let mut ctx = MockCtx::new();
            let mut log = Vec::new();
            if state(cpu) == "fetch_wait" {
                ctx.respond(cpu, data(TxnId(0), &addi(6, 0, 1).to_le_bytes()))
                    .unwrap();
                log.push(ctx.take_wake());
            }
            if state(cpu) == "mem_issue" {
                ctx.wake(cpu, MEMORY, Phase::Request).unwrap();
                ctx.sent.clear();
            }
            if state(cpu) == "mem_wait" {
                ctx.respond(cpu, data(TxnId(1), &[1, 2, 3, 4])).unwrap();
                log.push(ctx.take_wake());
            }
            assert_eq!(state(cpu), "commit_pending");
            assert!(ctx.traced.is_empty(), "nothing before the commit");
            (log, commit(cpu, &mut ctx))
        };
        let a = finish(&mut original);
        let b = finish(&mut copy);
        assert_eq!(a, b, "{name}");
        assert_eq!(a.1.len(), 2, "{name}: {:?}", a.1);
        assert_eq!(a.1[1], interrupt(ENTRY + 4, HANDLER));
        assert_eq!(snapshot_of(&copy), snapshot_of(&original));
    }
}

/// §6.4 case B: a restored pending level with MIE clear is not taken until software sets
/// MIE, and then at that instruction's boundary.
#[test]
fn a_restored_pending_level_with_mie_clear_waits_for_software() {
    let (mut cpu, mut ctx) = cpu_at(
        ENTRY,
        &[],
        Csrs {
            mie: false,
            irq: true,
            ..ARMED
        },
    );
    for insn in [addi(6, 0, 1), addi(6, 6, 1), csrrs(7, MIP, 0)] {
        assert_eq!(step(&mut cpu, &mut ctx, insn).len(), 1);
    }
    assert_eq!(reg(&cpu, 7), 0x800);
    let records = step(&mut cpu, &mut ctx, csrrsi(0, MSTATUS, 8));
    assert_eq!(records[1..], [interrupt(ENTRY + 16, HANDLER)]);
    assert_eq!(cpu.instret(), 4);
}

/// Where the restored handler's `MRET` returns to in cases C and D.
const RETURN_PC: u32 = 0x8000_0200;
/// The restored handler's `MRET`.
const MRET_PC: u32 = HANDLER + 0x10;
/// `instret` in the restored handler state.
const RESTORED_INSTRET: u64 = 41;

/// A CPU in handler state (MIE clear, MPIE and MEIE set, `mepc` = [`RETURN_PC`], `pc` at
/// the `MRET`) with the line at `irq`, through the real schema 2 path: the forged bytes are
/// decoded by `restore`, encoded again by `snapshot` (unchanged: no new field, same
/// order), and decoded again into the CPU that runs.
fn restored_handler(irq: bool) -> (Rv32iCpu, MockCtx) {
    let csrs = Csrs {
        mepc: RETURN_PC,
        irq,
        ..IN_HANDLER
    };
    assert_eq!(csrs.mstatus(), 0x1880, "MIE 0, MPIE 1, MPP 0b11");
    let forged = forge(LIMIT, MRET_PC, &[], RESTORED_INSTRET, &csrs);
    let decoded = restore(LIMIT, &forged);
    assert_eq!(decoded.snapshot_schema_version(), SNAPSHOT_SCHEMA_M2);
    let encoded = snapshot_of(&decoded);
    assert_eq!(encoded, forged, "schema 2 round-trips byte for byte");
    let cpu = restore(LIMIT, &encoded);
    assert_eq!(snapshot_of(&cpu), forged);
    assert_eq!(state(&cpu), "fetch_issue");
    assert_eq!(cpu.pc(), MRET_PC);
    assert_eq!(cpu.instret(), RESTORED_INSTRET);
    assert_eq!(seen(&cpu), Seen::from(csrs));
    (cpu, MockCtx::new())
}

/// The schemas the restored cases rely on are the frozen ones.
#[test]
fn the_restored_cases_use_the_frozen_schemas() {
    assert_eq!(SNAPSHOT_SCHEMA, 1);
    assert_eq!(SNAPSHOT_SCHEMA_M2, 2);
    let m1 = Rv32iCpu::new(config(Rv32iProfile::M1, LIMIT)).unwrap();
    let m2 = Rv32iCpu::new(config(Rv32iProfile::M2, LIMIT)).unwrap();
    assert_eq!(m1.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    assert_eq!(m2.snapshot_schema_version(), SNAPSHOT_SCHEMA_M2);
}

/// §6.4 case C: a handler restored with the line still asserted. Its `MRET` retires once
/// (MIE ← MPIE = 1, MPIE ← 1), the boundary samples MEIP still set, and the CPU re-enters
/// at once: nothing at `mepc` runs, `mepc` is written with the same return address, and
/// `instret` counts only the `MRET`.
#[test]
fn a_restored_handler_with_the_line_held_reenters_after_mret() {
    let (mut cpu, mut ctx) = restored_handler(true);
    assert_eq!(read(&cpu, "mip"), 0x800, "the restored level is pending");
    let records = step(&mut cpu, &mut ctx, MRET);
    assert_eq!(
        records,
        [
            (
                COMMIT_KIND,
                vec![
                    ("pc", u(MRET_PC)),
                    ("insn", u(MRET)),
                    ("rd", u(0)),
                    ("rd_value", u(0)),
                    ("next_pc", u(RETURN_PC)),
                ]
            ),
            interrupt(RETURN_PC, HANDLER),
        ]
    );
    assert_eq!(cpu.instret(), RESTORED_INSTRET + 1, "only the MRET retired");
    assert_eq!(cpu.pc(), HANDLER & !3);
    assert_eq!(read(&cpu, "mip"), 0x800);
    assert_eq!(read(&cpu, "mepc"), RETURN_PC);
    assert_eq!(read(&cpu, "mcause"), 0x8000_000b);
    assert_eq!(read(&cpu, "mtval"), 0);
    // MIE 0, MPIE 1, MPP 0b11.
    assert_eq!(read(&cpu, "mstatus"), 0x1880);
    // The next fetch is the handler's, not the instruction at RETURN_PC.
    issue_fetch(&mut cpu, &mut ctx);
    assert_eq!(cpu.pc(), HANDLER);
    assert!(ctx.traced.is_empty());
}

/// §6.4 case D: the same handler restored with the line deasserted. Its `MRET` retires
/// once (MIE ← MPIE) and returns normally: no entry, no `rv32.interrupt`, and the
/// instruction at `mepc` runs next.
#[test]
fn a_restored_handler_with_the_line_low_returns_after_mret() {
    let (mut cpu, mut ctx) = restored_handler(false);
    assert_eq!(read(&cpu, "mip"), 0);
    let records = step(&mut cpu, &mut ctx, MRET);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, COMMIT_KIND);
    assert_eq!(field(&records[0], "next_pc"), RETURN_PC);
    assert_eq!(cpu.instret(), RESTORED_INSTRET + 1);
    assert_eq!(cpu.pc(), RETURN_PC);
    assert_eq!(read(&cpu, "mepc"), RETURN_PC);
    // MIE ← MPIE = 1, MPIE ← 1, MPP 0b11.
    assert_eq!(read(&cpu, "mstatus"), 0x1888);
    // The instruction at RETURN_PC runs next, without an entry.
    let records = step(&mut cpu, &mut ctx, addi(6, 0, 1));
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(field(&records[0], "pc"), RETURN_PC);
    assert_eq!(reg(&cpu, 6), 1);
    assert_eq!(cpu.instret(), RESTORED_INSTRET + 2);
}

/// Every event of a program with level changes, entries, and an `MRET` re-entry: a CPU
/// restored from a snapshot at any point continues exactly as the original.
#[test]
fn restored_cpus_continue_identically_through_interrupts() {
    #[derive(Clone, Copy)]
    enum Ev {
        Fetch,
        Word(u32),
        Commit,
        Level(bool),
    }
    use Ev::*;
    let at = |insn: u32| [Fetch, Word(insn), Commit];
    let mut script = Vec::new();
    script.extend(at(addi(6, 0, 1)));
    script.push(Level(true));
    // Taken after this one (mepc = ENTRY + 8).
    script.extend(at(addi(6, 6, 1)));
    // Handler: one instruction, then MRET re-enters while the line is held.
    script.extend(at(addi(7, 7, 1)));
    script.extend(at(MRET));
    // The line falls during MRET's fetch: the second MRET returns.
    script.extend(at(addi(7, 7, 1)));
    script.extend([Fetch, Level(false), Word(MRET), Commit]);
    script.extend(at(addi(6, 6, 1)));
    let drive = |cpu: &mut Rv32iCpu, ctx: &mut MockCtx, ev: Ev, txn: &mut u64| match ev {
        Fetch => {
            ctx.wake(cpu, FETCH, Phase::Request).unwrap();
        }
        Word(insn) => {
            ctx.respond(cpu, data(TxnId(*txn), &insn.to_le_bytes()))
                .unwrap();
            *txn += 1;
        }
        Commit => ctx.wake(cpu, COMMIT, Phase::Commit).unwrap(),
        Level(level) => ctx.level(cpu, level).unwrap(),
    };
    let mut reference = None;
    for split in 0..=script.len() {
        let (mut original, mut ctx) = cpu_at(ENTRY, &[], ARMED);
        let mut txn = 0;
        for ev in &script[..split] {
            drive(&mut original, &mut ctx, *ev, &mut txn);
        }
        let mut copy = original.clone_via_snapshot();
        let (mut a, mut b) = (MockCtx::new(), MockCtx::new());
        let mut copy_txn = txn;
        for ev in &script[split..] {
            drive(&mut original, &mut a, *ev, &mut txn);
            drive(&mut copy, &mut b, *ev, &mut copy_txn);
            assert_eq!(copy.inspect(), original.inspect(), "split {split}");
        }
        assert_eq!((&b.sent, &b.woke, &b.traced), (&a.sent, &a.woke, &a.traced));
        assert_eq!(snapshot_of(&copy), snapshot_of(&original));
        ctx.traced.extend(a.traced);
        let end = (ctx.traced, snapshot_of(&original));
        match &reference {
            None => reference = Some(end),
            Some(r) => assert_eq!(&end, r, "split {split}"),
        }
    }
    let (traced, _) = reference.unwrap();
    let kinds: Vec<&str> = traced.iter().map(|r| r.0).collect();
    assert_eq!(
        kinds,
        [
            COMMIT_KIND,
            COMMIT_KIND,
            INTERRUPT_KIND,
            COMMIT_KIND,
            COMMIT_KIND,
            INTERRUPT_KIND,
            COMMIT_KIND,
            COMMIT_KIND,
            COMMIT_KIND,
        ]
    );
    assert_eq!(traced[2], interrupt(ENTRY + 8, HANDLER));
    assert_eq!(traced[5], interrupt(ENTRY + 8, HANDLER));
    assert_eq!(field(&traced[8], "pc"), ENTRY + 8);
}

// ---------------------------------------------------------------------------------------
// Trace and observation.

/// §6.5: one `rv32.interrupt` per entry, after the retirement's `rv32.commit`; none
/// without an entry; the entry adds no retirement.
#[test]
fn one_interrupt_record_per_entry_and_no_retirement() {
    let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], ARMED);
    let mut records = Vec::new();
    // Line low: no records but commits.
    records.extend(step(&mut cpu, &mut ctx, addi(6, 0, 1)));
    // Line up and down again before any boundary: nothing.
    ctx.level(&mut cpu, true).unwrap();
    ctx.level(&mut cpu, false).unwrap();
    records.extend(step(&mut cpu, &mut ctx, addi(6, 6, 1)));
    assert!(records.iter().all(|r| r.0 == COMMIT_KIND));
    // Line up: one entry; the handler's instructions retire without another.
    ctx.level(&mut cpu, true).unwrap();
    for insn in [addi(6, 6, 1), addi(7, 0, 1), addi(7, 7, 1)] {
        records.extend(step(&mut cpu, &mut ctx, insn));
    }
    let interrupts = records.iter().filter(|r| r.0 == INTERRUPT_KIND).count();
    let commits = records.iter().filter(|r| r.0 == COMMIT_KIND).count();
    assert_eq!((interrupts, commits), (1, 5));
    assert_eq!(cpu.instret(), 5);
    let i = records.iter().position(|r| r.0 == INTERRUPT_KIND).unwrap();
    assert_eq!(records[i - 1].0, COMMIT_KIND);
    assert_eq!(records[i], interrupt(ENTRY + 12, HANDLER));
}

/// Inspecting and snapshotting after every event changes neither the interrupt timing,
/// the retirements, nor the final state.
#[test]
fn observing_does_not_change_interrupt_timing() {
    let run = |observe: bool| {
        let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], ARMED);
        let mut views = Vec::new();
        let mut look = |cpu: &Rv32iCpu| {
            if observe {
                views.push((cpu.inspect(), snapshot_of(cpu)));
            }
        };
        for (insn, level) in [
            (addi(6, 0, 1), None),
            (addi(6, 6, 1), Some(true)),
            (addi(7, 0, 1), None),
            (MRET, None),
            (addi(7, 7, 1), Some(false)),
            (MRET, None),
            (addi(6, 6, 1), None),
        ] {
            let txn = issue_fetch(&mut cpu, &mut ctx);
            look(&cpu);
            if let Some(level) = level {
                ctx.level(&mut cpu, level).unwrap();
                look(&cpu);
            }
            deliver_word(&mut cpu, &mut ctx, txn, insn);
            look(&cpu);
            ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
            ctx.woke.clear();
            look(&cpu);
        }
        (ctx.traced, snapshot_of(&cpu), cpu.inspect())
    };
    let plain = run(false);
    assert_eq!(run(true), plain);
    let entries = plain.0.iter().filter(|r| r.0 == INTERRUPT_KIND).count();
    assert_eq!(entries, 2, "{:?}", plain.0);
}

// ---------------------------------------------------------------------------------------
// The boundary against the oracle.

/// Every combination of MIE, MEIE, and MEIP: only `111` enters.
#[test]
fn only_mie_meie_and_meip_together_enter() {
    for bits in 0..8u32 {
        let csrs = Csrs {
            mie: bits & 4 != 0,
            meie: bits & 2 != 0,
            irq: bits & 1 != 0,
            ..ARMED
        };
        for insn in [addi(6, 0, 1), jal(0, 8), csrrs(0, MIP, 0), FENCE] {
            let (mut cpu, mut ctx) = cpu_at(ENTRY, &[], csrs);
            let records = step(&mut cpu, &mut ctx, insn);
            let next = field(&records[0], "next_pc");
            let before = Seen::from(csrs);
            let mei = take_mei(next, before.mstatus, before.mie, before.mip, before.mtvec);
            assert_eq!(mei.taken, bits == 0b111, "{bits:03b}");
            assert_eq!(records.len(), 1 + usize::from(mei.taken), "{bits:03b}");
            assert_eq!(cpu.pc(), mei.new_pc);
            assert_eq!(seen(&cpu), before.after(&mei));
        }
    }
}

prop_compose! {
    fn any_csrs()(
        mie in any::<bool>(),
        mpie in any::<bool>(),
        meie in any::<bool>(),
        irq in any::<bool>(),
        mtvec in any::<u32>(),
        mscratch in any::<u32>(),
        mepc in any::<u32>(),
        mcause in any::<u32>(),
        mtval in any::<u32>(),
    ) -> Csrs {
        Csrs { mie, mpie, meie, irq, mtvec: mtvec & !3, mscratch, mepc: mepc & !3, mcause, mtval }
    }
}

/// An instruction with no memory access.
#[derive(Clone, Copy, Debug)]
enum Insn {
    Addi { rd: u32, imm: i32 },
    Jal { offset: i32 },
    Beq { offset: i32 },
}

impl Insn {
    fn word(self) -> u32 {
        match self {
            Insn::Addi { rd, imm } => addi(rd, rd, imm),
            Insn::Jal { offset } => jal(1, offset),
            Insn::Beq { offset } => beq(0, 0, offset),
        }
    }

    /// Its `next_pc` at `pc`, from the ISA manual.
    fn next(self, pc: u32) -> u32 {
        match self {
            Insn::Addi { .. } => pc.wrapping_add(4),
            Insn::Jal { offset } | Insn::Beq { offset } => pc.wrapping_add(offset as u32),
        }
    }
}

fn any_insn() -> impl Strategy<Value = Insn> {
    prop_oneof![
        (0u32..32, -2048i32..2048).prop_map(|(rd, imm)| Insn::Addi { rd, imm }),
        (-0x4_0000i32..0x4_0000).prop_map(|w| Insn::Jal { offset: w * 4 }),
        (-0x400i32..0x400).prop_map(|w| Insn::Beq { offset: w * 4 }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4_000))]

    /// §5.8: from any CSR state and level, at any aligned `pc` and with any instruction
    /// limit, one retirement's boundary is exactly the oracle's: the limit first, then
    /// `take_mei` on the next PC and the CSRs after the instruction.
    #[test]
    fn the_boundary_matches_the_oracle(
        csrs in any_csrs(),
        pc in any::<u32>(),
        insn in any_insn(),
        limit in prop_oneof![Just(1u64), Just(2), Just(LIMIT)],
    ) {
        let pc = pc & !3;
        let mut cpu = restore(limit, &forge(limit, pc, &[], 0, &csrs));
        let mut ctx = MockCtx::new();
        let before = Seen::from(csrs);
        prop_assert_eq!(seen(&cpu), before);
        let records = step(&mut cpu, &mut ctx, insn.word());
        prop_assert_eq!(records[0].0, COMMIT_KIND);
        let next = insn.next(pc);
        prop_assert_eq!(field(&records[0], "next_pc"), next);
        prop_assert_eq!(cpu.instret(), 1);
        let expected = boundary(1, limit, next, before.mstatus, before.mie, before.mip, before.mtvec);
        match expected {
            Boundary::InstructionLimit => {
                prop_assert_eq!(cpu.halt(), Some(Halt::InstructionLimit));
                prop_assert_eq!(records[1].0, HALT_KIND);
                prop_assert_eq!(records.len(), 2);
                prop_assert_eq!(cpu.pc(), next);
                prop_assert_eq!(seen(&cpu), before);
            }
            Boundary::Fetch(mei) => {
                prop_assert_eq!(cpu.halt(), None);
                prop_assert_eq!(cpu.pc(), mei.new_pc);
                prop_assert_eq!(seen(&cpu), before.after(&mei));
                if mei.taken {
                    prop_assert_eq!(&records[1..], &[interrupt(next, mei.new_pc)][..]);
                } else {
                    prop_assert_eq!(records.len(), 1);
                }
            }
        }
    }

    /// `mtvec` written with any raw value by the instruction that enables the interrupt:
    /// the handler is the oracle's `mtvec & !3` of the raw value.
    #[test]
    fn the_handler_is_mtvec_base_for_any_raw_write(
        raw in any::<u32>(),
        pc in any::<u32>(),
    ) {
        let pc = pc & !3;
        let csrs = Csrs { irq: true, ..ARMED };
        let (mut cpu, mut ctx) = cpu_at(pc, &[(2, raw)], csrs);
        let records = step(&mut cpu, &mut ctx, csrrw(0, 0x305, 2));
        let before = Seen::from(csrs);
        let mei = take_mei(pc.wrapping_add(4), before.mstatus, before.mie, before.mip, raw);
        prop_assert!(mei.taken);
        prop_assert_eq!(cpu.pc(), mei.new_pc);
        prop_assert_eq!(&records[1..], &[interrupt(pc.wrapping_add(4), mei.new_pc)][..]);
    }
}
