//! `Rv32iCpu` Sv32 translation in the `M3` profile (`docs/m3-design.md` §5.2–§5.6, M3.3),
//! driven against a RAM model: the two-level walk over the bus, superpages, invalid PTEs,
//! the U/S, SUM, MXR, and Svade rules, the page faults and their delegation, the fault
//! priority, no TLB, `SFENCE.VMA`, inspect and trace, and walk snapshots from every event.
//! Expected translations come from the independent reference [`common::sv32_ref`].

mod common;

use std::num::NonZeroU64;
use std::sync::OnceLock;

use common::MockCtx;
use common::asm::*;
use common::sv32_ref::{self, Kind, Outcome};
use proptest::prelude::*;
use systemscope_contracts::component::Component;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, WriteOutcome};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;
use systemscope_rv32i::cpu::{
    COMMIT_KIND, EXCEPTION_KIND, SNAPSHOT_SCHEMA_M2, SNAPSHOT_SCHEMA_M3, WALK,
};
use systemscope_rv32i::{Halt, Reg, Rv32iConfig, Rv32iCpu, Rv32iProfile, RvTrap, TrapCause};

const ENTRY: u32 = 0x8000_0000;
const CLOCK: ClockDomainId = ClockDomainId(0);
const LIMIT: u64 = 100_000;

const RAM_BASE: u64 = 0x8000_0000;
const RAM_SIZE: u64 = 8 << 20;

const MSTATUS: u16 = 0x300;
const MEDELEG: u16 = 0x302;
const MEPC: u16 = 0x341;
const SSTATUS: u16 = 0x100;
const STVEC: u16 = 0x105;
const SEPC: u16 = 0x141;
const SCAUSE: u16 = 0x142;
const STVAL: u16 = 0x143;
const SATP: u16 = 0x180;

const MRET: u32 = 0x3020_0073;
const SRET: u32 = 0x1020_0073;
const SFENCE_VMA: u32 = 0x1200_0073;

const U: u8 = sv32_ref::U;
const S: u8 = sv32_ref::S;
const M: u8 = sv32_ref::M;

const SUM_BIT: u32 = 1 << 18;
const MXR_BIT: u32 = 1 << 19;

/// PTE bits.
const V: u32 = 1 << 0;
const R: u32 = 1 << 1;
const W: u32 = 1 << 2;
const X: u32 = 1 << 3;
const PU: u32 = 1 << 4;
const G: u32 = 1 << 5;
const A: u32 = 1 << 6;
const D: u32 = 1 << 7;
const RWX_AD: u32 = V | R | W | X | A | D;

/// The page tables: the root and a level-0 table (and `0x80102`, kept free).
const ROOT: u32 = 0x80100;
const L0: u32 = 0x80101;
const SATP_SV32: u32 = 1 << 31 | ROOT;

/// The code megapage (`root[0x300]` → PA `0x8000_0000`), `U` = 1 for U-mode code.
const CODE_VA: u32 = 0xC000_1000;
const CODE_PA: u64 = 0x8000_1000;
/// The S handler's megapage (`root[0x301]` → PA `0x8000_0000`), always `U` = 0.
const HANDLER_VA: u32 = 0xC040_3000;
const HANDLER_PA: u64 = 0x8000_3000;
/// The data megapage region: `root[0x100]`.
const DATA_VA: u32 = 0x4000_0000;
/// Leaf targets: 4 KiB pages at and above this PPN, and the one aligned data megapage.
const DATA_PPN: u32 = 0x80200;
const MEGA_PPN: u32 = 0x80400;

const STORE_VALUE: u32 = 0xDEAD_BEEF;

fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
    u32::from(csr) << 20 | rs1 << 15 | 1 << 12 | rd << 7 | 0x73
}

fn csrrs(rd: u32, csr: u16, rs1: u32) -> u32 {
    u32::from(csr) << 20 | rs1 << 15 | 2 << 12 | rd << 7 | 0x73
}

/// `LUI` and `ADDI` that set `x{rd}` to `value`.
fn li(rd: u32, value: u32) -> [u32; 2] {
    let lo = ((value & 0xfff) as i32) << 20 >> 20;
    let hi = value.wrapping_sub(lo as u32) >> 12;
    [lui(rd, hi), addi(rd, rd, lo)]
}

fn pte(ppn: u32, flags: u32) -> u32 {
    ppn << 10 | flags
}

/// The address of entry `index` of the table with PPN `table`.
fn slot(table: u32, index: u32) -> u64 {
    u64::from(table) * 4096 + u64::from(index) * 4
}

fn vpn1(va: u32) -> u32 {
    va >> 22
}

fn vpn0(va: u32) -> u32 {
    (va >> 12) & 0x3ff
}

fn in_ram(addr: u64, len: u64) -> bool {
    addr >= RAM_BASE && addr + len <= RAM_BASE + RAM_SIZE
}

fn word_in(ram: &[u8], addr: u64) -> Option<u32> {
    in_ram(addr, 4).then(|| {
        let i = (addr - RAM_BASE) as usize;
        u32::from_le_bytes(ram[i..i + 4].try_into().unwrap())
    })
}

/// A value to find at `pa` that no other address holds.
fn marker(pa: u64) -> u32 {
    (pa as u32) ^ 0x5A5A_0000
}

fn config() -> Rv32iConfig {
    Rv32iConfig {
        clock: CLOCK,
        entry: ENTRY,
        max_instructions: NonZeroU64::new(LIMIT).unwrap(),
        profile: Rv32iProfile::M3,
    }
}

fn snapshot_of(cpu: &Rv32iCpu) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    cpu.snapshot(&mut w);
    w.into_bytes()
}

fn restore_into(cpu: &mut Rv32iCpu, schema: u32, bytes: &[u8]) -> Result<(), RestoreError> {
    let mut r = SnapshotReader::new(bytes);
    cpu.restore(&mut r, schema)?;
    r.finish().map_err(RestoreError::Decode)
}

fn restore_m3(bytes: &[u8]) -> Result<Rv32iCpu, RestoreError> {
    let mut cpu = Rv32iCpu::new(config()).unwrap();
    restore_into(&mut cpu, SNAPSHOT_SCHEMA_M3, bytes)?;
    Ok(cpu)
}

fn read_u(cpu: &Rv32iCpu, name: &str) -> u32 {
    match cpu.inspect().get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn read_str(cpu: &Rv32iCpu, name: &str) -> Option<String> {
    match cpu.inspect().get(name) {
        Some(Value::Str(v)) => Some(v.clone()),
        None => None,
        other => panic!("{name}: {other:?}"),
    }
}

fn mode(cpu: &Rv32iCpu) -> u8 {
    read_u(cpu, "priv") as u8
}

fn reg(cpu: &Rv32iCpu, i: u8) -> u32 {
    cpu.registers().read(Reg::new(i).unwrap())
}

// ---------------------------------------------------------------------------------------
// The machine: the CPU against a RAM, one runtime event at a time.

/// The CPU, the pending runtime events in its `MockCtx`, and a RAM at [`RAM_BASE`] that
/// answers every request inside it and faults every other.
struct Machine {
    cpu: Rv32iCpu,
    ctx: MockCtx,
    ram: Vec<u8>,
    /// The address of every read request, in order.
    reads: Vec<u64>,
    /// The address of every write request, in order.
    writes: Vec<u64>,
    /// Every wake scheduled for `WALK`.
    walk_wakes: Vec<(ScheduleWhen, Phase)>,
}

impl Machine {
    fn new() -> Machine {
        let mut cpu = Rv32iCpu::new(config()).unwrap();
        let mut ctx = MockCtx::new();
        cpu.init(&mut ctx).unwrap();
        Machine {
            cpu,
            ctx,
            ram: vec![0; RAM_SIZE as usize],
            reads: Vec::new(),
            writes: Vec::new(),
            walk_wakes: Vec::new(),
        }
    }

    fn poke(&mut self, addr: u64, word: u32) {
        assert!(in_ram(addr, 4), "{addr:#x}");
        let i = (addr - RAM_BASE) as usize;
        self.ram[i..i + 4].copy_from_slice(&word.to_le_bytes());
    }

    fn word(&self, addr: u64) -> Option<u32> {
        word_in(&self.ram, addr)
    }

    fn code(&mut self, addr: u64, insns: &[u32]) {
        for (i, insn) in insns.iter().enumerate() {
            self.poke(addr + 4 * i as u64, *insn);
        }
    }

    /// Delivers the one pending event; `false` when there is none.
    fn step(&mut self) -> bool {
        assert!(self.ctx.sent.len() + self.ctx.woke.len() <= 1);
        if let Some(sent) = self.ctx.sent.pop() {
            let resp = match sent.msg {
                MemMsg::ReadReq { txn, addr, len } => {
                    self.reads.push(addr);
                    let len = u64::from(len);
                    let outcome = if in_ram(addr, len) {
                        let i = (addr - RAM_BASE) as usize;
                        ReadOutcome::Data {
                            data: self.ram[i..i + len as usize].to_vec(),
                        }
                    } else {
                        ReadOutcome::Fault {
                            fault: MemFault::AccessFault,
                        }
                    };
                    MemMsg::ReadResp { txn, outcome }
                }
                MemMsg::WriteReq { txn, addr, data } => {
                    self.writes.push(addr);
                    let outcome = if in_ram(addr, data.len() as u64) {
                        let i = (addr - RAM_BASE) as usize;
                        self.ram[i..i + data.len()].copy_from_slice(&data);
                        WriteOutcome::Done
                    } else {
                        WriteOutcome::Fault {
                            fault: MemFault::AccessFault,
                        }
                    };
                    MemMsg::WriteResp { txn, outcome }
                }
                other => panic!("{other:?}"),
            };
            self.ctx.respond(&mut self.cpu, resp).unwrap();
        } else if let Some(woke) = self.ctx.woke.pop() {
            self.ctx
                .wake(&mut self.cpu, woke.token, woke.phase)
                .unwrap();
        } else {
            return false;
        }
        for woke in &self.ctx.woke {
            if woke.token == WALK {
                self.walk_wakes.push((woke.when, woke.phase));
            }
        }
        true
    }

    /// Runs to the halt, which must be a trap.
    fn run(&mut self) -> RvTrap {
        for _ in 0..LIMIT {
            if !self.step() {
                break;
            }
        }
        match self.cpu.halt() {
            Some(Halt::Trap(t)) => t,
            other => panic!("{other:?}"),
        }
    }

    /// Runs the M prologue until the CPU leaves M, then clears every log but the wake the
    /// `MRET` scheduled.
    fn enter(&mut self) {
        while mode(&self.cpu) == M {
            assert!(self.step(), "halted in M: {:?}", self.cpu.halt());
        }
        self.clear();
        for woke in &self.ctx.woke {
            if woke.token == WALK {
                self.walk_wakes.push((woke.when, woke.phase));
            }
        }
    }

    fn clear(&mut self) {
        self.reads.clear();
        self.writes.clear();
        self.walk_wakes.clear();
        self.ctx.traced.clear();
    }

    /// A copy whose CPU is restored from this CPU's snapshot, with the same pending
    /// events and RAM.
    fn fork(&self) -> Machine {
        let bytes = snapshot_of(&self.cpu);
        let cpu = restore_m3(&bytes).unwrap();
        assert_eq!(snapshot_of(&cpu), bytes);
        assert_eq!(cpu.inspect(), self.cpu.inspect());
        let mut ctx = MockCtx::new();
        ctx.sent = self.ctx.sent.clone();
        ctx.woke = self.ctx.woke.clone();
        Machine {
            cpu,
            ctx,
            ram: self.ram.clone(),
            reads: Vec::new(),
            writes: Vec::new(),
            walk_wakes: Vec::new(),
        }
    }

    /// Writes the M prologue at [`ENTRY`]: sets `regs`, `stvec` = [`HANDLER_VA`],
    /// `medeleg`, `satp`, and `mstatus` = `mstatus` with `MPP` = `privilege`, then `MRET`s to
    /// `entry`.
    fn boot(&mut self, b: &Boot) {
        let mut code = Vec::new();
        for (rd, value) in &b.regs {
            code.extend(li(*rd, *value));
        }
        for (csr, value) in [
            (STVEC, HANDLER_VA),
            (MEDELEG, b.medeleg),
            (SATP, b.satp),
            (MSTATUS, b.mstatus | u32::from(b.privilege) << 11),
            (MEPC, b.entry),
        ] {
            code.extend(li(31, value));
            code.push(csrrw(0, csr, 31));
        }
        code.push(MRET);
        assert!(code.len() * 4 <= 0x1000);
        self.code(u64::from(ENTRY), &code);
    }

    /// Maps the code and handler megapages.
    fn map_code(&mut self, user: bool) {
        let u = if user { PU } else { 0 };
        self.poke(slot(ROOT, 0x300), pte(0x80000, RWX_AD | u));
        self.poke(slot(ROOT, 0x301), pte(0x80000, RWX_AD));
    }

    /// Maps `va` through a level-0 table to the 4 KiB page `ppn` with `flags`.
    fn map_4k(&mut self, va: u32, table: u32, ppn: u32, flags: u32) {
        self.poke(slot(ROOT, vpn1(va)), pte(table, V));
        self.poke(slot(table, vpn0(va)), pte(ppn, flags));
    }

    fn committed(&self) -> Vec<&Vec<(&'static str, Value)>> {
        self.ctx
            .traced
            .iter()
            .filter(|r| r.0 == COMMIT_KIND)
            .map(|r| &r.1)
            .collect()
    }
}

struct Boot {
    privilege: u8,
    mstatus: u32,
    satp: u32,
    medeleg: u32,
    entry: u32,
    regs: Vec<(u32, u32)>,
}

impl Default for Boot {
    fn default() -> Boot {
        Boot {
            privilege: S,
            mstatus: 0,
            satp: SATP_SV32,
            medeleg: 0,
            entry: CODE_VA,
            regs: Vec::new(),
        }
    }
}

fn field(fields: &[(&'static str, Value)], name: &str) -> Value {
    fields
        .iter()
        .find(|f| f.0 == name)
        .unwrap_or_else(|| panic!("{name}: {fields:?}"))
        .1
        .clone()
}

fn uv(v: u64) -> Value {
    Value::U64(v)
}

/// The trap cause with `mcause` code `code`.
fn cause_of(code: u32) -> TrapCause {
    match code {
        1 => TrapCause::InstructionAccessFault,
        4 => TrapCause::LoadAddressMisaligned,
        5 => TrapCause::LoadAccessFault,
        6 => TrapCause::StoreAddressMisaligned,
        7 => TrapCause::StoreAccessFault,
        12 => TrapCause::InstructionPageFault,
        13 => TrapCause::LoadPageFault,
        15 => TrapCause::StorePageFault,
        _ => panic!("{code}"),
    }
}

fn ecall_from(privilege: u8) -> TrapCause {
    if privilege == U {
        TrapCause::EnvironmentCallFromU
    } else {
        TrapCause::EnvironmentCallFromS
    }
}

// ---------------------------------------------------------------------------------------
// One access against the reference.

/// One translated access: a fetch at `va`, or `LW x5, 0(x6)` / `SW x7, 0(x6)` with
/// `x6` = `va`, run from [`CODE_VA`] in `privilege`.
#[derive(Clone, Copy, Debug)]
struct Case {
    kind: Kind,
    privilege: u8,
    sum: bool,
    mxr: bool,
    va: u32,
}

impl Case {
    fn mstatus(&self) -> u32 {
        (if self.sum { SUM_BIT } else { 0 }) | (if self.mxr { MXR_BIT } else { 0 })
    }

    fn insn(&self) -> u32 {
        match self.kind {
            Kind::Load => lw(5, 6, 0),
            Kind::Store => sw(7, 6, 0),
            Kind::Fetch => ECALL,
        }
    }

    /// Boots `m` for this case and runs it into `privilege`. The page tables for `va` are
    /// the caller's.
    fn prepare(&self, m: &mut Machine) {
        m.map_code(self.privilege == U);
        m.code(CODE_PA, &[self.insn(), ECALL]);
        m.boot(&Boot {
            privilege: self.privilege,
            mstatus: self.mstatus(),
            entry: if self.kind == Kind::Fetch {
                self.va
            } else {
                CODE_VA
            },
            regs: vec![(6, self.va), (7, STORE_VALUE)],
            ..Boot::default()
        });
        m.enter();
    }
}

/// Runs `case` on `m` (already mapped) and checks every observable against the reference:
/// the trap, the bus reads and writes, the loaded or stored value, the trace's `paddr`, and
/// that no PTE changed. With `split`, also forks the machine after that many events and
/// requires the copy to continue identically.
fn check(case: Case, m: &mut Machine, split: Option<usize>) {
    case.prepare(m);
    let (walk, outcome) = sv32_ref::translate(
        SATP_SV32,
        case.privilege,
        case.sum,
        case.mxr,
        case.kind,
        case.va,
        |a| m.word(a),
    );
    let misaligned = case.kind != Kind::Fetch && !case.va.is_multiple_of(4);
    let target = match outcome {
        Outcome::Pa(pa) if !misaligned && in_ram(pa, 4) => Some(pa),
        _ => None,
    };
    if let Some(pa) = target {
        let value = if case.kind == Kind::Fetch {
            ECALL
        } else {
            marker(pa)
        };
        m.poke(pa, value);
    }
    let ram = m.ram.clone();
    let trap = match split {
        None => m.run(),
        Some(k) => {
            for _ in 0..k {
                if !m.step() {
                    break;
                }
            }
            let mut copy = m.fork();
            let reads = std::mem::take(&mut m.reads);
            let writes = std::mem::take(&mut m.writes);
            let traced = std::mem::take(&mut m.ctx.traced);
            let trap = m.run();
            assert_eq!(copy.run(), trap);
            assert_eq!(copy.cpu.inspect(), m.cpu.inspect());
            assert_eq!(snapshot_of(&copy.cpu), snapshot_of(&m.cpu));
            assert_eq!(
                (&copy.reads, &copy.writes, &copy.ctx.traced),
                (&m.reads, &m.writes, &m.ctx.traced)
            );
            assert_eq!(copy.ram, m.ram);
            m.reads.splice(0..0, reads);
            m.writes.splice(0..0, writes);
            m.ctx.traced.splice(0..0, traced);
            trap
        }
    };
    let pc = if case.kind == Kind::Fetch {
        case.va
    } else {
        CODE_VA
    };
    let mut reads = if case.kind == Kind::Fetch {
        vec![]
    } else {
        vec![slot(ROOT, 0x300), CODE_PA]
    };
    let mut writes = vec![];
    let fault = |code: u32| RvTrap {
        cause: cause_of(code),
        pc,
        tval: case.va,
    };
    let expected = if misaligned {
        let code = if case.kind == Kind::Load { 4 } else { 6 };
        fault(code)
    } else {
        reads.extend(&walk);
        match outcome {
            Outcome::PageFault => fault(case.kind.page_fault_code()),
            Outcome::PteAccessFault => fault(case.kind.access_fault_code()),
            Outcome::Pa(pa) => {
                if case.kind == Kind::Store {
                    writes.push(pa);
                } else {
                    reads.push(pa);
                }
                if target.is_none() {
                    fault(case.kind.access_fault_code())
                } else {
                    if case.kind != Kind::Fetch {
                        reads.extend([slot(ROOT, 0x300), CODE_PA + 4]);
                    }
                    RvTrap {
                        cause: ecall_from(case.privilege),
                        pc: if case.kind == Kind::Fetch { pc } else { pc + 4 },
                        tval: 0,
                    }
                }
            }
        }
    };
    assert_eq!(trap, expected, "{case:x?} -> {outcome:x?}");
    assert_eq!(m.reads, reads, "{case:x?} -> {outcome:x?}");
    assert_eq!(m.writes, writes, "{case:x?}");
    // Only the store's own word changed: no PTE was written.
    let mut after = ram.clone();
    if let (Kind::Store, Some(pa)) = (case.kind, target) {
        let i = (pa - RAM_BASE) as usize;
        after[i..i + 4].copy_from_slice(&STORE_VALUE.to_le_bytes());
    }
    assert!(after == m.ram, "{case:x?}: RAM changed beyond the store");
    if let (Some(pa), Kind::Load | Kind::Store) = (target, case.kind) {
        if case.kind == Kind::Load {
            assert_eq!(reg(&m.cpu, 5), marker(pa));
        }
        let commits = m.committed();
        let access = commits[0];
        assert_eq!(field(access, "addr"), uv(u64::from(case.va)));
        assert_eq!(field(access, "paddr"), uv(pa));
    }
}

/// Maps the data VA with `root` at `root[vpn1]` and `leaf` at `L0[vpn0]`.
fn data_tables(m: &mut Machine, va: u32, root: u32, leaf: u32) {
    m.poke(slot(ROOT, vpn1(va)), root);
    m.poke(slot(L0, vpn0(va)), leaf);
}

fn run_case(case: Case, root: u32, leaf: u32) -> Machine {
    let mut m = Machine::new();
    data_tables(&mut m, case.va, root, leaf);
    check(case, &mut m, None);
    m
}

fn load(privilege: u8, va: u32) -> Case {
    Case {
        kind: Kind::Load,
        privilege,
        sum: false,
        mxr: false,
        va,
    }
}

fn with(kind: Kind, case: Case) -> Case {
    Case { kind, ..case }
}

/// Runs `case` with the 4 KiB leaf `flags` → [`DATA_PPN`], and returns its trap cause.
fn cause_4k(case: Case, flags: u32) -> TrapCause {
    let m = run_case(case, pte(L0, V), pte(DATA_PPN, flags));
    match m.cpu.halt() {
        Some(Halt::Trap(t)) => t.cause,
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// Bare and the basic walk.

#[test]
fn bare_satp_and_m_mode_access_their_own_address_without_a_walk() {
    for (privilege, satp) in [(S, 0), (U, 0), (M, SATP_SV32)] {
        let mut m = Machine::new();
        let va = 0x8020_0010;
        m.poke(u64::from(va), 0x1234_5678);
        m.code(CODE_PA, &[lw(5, 6, 0), sw(5, 6, 4), ECALL]);
        m.boot(&Boot {
            privilege,
            satp,
            entry: CODE_PA as u32,
            regs: vec![(6, va)],
            ..Boot::default()
        });
        if privilege == M {
            // MRET to M: run from the MRET on.
            while m.cpu.pc() != CODE_PA as u32 {
                assert!(m.step());
            }
            m.clear();
        } else {
            m.enter();
        }
        let trap = m.run();
        assert_eq!(trap.pc, CODE_PA as u32 + 8);
        assert_eq!(
            m.reads,
            [CODE_PA, u64::from(va), CODE_PA + 4, CODE_PA + 8],
            "{privilege}"
        );
        assert_eq!(m.writes, [u64::from(va) + 4]);
        assert_eq!(reg(&m.cpu, 5), 0x1234_5678);
        let commits = m.committed();
        assert_eq!(field(commits[0], "paddr"), uv(u64::from(va)));
        assert_eq!(field(commits[1], "paddr"), uv(u64::from(va) + 4));
    }
}

#[test]
fn a_4k_walk_reads_two_ptes_over_the_bus_for_fetch_load_and_store() {
    let mut m = Machine::new();
    // Code at VA 0x4000_1000 → PA 0x8000_1000; data at VA 0x4000_5000 → PA 0x8020_3000.
    let code_va = 0x4000_1000;
    m.map_4k(code_va, L0, 0x80001, V | R | X | A);
    m.poke(slot(L0, 5), pte(0x80203, V | R | W | A | D));
    m.code(CODE_PA, &[lw(5, 6, 8), sw(7, 6, 16), ECALL]);
    m.poke(0x8020_3008, 0xCAFE_F00D);
    m.boot(&Boot {
        entry: code_va,
        regs: vec![(6, 0x4000_5000), (7, 0x77)],
        ..Boot::default()
    });
    m.enter();
    let trap = m.run();
    assert_eq!(
        trap,
        RvTrap {
            cause: TrapCause::EnvironmentCallFromS,
            pc: code_va + 8,
            tval: 0
        }
    );
    let fetch_walk = [slot(ROOT, 0x100), slot(L0, 1)];
    let data_walk = [slot(ROOT, 0x100), slot(L0, 5)];
    let mut expected = vec![];
    expected.extend(fetch_walk);
    expected.push(CODE_PA);
    expected.extend(data_walk);
    expected.push(0x8020_3008);
    expected.extend(fetch_walk);
    expected.push(CODE_PA + 4);
    expected.extend(data_walk);
    expected.extend(fetch_walk);
    expected.push(CODE_PA + 8);
    assert_eq!(m.reads, expected);
    assert_eq!(m.writes, [0x8020_3010]);
    assert_eq!(reg(&m.cpu, 5), 0xCAFE_F00D);
    assert_eq!(m.word(0x8020_3010), Some(0x77));
    // Every walk step is its own wake, a cycle after the previous event.
    assert_eq!(m.walk_wakes.len(), 10);
    assert!(m.walk_wakes.iter().all(|w| *w
        == (
            ScheduleWhen::Cycles {
                domain: CLOCK,
                k: 1
            },
            Phase::Request
        )));
    // The trace keeps the VA in pc and addr, and the PA in paddr.
    let commits = m.committed();
    assert_eq!(field(commits[0], "pc"), uv(u64::from(code_va)));
    assert_eq!(field(commits[0], "addr"), uv(0x4000_5008));
    assert_eq!(field(commits[0], "paddr"), uv(0x8020_3008));
    assert_eq!(field(commits[1], "addr"), uv(0x4000_5010));
    assert_eq!(field(commits[1], "paddr"), uv(0x8020_3010));
}

#[test]
fn inspect_names_the_walk_in_progress() {
    let mut m = Machine::new();
    let case = load(S, 0x4000_5008);
    m.map_4k(case.va, L0, DATA_PPN, V | R | A);
    case.prepare(&mut m);
    let mut seen = Vec::new();
    loop {
        if let Some(state) = read_str(&m.cpu, "state") {
            if state.starts_with("walk") {
                seen.push((
                    state,
                    read_str(&m.cpu, "walk_purpose").unwrap(),
                    read_u(&m.cpu, "walk_level"),
                    read_u(&m.cpu, "walk_table"),
                ));
            } else {
                assert_eq!(read_str(&m.cpu, "walk_purpose"), None);
            }
        }
        if !m.step() {
            break;
        }
    }
    let at = |s: &str, p: &str, l: u32, t: u32| (s.to_owned(), p.to_owned(), l, t);
    assert_eq!(
        seen,
        [
            // The load's own fetch, through the code megapage.
            at("walk_issue", "fetch", 1, ROOT),
            at("walk_wait", "fetch", 1, ROOT),
            // The load's walk: the root, then L0.
            at("walk_issue", "data", 1, ROOT),
            at("walk_wait", "data", 1, ROOT),
            at("walk_issue", "data", 0, L0),
            at("walk_wait", "data", 0, L0),
            // The next fetch's megapage walk.
            at("walk_issue", "fetch", 1, ROOT),
            at("walk_wait", "fetch", 1, ROOT),
        ]
    );
    // The walk fields follow `state`.
    let names: Vec<&str> = {
        let mut m = Machine::new();
        m.map_4k(case.va, L0, DATA_PPN, V | R | A);
        case.prepare(&mut m);
        m.step();
        m.cpu.inspect().fields.iter().map(|f| f.0).collect()
    };
    let at = names.iter().position(|n| *n == "state").unwrap();
    assert_eq!(
        &names[at..at + 4],
        ["state", "walk_purpose", "walk_level", "walk_table"]
    );
}

// ---------------------------------------------------------------------------------------
// Superpages and invalid PTEs.

#[test]
fn a_megapage_takes_one_pte_read_and_maps_its_whole_4_mib() {
    for offset in [0, 0x124, 0x1000, 0x3F_FFFC] {
        let va = DATA_VA + offset;
        for kind in [Kind::Load, Kind::Store, Kind::Fetch] {
            let case = with(kind, load(S, va));
            let mut m = Machine::new();
            m.poke(slot(ROOT, 0x100), pte(MEGA_PPN, RWX_AD));
            check(case, &mut m, None);
            let pa = u64::from(MEGA_PPN) << 12 | u64::from(offset);
            assert!(m.reads.contains(&slot(ROOT, 0x100)));
            assert!(
                !m.reads
                    .iter()
                    .any(|r| (slot(L0, 0)..slot(L0, 1024)).contains(r))
            );
            assert!(m.reads.contains(&pa) || m.writes.contains(&pa));
        }
    }
    // The next megapage is not mapped.
    let m = run_case(load(S, DATA_VA + 0x40_0000), 0, 0);
    assert_eq!(m.cpu.halt().map(cause), Some(TrapCause::LoadPageFault));
}

fn cause(h: Halt) -> TrapCause {
    match h {
        Halt::Trap(t) => t.cause,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_misaligned_megapage_is_a_page_fault_for_every_access() {
    for low in [1, 0x200, 0x3FF] {
        for kind in [Kind::Load, Kind::Store, Kind::Fetch] {
            let case = with(kind, load(S, DATA_VA + 0x2000));
            let mut m = Machine::new();
            m.poke(slot(ROOT, 0x100), pte(MEGA_PPN | low, RWX_AD));
            check(case, &mut m, None);
            let trap = m.cpu.halt().map(cause);
            let pf = [
                TrapCause::InstructionPageFault,
                TrapCause::LoadPageFault,
                TrapCause::StorePageFault,
            ];
            assert!(pf.contains(&trap.unwrap()), "{low:#x} {kind:?}: {trap:?}");
        }
    }
}

#[test]
fn invalid_ptes_are_page_faults_at_either_level() {
    let va = DATA_VA + 0x5008;
    let good_root = pte(L0, V);
    let good_leaf = pte(DATA_PPN, V | R | W | A | D);
    let cases = [
        // V = 0, at L1 and at L0, whatever else is set.
        (0, good_leaf),
        (pte(L0, RWX_AD & !V), good_leaf),
        (good_root, pte(DATA_PPN, RWX_AD & !V)),
        // R = 0 with W = 1, at L1 and at L0, with or without X.
        (pte(MEGA_PPN, V | W | A | D), good_leaf),
        (pte(MEGA_PPN, V | W | X | A | D), good_leaf),
        (good_root, pte(DATA_PPN, V | W | A | D)),
        (good_root, pte(DATA_PPN, V | W | X | A | D)),
        // A pointer at level 0.
        (good_root, pte(DATA_PPN, V)),
        // A pointer with a reserved non-leaf bit.
        (pte(L0, V | PU), good_leaf),
        (pte(L0, V | A), good_leaf),
        (pte(L0, V | D), good_leaf),
    ];
    for (root, leaf) in cases {
        for kind in [Kind::Load, Kind::Store, Kind::Fetch] {
            let m = run_case(with(kind, load(S, va)), root, leaf);
            let expected = match kind {
                Kind::Fetch => TrapCause::InstructionPageFault,
                Kind::Load => TrapCause::LoadPageFault,
                Kind::Store => TrapCause::StorePageFault,
            };
            assert_eq!(
                m.cpu.halt().map(cause),
                Some(expected),
                "{root:#x} {leaf:#x}"
            );
        }
    }
    // G and the RSW bits change nothing.
    for extra in [G, 1 << 8, 1 << 9, G | 3 << 8] {
        let m = run_case(
            load(S, va),
            pte(L0, V | extra),
            pte(DATA_PPN, V | R | A | extra),
        );
        assert_eq!(
            m.cpu.halt().map(cause),
            Some(TrapCause::EnvironmentCallFromS)
        );
    }
}

// ---------------------------------------------------------------------------------------
// Permissions, SUM, MXR, and Svade.

#[test]
fn the_permission_matrix_matches_the_reference() {
    let va = DATA_VA + 0x5008;
    for privilege in [S, U] {
        for rwxu in 0..16 {
            let flags = V | A | D | rwxu << 1;
            for kind in [Kind::Fetch, Kind::Load, Kind::Store] {
                for (sum, mxr) in [(false, false), (true, false), (false, true), (true, true)] {
                    let case = Case {
                        kind,
                        privilege,
                        sum,
                        mxr,
                        va,
                    };
                    run_case(case, pte(L0, V), pte(DATA_PPN, flags));
                }
            }
        }
    }
}

#[test]
fn the_permission_rules_spelled_out() {
    let va = DATA_VA + 0x5008;
    let ok_s = TrapCause::EnvironmentCallFromS;
    let ok_u = TrapCause::EnvironmentCallFromU;
    let (lpf, spf, ipf) = (
        TrapCause::LoadPageFault,
        TrapCause::StorePageFault,
        TrapCause::InstructionPageFault,
    );
    let s = load(S, va);
    let u = load(U, va);
    let sum = Case { sum: true, ..s };
    let mxr = Case { mxr: true, ..s };
    // X-only: fetch yes; load only with MXR; store never.
    assert_eq!(cause_4k(with(Kind::Fetch, s), V | X | A), ok_s);
    assert_eq!(cause_4k(s, V | X | A), lpf);
    assert_eq!(cause_4k(mxr, V | X | A), ok_s);
    assert_eq!(cause_4k(with(Kind::Store, mxr), V | X | A | D), spf);
    // R-only: no fetch.
    assert_eq!(cause_4k(with(Kind::Fetch, s), V | R | A), ipf);
    // U touches only U pages.
    assert_eq!(cause_4k(u, V | R | A), lpf);
    assert_eq!(cause_4k(u, V | R | PU | A), ok_u);
    assert_eq!(cause_4k(with(Kind::Fetch, u), V | X | A), ipf);
    // S touches a U page's data only with SUM, and never fetches from one.
    assert_eq!(cause_4k(s, V | R | PU | A), lpf);
    assert_eq!(cause_4k(sum, V | R | PU | A), ok_s);
    assert_eq!(cause_4k(with(Kind::Store, s), RWX_AD | PU), spf);
    assert_eq!(cause_4k(with(Kind::Store, sum), RWX_AD | PU), ok_s);
    assert_eq!(cause_4k(with(Kind::Fetch, s), RWX_AD | PU), ipf);
    assert_eq!(cause_4k(with(Kind::Fetch, sum), RWX_AD | PU), ipf);
    // SUM changes nothing for U, and MXR does not grant U access.
    let u_mxr = Case {
        sum: true,
        mxr: true,
        ..u
    };
    assert_eq!(cause_4k(u_mxr, V | X | A), lpf);
    assert_eq!(cause_4k(u_mxr, V | X | PU | A), ok_u);
}

#[test]
fn svade_faults_on_a_clear_a_or_a_clear_d_store_and_never_writes_a_pte() {
    let va = DATA_VA + 0x5008;
    let s = load(S, va);
    // A = 0: every access faults.
    assert_eq!(
        cause_4k(with(Kind::Fetch, s), V | R | W | X | D),
        TrapCause::InstructionPageFault
    );
    assert_eq!(cause_4k(s, V | R | W | X | D), TrapCause::LoadPageFault);
    assert_eq!(
        cause_4k(with(Kind::Store, s), V | R | W | X | D),
        TrapCause::StorePageFault
    );
    // D = 0: only the store faults.
    assert_eq!(
        cause_4k(with(Kind::Fetch, s), V | R | W | X | A),
        TrapCause::EnvironmentCallFromS
    );
    assert_eq!(
        cause_4k(s, V | R | W | X | A),
        TrapCause::EnvironmentCallFromS
    );
    assert_eq!(
        cause_4k(with(Kind::Store, s), V | R | W | X | A),
        TrapCause::StorePageFault
    );
    // A megapage too.
    let m = run_case(with(Kind::Store, s), pte(MEGA_PPN, V | R | W | A), 0);
    assert_eq!(m.cpu.halt().map(cause), Some(TrapCause::StorePageFault));
    // `check` requires the RAM to be unchanged but for the store; also no write request
    // went to a table.
    for flags in [V | R | W | X | D, V | R | W | X | A] {
        let m = run_case(with(Kind::Store, s), pte(L0, V), pte(DATA_PPN, flags));
        assert!(m.writes.is_empty());
    }
}

// ---------------------------------------------------------------------------------------
// Fault priority and access faults.

#[test]
fn misalignment_is_checked_before_any_walk() {
    // Nothing is mapped: a walk would page-fault.
    for (kind, va, code) in [
        (Kind::Load, DATA_VA + 2, 4),
        (Kind::Load, DATA_VA + 1, 4),
        (Kind::Store, DATA_VA + 3, 6),
    ] {
        let mut m = Machine::new();
        let case = with(kind, load(U, va));
        check(case, &mut m, None);
        let t = match m.cpu.halt() {
            Some(Halt::Trap(t)) => t,
            other => panic!("{other:?}"),
        };
        assert_eq!((t.cause.code(), t.tval), (code, va));
        // Only the instruction's own fetch was read.
        assert_eq!(m.reads, [slot(ROOT, 0x300), CODE_PA]);
    }
}

#[test]
fn a_pte_the_bus_refuses_is_the_accesss_access_fault_with_the_va() {
    let va = DATA_VA + 0x5008;
    let outside = 0x0010_0000; // PA 0x1_0000_0000: no memory.
    for kind in [Kind::Fetch, Kind::Load, Kind::Store] {
        let code = match kind {
            Kind::Fetch => 1,
            Kind::Load => 5,
            Kind::Store => 7,
        };
        // The level-0 table, and the root itself, lie outside memory.
        let m = run_case(with(kind, load(S, va)), pte(outside, V), 0);
        let t = m.cpu.halt().unwrap();
        assert_eq!(
            t,
            Halt::Trap(RvTrap {
                cause: cause_of(code),
                pc: if kind == Kind::Fetch { va } else { CODE_VA },
                tval: va
            })
        );
        assert_eq!(m.reads.last(), Some(&slot(outside, vpn0(va))));
    }
    // The root outside memory: a fetch walk from S faults before any code runs.
    let mut m = Machine::new();
    m.map_code(false);
    m.boot(&Boot {
        satp: 1 << 31 | 0x0010_0000,
        ..Boot::default()
    });
    m.enter();
    let t = m.run();
    assert_eq!(
        (t.cause, t.pc, t.tval),
        (TrapCause::InstructionAccessFault, CODE_VA, CODE_VA)
    );
    assert_eq!(m.reads, [slot(0x0010_0000, 0x300)]);
}

#[test]
fn the_final_access_faults_after_the_walk_but_a_denied_leaf_is_a_page_fault() {
    let va = DATA_VA + 0x5008;
    let nowhere = 0x0030_0000; // PA 0x3_0000_0000: no memory.
    // Allowed: the walk succeeds and the access is refused.
    for kind in [Kind::Fetch, Kind::Load, Kind::Store] {
        let m = run_case(with(kind, load(S, va)), pte(L0, V), pte(nowhere, RWX_AD));
        let t = m.cpu.halt().map(cause).unwrap();
        assert_eq!(t.code(), kind.access_fault_code());
    }
    // Denied: the page fault wins, and the access is never issued.
    for kind in [Kind::Fetch, Kind::Load, Kind::Store] {
        let m = run_case(with(kind, load(U, va)), pte(L0, V), pte(nowhere, RWX_AD));
        let t = m.cpu.halt().map(cause).unwrap();
        assert_eq!(t.code(), kind.page_fault_code());
        assert!(!m.reads.contains(&(u64::from(nowhere) << 12 | 0x008)));
    }
}

// ---------------------------------------------------------------------------------------
// Delegation.

/// Runs `case` with `medeleg`, the data VA mapped by `leaf` through `L0`, and the handler
/// `ECALL`; returns the machine at the halt.
fn delegated(case: Case, medeleg: u32, leaf: u32) -> Machine {
    let mut m = Machine::new();
    m.map_code(case.privilege == U);
    m.map_4k(case.va, L0, DATA_PPN, leaf);
    m.code(CODE_PA, &[case.insn(), ECALL]);
    m.code(HANDLER_PA, &[ECALL]);
    m.boot(&Boot {
        privilege: case.privilege,
        mstatus: case.mstatus(),
        medeleg,
        entry: if case.kind == Kind::Fetch {
            case.va
        } else {
            CODE_VA
        },
        regs: vec![(6, case.va), (7, STORE_VALUE)],
        ..Boot::default()
    });
    m.enter();
    m.run();
    m
}

#[test]
fn page_faults_are_delegated_to_s_with_the_va() {
    let va = DATA_VA + 0x5008;
    for privilege in [U, S] {
        for (kind, code) in [(Kind::Fetch, 12), (Kind::Load, 13), (Kind::Store, 15)] {
            let case = with(kind, load(privilege, va));
            // An invalid leaf.
            let m = delegated(case, 1 << code, 0);
            let pc = if kind == Kind::Fetch { va } else { CODE_VA };
            assert_eq!(
                m.cpu.halt(),
                Some(Halt::Trap(RvTrap {
                    cause: TrapCause::EnvironmentCallFromS,
                    pc: HANDLER_VA,
                    tval: 0
                })),
                "{privilege} {kind:?}"
            );
            assert_eq!(read_u(&m.cpu, "scause"), code);
            assert_eq!(read_u(&m.cpu, "stval"), va);
            assert_eq!(read_u(&m.cpu, "sepc"), pc);
            assert_eq!(
                read_u(&m.cpu, "sstatus") & 1 << 8 != 0,
                privilege == S,
                "SPP"
            );
            let (kind_name, f) = &m.ctx.traced[0];
            assert_eq!(*kind_name, EXCEPTION_KIND);
            let insn = if kind == Kind::Fetch { 0 } else { case.insn() };
            assert_eq!(field(f, "insn"), uv(u64::from(insn)));
            assert_eq!(field(f, "tval"), uv(u64::from(va)));
            // M saw nothing.
            assert_eq!(read_u(&m.cpu, "mcause"), 0);
        }
    }
}

#[test]
fn page_faults_without_their_medeleg_bit_halt() {
    let va = DATA_VA + 0x5008;
    for (kind, code) in [(Kind::Fetch, 12), (Kind::Load, 13), (Kind::Store, 15)] {
        let case = with(kind, load(U, va));
        let m = delegated(case, 0xB1FF & !(1 << code), 0);
        let pc = if kind == Kind::Fetch { va } else { CODE_VA };
        assert_eq!(
            m.cpu.halt(),
            Some(Halt::Trap(RvTrap {
                cause: cause_of(code),
                pc,
                tval: va
            }))
        );
        assert_eq!(read_u(&m.cpu, "scause"), 0);
    }
    // A PTE access fault is delegated by its own bit.
    let case = load(U, va);
    let mut m = Machine::new();
    m.map_code(true);
    m.poke(slot(ROOT, 0x100), pte(0x0010_0000, V));
    m.code(CODE_PA, &[case.insn()]);
    m.code(HANDLER_PA, &[ECALL]);
    m.boot(&Boot {
        privilege: U,
        medeleg: 1 << 5,
        regs: vec![(6, va)],
        ..Boot::default()
    });
    m.enter();
    m.run();
    assert_eq!((read_u(&m.cpu, "scause"), read_u(&m.cpu, "stval")), (5, va));
}

// ---------------------------------------------------------------------------------------
// No TLB, and SFENCE.VMA.

#[test]
fn a_pte_change_is_seen_by_the_next_access_without_sfence() {
    // S maps its tables at VA = PA through root[0x200], and rewrites L0[5] between loads.
    let va = DATA_VA + 0x5008;
    let mut m = Machine::new();
    m.map_code(false);
    m.poke(slot(ROOT, 0x200), pte(0x80000, V | R | W | A | D));
    m.map_4k(va, L0, DATA_PPN, V | R | A);
    m.poke(0x8020_0008, 0x1111_1111);
    m.poke(0x8020_1008, 0x2222_2222);
    let new_leaf = pte(DATA_PPN + 1, V | R | A);
    m.code(
        CODE_PA,
        &[
            lw(5, 6, 0),
            sw(8, 9, 0),
            lw(10, 6, 0),
            SFENCE_VMA,
            sw(0, 9, 0),
            lw(11, 6, 0),
            ECALL,
        ],
    );
    m.boot(&Boot {
        regs: vec![(6, va), (8, new_leaf), (9, slot(L0, vpn0(va)) as u32)],
        ..Boot::default()
    });
    m.enter();
    m.run();
    assert_eq!(reg(&m.cpu, 5), 0x1111_1111);
    assert_eq!(reg(&m.cpu, 10), 0x2222_2222);
    // After SFENCE.VMA and an invalidated leaf, the load faults... it halted at the third
    // load with a page fault.
    assert_eq!(
        m.cpu.halt(),
        Some(Halt::Trap(RvTrap {
            cause: TrapCause::LoadPageFault,
            pc: CODE_VA + 20,
            tval: va
        }))
    );
    // Each load walked afresh: three walks of the data VA.
    let walks = m.reads.iter().filter(|r| **r == slot(L0, 5)).count();
    assert_eq!(walks, 3);
}

#[test]
fn sfence_vma_retires_under_sv32_in_s_and_is_illegal_in_u() {
    for privilege in [S, U] {
        let mut m = Machine::new();
        m.map_code(privilege == U);
        m.code(CODE_PA, &[SFENCE_VMA, ECALL]);
        m.boot(&Boot {
            privilege,
            ..Boot::default()
        });
        m.enter();
        let t = m.run();
        if privilege == S {
            assert_eq!(
                (t.cause, t.pc),
                (TrapCause::EnvironmentCallFromS, CODE_VA + 4)
            );
            let commits = m.committed();
            assert_eq!(field(commits[0], "insn"), uv(u64::from(SFENCE_VMA)));
            // It walked for its own fetch and the next, and touched nothing else.
            assert_eq!(
                m.reads,
                [slot(ROOT, 0x300), CODE_PA, slot(ROOT, 0x300), CODE_PA + 4]
            );
        } else {
            assert_eq!(
                t,
                RvTrap {
                    cause: TrapCause::IllegalInstruction,
                    pc: CODE_VA,
                    tval: SFENCE_VMA
                }
            );
        }
    }
}

#[test]
fn s_can_turn_translation_on_and_off_and_satp_keeps_mode_and_ppn() {
    // S runs at VA = PA through root[0x200]; it turns Sv32 on, reads satp, and loads
    // through a 4 KiB page.
    let mut m = Machine::new();
    m.poke(slot(ROOT, 0x200), pte(0x80000, RWX_AD));
    m.map_4k(DATA_VA, L0, DATA_PPN, V | R | A);
    m.poke(0x8020_0000, 0xABCD);
    let [a, b] = li(1, SATP_SV32);
    m.code(
        CODE_PA,
        &[
            a,
            b,
            csrrw(0, SATP, 1),
            csrrs(2, SATP, 0),
            lw(3, 6, 0),
            ECALL,
        ],
    );
    m.boot(&Boot {
        satp: 0,
        entry: CODE_PA as u32,
        regs: vec![(6, DATA_VA)],
        ..Boot::default()
    });
    m.enter();
    m.run();
    assert_eq!(reg(&m.cpu, 2), SATP_SV32);
    assert_eq!(reg(&m.cpu, 3), 0xABCD);
    // The fetches before the write went straight to memory; the ones after walked.
    assert_eq!(&m.reads[..3], [CODE_PA, CODE_PA + 4, CODE_PA + 8]);
    assert_eq!(&m.reads[3..5], [slot(ROOT, 0x200), CODE_PA + 12]);
}

// ---------------------------------------------------------------------------------------
// Protocol.

#[test]
fn a_pte_read_answered_wrongly_is_a_component_fault() {
    for bad in 0..2 {
        let mut m = Machine::new();
        let case = load(S, DATA_VA + 8);
        case.prepare(&mut m);
        // Run until the first walk sends its PTE read.
        loop {
            assert!(m.step());
            if read_str(&m.cpu, "state").as_deref() == Some("walk_wait") {
                break;
            }
        }
        let sent = m.ctx.take_sent();
        let MemMsg::ReadReq { txn, len: 4, .. } = sent else {
            panic!("{sent:?}");
        };
        let resp = if bad == 0 {
            MemMsg::ReadResp {
                txn,
                outcome: ReadOutcome::Data { data: vec![0; 2] },
            }
        } else {
            MemMsg::WriteResp {
                txn,
                outcome: WriteOutcome::Done,
            }
        };
        assert!(m.ctx.respond(&mut m.cpu, resp).is_err());
    }
}

// ---------------------------------------------------------------------------------------
// Snapshots.

/// M boots, delegates, and enters S; S turns Sv32 on, loads and stores through a 4 KiB
/// page, and enters U; U loads from an S page, and the page fault is delegated to S; the
/// handler reads scause and stval and halts on ECALL.
fn scenario() -> Machine {
    let mut m = Machine::new();
    let s_pc = CODE_PA as u32;
    let u_va = DATA_VA + 0x1000;
    let data = DATA_VA + 0x5008;
    // S runs at VA = PA; U code at U_VA → PA 0x8000_2000; data at DATA → PA 0x8020_3008.
    m.poke(slot(ROOT, 0x200), pte(0x80000, RWX_AD));
    m.map_4k(u_va, L0, 0x80002, V | R | X | PU | A);
    m.poke(slot(L0, 5), pte(0x80203, V | R | W | A | D));
    m.poke(0x8020_3008, 0xCAFE_F00D);
    let mut s = Vec::new();
    s.extend(li(2, SATP_SV32));
    s.push(csrrw(0, SATP, 2));
    s.extend(li(6, data));
    s.push(lw(5, 6, 0));
    s.extend(li(7, 0x55));
    s.push(sw(7, 6, 4));
    s.extend(li(3, u_va));
    s.push(csrrw(0, SEPC, 3));
    s.push(SRET);
    m.code(CODE_PA, &s);
    m.code(0x8000_2000, &[lw(8, 6, 0)]);
    m.code(
        HANDLER_PA,
        &[
            csrrs(9, SCAUSE, 0),
            csrrs(10, STVAL, 0),
            csrrs(11, SSTATUS, 0),
            ECALL,
        ],
    );
    let mut boot = Vec::new();
    for (csr, value) in [
        (STVEC, HANDLER_PA as u32),
        (MEDELEG, 0xffff_ffff),
        (MSTATUS, u32::from(S) << 11),
        (MEPC, s_pc),
    ] {
        boot.extend(li(31, value));
        boot.push(csrrw(0, csr, 31));
    }
    boot.push(MRET);
    m.code(u64::from(ENTRY), &boot);
    m
}

fn check_scenario_end(m: &Machine) {
    assert_eq!(
        m.cpu.halt(),
        Some(Halt::Trap(RvTrap {
            cause: TrapCause::EnvironmentCallFromS,
            pc: HANDLER_PA as u32 + 12,
            tval: 0
        }))
    );
    assert_eq!(reg(&m.cpu, 5), 0xCAFE_F00D);
    assert_eq!(m.word(0x8020_300C), Some(0x55));
    assert_eq!((reg(&m.cpu, 9), reg(&m.cpu, 10)), (13, DATA_VA + 0x5008));
    assert_eq!(reg(&m.cpu, 8), 0);
    assert_eq!(read_u(&m.cpu, "sepc"), DATA_VA + 0x1000);
    assert_eq!(mode(&m.cpu), S);
}

#[test]
fn the_scenario_runs_to_its_end() {
    let mut m = scenario();
    m.run();
    check_scenario_end(&m);
}

#[test]
fn restored_cpus_continue_identically_from_every_event_of_a_translated_run() {
    let events = {
        let mut m = scenario();
        let mut n = 0;
        while m.step() {
            n += 1;
        }
        n
    };
    let mut seen = std::collections::BTreeSet::new();
    for split in 0..=events {
        let mut original = scenario();
        for _ in 0..split {
            assert!(original.step());
        }
        let state = read_str(&original.cpu, "state").unwrap();
        let level = original
            .cpu
            .inspect()
            .get("walk_level")
            .cloned()
            .map(|v| format!("{v:?}"));
        seen.insert((state, level, mode(&original.cpu)));
        let mut copy = original.fork();
        original.clear();
        loop {
            let (a, b) = (original.step(), copy.step());
            assert_eq!(a, b);
            assert_eq!(copy.cpu.inspect(), original.cpu.inspect(), "split {split}");
            if !a {
                break;
            }
        }
        assert_eq!(copy.ctx.traced, original.ctx.traced, "split {split}");
        assert_eq!(
            (&copy.reads, &copy.writes),
            (&original.reads, &original.writes)
        );
        assert_eq!(snapshot_of(&copy.cpu), snapshot_of(&original.cpu));
        check_scenario_end(&copy);
    }
    // The splits covered every walk position, the final access, and the delegated fault.
    let has = |state: &str, level: Option<&str>| {
        seen.iter()
            .any(|(s, l, _)| s == state && l.as_deref() == level)
    };
    for level in ["U64(1)", "U64(0)"] {
        assert!(has("walk_issue", Some(level)), "{seen:?}");
        assert!(has("walk_wait", Some(level)), "{seen:?}");
    }
    for state in ["fetch_wait", "mem_issue", "mem_wait", "commit_pending"] {
        assert!(has(state, None), "{seen:?}");
    }
    assert!(seen.iter().any(|(s, _, p)| s == "walk_wait" && *p == U));
}

/// Every snapshot the scenario passes through, and the state each was taken in.
fn scenario_snapshots() -> &'static [(String, Vec<u8>)] {
    static SNAPSHOTS: OnceLock<Vec<(String, Vec<u8>)>> = OnceLock::new();
    SNAPSHOTS.get_or_init(|| {
        let mut m = scenario();
        let mut out = vec![];
        loop {
            out.push((read_str(&m.cpu, "state").unwrap(), snapshot_of(&m.cpu)));
            if !m.step() {
                break;
            }
        }
        out
    })
}

/// The state record's offset: after clock, entry, limit, pc, x1..x31, instret, next_txn.
const STATE_AT: usize = 4 + 4 + 8 + 4 + 31 * 4 + 8 + 8;

fn first_snapshot(state: &str, purpose_data: bool, level: u8) -> Vec<u8> {
    scenario_snapshots()
        .iter()
        .find(|(s, b)| {
            s == state
                && (b[STATE_AT + 9] == 1) == purpose_data
                && b[STATE_AT + 9 + if purpose_data { 5 } else { 1 }] == level
        })
        .map(|(_, b)| b.clone())
        .unwrap_or_else(|| panic!("no {state} {purpose_data} {level}"))
}

#[test]
fn restore_rejects_walk_and_pa_records_that_break_the_rules_and_changes_nothing() {
    // A data walk at level 0 of L0: tag 7, txn, purpose 1, insn, level, table.
    let wait0 = first_snapshot("walk_wait", true, 0);
    assert_eq!(wait0[STATE_AT], 7);
    let level_at = STATE_AT + 1 + 8 + 1 + 4;
    let table_at = level_at + 1;
    assert_eq!(
        u32::from_le_bytes(wait0[table_at..table_at + 4].try_into().unwrap()),
        L0
    );
    let wait1 = first_snapshot("walk_wait", true, 1);
    let edit = |base: &[u8], at: usize, bytes: &[u8]| {
        let mut b = base.to_vec();
        b[at..at + bytes.len()].copy_from_slice(bytes);
        b
    };
    let insn_at = STATE_AT + 1 + 8 + 1;
    let bad = [
        // Level 2.
        edit(&wait0, level_at, &[2]),
        // A level-1 table other than satp.PPN.
        edit(&wait1, table_at, &L0.to_le_bytes()),
        // A table wider than a PPN.
        edit(&wait0, table_at, &(1u32 << 22).to_le_bytes()),
        // A data walk for an instruction that does not access memory.
        edit(&wait0, insn_at, &addi(1, 1, 1).to_le_bytes()),
        // A data walk for a misaligned access (x6 + 1).
        edit(&wait0, insn_at, &lw(5, 6, 1).to_le_bytes()),
        // An unknown purpose tag.
        edit(&wait0, STATE_AT + 9, &[2]),
    ];
    // A walk with translation off: satp is the last word of the snapshot.
    let mut bare = wait0.clone();
    let n = bare.len();
    bare[n - 4..].copy_from_slice(&0u32.to_le_bytes());
    // The mode is before the M3 CSR words: 7 u8 then 7 u32 from the end.
    let mut in_m = wait0.clone();
    in_m[n - 7 * 4 - 7] = M;
    let mut target = Machine::new();
    target.cpu = restore_m3(&scenario_snapshots()[40].1).unwrap();
    let before = snapshot_of(&target.cpu);
    for (i, bytes) in bad.iter().chain([&bare, &in_m]).enumerate() {
        let err = restore_into(&mut target.cpu, SNAPSHOT_SCHEMA_M3, bytes);
        assert!(err.is_err(), "case {i}");
        assert_eq!(snapshot_of(&target.cpu), before, "case {i}");
    }
    // The control: the unedited record restores.
    assert!(restore_m3(&wait0).is_ok());
}

#[test]
fn restore_rejects_a_pa_that_breaks_the_rules() {
    // A translated MemIssue: tag 2, insn, plan, then pa 1 + u64.
    let (_, issue) = scenario_snapshots()
        .iter()
        .find(|(s, b)| s == "mem_issue" && b.len() > STATE_AT + 20)
        .unwrap()
        .clone();
    let restored = restore_m3(&issue).unwrap();
    assert_eq!(snapshot_of(&restored), issue);
    // The pa is the 8 bytes before the CSR blocks (24 + 35 bytes from the end).
    let tail = 24 + 35;
    let pa_at = issue.len() - tail - 8;
    let pa = u64::from_le_bytes(issue[pa_at..pa_at + 8].try_into().unwrap());
    assert_eq!(pa & 0xfff, u64::from(DATA_VA + 0x5008) & 0xfff);
    assert_eq!(issue[pa_at - 1], 1);
    for bad_pa in [pa ^ 4, pa | 1 << 34, pa ^ 0x800] {
        let mut b = issue.clone();
        b[pa_at..pa_at + 8].copy_from_slice(&bad_pa.to_le_bytes());
        assert!(restore_m3(&b).is_err(), "{bad_pa:#x}");
    }
    // No pa while translating.
    let mut none = issue[..pa_at - 1].to_vec();
    none.push(0);
    none.extend(&issue[pa_at + 8..]);
    assert!(restore_m3(&none).is_err());
}

#[test]
fn schema_2_rejects_the_walk_tags() {
    let mut cpu = Rv32iCpu::new(Rv32iConfig {
        profile: Rv32iProfile::M2,
        ..config()
    })
    .unwrap();
    let base = snapshot_of(&cpu);
    assert_eq!(base[STATE_AT], 0);
    for tag in [6u8, 7] {
        let mut b = base[..STATE_AT].to_vec();
        b.push(tag);
        if tag == 7 {
            b.extend(0u64.to_le_bytes());
        }
        // A fetch walk at level 1 of the root.
        b.push(0);
        b.push(1);
        b.extend(ROOT.to_le_bytes());
        b.extend(&base[STATE_AT + 1..]);
        assert!(restore_into(&mut cpu, SNAPSHOT_SCHEMA_M2, &b).is_err());
        assert_eq!(snapshot_of(&cpu), base);
    }
}

// ---------------------------------------------------------------------------------------
// Properties.

fn flags_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        // Everything allowed, U either way: the access succeeds unless the mode forbids it.
        3 => any::<bool>().prop_map(|u| RWX_AD | if u { PU } else { 0 }),
        // Valid and accessed, anything else.
        3 => any::<u8>().prop_map(|f| u32::from(f) | V | A),
        1 => any::<u8>().prop_map(u32::from),
    ]
    .prop_flat_map(|f| (Just(f), 0u32..4).prop_map(|(f, rsw)| f | rsw << 8))
}

/// A PPN that never lands on the prologue, the code, or the tables.
fn wild_ppn() -> impl Strategy<Value = u32> {
    (0u32..1 << 22).prop_filter("not the code or the tables", |p| {
        !(0x80000..=0x80102).contains(p)
    })
}

fn leaf_ppn() -> impl Strategy<Value = u32> {
    prop_oneof![
        4 => (0u32..0x200).prop_map(|p| DATA_PPN + p),
        1 => wild_ppn(),
    ]
}

fn root_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        // A pointer to L0, maybe with reserved bits.
        4 => (0u32..4, prop::bool::weighted(0.1)).prop_map(|(rsw, junk)| {
            pte(L0, V | rsw << 8 | if junk { A | PU } else { 0 })
        }),
        // A megapage, aligned or not.
        2 => (flags_strategy(), prop::bool::weighted(0.8))
            .prop_map(|(f, aligned)| pte(if aligned { MEGA_PPN } else { MEGA_PPN | 1 }, f)),
        // A pointer somewhere else.
        1 => wild_ppn().prop_map(|p| pte(p, V)),
        1 => any::<u32>(),
    ]
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        prop_oneof![Just(Kind::Fetch), Just(Kind::Load), Just(Kind::Store)],
        prop_oneof![Just(S), Just(U)],
        any::<bool>(),
        any::<bool>(),
        0u32..0x40_0000,
        prop::bool::weighted(0.1),
    )
        .prop_map(|(kind, privilege, sum, mxr, offset, misalign)| {
            let aligned = if misalign && kind != Kind::Fetch {
                offset | 1
            } else {
                offset & !3
            };
            Case {
                kind,
                privilege,
                sum,
                mxr,
                va: DATA_VA + aligned,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The CPU translates exactly as the reference does, and a copy restored after any
    /// event continues identically.
    #[test]
    fn the_cpu_matches_the_reference_translator(
        case in case_strategy(),
        root in root_strategy(),
        leaf_flags in flags_strategy(),
        leaf_ppn in leaf_ppn(),
        split in prop::option::of(0usize..40),
    ) {
        let mut m = Machine::new();
        data_tables(&mut m, case.va, root, pte(leaf_ppn, leaf_flags));
        check(case, &mut m, split);
    }

    /// With Bare, S and U access their own address, with no PTE read.
    #[test]
    fn bare_translation_is_the_identity(
        privilege in prop_oneof![Just(S), Just(U)],
        ppn in 0u32..1 << 22,
        word in 0u32..0x2_0000,
    ) {
        let mut m = Machine::new();
        let va = 0x8020_0000 + word * 4;
        m.poke(u64::from(va), marker(u64::from(va)));
        m.code(CODE_PA, &[lw(5, 6, 0), ECALL]);
        // MODE = 0: the PPN is stored but unused.
        m.boot(&Boot {
            privilege,
            satp: ppn,
            entry: CODE_PA as u32,
            regs: vec![(6, va)],
            ..Boot::default()
        });
        m.enter();
        m.run();
        prop_assert_eq!(read_u(&m.cpu, "satp"), ppn);
        prop_assert_eq!(&m.reads, &vec![CODE_PA, u64::from(va), CODE_PA + 4]);
        prop_assert_eq!(reg(&m.cpu, 5), marker(u64::from(va)));
    }

    /// A PTE rewritten between two loads is used by the second, with no SFENCE.VMA.
    #[test]
    fn no_tlb_the_second_load_uses_the_new_pte(
        privilege in prop_oneof![Just(S), Just(U)],
        leaf_flags in flags_strategy(),
        leaf_ppn in leaf_ppn(),
        offset in 0u32..0x400,
    ) {
        let va = DATA_VA + 0x5000 + offset * 4;
        let mut m = Machine::new();
        let first = pte(DATA_PPN, V | R | PU | A);
        data_tables(&mut m, va, pte(L0, V), first);
        m.map_code(privilege == U);
        m.code(CODE_PA, &[lw(5, 6, 0), lw(8, 6, 0), ECALL]);
        m.boot(&Boot {
            privilege,
            mstatus: SUM_BIT,
            regs: vec![(6, va)],
            ..Boot::default()
        });
        m.enter();
        let pa1 = u64::from(DATA_PPN) << 12 | u64::from(va & 0xfff);
        m.poke(pa1, marker(pa1));
        while m.committed().is_empty() {
            prop_assert!(m.step());
        }
        prop_assert_eq!(reg(&m.cpu, 5), marker(pa1));
        let second = pte(leaf_ppn, leaf_flags);
        m.poke(slot(L0, vpn0(va)), second);
        let (_, outcome) = sv32_ref::translate(
            SATP_SV32, privilege, true, false, Kind::Load, va, |a| m.word(a),
        );
        if let Outcome::Pa(pa) = outcome
            && in_ram(pa, 4)
        {
            m.poke(pa, marker(pa));
        }
        let trap = m.run();
        match outcome {
            Outcome::Pa(pa) if in_ram(pa, 4) => {
                prop_assert_eq!(trap.cause, ecall_from(privilege));
                prop_assert_eq!(reg(&m.cpu, 8), marker(pa));
            }
            Outcome::Pa(_) | Outcome::PteAccessFault => {
                prop_assert_eq!(trap.cause, TrapCause::LoadAccessFault);
            }
            Outcome::PageFault => {
                prop_assert_eq!(
                    (trap.cause, trap.pc, trap.tval),
                    (TrapCause::LoadPageFault, CODE_VA + 4, va)
                );
            }
        }
    }

    /// Arbitrary bytes never panic a schema 3 restore; whatever restores re-encodes to the
    /// same bytes; whatever is rejected leaves the CPU as it was.
    #[test]
    fn arbitrary_schema_3_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        let mut cpu = restore_m3(&scenario_snapshots()[40].1).unwrap();
        let before = snapshot_of(&cpu);
        match restore_into(&mut cpu, SNAPSHOT_SCHEMA_M3, &bytes) {
            Ok(()) => prop_assert_eq!(snapshot_of(&cpu), bytes),
            Err(_) => prop_assert_eq!(snapshot_of(&cpu), before),
        }
    }

    /// Mutated walk-era snapshots never panic either, with the same two outcomes.
    #[test]
    fn mutated_scenario_snapshots_restore_deterministically_or_not_at_all(
        which in any::<prop::sample::Index>(),
        at in any::<prop::sample::Index>(),
        xor in 1u8..=255,
        into in any::<prop::sample::Index>(),
    ) {
        let all = scenario_snapshots();
        let (_, base) = &all[which.index(all.len())];
        let mut bytes = base.clone();
        let i = at.index(bytes.len());
        bytes[i] ^= xor;
        let mut cpu = restore_m3(&all[into.index(all.len())].1).unwrap();
        let before = snapshot_of(&cpu);
        match restore_into(&mut cpu, SNAPSHOT_SCHEMA_M3, &bytes) {
            Ok(()) => prop_assert_eq!(snapshot_of(&cpu), bytes),
            Err(_) => prop_assert_eq!(snapshot_of(&cpu), before),
        }
    }
}
