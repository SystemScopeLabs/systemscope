//! `Rv32iCpu` in the `M2` profile, driven directly (`docs/m2-design.md` §4–§6, M2.2a):
//! Zicsr and `MRET` at `Commit`, legality in both profiles, snapshot schema 2 and its
//! restore checks, inspect and trace, and checked `TxnId` allocation. The expected values
//! come from an oracle written from §4.3 and §5.3 that shares no code with the crate,
//! followed at the retirement boundary by the pure interrupt oracle of §5.8
//! ([`common::mei`]); the interrupt itself is tested in `cpu_mei.rs`.

mod common;

use std::num::NonZeroU64;

use common::asm::*;
use common::mei::{MEI_CAUSE, take_mei};
use common::{MockCtx, Traced};
use proptest::prelude::*;
use systemscope_contracts::component::Component;
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::mem_v1::{MemMsg, ReadOutcome, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_rv32i::cpu::{
    COMMIT, COMMIT_KIND, FETCH, INTERRUPT_KIND, SNAPSHOT_SCHEMA, SNAPSHOT_SCHEMA_M2, TRAP_KIND,
};
use systemscope_rv32i::{Halt, Reg, Rv32iConfig, Rv32iCpu, Rv32iProfile, RvTrap, TrapCause};

const ENTRY: u32 = 0x8000_0000;
const CLOCK: ClockDomainId = ClockDomainId(0);
const LIMIT: u64 = 1000;

const MSTATUS: u16 = 0x300;
const MIE: u16 = 0x304;
const MTVEC: u16 = 0x305;
const MSCRATCH: u16 = 0x340;
const MEPC: u16 = 0x341;
const MCAUSE: u16 = 0x342;
const MTVAL: u16 = 0x343;
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
fn csrrwi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(5, rd, uimm, csr)
}
fn csrrsi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(6, rd, uimm, csr)
}
fn csrrci(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(7, rd, uimm, csr)
}

fn config(profile: Rv32iProfile) -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(LIMIT).unwrap(),
        profile,
    }
}

fn start(profile: Rv32iProfile) -> (Rv32iCpu, MockCtx) {
    let mut cpu = Rv32iCpu::new(config(profile)).unwrap();
    let mut ctx = MockCtx::new();
    cpu.init(&mut ctx).unwrap();
    assert_eq!(ctx.take_wake().token, FETCH);
    (cpu, ctx)
}

fn fetch_word(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> u64 {
    ctx.wake(cpu, FETCH, Phase::Request).unwrap();
    let MemMsg::ReadReq { txn, addr, len: 4 } = ctx.take_sent() else {
        panic!("not a fetch");
    };
    assert_eq!(addr, u64::from(cpu.pc()));
    let resp = MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Data {
            data: insn.to_le_bytes().to_vec(),
        },
    };
    ctx.respond(cpu, resp).unwrap();
    let wake = ctx.take_wake();
    if wake.token == COMMIT {
        // CSR instructions and MRET keep M1's timing: commit in the same tick.
        assert_eq!((wake.when, wake.phase), (ScheduleWhen::Now, Phase::Commit));
    }
    wake.token
}

/// Runs `insn`, which must not touch memory, through its commit; returns every trace
/// record the commit made: one, or a retirement and an interrupt entry.
fn run_all(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<Traced> {
    assert_eq!(fetch_word(cpu, ctx, insn), COMMIT);
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    let records = std::mem::take(&mut ctx.traced);
    assert!(!records.is_empty());
    if records[0].0 == COMMIT_KIND {
        let next = ctx.take_wake();
        assert_eq!(next.token, FETCH);
        assert_eq!(
            (next.when, next.phase),
            (
                ScheduleWhen::Cycles {
                    domain: CLOCK,
                    k: 1
                },
                Phase::Request
            )
        );
    } else {
        assert!(ctx.woke.is_empty());
    }
    records
}

/// Runs `insn`, which must not touch memory, through its commit; returns the one trace
/// record it made.
fn run(
    cpu: &mut Rv32iCpu,
    ctx: &mut MockCtx,
    insn: u32,
) -> (&'static str, Vec<(&'static str, Value)>) {
    let mut records = run_all(cpu, ctx, insn);
    assert_eq!(records.len(), 1, "{records:?}");
    records.pop().unwrap()
}

/// Runs `insn` and requires it to retire; returns its `rv32.commit` fields.
fn retire(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<(&'static str, Value)> {
    let (kind, fields) = run(cpu, ctx, insn);
    assert_eq!(kind, COMMIT_KIND, "{fields:?}");
    fields
}

fn reg(cpu: &Rv32iCpu, i: u8) -> u32 {
    cpu.registers().read(Reg::new(i).unwrap())
}

/// Sets `x{rd}` to `value` with `LUI` and `ADDI`.
fn li(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, rd: u32, value: u32) {
    let lo = ((value & 0xfff) as i32) << 20 >> 20;
    let hi = value.wrapping_sub(lo as u32) >> 12;
    retire(cpu, ctx, lui(rd, hi));
    retire(cpu, ctx, addi(rd, rd, lo));
    assert_eq!(reg(cpu, rd as u8), value);
}

fn view_u(view: &StateView, name: &str) -> u32 {
    match view.get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn read(cpu: &Rv32iCpu, name: &str) -> u32 {
    view_u(&cpu.inspect(), name)
}

fn u(v: u32) -> Value {
    Value::U64(u64::from(v))
}

/// The M1 fields of a retirement at `pc` with no memory access.
fn m1_fields(
    pc: u32,
    insn: u32,
    rd: u32,
    rd_value: u32,
    next_pc: u32,
) -> Vec<(&'static str, Value)> {
    vec![
        ("pc", u(pc)),
        ("insn", u(insn)),
        ("rd", u(rd)),
        ("rd_value", u(if rd == 0 { 0 } else { rd_value })),
        ("next_pc", u(next_pc)),
    ]
}

fn csr_fields(
    pc: u32,
    insn: u32,
    rd: u32,
    old: u32,
    csr: u16,
    value: Option<u32>,
) -> Vec<(&'static str, Value)> {
    let mut f = m1_fields(pc, insn, rd, old, pc + 4);
    f.push(("csr", u(u32::from(csr))));
    if let Some(v) = value {
        f.push(("csr_value", u(v)));
    }
    f
}

fn assert_illegal(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) {
    let before = cpu.inspect();
    let pc = cpu.pc();
    let (kind, fields) = run(cpu, ctx, insn);
    assert_eq!(kind, TRAP_KIND, "{insn:#010x}: {fields:?}");
    assert_eq!(
        fields,
        vec![
            ("pc", u(pc)),
            ("insn", u(insn)),
            ("cause", Value::Str("IllegalInstruction".to_owned())),
            ("tval", u(insn)),
        ]
    );
    assert_eq!(
        cpu.halt(),
        Some(Halt::Trap(RvTrap {
            cause: TrapCause::IllegalInstruction,
            pc,
            tval: insn,
        }))
    );
    // Nothing retired and no architectural state changed, CSRs included.
    let after = cpu.inspect();
    let arch = |v: &StateView| {
        v.fields
            .iter()
            .filter(|(n, _)| !matches!(*n, "state" | "halt" | "cause" | "trap_pc" | "tval"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(arch(&after), arch(&before));
}

// ---------------------------------------------------------------------------------------
// Legality in both profiles.

#[test]
fn the_m1_profile_keeps_every_csr_instruction_and_mret_illegal() {
    for insn in [
        csrrw(1, MSCRATCH, 2),
        csrrs(1, MSTATUS, 0),
        csrrc(0, MIE, 3),
        csrrwi(1, MTVEC, 4),
        csrrsi(1, MEPC, 0),
        csrrci(1, MIP, 31),
        MRET,
    ] {
        let (mut cpu, mut ctx) = start(Rv32iProfile::M1);
        assert_illegal(&mut cpu, &mut ctx, insn);
        let fields: Vec<&str> = cpu.inspect().fields.iter().map(|f| f.0).collect();
        assert!(!fields.contains(&"mstatus"), "M1 inspect is unchanged");
        assert_eq!(cpu.snapshot_schema_version(), SNAPSHOT_SCHEMA);
    }
}

#[test]
fn the_m2_profile_keeps_every_other_system_encoding_illegal() {
    for insn in [
        0x1050_0073u32, // WFI
        0x1020_0073,    // SRET
        0x0020_0073,    // URET
        0x1200_0073,    // SFENCE.VMA
        csr_word(4, 1, 2, MSCRATCH),
        MRET | 1 << 7,
        MRET | 1 << 15,
    ] {
        let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
        assert_illegal(&mut cpu, &mut ctx, insn);
    }
    // ECALL and EBREAK keep their traps.
    for (insn, cause) in [(ECALL, "EnvironmentCall"), (EBREAK, "Breakpoint")] {
        let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
        let (kind, fields) = run(&mut cpu, &mut ctx, insn);
        assert_eq!(kind, TRAP_KIND);
        assert_eq!(fields[2].1, Value::Str(cause.to_owned()));
    }
}

#[test]
fn unsupported_csrs_are_illegal_even_when_the_write_is_suppressed() {
    // Including every CSR Spike implements and SystemScope does not (§4.5).
    for csr in [
        0x301u16, 0xf14, 0xf11, 0xf12, 0xf13, 0xf15, 0x310, 0x302, 0x303, 0x306, 0xb00, 0xb02,
        0xc00, 0xc01, 0xc02, 0x180, 0x3a0, 0x3b0, 0x7c0,
    ] {
        for insn in [
            csrrw(1, csr, 2),
            csrrw(0, csr, 0),
            csrrs(1, csr, 0),
            csrrc(1, csr, 0),
            csrrs(1, csr, 2),
            csrrwi(1, csr, 0),
            csrrsi(1, csr, 0),
            csrrci(0, csr, 0),
            csrrci(1, csr, 7),
        ] {
            let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
            li(&mut cpu, &mut ctx, 2, 0xffff_ffff);
            retire(&mut cpu, &mut ctx, csrrw(0, MSCRATCH, 2));
            assert_illegal(&mut cpu, &mut ctx, insn);
            assert_eq!(cpu.instret(), 3);
        }
    }
}

// ---------------------------------------------------------------------------------------
// The six forms at Commit.

#[test]
fn the_six_forms_read_the_old_value_and_write_as_their_form_says() {
    let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
    li(&mut cpu, &mut ctx, 5, 0x1234_5678);
    li(&mut cpu, &mut ctx, 6, 0x0000_ff00);
    let mut pc = cpu.pc();
    let mut step = |cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn, rd, old, value| {
        let fields = retire(cpu, ctx, insn);
        assert_eq!(
            fields,
            csr_fields(pc, insn, rd, old, MSCRATCH, value),
            "{insn:#010x}"
        );
        pc += 4;
        assert_eq!(cpu.pc(), pc);
        if rd != 0 {
            assert_eq!(reg(cpu, rd as u8), old);
        }
    };
    // CSRRW: mscratch = x5.
    step(
        &mut cpu,
        &mut ctx,
        csrrw(7, MSCRATCH, 5),
        7,
        0,
        Some(0x1234_5678),
    );
    // CSRRS: set x6's bits.
    step(
        &mut cpu,
        &mut ctx,
        csrrs(8, MSCRATCH, 6),
        8,
        0x1234_5678,
        Some(0x1234_ff78),
    );
    // CSRRC: clear x5's bits.
    step(
        &mut cpu,
        &mut ctx,
        csrrc(9, MSCRATCH, 5),
        9,
        0x1234_ff78,
        Some(0x0000_a900),
    );
    // CSRRWI: mscratch = uimm, zero-extended.
    step(
        &mut cpu,
        &mut ctx,
        csrrwi(10, MSCRATCH, 31),
        10,
        0x0000_a900,
        Some(31),
    );
    // CSRRSI and CSRRCI.
    step(
        &mut cpu,
        &mut ctx,
        csrrsi(11, MSCRATCH, 0x10),
        11,
        31,
        Some(31),
    );
    step(
        &mut cpu,
        &mut ctx,
        csrrci(12, MSCRATCH, 1),
        12,
        31,
        Some(30),
    );
    // rd = x0: still written, nothing in rd.
    step(&mut cpu, &mut ctx, csrrwi(0, MSCRATCH, 5), 0, 30, Some(5));
    assert_eq!(read(&cpu, "mscratch"), 5);
    assert_eq!(cpu.instret(), 11);
}

#[test]
fn suppressed_writes_read_but_do_not_write() {
    let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
    retire(&mut cpu, &mut ctx, csrrwi(0, MSCRATCH, 9));
    for insn in [
        csrrs(3, MSCRATCH, 0),
        csrrc(3, MSCRATCH, 0),
        csrrsi(3, MSCRATCH, 0),
        csrrci(3, MSCRATCH, 0),
    ] {
        let pc = cpu.pc();
        let fields = retire(&mut cpu, &mut ctx, insn);
        // No csr_value: the instruction does not write.
        assert_eq!(fields, csr_fields(pc, insn, 3, 9, MSCRATCH, None));
        assert_eq!(reg(&cpu, 3), 9);
        assert_eq!(read(&cpu, "mscratch"), 9);
    }
    // uimm 1 and 31 do write.
    let pc = cpu.pc();
    let fields = retire(&mut cpu, &mut ctx, csrrci(3, MSCRATCH, 1));
    assert_eq!(
        fields,
        csr_fields(pc, csrrci(3, MSCRATCH, 1), 3, 9, MSCRATCH, Some(8))
    );
    let fields = retire(&mut cpu, &mut ctx, csrrsi(3, MSCRATCH, 31));
    assert_eq!(
        fields,
        csr_fields(pc + 4, csrrsi(3, MSCRATCH, 31), 3, 8, MSCRATCH, Some(31))
    );
}

#[test]
fn the_operand_is_the_register_read_before_the_instruction() {
    // csrrw x5, mscratch, x5 swaps: rd's write happens after the operand was read.
    let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
    retire(&mut cpu, &mut ctx, csrrwi(0, MSCRATCH, 17));
    li(&mut cpu, &mut ctx, 5, 0xdead_beec);
    retire(&mut cpu, &mut ctx, csrrw(5, MSCRATCH, 5));
    assert_eq!(reg(&cpu, 5), 17);
    assert_eq!(read(&cpu, "mscratch"), 0xdead_beec);
}

#[test]
fn per_csr_rules_apply_at_commit_and_csr_value_is_the_value_read_back() {
    let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
    li(&mut cpu, &mut ctx, 1, 0xffff_ffff);
    li(&mut cpu, &mut ctx, 2, 0x8000_0002);
    for (insn, csr, value) in [
        (csrrw(0, MSTATUS, 1), MSTATUS, 0x1888),
        (csrrw(0, MSTATUS, 0), MSTATUS, 0x1800),
        (csrrw(0, MIE, 1), MIE, 0x800),
        (csrrw(0, MTVEC, 2), MTVEC, 0x8000_0000),
        (csrrw(0, MTVEC, 1), MTVEC, 0xffff_fffc),
        (csrrw(0, MEPC, 2), MEPC, 0x8000_0000),
        (csrrw(0, MEPC, 1), MEPC, 0xffff_fffc),
        (csrrw(0, MSCRATCH, 1), MSCRATCH, 0xffff_ffff),
        (csrrw(0, MCAUSE, 1), MCAUSE, 0xffff_ffff),
        (csrrw(0, MTVAL, 2), MTVAL, 0x8000_0002),
        // mip ignores the write, but the write happens: csr_value is mip as read.
        (csrrw(0, MIP, 1), MIP, 0),
        (csrrs(0, MIP, 1), MIP, 0),
        (csrrci(0, MIP, 31), MIP, 0),
    ] {
        let fields = retire(&mut cpu, &mut ctx, insn);
        assert_eq!(fields[5], ("csr", u(u32::from(csr))));
        assert_eq!(fields[6], ("csr_value", u(value)), "{insn:#010x}");
    }
    assert_eq!(
        [
            "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval"
        ]
        .map(|n| read(&cpu, n)),
        [
            0x1800,
            0x800,
            0,
            0xffff_fffc,
            0xffff_ffff,
            0xffff_fffc,
            0xffff_ffff,
            0x8000_0002
        ]
    );
    // csrrs mip, x0 reads and has no csr_value.
    let fields = retire(&mut cpu, &mut ctx, csrrs(4, MIP, 0));
    assert_eq!(fields.len(), 6);
}

#[test]
fn a_synchronous_trap_writes_no_csr() {
    for insn in [ECALL, EBREAK, 0xffff_ffff, lw(1, 2, 1)] {
        let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
        li(&mut cpu, &mut ctx, 1, 0x80);
        retire(&mut cpu, &mut ctx, csrrw(0, MSTATUS, 1));
        retire(&mut cpu, &mut ctx, csrrwi(0, MTVEC, 16));
        retire(&mut cpu, &mut ctx, csrrwi(0, MEPC, 8));
        retire(&mut cpu, &mut ctx, csrrwi(0, MCAUSE, 3));
        retire(&mut cpu, &mut ctx, csrrwi(0, MTVAL, 4));
        let csrs = *cpu.csrs();
        let (kind, _) = run(&mut cpu, &mut ctx, insn);
        assert_eq!(kind, TRAP_KIND);
        assert_eq!(*cpu.csrs(), csrs, "{insn:#010x}");
        assert!(matches!(cpu.halt(), Some(Halt::Trap(_))));
    }
}

// ---------------------------------------------------------------------------------------
// MRET.

#[test]
fn mret_restores_mie_jumps_to_mepc_and_retires() {
    for (before, after) in [(0x1880, 0x1888), (0x1808, 0x1880), (0x1888, 0x1888)] {
        let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
        li(&mut cpu, &mut ctx, 1, before);
        li(&mut cpu, &mut ctx, 2, 0x8000_0102);
        retire(&mut cpu, &mut ctx, csrrw(0, MSTATUS, 1));
        retire(&mut cpu, &mut ctx, csrrw(0, MEPC, 2));
        assert_eq!(read(&cpu, "mstatus"), before);
        let regs = cpu.registers().clone();
        let (pc, instret) = (cpu.pc(), cpu.instret());
        let fields = retire(&mut cpu, &mut ctx, MRET);
        // No register write, no csr fields; next_pc is mepc.
        assert_eq!(fields, m1_fields(pc, MRET, 0, 0, 0x8000_0100));
        assert_eq!(cpu.pc(), 0x8000_0100);
        assert_eq!(cpu.instret(), instret + 1);
        assert_eq!(cpu.registers(), &regs);
        assert_eq!(read(&cpu, "mstatus"), after, "{before:#x}");
        assert_eq!(read(&cpu, "mstatus") & 0x1800, 0x1800, "MPP stays");
        assert_eq!(read(&cpu, "mepc"), 0x8000_0100);
    }
}

// ---------------------------------------------------------------------------------------
// Inspect.

#[test]
fn m2_inspect_is_the_m1_fields_then_the_csrs() {
    let (m1, _) = start(Rv32iProfile::M1);
    let (m2, _) = start(Rv32iProfile::M2);
    let m1 = m1.inspect();
    let m2 = m2.inspect();
    assert_eq!(&m2.fields[..m1.fields.len()], &m1.fields[..]);
    let names: Vec<&str> = m2.fields[m1.fields.len()..].iter().map(|f| f.0).collect();
    assert_eq!(
        names,
        [
            "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval"
        ]
    );
    assert_eq!(view_u(&m2, "mstatus"), 0x1800);
    for n in &names[1..] {
        assert_eq!(view_u(&m2, n), 0);
    }
}

// ---------------------------------------------------------------------------------------
// Snapshots.

fn snapshot_of(cpu: &Rv32iCpu) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    cpu.snapshot(&mut w);
    w.into_bytes()
}

fn restore_as(profile: Rv32iProfile, schema: u32, bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    let mut cpu = Rv32iCpu::new(config(profile)).unwrap();
    let mut r = SnapshotReader::new(bytes);
    cpu.restore(&mut r, schema)?;
    r.finish().map_err(RestoreError::Decode)?;
    Ok(cpu)
}

fn restore_m2(bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    restore_as(Rv32iProfile::M2, SNAPSHOT_SCHEMA_M2, bytes)
}

/// Round-trips `cpu` (M2) and checks the copy is identical.
fn round_trip(cpu: &Rv32iCpu) -> Rv32iCpu {
    let bytes = snapshot_of(cpu);
    let copy = restore_m2(&bytes).unwrap();
    assert_eq!(snapshot_of(&copy), bytes);
    assert_eq!(copy.inspect(), cpu.inspect());
    copy
}

/// The CSR block of schema 2.
struct Csrs {
    mie: u8,
    mpie: u8,
    meie: u8,
    mtvec: u32,
    mscratch: u32,
    mepc: u32,
    mcause: u32,
    mtval: u32,
    irq: u8,
}

const RESET_CSRS: Csrs = Csrs {
    mie: 0,
    mpie: 0,
    meie: 0,
    mtvec: 0,
    mscratch: 0,
    mepc: 0,
    mcause: 0,
    mtval: 0,
    irq: 0,
};

/// A snapshot from its parts: `regs` are x1..x31; `state` writes the execution state.
fn forge(
    pc: u32,
    regs: &[u32; 31],
    next_txn: u64,
    state: impl FnOnce(&mut SnapshotWriter),
    csrs: Option<&Csrs>,
) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(LIMIT);
    w.u32(pc);
    for r in regs {
        w.u32(*r);
    }
    w.u64(0);
    w.u64(next_txn);
    state(&mut w);
    if let Some(c) = csrs {
        w.u8(c.mie);
        w.u8(c.mpie);
        w.u8(c.meie);
        w.u32(c.mtvec);
        w.u32(c.mscratch);
        w.u32(c.mepc);
        w.u32(c.mcause);
        w.u32(c.mtval);
        w.u8(c.irq);
    }
    w.into_bytes()
}

fn fetch_issue(w: &mut SnapshotWriter) {
    w.u8(0);
}

#[test]
fn the_profile_is_never_encoded() {
    // At reset, M2's bytes are M1's (schema 1, config block included) plus the CSR block.
    let (m1, _) = start(Rv32iProfile::M1);
    let (m2, _) = start(Rv32iProfile::M2);
    let (b1, b2) = (snapshot_of(&m1), snapshot_of(&m2));
    assert_eq!(b1, forge(ENTRY, &[0; 31], 0, fetch_issue, None));
    assert_eq!(
        b2,
        forge(ENTRY, &[0; 31], 0, fetch_issue, Some(&RESET_CSRS))
    );
    assert_eq!(&b2[..b1.len()], &b1[..]);
    assert_eq!(b2.len() - b1.len(), 3 + 5 * 4 + 1);
    assert_eq!(m1.snapshot_schema_version(), 1);
    assert_eq!(m2.snapshot_schema_version(), 2);
    // Each profile restores only its own schema.
    assert!(restore_as(Rv32iProfile::M1, SNAPSHOT_SCHEMA, &b1).is_ok());
    assert!(restore_as(Rv32iProfile::M2, SNAPSHOT_SCHEMA_M2, &b2).is_ok());
    for (profile, schema, bytes) in [
        (Rv32iProfile::M1, SNAPSHOT_SCHEMA_M2, &b1),
        (Rv32iProfile::M1, SNAPSHOT_SCHEMA_M2, &b2),
        (Rv32iProfile::M2, SNAPSHOT_SCHEMA, &b2),
        (Rv32iProfile::M2, SNAPSHOT_SCHEMA, &b1),
    ] {
        assert!(
            matches!(
                restore_as(profile, schema, bytes),
                Err(RestoreError::InvalidState(_))
            ),
            "{profile:?} schema {schema}"
        );
    }
    // Schema 1 bytes are not a schema 2 snapshot, nor the reverse.
    assert!(restore_m2(&b1).is_err());
    assert!(restore_as(Rv32iProfile::M1, SNAPSHOT_SCHEMA, &b2).is_err());
}

#[test]
fn the_schema_2_layout_is_schema_1_then_the_csrs() {
    let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
    li(&mut cpu, &mut ctx, 1, 0x88);
    retire(&mut cpu, &mut ctx, csrrw(0, MSTATUS, 1));
    retire(&mut cpu, &mut ctx, csrrwi(0, MTVEC, 20));
    retire(&mut cpu, &mut ctx, csrrwi(0, MEPC, 12));
    retire(&mut cpu, &mut ctx, csrrwi(0, MSCRATCH, 1));
    retire(&mut cpu, &mut ctx, csrrwi(0, MCAUSE, 2));
    retire(&mut cpu, &mut ctx, csrrwi(0, MTVAL, 3));
    li(&mut cpu, &mut ctx, 2, 0x800);
    retire(&mut cpu, &mut ctx, csrrs(0, MIE, 2));
    let mut regs = [0; 31];
    regs[0] = 0x88;
    regs[1] = 0x800;
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(LIMIT);
    w.u32(cpu.pc());
    for r in regs {
        w.u32(r);
    }
    w.u64(cpu.instret());
    w.u64(cpu.instret());
    w.u8(0);
    for b in [1u8, 1, 1] {
        w.u8(b);
    }
    for v in [20u32, 1, 12, 2, 3] {
        w.u32(v);
    }
    w.u8(0);
    assert_eq!(snapshot_of(&cpu), w.into_bytes());
}

/// A pending CSR operation is stored as tag 2: the CSR, the operation, whether it writes,
/// the operand, `rd`, and `next_pc`; `MRET` as tag 3.
#[test]
fn pending_csr_operations_are_stored_and_recomputed() {
    let mut regs = [0; 31];
    regs[4] = 0xf0f0_f0f0; // x5
    let pending = |insn: u32, body: &dyn Fn(&mut SnapshotWriter)| {
        forge(
            ENTRY,
            &regs,
            1,
            |w| {
                w.u8(4);
                w.u8(1);
                w.u32(insn);
                body(w);
            },
            Some(&RESET_CSRS),
        )
    };
    let csr_tag = |csr: u16, op: u8, write: u8, operand: u32, rd: u8| {
        move |w: &mut SnapshotWriter| {
            w.u8(2);
            w.u16(csr);
            w.u8(op);
            w.u8(write);
            w.u32(operand);
            w.u8(rd);
            w.u32(ENTRY + 4);
        }
    };
    type Tag = Box<dyn Fn(&mut SnapshotWriter)>;
    let cases: Vec<(u32, Tag)> = vec![
        (
            csrrw(3, MSCRATCH, 5),
            Box::new(csr_tag(MSCRATCH, 0, 1, 0xf0f0_f0f0, 3)),
        ),
        (
            csrrs(3, MSTATUS, 5),
            Box::new(csr_tag(MSTATUS, 1, 1, 0xf0f0_f0f0, 3)),
        ),
        (
            csrrc(0, MIE, 5),
            Box::new(csr_tag(MIE, 2, 1, 0xf0f0_f0f0, 0)),
        ),
        (csrrwi(3, MTVEC, 17), Box::new(csr_tag(MTVEC, 0, 1, 17, 3))),
        (csrrsi(3, MIP, 0), Box::new(csr_tag(MIP, 1, 0, 0, 3))),
        (csrrs(3, MEPC, 0), Box::new(csr_tag(MEPC, 1, 0, 0, 3))),
        (MRET, Box::new(|w: &mut SnapshotWriter| w.u8(3))),
    ];
    for (insn, body) in &cases {
        // What the CPU writes after fetching the word is exactly the forged bytes.
        let (mut cpu, mut ctx) = start(Rv32iProfile::M2);
        let reset = forge(ENTRY, &regs, 0, fetch_issue, Some(&RESET_CSRS));
        let mut r = SnapshotReader::new(&reset);
        cpu.restore(&mut r, SNAPSHOT_SCHEMA_M2).unwrap();
        assert_eq!(fetch_word(&mut cpu, &mut ctx, *insn), COMMIT);
        let bytes = pending(*insn, body.as_ref());
        assert_eq!(snapshot_of(&cpu), bytes, "{insn:#010x}");
        // It restores, and the copy commits exactly as the original.
        let mut copy = round_trip(&cpu);
        let mut copy_ctx = MockCtx::new();
        ctx = MockCtx::new();
        ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
        copy_ctx.wake(&mut copy, COMMIT, Phase::Commit).unwrap();
        assert_eq!(copy_ctx.traced, ctx.traced);
        assert_eq!(copy_ctx.woke, ctx.woke);
        assert_eq!(snapshot_of(&copy), snapshot_of(&cpu));
        // An M1 CPU refuses the new tags.
        let mut m1 = bytes.clone();
        m1.truncate(bytes.len() - 24);
        assert!(restore_as(Rv32iProfile::M1, SNAPSHOT_SCHEMA, &m1).is_err());
    }
    // A pending outcome that does not match its instruction is rejected.
    let reject = |bytes: Vec<u8>| {
        assert!(
            matches!(restore_m2(&bytes), Err(RestoreError::InvalidState(_))),
            "{:?}",
            restore_m2(&bytes).err()
        );
    };
    reject(pending(
        csrrw(3, MSCRATCH, 5),
        &csr_tag(MSCRATCH, 0, 1, 0xf0f0_f0f1, 3),
    ));
    reject(pending(
        csrrw(3, MSCRATCH, 5),
        &csr_tag(MSCRATCH, 1, 1, 0xf0f0_f0f0, 3),
    ));
    reject(pending(
        csrrw(3, MSCRATCH, 5),
        &csr_tag(MSCRATCH, 0, 0, 0xf0f0_f0f0, 3),
    ));
    reject(pending(
        csrrw(3, MSCRATCH, 5),
        &csr_tag(MSCRATCH, 0, 1, 0xf0f0_f0f0, 4),
    ));
    reject(pending(
        csrrw(3, MSCRATCH, 5),
        &csr_tag(MTVAL, 0, 1, 0xf0f0_f0f0, 3),
    ));
    reject(pending(csrrsi(3, MIP, 0), &csr_tag(MIP, 1, 1, 0, 3)));
    reject(pending(
        csrrw(3, 0x7c0, 5),
        &csr_tag(0x7c0, 0, 1, 0xf0f0_f0f0, 3),
    ));
    reject(pending(csrrw(3, MSCRATCH, 5), &|w: &mut SnapshotWriter| {
        w.u8(3)
    }));
    reject(pending(MRET, &csr_tag(MSCRATCH, 0, 1, 0xf0f0_f0f0, 3)));
    // An unsupported CSR's pending outcome is its trap.
    let trap = pending(csrrw(3, 0x7c0, 5), &|w: &mut SnapshotWriter| {
        w.u8(1);
        w.u8(2); // IllegalInstruction
        w.u32(csrrw(3, 0x7c0, 5));
    });
    restore_m2(&trap).unwrap();
    // Unknown codes fail to decode.
    assert!(
        restore_m2(&pending(
            csrrw(3, MSCRATCH, 5),
            &csr_tag(MSCRATCH, 3, 1, 0, 3)
        ))
        .is_err()
    );
    assert!(
        restore_m2(&pending(
            csrrw(3, MSCRATCH, 5),
            &csr_tag(MSCRATCH, 0, 2, 0, 3)
        ))
        .is_err()
    );
    assert!(restore_m2(&pending(MRET, &|w: &mut SnapshotWriter| w.u8(4))).is_err());
}

#[test]
fn restore_checks_the_csr_block() {
    let with = |c: Csrs| forge(ENTRY, &[0; 31], 0, fetch_issue, Some(&c));
    let ok = with(Csrs {
        mie: 1,
        mpie: 1,
        meie: 1,
        mtvec: 0xffff_fffc,
        mscratch: 0xffff_ffff,
        mepc: 0x8000_0004,
        mcause: 0x8000_000b,
        mtval: 7,
        irq: 1,
    });
    let cpu = restore_m2(&ok).unwrap();
    assert_eq!(snapshot_of(&cpu), ok);
    // irq_level = true is a legal snapshot and mip reads it.
    assert_eq!(read(&cpu, "mip"), 0x800);
    assert_eq!(read(&cpu, "mstatus"), 0x1888);
    assert_eq!(read(&cpu, "mie"), 0x800);
    for bad in [
        Csrs {
            mie: 2,
            ..RESET_CSRS
        },
        Csrs {
            mpie: 2,
            ..RESET_CSRS
        },
        Csrs {
            meie: 0xff,
            ..RESET_CSRS
        },
        Csrs {
            irq: 2,
            ..RESET_CSRS
        },
        Csrs {
            mtvec: 1,
            ..RESET_CSRS
        },
        Csrs {
            mtvec: 0x8000_0002,
            ..RESET_CSRS
        },
        Csrs {
            mepc: 3,
            ..RESET_CSRS
        },
        Csrs {
            mepc: 0x8000_0001,
            ..RESET_CSRS
        },
    ] {
        assert!(matches!(
            restore_m2(&with(bad)),
            Err(RestoreError::InvalidState(_))
        ));
    }
    // A truncated or extended block is rejected.
    let mut short = with(RESET_CSRS);
    short.pop();
    assert!(restore_m2(&short).is_err());
    let mut long = with(RESET_CSRS);
    long.push(0);
    assert!(restore_m2(&long).is_err());
}

#[test]
fn restored_m2_cpus_continue_identically_from_every_state() {
    let program = [
        lui(1, 0x80001),
        csrrw(0, MSCRATCH, 1),
        csrrs(2, MSCRATCH, 0),
        csrrwi(0, MSTATUS, 8),
        addi(3, 0, 0x80),
        csrrs(0, MSTATUS, 3),
        csrrsi(4, MIP, 0),
        csrrw(0, MEPC, 1),
        csrrci(0, MSTATUS, 8),
        MRET,
    ];
    // Drive every instruction, snapshotting at every event boundary.
    type Step = Box<dyn Fn(&mut Rv32iCpu, &mut MockCtx) -> Result<(), SimError>>;
    let mut steps: Vec<Step> = Vec::new();
    for (i, insn) in program.into_iter().enumerate() {
        let txn = TxnId(i as u64);
        steps.push(Box::new(|c: &mut Rv32iCpu, x: &mut MockCtx| {
            x.wake(c, FETCH, Phase::Request)
        }));
        steps.push(Box::new(move |c: &mut Rv32iCpu, x: &mut MockCtx| {
            x.respond(
                c,
                MemMsg::ReadResp {
                    txn,
                    outcome: ReadOutcome::Data {
                        data: insn.to_le_bytes().to_vec(),
                    },
                },
            )
        }));
        steps.push(Box::new(|c: &mut Rv32iCpu, x: &mut MockCtx| {
            x.wake(c, COMMIT, Phase::Commit)
        }));
    }
    let drive = |split: usize| {
        let (mut original, mut ctx) = start(Rv32iProfile::M2);
        for step in &steps[..split] {
            step(&mut original, &mut ctx).unwrap();
        }
        let mut copy = round_trip(&original);
        let (mut a, mut b) = (MockCtx::new(), MockCtx::new());
        for step in &steps[split..] {
            step(&mut original, &mut a).unwrap();
            step(&mut copy, &mut b).unwrap();
            assert_eq!(copy.inspect(), original.inspect());
        }
        assert_eq!((b.sent, b.woke, b.traced), (a.sent, a.woke, a.traced));
        assert_eq!(snapshot_of(&copy), snapshot_of(&original));
        original
    };
    for split in 0..=steps.len() {
        let end = drive(split);
        assert_eq!(end.pc(), 0x8000_1000);
        assert_eq!(read(&end, "mstatus"), 0x1888);
        assert_eq!(reg(&end, 2), 0x8000_1000);
        assert_eq!(end.instret(), program.len() as u64);
    }
}

// ---------------------------------------------------------------------------------------
// TxnId.

#[test]
fn m2_txn_allocation_never_wraps() {
    let bytes = forge(ENTRY, &[0; 31], u64::MAX, fetch_issue, Some(&RESET_CSRS));
    let mut cpu = restore_m2(&bytes).unwrap();
    let mut ctx = MockCtx::new();
    let before = cpu.inspect();
    let result = ctx.wake(&mut cpu, FETCH, Phase::Request);
    assert!(
        matches!(result, Err(SimError::ComponentFault(_))),
        "{result:?}"
    );
    assert!(ctx.sent.is_empty() && ctx.woke.is_empty() && ctx.traced.is_empty());
    assert_eq!(cpu.inspect(), before);
    assert_eq!(snapshot_of(&cpu), bytes, "the counter keeps its value");
    // The last TxnId below the limit is still issued.
    let bytes = forge(
        ENTRY,
        &[0; 31],
        u64::MAX - 1,
        fetch_issue,
        Some(&RESET_CSRS),
    );
    let mut cpu = restore_m2(&bytes).unwrap();
    ctx.wake(&mut cpu, FETCH, Phase::Request).unwrap();
    let MemMsg::ReadReq { txn, .. } = ctx.take_sent() else {
        panic!("not a fetch");
    };
    assert_eq!(txn, TxnId(u64::MAX - 1));
}

// ---------------------------------------------------------------------------------------
// The CPU against the oracle.

/// The CSRs as software reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Model {
    mstatus: u32,
    mie: u32,
    mip: u32,
    mtvec: u32,
    mscratch: u32,
    mepc: u32,
    mcause: u32,
    mtval: u32,
}

impl Model {
    fn get(&self, csr: u16) -> Option<u32> {
        Some(match csr {
            0x300 => self.mstatus,
            0x304 => self.mie,
            0x344 => self.mip,
            0x305 => self.mtvec,
            0x340 => self.mscratch,
            0x341 => self.mepc,
            0x342 => self.mcause,
            0x343 => self.mtval,
            _ => return None,
        })
    }

    fn set(&mut self, csr: u16, v: u32) {
        match csr {
            0x300 => self.mstatus = 0x1800 | (v & 0x88),
            0x304 => self.mie = v & 0x800,
            0x344 => {}
            0x305 => self.mtvec = v & 0xffff_fffc,
            0x340 => self.mscratch = v,
            0x341 => self.mepc = v & 0xffff_fffc,
            0x342 => self.mcause = v,
            0x343 => self.mtval = v,
            _ => unreachable!(),
        }
    }

    fn csrs(&self) -> Csrs {
        Csrs {
            mie: (self.mstatus >> 3 & 1) as u8,
            mpie: (self.mstatus >> 7 & 1) as u8,
            meie: (self.mie >> 11) as u8,
            mtvec: self.mtvec,
            mscratch: self.mscratch,
            mepc: self.mepc,
            mcause: self.mcause,
            mtval: self.mtval,
            irq: (self.mip >> 11) as u8,
        }
    }

    fn read_from(view: &StateView) -> Model {
        Model {
            mstatus: view_u(view, "mstatus"),
            mie: view_u(view, "mie"),
            mip: view_u(view, "mip"),
            mtvec: view_u(view, "mtvec"),
            mscratch: view_u(view, "mscratch"),
            mepc: view_u(view, "mepc"),
            mcause: view_u(view, "mcause"),
            mtval: view_u(view, "mtval"),
        }
    }
}

/// What the oracle expects of one instruction.
#[derive(Debug, PartialEq, Eq)]
enum Expect {
    Retire {
        pc: u32,
        rd: Option<(u8, u32)>,
        csrs: Model,
        csr_value: Option<(u16, Option<u32>)>,
    },
    Illegal,
}

/// §4.2–§4.6 and §5.3, from the word's bits.
fn oracle(m: &Model, regs: &[u32; 31], pc: u32, word: u32) -> Expect {
    let x = |i: u32| if i == 0 { 0 } else { regs[i as usize - 1] };
    if word == 0x3020_0073 {
        let mut n = *m;
        n.mstatus = 0x1800 | (m.mstatus >> 7 & 1) << 3 | 0x80;
        return Expect::Retire {
            pc: m.mepc,
            rd: None,
            csrs: n,
            csr_value: None,
        };
    }
    let funct3 = word >> 12 & 7;
    let (rd, field, csr) = (word >> 7 & 31, word >> 15 & 31, (word >> 20) as u16);
    assert!(word & 0x7f == 0x73 && funct3 != 0 && funct3 != 4);
    let Some(old) = m.get(csr) else {
        return Expect::Illegal;
    };
    let operand = if funct3 >= 5 { field } else { x(field) };
    let writes = funct3 & 3 == 1 || field != 0;
    let mut n = *m;
    let mut csr_value = None;
    if writes {
        n.set(
            csr,
            match funct3 & 3 {
                1 => operand,
                2 => old | operand,
                _ => old & !operand,
            },
        );
        csr_value = n.get(csr);
    }
    Expect::Retire {
        pc: pc.wrapping_add(4),
        rd: (rd != 0).then_some((rd as u8, old)),
        csrs: n,
        csr_value: Some((csr, csr_value)),
    }
}

prop_compose! {
    fn any_model()(
        mie in any::<bool>(),
        mpie in any::<bool>(),
        meie in any::<bool>(),
        irq in any::<bool>(),
        mtvec in any::<u32>(),
        mscratch in any::<u32>(),
        mepc in any::<u32>(),
        mcause in any::<u32>(),
        mtval in any::<u32>(),
    ) -> Model {
        Model {
            mstatus: 0x1800 | u32::from(mie) << 3 | u32::from(mpie) << 7,
            mie: u32::from(meie) << 11,
            mip: u32::from(irq) << 11,
            mtvec: mtvec & !3,
            mscratch,
            mepc: mepc & !3,
            mcause,
            mtval,
        }
    }
}

fn any_word() -> impl Strategy<Value = u32> {
    let csr = prop_oneof![
        4 => proptest::sample::select(vec![
            MSTATUS, MIE, MTVEC, MSCRATCH, MEPC, MCAUSE, MTVAL, MIP
        ]),
        1 => 0u16..0x1000,
    ];
    prop_oneof![
        8 => (
            proptest::sample::select(vec![1u32, 2, 3, 5, 6, 7]),
            0u32..32,
            0u32..32,
            csr,
        )
            .prop_map(|(f3, rd, field, csr)| csr_word(f3, rd, field, csr)),
        1 => Just(MRET),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4_000))]

    /// One CSR instruction or `MRET` from any CSR and register state and any `irq` level:
    /// the M2 CPU retires or traps exactly as the oracle says, then at the retirement
    /// boundary takes the interrupt exactly as `take_mei` says for the CSRs after the
    /// instruction (§5.1, §5.5, §5.8); a trap never samples. The M1 CPU always traps.
    #[test]
    fn one_instruction_matches_the_oracle(
        m in any_model(),
        regs in proptest::array::uniform31(any::<u32>()),
        word in any_word(),
    ) {
        let pc = ENTRY + 0x40;
        let bytes = forge(pc, &regs, 0, fetch_issue, Some(&m.csrs()));
        let mut cpu = restore_m2(&bytes).unwrap();
        prop_assert_eq!(Model::read_from(&cpu.inspect()), m);
        let mut ctx = MockCtx::new();
        let expected = oracle(&m, &regs, pc, word);
        let records = run_all(&mut cpu, &mut ctx, word);
        let (kind, fields) = records[0].clone();
        match expected {
            Expect::Illegal => {
                prop_assert_eq!(records.len(), 1);
                prop_assert_eq!(kind, TRAP_KIND);
                prop_assert_eq!(&fields[3], &("tval", u(word)));
                prop_assert_eq!(Model::read_from(&cpu.inspect()), m);
                prop_assert_eq!(cpu.instret(), 0);
            }
            Expect::Retire { pc: next, rd, csrs, csr_value } => {
                prop_assert_eq!(kind, COMMIT_KIND);
                let mei = take_mei(next, csrs.mstatus, csrs.mie, csrs.mip, csrs.mtvec);
                let mut after = csrs;
                after.mstatus = mei.new_mstatus;
                after.mepc = mei.mepc.unwrap_or(after.mepc);
                after.mcause = mei.mcause.unwrap_or(after.mcause);
                after.mtval = mei.mtval.unwrap_or(after.mtval);
                prop_assert_eq!(cpu.pc(), mei.new_pc);
                prop_assert_eq!(cpu.instret(), 1);
                prop_assert_eq!(Model::read_from(&cpu.inspect()), after);
                if mei.taken {
                    prop_assert_eq!(
                        &records[1..],
                        &[(
                            INTERRUPT_KIND,
                            vec![
                                ("mepc", u(next)),
                                ("mcause", u(MEI_CAUSE)),
                                ("handler", u(mei.new_pc)),
                            ],
                        )][..]
                    );
                } else {
                    prop_assert_eq!(records.len(), 1);
                }
                let mut want = regs;
                if let Some((rd, v)) = rd {
                    want[rd as usize - 1] = v;
                }
                let got: Vec<u32> = (1..32).map(|i| reg(&cpu, i)).collect();
                prop_assert_eq!(got, want.to_vec());
                let (rd, rd_value) = rd.map_or((0, 0), |(r, v)| (u32::from(r), v));
                let mut want = m1_fields(pc, word, rd, rd_value, next);
                if let Some((csr, value)) = csr_value {
                    want.push(("csr", u(u32::from(csr))));
                    if let Some(v) = value {
                        want.push(("csr_value", u(v)));
                    }
                }
                prop_assert_eq!(fields, want);
            }
        }

        let bytes = forge(pc, &regs, 0, fetch_issue, None);
        let mut m1 = restore_as(Rv32iProfile::M1, SNAPSHOT_SCHEMA, &bytes).unwrap();
        let (kind, _) = run(&mut m1, &mut MockCtx::new(), word);
        prop_assert_eq!(kind, TRAP_KIND);
    }
}
