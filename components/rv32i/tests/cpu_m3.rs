//! `Rv32iCpu` in the `M3` profile, driven directly (`docs/m3-design.md` §5.1, §5.3, §5.5,
//! §5.6, M3.2): modes, the supervisor CSR subset and the access rule, delegated
//! synchronous exceptions, `MRET` and `SRET`, `SFENCE.VMA`, `satp` with Bare and Sv32 (the
//! walk itself is `cpu_sv32.rs`'s), the machine external interrupt with modes, inspect and
//! trace, and snapshot schema 3 with its restore checks. Expected values are spelled out from the design; the interrupt's come from the
//! independent oracle [`common::mei_m3`].

mod common;

use std::num::NonZeroU64;

use common::asm::*;
use common::mei_m3::{MEI_CAUSE, take_mei_m3};
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
    COMMIT, COMMIT_KIND, EXCEPTION_KIND, FETCH, HALT_KIND, INTERRUPT_KIND, MEMORY,
    SNAPSHOT_SCHEMA_M2, SNAPSHOT_SCHEMA_M3, TRAP_KIND,
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
const SSTATUS: u16 = 0x100;
const SIE: u16 = 0x104;
const STVEC: u16 = 0x105;
const SSCRATCH: u16 = 0x140;
const SEPC: u16 = 0x141;
const SCAUSE: u16 = 0x142;
const STVAL: u16 = 0x143;
const SIP: u16 = 0x144;
const SATP: u16 = 0x180;
const MEDELEG: u16 = 0x302;
const MIDELEG: u16 = 0x303;

const MRET: u32 = 0x3020_0073;
const SRET: u32 = 0x1020_0073;
const WFI: u32 = 0x1050_0073;
const SFENCE_VMA: u32 = 0x1200_0073;

/// Mode encodings.
const U: u8 = 0;
const S: u8 = 1;
const M: u8 = 3;

/// `mstatus` bits.
const SIE_BIT: u32 = 1 << 1;
const MIE_BIT: u32 = 1 << 3;
const SPIE_BIT: u32 = 1 << 5;
const MPIE_BIT: u32 = 1 << 7;
const SPP_BIT: u32 = 1 << 8;
const SUM_BIT: u32 = 1 << 18;
const MXR_BIT: u32 = 1 << 19;

/// The handler every test that delegates uses.
const STVEC_BASE: u32 = 0x8000_4000;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}
fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(1, rd, rs1, csr)
}
fn csrrs(rd: u32, csr: u16, rs1: u32) -> u32 {
    csr_word(2, rd, rs1, csr)
}
fn csrrwi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(5, rd, uimm, csr)
}
fn csrrsi(rd: u32, csr: u16, uimm: u32) -> u32 {
    csr_word(6, rd, uimm, csr)
}

fn config(profile: Rv32iProfile) -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(LIMIT).unwrap(),
        profile,
    }
}

fn start() -> (Rv32iCpu, MockCtx) {
    let mut cpu = Rv32iCpu::new(config(Rv32iProfile::M3)).unwrap();
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
        assert_eq!((wake.when, wake.phase), (ScheduleWhen::Now, Phase::Commit));
    }
    wake.token
}

/// Requires the next wake to be the fetch, one cycle later.
fn expect_next_fetch(ctx: &mut MockCtx) {
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
}

/// Takes what a commit left: its trace records, and the fetch it scheduled unless the CPU
/// halted.
fn after_commit(ctx: &mut MockCtx) -> Vec<Traced> {
    let records = std::mem::take(&mut ctx.traced);
    assert!(!records.is_empty());
    let halted = records[0].0 == TRAP_KIND || records.iter().any(|r| r.0 == HALT_KIND);
    if halted {
        assert!(ctx.woke.is_empty());
    } else {
        expect_next_fetch(ctx);
    }
    records
}

/// Runs `insn`, which must not touch memory, through its commit; returns every trace
/// record the commit made.
fn run_all(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<Traced> {
    assert_eq!(fetch_word(cpu, ctx, insn), COMMIT);
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    after_commit(ctx)
}

/// Runs `insn` and returns the one trace record it made.
fn run(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Traced {
    let mut records = run_all(cpu, ctx, insn);
    assert_eq!(records.len(), 1, "{records:?}");
    records.pop().unwrap()
}

/// Runs `insn` and requires it to retire; returns its `rv32.commit` fields.
fn retire(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) -> Vec<(&'static str, Value)> {
    let (kind, fields) = run(cpu, ctx, insn);
    assert_eq!(kind, COMMIT_KIND, "{insn:#010x}: {fields:?}");
    fields
}

/// Runs the load or store `insn` against a memory that answers `response`; returns every
/// trace record its commit made.
fn run_memory(
    cpu: &mut Rv32iCpu,
    ctx: &mut MockCtx,
    insn: u32,
    response: impl FnOnce(TxnId) -> MemMsg,
) -> Vec<Traced> {
    assert_eq!(fetch_word(cpu, ctx, insn), MEMORY);
    ctx.wake(cpu, MEMORY, Phase::Request).unwrap();
    let txn = match ctx.take_sent() {
        MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => txn,
        other => panic!("{other:?}"),
    };
    ctx.respond(cpu, response(txn)).unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    ctx.wake(cpu, COMMIT, Phase::Commit).unwrap();
    after_commit(ctx)
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

/// Writes `value` to `csr` through `x31`; the instruction must retire.
fn csrw(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, csr: u16, value: u32) {
    li(cpu, ctx, 31, value);
    retire(cpu, ctx, csrrw(0, csr, 31));
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

fn mode(cpu: &Rv32iCpu) -> u8 {
    read(cpu, "priv") as u8
}

fn u(v: u32) -> Value {
    Value::U64(u64::from(v))
}

fn s(v: &str) -> Value {
    Value::Str(v.to_owned())
}

/// The `rv32.commit` fields of a retirement with no memory access in mode `priv`.
fn fields(
    pc: u32,
    insn: u32,
    rd: u32,
    rd_value: u32,
    next_pc: u32,
    privilege: u8,
) -> Vec<(&'static str, Value)> {
    vec![
        ("pc", u(pc)),
        ("insn", u(insn)),
        ("rd", u(rd)),
        ("rd_value", u(if rd == 0 { 0 } else { rd_value })),
        ("next_pc", u(next_pc)),
        ("priv", u(u32::from(privilege))),
    ]
}

/// From M, enters `target` at `pc` with `MRET`, leaving `mstatus` = `extra` | MPIE
/// (MRET sets it) with `MIE` = `extra`'s `MPIE`.
fn enter(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, target: u8, pc: u32, extra: u32) {
    assert_eq!(mode(cpu), M);
    csrw(cpu, ctx, MSTATUS, extra | u32::from(target) << 11);
    csrw(cpu, ctx, MEPC, pc);
    let from = cpu.pc();
    assert_eq!(retire(cpu, ctx, MRET), fields(from, MRET, 0, 0, pc, M));
    assert_eq!((mode(cpu), cpu.pc()), (target, pc));
}

/// Delegates every delegable cause to a handler at [`STVEC_BASE`].
fn delegate_all(cpu: &mut Rv32iCpu, ctx: &mut MockCtx) {
    csrw(cpu, ctx, STVEC, STVEC_BASE);
    csrw(cpu, ctx, MEDELEG, 0xffff_ffff);
    assert_eq!(read(cpu, "medeleg"), 0xB1FF);
}

/// The inspect fields that are architectural state, not the halt.
fn arch(view: &StateView) -> Vec<(&'static str, Value)> {
    view.fields
        .iter()
        .filter(|(n, _)| !matches!(*n, "state" | "halt" | "cause" | "trap_pc" | "tval"))
        .cloned()
        .collect()
}

/// Runs `insn`, which must halt the CPU with `cause` (M3 name `name`) and `tval`, changing
/// no architectural state.
fn assert_halts(
    cpu: &mut Rv32iCpu,
    ctx: &mut MockCtx,
    insn: u32,
    cause: TrapCause,
    name: &str,
    tval: u32,
) {
    let before = cpu.inspect();
    let pc = cpu.pc();
    let (kind, f) = run(cpu, ctx, insn);
    assert_eq!(kind, TRAP_KIND, "{insn:#010x}: {f:?}");
    assert_eq!(
        f,
        vec![
            ("pc", u(pc)),
            ("insn", u(insn)),
            ("cause", s(name)),
            ("tval", u(tval)),
        ]
    );
    assert_eq!(cpu.halt(), Some(Halt::Trap(RvTrap { cause, pc, tval })));
    assert_eq!(arch(&cpu.inspect()), arch(&before));
    assert_eq!(read_str(cpu, "cause"), name);
}

fn assert_illegal(cpu: &mut Rv32iCpu, ctx: &mut MockCtx, insn: u32) {
    assert_halts(
        cpu,
        ctx,
        insn,
        TrapCause::IllegalInstruction,
        "IllegalInstruction",
        insn,
    );
}

fn read_str(cpu: &Rv32iCpu, name: &str) -> String {
    match cpu.inspect().get(name) {
        Some(Value::Str(v)) => v.clone(),
        other => panic!("{name}: {other:?}"),
    }
}

/// Runs `insn`, which must be delegated to S: checks the `rv32.exception` record and the
/// entry (§5.3), and that nothing retired.
fn assert_delegated(
    cpu: &mut Rv32iCpu,
    ctx: &mut MockCtx,
    insn: u32,
    name: &str,
    code: u32,
    tval: u32,
) {
    let (pc, from) = (cpu.pc(), mode(cpu));
    let sie = read(cpu, "sstatus") & SIE_BIT != 0;
    let (instret, regs, mstatus) = (cpu.instret(), cpu.registers().clone(), read(cpu, "mstatus"));
    let machine = ["mtvec", "mscratch", "mepc", "mcause", "mtval", "mie"].map(|n| read(cpu, n));
    let (kind, f) = run(cpu, ctx, insn);
    assert_eq!(kind, EXCEPTION_KIND, "{insn:#010x}: {f:?}");
    let from_name = ["U", "S", "", "M"][usize::from(from)];
    assert_eq!(
        f,
        vec![
            ("pc", u(pc)),
            ("insn", u(insn)),
            ("cause", s(name)),
            ("tval", u(tval)),
            ("from", s(from_name)),
            ("to", s("S")),
        ]
    );
    assert_eq!(cpu.halt(), None);
    assert_eq!(read_str(cpu, "state"), "fetch_issue");
    assert_eq!((mode(cpu), cpu.pc()), (S, STVEC_BASE));
    assert_eq!(read(cpu, "sepc"), pc);
    assert_eq!(read(cpu, "scause"), code);
    assert_eq!(read(cpu, "stval"), tval);
    let sstatus = read(cpu, "sstatus");
    assert_eq!(sstatus & SIE_BIT, 0, "SIE <- 0");
    assert_eq!(sstatus & SPIE_BIT != 0, sie, "SPIE <- SIE");
    assert_eq!(sstatus & SPP_BIT != 0, from == S, "SPP <- from");
    // Only SIE, SPIE, and SPP change in mstatus; no machine CSR changes; nothing retires.
    let mask = SIE_BIT | SPIE_BIT | SPP_BIT;
    assert_eq!(read(cpu, "mstatus") & !mask, mstatus & !mask);
    assert_eq!(
        ["mtvec", "mscratch", "mepc", "mcause", "mtval", "mie"].map(|n| read(cpu, n)),
        machine
    );
    assert_eq!(cpu.instret(), instret);
    assert_eq!(cpu.registers(), &regs);
}

// ---------------------------------------------------------------------------------------
// Reset, inspect, and the M1/M2 profiles.

#[test]
fn m3_resets_in_m_with_every_csr_zero_and_inspect_appends_priv_and_the_csrs() {
    let (cpu, _) = start();
    let (m1, _) = {
        let mut c = Rv32iCpu::new(config(Rv32iProfile::M1)).unwrap();
        let mut x = MockCtx::new();
        c.init(&mut x).unwrap();
        (c, x)
    };
    let (m1, m3) = (m1.inspect(), cpu.inspect());
    assert_eq!(&m3.fields[..m1.fields.len()], &m1.fields[..]);
    let names: Vec<&str> = m3.fields[m1.fields.len()..].iter().map(|f| f.0).collect();
    assert_eq!(
        names,
        [
            "priv", "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval",
            "sstatus", "sie", "sip", "stvec", "sscratch", "sepc", "scause", "stval", "satp",
            "medeleg", "mideleg"
        ]
    );
    assert_eq!(view_u(&m3, "priv"), 3);
    // MPP resets to U, so mstatus reads 0.
    for n in &names[1..] {
        assert_eq!(view_u(&m3, n), 0, "{n}");
    }
    assert_eq!(cpu.snapshot_schema_version(), SNAPSHOT_SCHEMA_M3);
}

#[test]
fn m1_and_m2_keep_sret_sfence_and_supervisor_csrs_illegal() {
    for profile in [Rv32iProfile::M1, Rv32iProfile::M2] {
        for insn in [
            SRET,
            SFENCE_VMA,
            WFI,
            csrrs(1, SSTATUS, 0),
            csrrs(1, SATP, 0),
            csrrs(1, MEDELEG, 0),
        ] {
            let mut cpu = Rv32iCpu::new(config(profile)).unwrap();
            let mut ctx = MockCtx::new();
            cpu.init(&mut ctx).unwrap();
            ctx.take_wake();
            fetch_word(&mut cpu, &mut ctx, insn);
            ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
            let traced = std::mem::take(&mut ctx.traced);
            assert_eq!(traced.len(), 1);
            assert_eq!(traced[0].0, TRAP_KIND);
            assert_eq!(traced[0].1[2].1, s("IllegalInstruction"));
            assert!(!cpu.inspect().fields.iter().any(|f| f.0 == "priv"));
        }
    }
}

// ---------------------------------------------------------------------------------------
// The CSR subset and the access rule.

#[test]
fn every_whitelisted_csr_follows_its_m3_rule() {
    let (mut cpu, mut ctx) = start();
    li(&mut cpu, &mut ctx, 1, 0xffff_ffff);
    for (csr, value) in [
        // mstatus: SIE MIE SPIE MPIE SPP MPP=M SUM MXR.
        (MSTATUS, 0x000c_19aa),
        (MIE, 0x800),
        (MIP, 0),
        (MTVEC, 0xffff_fffc),
        (MSCRATCH, 0xffff_ffff),
        (MEPC, 0xffff_fffc),
        (MCAUSE, 0xffff_ffff),
        (MTVAL, 0xffff_ffff),
        (SSTATUS, 0x000c_0122),
        (SIE, 0),
        (SIP, 0),
        (STVEC, 0xffff_fffc),
        (SSCRATCH, 0xffff_ffff),
        (SEPC, 0xffff_fffc),
        (SCAUSE, 0xffff_ffff),
        (STVAL, 0xffff_ffff),
        // Sv32 is supported from M3.3: MODE and PPN stored, ASID 0. M stays bare.
        (SATP, 0x803f_ffff),
        (MEDELEG, 0xB1FF),
        (MIDELEG, 0),
    ] {
        let f = retire(&mut cpu, &mut ctx, csrrw(0, csr, 1));
        assert_eq!(f[6], ("csr", u(u32::from(csr))));
        assert_eq!(f[7], ("csr_value", u(value)), "{csr:#05x}");
    }
    for (csr, value) in [(MSTATUS, 0), (SSTATUS, 0)] {
        let f = retire(&mut cpu, &mut ctx, csrrw(0, csr, 0));
        assert_eq!(f[7], ("csr_value", u(value)), "{csr:#05x}");
    }
}

#[test]
fn mstatus_mpp_is_warl_and_the_reserved_encoding_reads_u() {
    let (mut cpu, mut ctx) = start();
    for (mpp, stored) in [(0u32, 0u32), (1, 1), (2, 0), (3, 3)] {
        csrw(&mut cpu, &mut ctx, MSTATUS, mpp << 11);
        assert_eq!(read(&cpu, "mstatus"), stored << 11, "MPP {mpp}");
        // Set and clear go through the same rule.
        retire(&mut cpu, &mut ctx, csrrw(0, MSTATUS, 0));
        li(&mut cpu, &mut ctx, 2, mpp << 11);
        let f = retire(&mut cpu, &mut ctx, csrrs(0, MSTATUS, 2));
        assert_eq!(f[7], ("csr_value", u(stored << 11)));
    }
    // MRET with a written 0b10 returns to U.
    csrw(&mut cpu, &mut ctx, MSTATUS, 0b10 << 11);
    csrw(&mut cpu, &mut ctx, MEPC, 0x8000_0100);
    retire(&mut cpu, &mut ctx, MRET);
    assert_eq!(mode(&cpu), U);
}

#[test]
fn sstatus_is_a_view_of_the_s_bits_of_mstatus() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MSTATUS, 0xffff_ffff);
    assert_eq!(read(&cpu, "mstatus"), 0x000c_19aa);
    assert_eq!(read(&cpu, "sstatus"), 0x000c_0122);
    // An sstatus write changes only the S bits.
    csrw(&mut cpu, &mut ctx, SSTATUS, 0);
    assert_eq!(read(&cpu, "mstatus"), 0x0000_1888);
    assert_eq!(read(&cpu, "sstatus"), 0);
    csrw(&mut cpu, &mut ctx, SSTATUS, SPP_BIT | SUM_BIT);
    assert_eq!(read(&cpu, "mstatus"), 0x0004_1988);
}

#[test]
fn stvec_and_sepc_drop_their_low_bits_and_scause_stval_are_full() {
    let (mut cpu, mut ctx) = start();
    for (csr, name, value, stored) in [
        (STVEC, "stvec", 0x1234_5677, 0x1234_5674),
        (SEPC, "sepc", 0x8000_0003, 0x8000_0000),
        (SCAUSE, "scause", 0x8000_0007, 0x8000_0007),
        (STVAL, "stval", 0xdead_beef, 0xdead_beef),
        (SSCRATCH, "sscratch", 0x0bad_f00d, 0x0bad_f00d),
    ] {
        csrw(&mut cpu, &mut ctx, csr, value);
        assert_eq!(read(&cpu, name), stored);
    }
}

#[test]
fn medeleg_keeps_only_its_mask_and_mideleg_sie_sip_read_zero() {
    let (mut cpu, mut ctx) = start();
    for (value, stored) in [
        (0xffff_ffffu32, 0xB1FFu32),
        (1 << 9, 0),
        (1 << 11, 0),
        (1 << 10, 0),
        (1 << 14, 0),
        (1 << 16, 0),
        (1 << 8 | 1 << 12 | 1 << 13 | 1 << 15, 0xB100),
    ] {
        csrw(&mut cpu, &mut ctx, MEDELEG, value);
        assert_eq!(read(&cpu, "medeleg"), stored, "{value:#x}");
    }
    for csr in [MIDELEG, SIE, SIP] {
        let f = retire(&mut cpu, &mut ctx, csrrw(5, csr, 31));
        assert_eq!(f[7], ("csr_value", u(0)));
        assert_eq!(reg(&cpu, 5), 0);
    }
}

#[test]
fn satp_stores_bare_and_sv32_and_zeroes_asid() {
    let (mut cpu, mut ctx) = start();
    // Bare, PPN written, ASID dropped.
    csrw(&mut cpu, &mut ctx, SATP, 0x7fff_ffff);
    assert_eq!(read(&cpu, "satp"), 0x003f_ffff);
    // Sv32 is supported from M3.3: MODE and PPN are stored, ASID reads 0, and rd gets
    // the old value.
    li(&mut cpu, &mut ctx, 1, 0xc000_0123);
    let pc = cpu.pc();
    let insn = csrrw(5, SATP, 1);
    let f = retire(&mut cpu, &mut ctx, insn);
    let mut want = fields(pc, insn, 5, 0x003f_ffff, pc + 4, M);
    want.push(("csr", u(u32::from(SATP))));
    want.push(("csr_value", u(0x8000_0123)));
    assert_eq!(f, want);
    assert_eq!(reg(&cpu, 5), 0x003f_ffff);
    assert_eq!(read(&cpu, "satp"), 0x8000_0123);
    // M stays bare under Sv32: the next fetch goes straight to the bus.
    retire(&mut cpu, &mut ctx, addi(6, 0, 1));
    // Bare again, then from S.
    csrw(&mut cpu, &mut ctx, SATP, 0);
    enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
    retire(&mut cpu, &mut ctx, csrrwi(0, SATP, 5));
    assert_eq!(read(&cpu, "satp"), 5);
}

#[test]
fn satp_is_s_level_and_illegal_from_u() {
    let (mut cpu, mut ctx) = start();
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_illegal(&mut cpu, &mut ctx, csrrs(1, SATP, 0));
}

#[test]
fn csrs_above_the_current_mode_are_illegal() {
    // From S: every machine CSR; from U: every CSR.
    let machine = [
        MSTATUS, MIE, MIP, MTVEC, MSCRATCH, MEPC, MCAUSE, MTVAL, MEDELEG, MIDELEG,
    ];
    let supervisor = [
        SSTATUS, SIE, SIP, STVEC, SSCRATCH, SEPC, SCAUSE, STVAL, SATP,
    ];
    for (target, csrs) in [
        (S, machine.to_vec()),
        (U, machine.iter().chain(&supervisor).copied().collect()),
    ] {
        for csr in csrs {
            for insn in [csrrs(1, csr, 0), csrrw(0, csr, 2), csrrsi(1, csr, 0)] {
                let (mut cpu, mut ctx) = start();
                enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
                assert_illegal(&mut cpu, &mut ctx, insn);
            }
        }
    }
    // S reaches every supervisor CSR.
    for csr in supervisor {
        let (mut cpu, mut ctx) = start();
        enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
        retire(&mut cpu, &mut ctx, csrrs(1, csr, 0));
        retire(&mut cpu, &mut ctx, csrrw(0, csr, 0));
    }
}

#[test]
fn unsupported_and_read_only_csrs_are_illegal_in_every_mode() {
    for target in [M, S, U] {
        // Read-only numbers ([11:10] = 11) that exist on real hardware, and ones that are
        // off the whitelist altogether.
        for insn in [
            csrrs(1, 0xf14, 0), // mhartid
            csrrs(1, 0xc00, 0), // cycle
            csrrw(0, 0xf11, 1),
            csrrs(1, 0x306, 0), // mcounteren
            csrrs(1, 0x10a, 0), // senvcfg
            csrrs(1, 0x106, 0), // scounteren
            csrrs(1, 0x7c0, 0),
            csrrs(1, 0x5a8, 0), // scontext
        ] {
            let (mut cpu, mut ctx) = start();
            if target != M {
                enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
            }
            assert_illegal(&mut cpu, &mut ctx, insn);
        }
    }
}

#[test]
fn wfi_is_illegal_in_every_mode() {
    for target in [M, S, U] {
        let (mut cpu, mut ctx) = start();
        if target != M {
            enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
        }
        assert_illegal(&mut cpu, &mut ctx, WFI);
    }
}

// ---------------------------------------------------------------------------------------
// MRET, SRET, SFENCE.VMA.

#[test]
fn mret_returns_to_mpp_and_sets_mpp_to_u() {
    for (mpp, mpie, target) in [(U, true, U), (S, false, S), (M, true, M)] {
        let (mut cpu, mut ctx) = start();
        let extra = if mpie { MPIE_BIT } else { 0 } | SIE_BIT | SUM_BIT;
        csrw(&mut cpu, &mut ctx, MSTATUS, extra | u32::from(mpp) << 11);
        csrw(&mut cpu, &mut ctx, MEPC, 0x8000_0203);
        let pc = cpu.pc();
        // mepc's low bits read 0.
        assert_eq!(
            retire(&mut cpu, &mut ctx, MRET),
            fields(pc, MRET, 0, 0, 0x8000_0200, M)
        );
        assert_eq!((mode(&cpu), cpu.pc()), (target, 0x8000_0200));
        // MIE <- MPIE, MPIE <- 1, MPP <- U; the S bits are kept.
        let want = SIE_BIT | SUM_BIT | MPIE_BIT | if mpie { MIE_BIT } else { 0 };
        assert_eq!(cpu.m3().mstatus(cpu.csrs()), want);
        // The next instruction runs in the new mode.
        let next = cpu.pc();
        assert_eq!(
            retire(&mut cpu, &mut ctx, addi(1, 0, 1)),
            fields(next, addi(1, 0, 1), 1, 1, next + 4, target)
        );
    }
}

#[test]
fn mret_is_illegal_below_m() {
    for target in [S, U] {
        let (mut cpu, mut ctx) = start();
        enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
        assert_illegal(&mut cpu, &mut ctx, MRET);
    }
}

#[test]
fn sret_returns_to_spp_and_clears_spp_in_s_and_m() {
    for from in [M, S] {
        for (spp, spie) in [(U, true), (S, false), (S, true)] {
            let (mut cpu, mut ctx) = start();
            let sstatus =
                if spp == S { SPP_BIT } else { 0 } | if spie { SPIE_BIT } else { 0 } | MXR_BIT;
            csrw(&mut cpu, &mut ctx, SEPC, 0x8000_0306);
            if from == S {
                enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
            }
            csrw(&mut cpu, &mut ctx, SSTATUS, sstatus);
            let (pc, machine) = (
                cpu.pc(),
                read(&cpu, "mstatus") & (MIE_BIT | MPIE_BIT | 0b11 << 11),
            );
            assert_eq!(
                retire(&mut cpu, &mut ctx, SRET),
                fields(pc, SRET, 0, 0, 0x8000_0304, from)
            );
            assert_eq!((mode(&cpu), cpu.pc()), (spp, 0x8000_0304));
            // SIE <- SPIE, SPIE <- 1, SPP <- U; MXR and the M bits are kept.
            let want = SPIE_BIT | MXR_BIT | if spie { SIE_BIT } else { 0 };
            assert_eq!(read(&cpu, "sstatus"), want, "from {from} spp {spp}");
            assert_eq!(read(&cpu, "mstatus") & !0x000c_0122, machine);
        }
    }
}

#[test]
fn sret_is_illegal_in_u() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, SEPC, 0x8000_0300);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_illegal(&mut cpu, &mut ctx, SRET);
}

#[test]
fn sfence_vma_retires_as_a_no_op_in_s_and_m_and_is_illegal_in_u() {
    let encodings = [
        SFENCE_VMA,
        SFENCE_VMA | 5 << 15,
        SFENCE_VMA | 7 << 20 | 1 << 15,
    ];
    for target in [M, S] {
        for insn in encodings {
            let (mut cpu, mut ctx) = start();
            if target != M {
                enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
            }
            let (pc, before) = (cpu.pc(), cpu.inspect());
            assert_eq!(
                retire(&mut cpu, &mut ctx, insn),
                fields(pc, insn, 0, 0, pc + 4, target)
            );
            let after = cpu.inspect();
            let keep = |v: &StateView| {
                v.fields
                    .iter()
                    .filter(|(n, _)| !matches!(*n, "pc" | "instret"))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            assert_eq!(keep(&after), keep(&before));
        }
    }
    for insn in encodings {
        let (mut cpu, mut ctx) = start();
        enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
        assert_illegal(&mut cpu, &mut ctx, insn);
    }
    // Other encodings near it stay illegal in S.
    for insn in [
        SFENCE_VMA | 1 << 7,
        SFENCE_VMA | 1 << 12,
        0x1400_0073,
        0x1600_0073,
    ] {
        let (mut cpu, mut ctx) = start();
        enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
        assert_illegal(&mut cpu, &mut ctx, insn);
    }
}

// ---------------------------------------------------------------------------------------
// ECALL and synchronous traps.

#[test]
fn ecall_names_the_mode_and_halts_when_not_delegated() {
    for (target, cause, name) in [
        (U, TrapCause::EnvironmentCallFromU, "EnvironmentCallFromU"),
        (S, TrapCause::EnvironmentCallFromS, "EnvironmentCallFromS"),
        (M, TrapCause::EnvironmentCall, "EnvironmentCallFromM"),
    ] {
        let (mut cpu, mut ctx) = start();
        if target != M {
            enter(&mut cpu, &mut ctx, target, 0x8000_1000, 0);
        }
        assert_halts(&mut cpu, &mut ctx, ECALL, cause, name, 0);
        assert_eq!(mode(&cpu), target, "a halt does not change the mode");
    }
}

#[test]
fn only_ecall_from_u_is_delegable_among_the_ecalls() {
    // medeleg can hold bit 8 but never 9 or 11.
    let (mut cpu, mut ctx) = start();
    delegate_all(&mut cpu, &mut ctx);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_delegated(&mut cpu, &mut ctx, ECALL, "EnvironmentCallFromU", 8, 0);
    assert_halts(
        &mut cpu,
        &mut ctx,
        ECALL,
        TrapCause::EnvironmentCallFromS,
        "EnvironmentCallFromS",
        0,
    );
}

#[test]
fn delegated_exceptions_from_u_and_s_enter_s_with_every_field() {
    for from in [U, S] {
        for sie in [false, true] {
            // Illegal instruction, breakpoint, misaligned load and store, access faults.
            let (mut cpu, mut ctx) = start();
            delegate_all(&mut cpu, &mut ctx);
            enter(
                &mut cpu,
                &mut ctx,
                from,
                0x8000_1000,
                if sie { SIE_BIT } else { 0 },
            );
            assert_delegated(
                &mut cpu,
                &mut ctx,
                0xffff_ffff,
                "IllegalInstruction",
                2,
                0xffff_ffff,
            );
            // Return to `from` for the next one.
            let back = |cpu: &mut Rv32iCpu, ctx: &mut MockCtx| {
                retire(cpu, ctx, csrrw(0, SEPC, 0));
                let pc = if from == S { SPP_BIT } else { 0 } | if sie { SPIE_BIT } else { 0 };
                li(cpu, ctx, 30, pc);
                retire(cpu, ctx, csrrw(0, SSTATUS, 30));
                li(cpu, ctx, 30, 0x8000_2000);
                retire(cpu, ctx, csrrw(0, SEPC, 30));
                retire(cpu, ctx, SRET);
                assert_eq!((mode(cpu), cpu.pc()), (from, 0x8000_2000));
                assert_eq!(read(cpu, "sstatus") & SIE_BIT != 0, sie);
            };
            back(&mut cpu, &mut ctx);
            assert_delegated(&mut cpu, &mut ctx, EBREAK, "Breakpoint", 3, 0x8000_2000);
            back(&mut cpu, &mut ctx);
            li(&mut cpu, &mut ctx, 2, 0x8000_8001);
            assert_delegated(
                &mut cpu,
                &mut ctx,
                lw(1, 2, 0),
                "LoadAddressMisaligned",
                4,
                0x8000_8001,
            );
            back(&mut cpu, &mut ctx);
            li(&mut cpu, &mut ctx, 2, 0x8000_8002);
            assert_delegated(
                &mut cpu,
                &mut ctx,
                sw(1, 2, 0),
                "StoreAddressMisaligned",
                6,
                0x8000_8002,
            );
            back(&mut cpu, &mut ctx);
            li(&mut cpu, &mut ctx, 2, 0x8000_0002);
            let pc = cpu.pc();
            assert_delegated(
                &mut cpu,
                &mut ctx,
                jalr(0, 2, 0),
                "InstructionAddressMisaligned",
                0,
                0x8000_0002,
            );
            assert_eq!(read(&cpu, "sepc"), pc);
            back(&mut cpu, &mut ctx);
            // A load access fault.
            li(&mut cpu, &mut ctx, 2, 0x9000_0000);
            let (pc, instret) = (cpu.pc(), cpu.instret());
            let records = run_memory(&mut cpu, &mut ctx, lw(1, 2, 0), |txn| MemMsg::ReadResp {
                txn,
                outcome: ReadOutcome::Fault {
                    fault: systemscope_contracts::protocol::mem_v1::MemFault::AccessFault,
                },
            });
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, EXCEPTION_KIND);
            assert_eq!(records[0].1[2], ("cause", s("LoadAccessFault")));
            assert_eq!(
                (mode(&cpu), cpu.pc(), cpu.instret()),
                (S, STVEC_BASE, instret)
            );
            assert_eq!(
                (
                    read(&cpu, "sepc"),
                    read(&cpu, "scause"),
                    read(&cpu, "stval")
                ),
                (pc, 5, 0x9000_0000)
            );
        }
    }
}

#[test]
fn an_instruction_access_fault_is_delegated_with_insn_zero() {
    let (mut cpu, mut ctx) = start();
    delegate_all(&mut cpu, &mut ctx);
    enter(&mut cpu, &mut ctx, U, 0x9000_0000, 0);
    ctx.wake(&mut cpu, FETCH, Phase::Request).unwrap();
    let MemMsg::ReadReq { txn, .. } = ctx.take_sent() else {
        panic!("not a fetch");
    };
    ctx.respond(
        &mut cpu,
        MemMsg::ReadResp {
            txn,
            outcome: ReadOutcome::Fault {
                fault: systemscope_contracts::protocol::mem_v1::MemFault::AccessFault,
            },
        },
    )
    .unwrap();
    assert_eq!(ctx.take_wake().token, COMMIT);
    ctx.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    let records = after_commit(&mut ctx);
    assert_eq!(
        records,
        vec![(
            EXCEPTION_KIND,
            vec![
                ("pc", u(0x9000_0000)),
                ("insn", u(0)),
                ("cause", s("InstructionAccessFault")),
                ("tval", u(0x9000_0000)),
                ("from", s("U")),
                ("to", s("S")),
            ]
        )]
    );
    assert_eq!((read(&cpu, "sepc"), read(&cpu, "scause")), (0x9000_0000, 1));
}

#[test]
fn exceptions_in_m_or_without_their_medeleg_bit_halt_in_the_mode() {
    // M ignores medeleg.
    let (mut cpu, mut ctx) = start();
    delegate_all(&mut cpu, &mut ctx);
    assert_illegal(&mut cpu, &mut ctx, 0xffff_ffff);
    assert_eq!(mode(&cpu), M);
    // Without bit 2, an illegal instruction in U halts; with only bit 2, a breakpoint does.
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, STVEC, STVEC_BASE);
    csrw(&mut cpu, &mut ctx, MEDELEG, 1 << 2);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_halts(
        &mut cpu,
        &mut ctx,
        EBREAK,
        TrapCause::Breakpoint,
        "Breakpoint",
        0x8000_1000,
    );
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MEDELEG, 1 << 3);
    enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
    assert_illegal(&mut cpu, &mut ctx, 0);
}

#[test]
fn stvec_mode_bits_are_ignored_on_entry() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, STVEC, STVEC_BASE | 3);
    assert_eq!(read(&cpu, "stvec"), STVEC_BASE);
    csrw(&mut cpu, &mut ctx, MEDELEG, 1 << 8);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_delegated(&mut cpu, &mut ctx, ECALL, "EnvironmentCallFromU", 8, 0);
}

#[test]
fn a_delegated_exception_is_not_a_retirement_for_the_instruction_limit() {
    // With one instruction left, a delegated exception neither halts nor counts.
    let mut cpu = Rv32iCpu::new(Rv32iConfig {
        max_instructions: NonZeroU64::new(14).unwrap(),
        ..config(Rv32iProfile::M3)
    })
    .unwrap();
    let mut ctx = MockCtx::new();
    cpu.init(&mut ctx).unwrap();
    ctx.take_wake();
    // Three instructions each, then seven.
    csrw(&mut cpu, &mut ctx, STVEC, STVEC_BASE);
    csrw(&mut cpu, &mut ctx, MEDELEG, 1 << 8);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    assert_eq!(cpu.instret(), 13);
    assert_delegated(&mut cpu, &mut ctx, ECALL, "EnvironmentCallFromU", 8, 0);
    assert_eq!(cpu.instret(), 13);
    let records = run_all(&mut cpu, &mut ctx, addi(1, 0, 1));
    assert_eq!(records[1], (HALT_KIND, vec![("instret", Value::U64(14))]));
}

// ---------------------------------------------------------------------------------------
// Loads and stores carry paddr.

#[test]
fn loads_and_stores_trace_priv_and_paddr_after_addr() {
    let (mut cpu, mut ctx) = start();
    enter(&mut cpu, &mut ctx, S, 0x8000_1000, 0);
    li(&mut cpu, &mut ctx, 2, 0x8000_8000);
    let pc = cpu.pc();
    let records = run_memory(&mut cpu, &mut ctx, lw(1, 2, 4), |txn| MemMsg::ReadResp {
        txn,
        outcome: ReadOutcome::Data {
            data: vec![1, 2, 3, 4],
        },
    });
    let mut want = fields(pc, lw(1, 2, 4), 1, 0x0403_0201, pc + 4, S);
    want.push(("addr", u(0x8000_8004)));
    want.push(("paddr", u(0x8000_8004)));
    assert_eq!(records, vec![(COMMIT_KIND, want)]);
    let records = run_memory(&mut cpu, &mut ctx, sh(1, 2, 2), |txn| MemMsg::WriteResp {
        txn,
        outcome: systemscope_contracts::protocol::mem_v1::WriteOutcome::Done,
    });
    let mut want = fields(pc + 4, sh(1, 2, 2), 0, 0, pc + 8, S);
    want.push(("addr", u(0x8000_8002)));
    want.push(("paddr", u(0x8000_8002)));
    want.push(("width", Value::U64(2)));
    want.push(("value", Value::U64(0x0201)));
    assert_eq!(records, vec![(COMMIT_KIND, want)]);
}

// ---------------------------------------------------------------------------------------
// The machine external interrupt with modes.

#[test]
fn mei_is_taken_below_m_whatever_mie_says_and_records_the_mode() {
    for from in [U, S] {
        let (mut cpu, mut ctx) = start();
        csrw(&mut cpu, &mut ctx, MTVEC, 0x8000_0400);
        csrw(&mut cpu, &mut ctx, MIE, 0x800);
        // MIE = 0 after MRET (MPIE = 0), SIE set.
        enter(&mut cpu, &mut ctx, from, 0x8000_1000, SIE_BIT);
        assert_eq!(read(&cpu, "mstatus") & MIE_BIT, 0);
        ctx.level(&mut cpu, true).unwrap();
        let records = run_all(&mut cpu, &mut ctx, addi(1, 0, 7));
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].1,
            fields(0x8000_1000, addi(1, 0, 7), 1, 7, 0x8000_1004, from)
        );
        assert_eq!(
            records[1],
            (
                INTERRUPT_KIND,
                vec![
                    ("mepc", u(0x8000_1004)),
                    ("mcause", u(MEI_CAUSE)),
                    ("handler", u(0x8000_0400)),
                    ("from", s(["U", "S"][usize::from(from)])),
                ]
            )
        );
        assert_eq!((mode(&cpu), cpu.pc()), (M, 0x8000_0400));
        // MPP <- from, MPIE <- MIE (0), MIE <- 0; SIE is kept.
        assert_eq!(read(&cpu, "mstatus"), SIE_BIT | u32::from(from) << 11);
        assert_eq!((read(&cpu, "mepc"), read(&cpu, "mtval")), (0x8000_1004, 0));
        // MRET goes back to where it came from.
        csrw(&mut cpu, &mut ctx, MIE, 0);
        retire(&mut cpu, &mut ctx, MRET);
        assert_eq!((mode(&cpu), cpu.pc()), (from, 0x8000_1004));
    }
}

#[test]
fn mei_in_m_needs_mie() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MTVEC, 0x8000_0400);
    csrw(&mut cpu, &mut ctx, MIE, 0x800);
    ctx.level(&mut cpu, true).unwrap();
    // MIE = 0: not taken.
    let records = run_all(&mut cpu, &mut ctx, addi(1, 0, 1));
    assert_eq!(records.len(), 1);
    // MIE = 1 (MPP = S stays as written): taken, MPP <- M.
    li(&mut cpu, &mut ctx, 3, MIE_BIT | 1 << 11);
    let records = run_all(&mut cpu, &mut ctx, csrrw(0, MSTATUS, 3));
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].1[3], ("from", s("M")));
    assert_eq!(read(&cpu, "mstatus"), MPIE_BIT | 3 << 11);
}

#[test]
fn a_delegated_exception_does_not_sample_the_interrupt() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MTVEC, 0x8000_0400);
    csrw(&mut cpu, &mut ctx, MIE, 0x800);
    delegate_all(&mut cpu, &mut ctx);
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    ctx.level(&mut cpu, true).unwrap();
    assert_delegated(&mut cpu, &mut ctx, ECALL, "EnvironmentCallFromU", 8, 0);
    // The next retirement, in S, takes it.
    let records = run_all(&mut cpu, &mut ctx, addi(0, 0, 0));
    assert_eq!(records[1].0, INTERRUPT_KIND);
    assert_eq!(records[1].1[3], ("from", s("S")));
}

/// A schema 3 CPU with the given mode and CSRs, fetching at `pc`.
fn forged(privilege: u8, pc: u32, c: &Csrs, m: &M3Csrs) -> Rv32iCpu {
    let m = M3Csrs { privilege, ..*m };
    let bytes = forge(pc, &[0; 31], 0, 0, fetch_issue, c, &m);
    restore_m3(&bytes).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// §5.1 against the independent oracle, over the mode, MIE, MPIE, MEIE, the level, the
    /// S bits, and mtvec, at one retirement boundary.
    #[test]
    fn m3_mei_matches_the_oracle(
        privilege in prop::sample::select(vec![U, S, M]),
        mie in any::<bool>(),
        mpie in any::<bool>(),
        meie in any::<bool>(),
        level in any::<bool>(),
        mpp in prop::sample::select(vec![U, S, M]),
        sbits in 0u32..32,
        mtvec in any::<u32>(),
        pc in (0u32..0x4000_0000).prop_map(|p| p * 4),
    ) {
        let spie = sbits & 1;
        let sie = sbits >> 1 & 1;
        let spp = sbits >> 2 & 1;
        let (sum, mxr) = (sbits >> 3 & 1, sbits >> 4 & 1);
        let c = Csrs {
            mie: u8::from(mie),
            mpie: u8::from(mpie),
            meie: u8::from(meie),
            mtvec: mtvec & !3,
            ..RESET_CSRS
        };
        let m = M3Csrs {
            sie: sie as u8,
            spie: spie as u8,
            spp: spp as u8,
            mpp,
            sum: sum as u8,
            mxr: mxr as u8,
            ..RESET_M3
        };
        let mut cpu = forged(privilege, pc, &c, &m);
        let mut ctx = MockCtx::new();
        ctx.level(&mut cpu, level).unwrap();
        let mstatus = read(&cpu, "mstatus");
        let oracle = take_mei_m3(
            pc.wrapping_add(4),
            privilege,
            mstatus,
            read(&cpu, "mie"),
            read(&cpu, "mip"),
            read(&cpu, "mtvec"),
        );
        let records = run_all(&mut cpu, &mut ctx, addi(1, 0, 1));
        prop_assert_eq!(records.len(), 1 + usize::from(oracle.taken));
        prop_assert_eq!(cpu.pc(), oracle.new_pc);
        prop_assert_eq!(mode(&cpu), oracle.new_priv);
        prop_assert_eq!(read(&cpu, "mstatus"), oracle.new_mstatus);
        let entry = (read(&cpu, "mepc"), read(&cpu, "mcause"), read(&cpu, "mtval"));
        prop_assert_eq!(entry, oracle.entry.unwrap_or((0, 0, 0)));
        if oracle.taken {
            prop_assert_eq!(
                &records[1].1[3],
                &("from", s(["U", "S", "", "M"][usize::from(privilege)]))
            );
        }
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

fn restore_m3(bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    restore_as(Rv32iProfile::M3, SNAPSHOT_SCHEMA_M3, bytes)
}

/// Round-trips `cpu` (M3) and checks the copy is identical.
fn round_trip(cpu: &Rv32iCpu) -> Rv32iCpu {
    let bytes = snapshot_of(cpu);
    let copy = restore_m3(&bytes).unwrap();
    assert_eq!(snapshot_of(&copy), bytes);
    assert_eq!(copy.inspect(), cpu.inspect());
    copy
}

/// Schema 2's CSR block.
#[derive(Clone, Copy)]
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

/// Schema 3's block after schema 2's.
#[derive(Clone, Copy)]
struct M3Csrs {
    privilege: u8,
    sie: u8,
    spie: u8,
    spp: u8,
    mpp: u8,
    sum: u8,
    mxr: u8,
    medeleg: u32,
    stvec: u32,
    sscratch: u32,
    sepc: u32,
    scause: u32,
    stval: u32,
    satp: u32,
}

const RESET_M3: M3Csrs = M3Csrs {
    privilege: M,
    sie: 0,
    spie: 0,
    spp: 0,
    mpp: 0,
    sum: 0,
    mxr: 0,
    medeleg: 0,
    stvec: 0,
    sscratch: 0,
    sepc: 0,
    scause: 0,
    stval: 0,
    satp: 0,
};

/// A schema 3 snapshot from its parts: `regs` are x1..x31; `state` writes the execution
/// state.
fn forge(
    pc: u32,
    regs: &[u32; 31],
    instret: u64,
    next_txn: u64,
    state: impl FnOnce(&mut SnapshotWriter),
    c: &Csrs,
    m: &M3Csrs,
) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    w.u32(CLOCK.0);
    w.u32(ENTRY);
    w.u64(LIMIT);
    w.u32(pc);
    for r in regs {
        w.u32(*r);
    }
    w.u64(instret);
    w.u64(next_txn);
    state(&mut w);
    w.u8(c.mie);
    w.u8(c.mpie);
    w.u8(c.meie);
    w.u32(c.mtvec);
    w.u32(c.mscratch);
    w.u32(c.mepc);
    w.u32(c.mcause);
    w.u32(c.mtval);
    w.u8(c.irq);
    for b in [m.privilege, m.sie, m.spie, m.spp, m.mpp, m.sum, m.mxr] {
        w.u8(b);
    }
    for v in [
        m.medeleg, m.stvec, m.sscratch, m.sepc, m.scause, m.stval, m.satp,
    ] {
        w.u32(v);
    }
    w.into_bytes()
}

/// `FetchIssue`, untranslated.
fn fetch_issue(w: &mut SnapshotWriter) {
    w.u8(0);
    w.u8(0);
}

fn reset_snapshot() -> Vec<u8> {
    forge(ENTRY, &[0; 31], 0, 0, fetch_issue, &RESET_CSRS, &RESET_M3)
}

#[test]
fn the_schema_3_layout_is_schema_2_then_the_m3_block() {
    let (cpu, _) = start();
    assert_eq!(snapshot_of(&cpu), reset_snapshot());
    // Every field in place, after a program that sets them all.
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, MTVEC, 20);
    csrw(&mut cpu, &mut ctx, MSCRATCH, 1);
    csrw(&mut cpu, &mut ctx, MCAUSE, 2);
    csrw(&mut cpu, &mut ctx, MTVAL, 3);
    csrw(&mut cpu, &mut ctx, MIE, 0x800);
    csrw(&mut cpu, &mut ctx, MEDELEG, 0x100);
    csrw(&mut cpu, &mut ctx, STVEC, 0x40);
    csrw(&mut cpu, &mut ctx, SSCRATCH, 5);
    csrw(&mut cpu, &mut ctx, SEPC, 0x44);
    csrw(&mut cpu, &mut ctx, SCAUSE, 6);
    csrw(&mut cpu, &mut ctx, STVAL, 7);
    csrw(&mut cpu, &mut ctx, SATP, 0x9);
    csrw(&mut cpu, &mut ctx, MEPC, 0x8000_1000);
    // MPIE MPP=S SPIE SIE SUM MXR, then MRET: priv S, MIE 1, MPIE 1, MPP U.
    csrw(
        &mut cpu,
        &mut ctx,
        MSTATUS,
        MPIE_BIT | 1 << 11 | SPIE_BIT | SUM_BIT | MXR_BIT,
    );
    let mstatus = read(&cpu, "mstatus") | SIE_BIT | SPP_BIT;
    csrw(&mut cpu, &mut ctx, MSTATUS, mstatus);
    retire(&mut cpu, &mut ctx, MRET);
    let mut regs = [0; 31];
    regs[30] = reg(&cpu, 31);
    let c = Csrs {
        mie: 1,
        mpie: 1,
        meie: 1,
        mtvec: 20,
        mscratch: 1,
        mepc: 0x8000_1000,
        mcause: 2,
        mtval: 3,
        irq: 0,
    };
    let m = M3Csrs {
        privilege: S,
        sie: 1,
        spie: 1,
        spp: 1,
        mpp: U,
        sum: 1,
        mxr: 1,
        medeleg: 0x100,
        stvec: 0x40,
        sscratch: 5,
        sepc: 0x44,
        scause: 6,
        stval: 7,
        satp: 9,
    };
    let want = forge(
        0x8000_1000,
        &regs,
        cpu.instret(),
        cpu.instret(),
        fetch_issue,
        &c,
        &m,
    );
    assert_eq!(snapshot_of(&cpu), want);
    // Its length is schema 2's plus the state's physical-address tag, 7 bytes, and 7
    // words.
    assert_eq!(
        want.len(),
        4 + 4 + 8 + 4 + 31 * 4 + 8 + 8 + 1 + 1 + 3 + 5 * 4 + 1 + 7 + 7 * 4
    );
    round_trip(&cpu);
}

#[test]
fn each_profile_restores_only_its_own_schema() {
    let (m3, _) = start();
    let b3 = snapshot_of(&m3);
    let mut m2 = Rv32iCpu::new(config(Rv32iProfile::M2)).unwrap();
    let b2 = snapshot_of(&m2);
    assert_eq!(&b3[..b2.len()], &b2[..]);
    assert!(restore_m3(&b3).is_ok());
    for (profile, schema, bytes) in [
        (Rv32iProfile::M3, SNAPSHOT_SCHEMA_M2, &b3),
        (Rv32iProfile::M3, SNAPSHOT_SCHEMA_M2, &b2),
        (Rv32iProfile::M2, SNAPSHOT_SCHEMA_M3, &b3),
        (Rv32iProfile::M2, SNAPSHOT_SCHEMA_M3, &b2),
    ] {
        assert!(
            matches!(
                restore_as(profile, schema, bytes),
                Err(RestoreError::InvalidState(_))
            ),
            "{profile:?} schema {schema}"
        );
    }
    // Schema 2 bytes are too short for schema 3, and schema 3 too long for schema 2.
    assert!(restore_m3(&b2).is_err());
    let mut r = SnapshotReader::new(&b3);
    m2.restore(&mut r, SNAPSHOT_SCHEMA_M2).unwrap();
    assert!(r.finish().is_err());
}

#[test]
fn schema_2_rejects_the_m3_cause_codes_and_the_sret_tag() {
    // An M2 halted trap with codes 9..13, and a pending SRET, do not decode.
    let m2 = |state: &dyn Fn(&mut SnapshotWriter)| {
        let mut w = SnapshotWriter::new();
        w.u32(CLOCK.0);
        w.u32(ENTRY);
        w.u64(LIMIT);
        w.u32(ENTRY);
        for _ in 0..31 {
            w.u32(0);
        }
        w.u64(0);
        w.u64(1);
        state(&mut w);
        for b in [0u8, 0, 0] {
            w.u8(b);
        }
        for _ in 0..5 {
            w.u32(0);
        }
        w.u8(0);
        w.into_bytes()
    };
    let halted = |code: u8| {
        m2(&move |w: &mut SnapshotWriter| {
            w.u8(5);
            w.u8(0);
            w.u8(code);
            w.u32(ENTRY);
            w.u32(0);
        })
    };
    let restore_m2 = |b: &[u8]| restore_as(Rv32iProfile::M2, SNAPSHOT_SCHEMA_M2, b);
    // Code 4, EnvironmentCall, is fine.
    restore_m2(&halted(4)).unwrap();
    for code in 9..=13 {
        assert!(
            matches!(restore_m2(&halted(code)), Err(RestoreError::Decode(_))),
            "code {code}"
        );
    }
    let sret = m2(&|w: &mut SnapshotWriter| {
        w.u8(4);
        w.u8(1);
        w.u32(SRET);
        w.u8(4);
    });
    assert!(matches!(restore_m2(&sret), Err(RestoreError::Decode(_))));
    // The same pending trap, cause 9, in a pending outcome.
    let pending = m2(&|w: &mut SnapshotWriter| {
        w.u8(4);
        w.u8(1);
        w.u32(ECALL);
        w.u8(1);
        w.u8(9);
        w.u32(0);
    });
    assert!(matches!(restore_m2(&pending), Err(RestoreError::Decode(_))));
}

#[test]
fn restore_checks_the_m3_block() {
    let with = |m: M3Csrs| forge(ENTRY, &[0; 31], 0, 0, fetch_issue, &RESET_CSRS, &m);
    // Every legal value restores and reads back.
    let ok = with(M3Csrs {
        privilege: U,
        sie: 1,
        spie: 1,
        spp: 1,
        mpp: S,
        sum: 1,
        mxr: 1,
        medeleg: 0xB1FF,
        stvec: 0xffff_fffc,
        sscratch: 0xffff_ffff,
        sepc: 0xffff_fffc,
        scause: 0xffff_ffff,
        stval: 0xffff_ffff,
        satp: 0x003f_ffff,
    });
    let cpu = restore_m3(&ok).unwrap();
    assert_eq!(snapshot_of(&cpu), ok);
    assert_eq!(mode(&cpu), U);
    assert_eq!(read(&cpu, "mstatus"), 0x000c_0922);
    // Sv32 with any PPN restores; in M nothing is translated, so `FetchIssue` has no pa.
    let sv32 = with(M3Csrs {
        satp: 0x803f_ffff,
        ..RESET_M3
    });
    assert_eq!(snapshot_of(&restore_m3(&sv32).unwrap()), sv32);
    for bad in [
        M3Csrs {
            privilege: 2,
            ..RESET_M3
        },
        M3Csrs {
            privilege: 4,
            ..RESET_M3
        },
        M3Csrs {
            privilege: 0xff,
            ..RESET_M3
        },
        M3Csrs { mpp: 2, ..RESET_M3 },
        M3Csrs { mpp: 7, ..RESET_M3 },
        M3Csrs { spp: 2, ..RESET_M3 },
        M3Csrs { sie: 2, ..RESET_M3 },
        M3Csrs {
            spie: 2,
            ..RESET_M3
        },
        M3Csrs { sum: 2, ..RESET_M3 },
        M3Csrs {
            mxr: 0xff,
            ..RESET_M3
        },
        M3Csrs {
            medeleg: 1 << 9,
            ..RESET_M3
        },
        M3Csrs {
            medeleg: 1 << 11,
            ..RESET_M3
        },
        M3Csrs {
            medeleg: 1 << 16,
            ..RESET_M3
        },
        M3Csrs {
            stvec: 1,
            ..RESET_M3
        },
        M3Csrs {
            stvec: 2,
            ..RESET_M3
        },
        M3Csrs {
            sepc: 2,
            ..RESET_M3
        },
        M3Csrs {
            satp: 1 << 22,
            ..RESET_M3
        },
        M3Csrs {
            satp: 0x8000_0000 | 1 << 30,
            ..RESET_M3
        },
    ] {
        let bytes = with(bad);
        assert!(
            matches!(restore_m3(&bytes), Err(RestoreError::InvalidState(_))),
            "{:?}",
            restore_m3(&bytes).err()
        );
    }
    // Truncated or extended bytes fail.
    let mut short = reset_snapshot();
    short.pop();
    assert!(restore_m3(&short).is_err());
    let mut long = reset_snapshot();
    long.push(0);
    assert!(restore_m3(&long).is_err());
}

#[test]
fn a_rejected_restore_changes_nothing() {
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, SSCRATCH, 0x1234);
    enter(&mut cpu, &mut ctx, S, 0x8000_1000, SUM_BIT);
    let before = snapshot_of(&cpu);
    // A block that decodes to the end, with every field changed, and one bad value last.
    let bytes = forge(
        ENTRY,
        &[7; 31],
        3,
        3,
        fetch_issue,
        &Csrs {
            mscratch: 9,
            ..RESET_CSRS
        },
        &M3Csrs {
            privilege: U,
            sscratch: 5,
            satp: 1 << 22,
            ..RESET_M3
        },
    );
    let mut r = SnapshotReader::new(&bytes);
    assert!(matches!(
        cpu.restore(&mut r, SNAPSHOT_SCHEMA_M3),
        Err(RestoreError::InvalidState(_))
    ));
    assert_eq!(snapshot_of(&cpu), before);
    // And the CPU runs on.
    ctx = MockCtx::new();
    retire(&mut cpu, &mut ctx, addi(1, 0, 1));
    assert_eq!(mode(&cpu), S);
}

/// A state record whose checks need the mode decoded after it.
fn halted(cause: u8, pc: u32) -> impl FnOnce(&mut SnapshotWriter) {
    move |w: &mut SnapshotWriter| {
        w.u8(5);
        w.u8(0);
        w.u8(cause);
        w.u32(pc);
        w.u32(0);
    }
}

fn pending(
    insn: u32,
    outcome: impl FnOnce(&mut SnapshotWriter),
) -> impl FnOnce(&mut SnapshotWriter) {
    move |w: &mut SnapshotWriter| {
        w.u8(4);
        w.u8(1);
        w.u32(insn);
        outcome(w);
        // No physical address.
        w.u8(0);
    }
}

#[test]
fn the_state_record_is_checked_against_the_mode_read_after_it() {
    let at = |privilege: u8, state: Box<dyn FnOnce(&mut SnapshotWriter)>, medeleg: u32| {
        restore_m3(&forge(
            ENTRY,
            &[0; 31],
            0,
            1,
            state,
            &RESET_CSRS,
            &M3Csrs {
                privilege,
                medeleg,
                ..RESET_M3
            },
        ))
    };
    // A halt on an ECALL names the mode it happened in: codes 9 (U), 10 (S), 4 (M).
    for (privilege, code) in [(U, 9u8), (S, 10), (M, 4)] {
        let cpu = at(privilege, Box::new(halted(code, ENTRY)), 0).unwrap();
        assert_eq!(mode(&cpu), privilege);
        for other in [9u8, 10, 4] {
            if other != code {
                assert!(
                    matches!(
                        at(privilege, Box::new(halted(other, ENTRY)), 0),
                        Err(RestoreError::InvalidState(_))
                    ),
                    "priv {privilege} code {other}"
                );
            }
        }
    }
    // A halt on an exception the mode delegates is not reachable; in M it is.
    assert!(at(U, Box::new(halted(2, ENTRY)), 1 << 2).is_err());
    assert!(at(S, Box::new(halted(3, ENTRY)), 1 << 3).is_err());
    at(M, Box::new(halted(2, ENTRY)), 1 << 2).unwrap();
    at(U, Box::new(halted(2, ENTRY)), 1 << 3).unwrap();
    // A page fault needs translation, which is off here (satp Bare): codes 11, 12, 13.
    for code in 11..=13u8 {
        assert!(matches!(
            at(S, Box::new(halted(code, ENTRY)), 0),
            Err(RestoreError::InvalidState(_))
        ));
        let trap = pending(0, move |w: &mut SnapshotWriter| {
            w.u8(1);
            w.u8(code);
            w.u32(0);
        });
        assert!(matches!(
            at(S, Box::new(trap), 0),
            Err(RestoreError::InvalidState(_))
        ));
    }
    // A halt's pc is the architectural pc.
    assert!(at(M, Box::new(halted(4, ENTRY + 4)), 0).is_err());
    // A pending SRET needs S or M; a pending MRET needs M.
    let sret = || pending(SRET, |w: &mut SnapshotWriter| w.u8(4));
    let mret = || pending(MRET, |w: &mut SnapshotWriter| w.u8(3));
    at(S, Box::new(sret()), 0).unwrap();
    at(M, Box::new(sret()), 0).unwrap();
    assert!(at(U, Box::new(sret()), 0).is_err());
    at(M, Box::new(mret()), 0).unwrap();
    assert!(at(S, Box::new(mret()), 0).is_err());
    // A pending illegal-instruction trap for SRET is U's; for a CSR, the mode's access.
    let illegal = |insn: u32| {
        pending(insn, move |w: &mut SnapshotWriter| {
            w.u8(1);
            w.u8(2);
            w.u32(insn);
        })
    };
    at(U, Box::new(illegal(SRET)), 0).unwrap();
    assert!(at(S, Box::new(illegal(SRET)), 0).is_err());
    at(S, Box::new(illegal(csrrs(1, MSTATUS, 0))), 0).unwrap();
    assert!(at(M, Box::new(illegal(csrrs(1, MSTATUS, 0))), 0).is_err());
    // A pending ECALL's cause follows the mode.
    let ecall = |code: u8| {
        pending(ECALL, move |w: &mut SnapshotWriter| {
            w.u8(1);
            w.u8(code);
            w.u32(0);
        })
    };
    at(U, Box::new(ecall(9)), 0).unwrap();
    assert!(at(U, Box::new(ecall(4)), 0).is_err());
    at(S, Box::new(ecall(10)), 0).unwrap();
    at(M, Box::new(ecall(4)), 0).unwrap();
    assert!(at(M, Box::new(ecall(9)), 0).is_err());
    // An unknown outcome tag or cause code does not decode.
    assert!(matches!(
        at(
            S,
            Box::new(pending(SRET, |w: &mut SnapshotWriter| w.u8(5))),
            0
        ),
        Err(RestoreError::Decode(_))
    ));
    assert!(matches!(
        at(S, Box::new(halted(14, ENTRY)), 0),
        Err(RestoreError::Decode(_))
    ));
}

#[test]
fn pending_sret_and_halted_ecalls_round_trip_and_continue() {
    // A pending SRET, stored as tag 4 and committed identically after a round trip.
    let (mut cpu, mut ctx) = start();
    csrw(&mut cpu, &mut ctx, SEPC, 0x8000_0500);
    csrw(&mut cpu, &mut ctx, SSTATUS, SPP_BIT);
    assert_eq!(fetch_word(&mut cpu, &mut ctx, SRET), COMMIT);
    let bytes = snapshot_of(&cpu);
    let mut regs = [0; 31];
    regs[30] = SPP_BIT;
    let want = forge(
        cpu.pc(),
        &regs,
        cpu.instret(),
        cpu.instret() + 1,
        pending(SRET, |w: &mut SnapshotWriter| w.u8(4)),
        &RESET_CSRS,
        &M3Csrs {
            spp: 1,
            sepc: 0x8000_0500,
            ..RESET_M3
        },
    );
    assert_eq!(bytes, want);
    let mut copy = round_trip(&cpu);
    let (mut a, mut b) = (MockCtx::new(), MockCtx::new());
    a.wake(&mut cpu, COMMIT, Phase::Commit).unwrap();
    b.wake(&mut copy, COMMIT, Phase::Commit).unwrap();
    assert_eq!((a.traced, a.woke), (b.traced, b.woke));
    assert_eq!(mode(&copy), S);
    assert_eq!(snapshot_of(&copy), snapshot_of(&cpu));
    // Halted on ECALL from U.
    let (mut cpu, mut ctx) = start();
    enter(&mut cpu, &mut ctx, U, 0x8000_1000, 0);
    run(&mut cpu, &mut ctx, ECALL);
    let copy = round_trip(&cpu);
    assert_eq!(copy.halt(), cpu.halt());
    assert_eq!(read_str(&copy, "cause"), "EnvironmentCallFromU");
}

#[test]
fn restored_m3_cpus_continue_identically_from_every_event() {
    // M sets up delegation and enters S; S enters U; U traps to S twice; S halts on ECALL.
    let program = [
        lui(1, 0x80001),
        csrrw(0, STVEC, 1),
        addi(2, 0, -1),
        csrrw(0, MEDELEG, 2),
        lui(3, 1),
        addi(3, 3, -2048),
        csrrw(0, MSTATUS, 3),
        csrrw(0, MEPC, 1),
        MRET,
        csrrsi(0, SSTATUS, 2),
        csrrw(0, SEPC, 1),
        SRET,
        ECALL,
        csrrs(4, SCAUSE, 0),
        SFENCE_VMA,
        SRET,
        EBREAK,
        csrrs(5, SSTATUS, 0),
        ECALL,
    ];
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
        let (mut original, mut ctx) = start();
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
        assert_eq!(
            end.halt(),
            Some(Halt::Trap(RvTrap {
                cause: TrapCause::EnvironmentCallFromS,
                pc: 0x8000_1004,
                tval: 0,
            }))
        );
        assert_eq!(mode(&end), S);
        // Two exceptions were delegated and did not retire.
        assert_eq!(end.instret(), program.len() as u64 - 3);
        assert_eq!(reg(&end, 4), 8);
        // After EBREAK from U: SPP = U, SPIE = SIE (0), SIE = 0.
        assert_eq!(reg(&end, 5), 0);
        assert_eq!(read(&end, "scause"), 3);
        assert_eq!(read(&end, "stval"), 0x8000_1000);
    }
}
