//! The directed M3.3 programs for the Spike differential: Sv32 translation
//! (`docs/m3-design.md` §5.2–§5.4) run with the M3 CPU profile, from the cases of
//! m3-3-spike-appendix C.
//!
//! Like [`privgen`](crate::privgen), each program is machine code and a minimal ELF
//! written directly, with no assembler, and ends with an exception taken in M, where
//! SystemScope halts and the comparison ends. Every program has the same frame:
//!
//! - it starts in M, points `mtvec` at the §10.2 pass sequence and `stvec` at the S
//!   handler, sets `medeleg`, and writes the page tables of `Tables` with ordinary
//!   stores into zeroed RAM;
//! - it sets `satp` to Sv32 with `ASID` 0 and enters S or U with `MRET`. S code runs
//!   through an identity megapage (`U` = 0); U code runs from the same bytes through an
//!   alias megapage with `U` = 1 at `USER_ALIAS` above them;
//! - the S handler reads `scause`, `sepc`, `stval`, and `sstatus` into `t3`–`t6`. It
//!   returns to `ra` after a fetch fault (causes 1 and 12), since the faulting target has
//!   no next instruction, and to `sepc` + 4 otherwise.
//!
//! A fetch is `jalr ra, 0(t1)` to the VA; the target page holds `addi a3, x0, 0x77` and
//! `jalr x0, 0(ra)`. Loads and stores use `t1` as the VA and `a0`–`a2` as data.
//!
//! The bodies stay clear of the recorded divergences: the M3.2 ones (m3-2-spike-appendix
//! B.6), and every PTE is written before translation is on, but in `vm-sfence`, which
//! rewrites one and runs `SFENCE.VMA` before using it: Spike caches translations, and
//! without the fence the ISA leaves the result open (SystemScope's no-TLB behaviour is
//! covered by its own tests).

use systemscope_rv32i::csr::{MEPC, MSTATUS, MTVEC};
use systemscope_rv32i::privilege::{MEDELEG, SATP, SCAUSE, SEPC, SRET, SSTATUS, STVAL, STVEC};

use crate::csrgen::csr_word;
use crate::progen::{
    CODE_BASE, ExpectedTrap, JALR, LOAD, OP_IMM, Program, SYSTEM, WINDOW_SIZE, elf, i, li,
    pass_sequence, s,
};

const RA: u8 = 1;
const T0: u8 = 5;
const T1: u8 = 6;
const T2: u8 = 7;
const A0: u8 = 10;
const A1: u8 = 11;
const A2: u8 = 12;
const T3: u8 = 28;
const T4: u8 = 29;
const T5: u8 = 30;
const T6: u8 = 31;

const CSRRW: u32 = 1;
const CSRRS: u32 = 2;
const CSRRC: u32 = 3;
const BRANCH: u32 = 0x63;
const MRET: u32 = 0x3020_0073;

/// `ECALL`.
const ECALL: u32 = SYSTEM;
/// `SFENCE.VMA x0, x0`.
const SFENCE_VMA: u32 = 0x1200_0073;

const MPP_U: u32 = 0;
const MPP_S: u32 = 1 << 11;
const SUM: u32 = 1 << 18;
const MXR: u32 = 1 << 19;

/// `medeleg`: every M3 exception but an `ECALL` from U.
const DELEGATE: u32 = 0xB0FF;

/// PTE bits.
const V: u32 = 1 << 0;
const R: u32 = 1 << 1;
const W: u32 = 1 << 2;
const X: u32 = 1 << 3;
const U: u32 = 1 << 4;
const G: u32 = 1 << 5;
const A: u32 = 1 << 6;
const D: u32 = 1 << 7;
const RWX_AD: u32 = V | R | W | X | A | D;

/// The root table and the one level-0 table.
const ROOT: u32 = 0x8010_0000;
const L0: u32 = 0x8010_1000;
/// The data page, whose first word is [`PG_WORD`], and a second one for `vm-sfence`.
const PG: u32 = 0x8020_0000;
const PG2: u32 = 0x8020_2000;
const PG_WORD: u32 = 0x1111_1111;
const PG2_WORD: u32 = 0x2222_2222;
/// The fetch target page.
const FTARGET: u32 = 0x8020_1000;
/// A physical address with no memory on either side.
const NOWHERE: u32 = 0x4000_0000;
/// U code runs at its PA plus this, through `root[0x300]`.
const USER_ALIAS: u32 = 0x4000_0000;

/// `satp`: Sv32, `ASID` 0, the root.
const SATP_SV32: u32 = 1 << 31 | ROOT >> 12;

/// The VA of level-0 entry `n`: `0x4040_0000` + `n` × 4 KiB.
const fn l0va(n: u32) -> u32 {
    0x4040_0000 + n * 0x1000
}

/// Level-1 VAs (m3-3-spike-appendix C.1).
const VA_PTE_FAULT: u32 = 0x4000_0000;
const VA_RESERVED_POINTER: u32 = 0x4080_0000;
const VA_R0W1_L1: u32 = 0x40C0_0000;
const VA_XONLY_MEGA: u32 = 0x4100_0000;
const VA_MISALIGNED_MEGA: u32 = 0x4140_0000;
const VA_MEGA: u32 = 0x4180_0000;
const VA_INVALID_L1: u32 = 0x41C0_0000;
const VA_A0_MEGA: u32 = 0x4240_0000;
const VA_D0_MEGA: u32 = 0x4280_0000;

const fn pte(pa: u32, flags: u32) -> u32 {
    (pa >> 12) << 10 | flags
}

/// The page tables every program writes: the identity and alias megapages, the level-1
/// cases, and `l0`.
struct Tables;

impl Tables {
    /// `(address, value)` of every non-zero PTE and data word.
    fn words() -> Vec<(u32, u32)> {
        let root = |va: u32| ROOT + (va >> 22) * 4;
        let l0 = |n: u32| L0 + n * 4;
        vec![
            // The identity megapage over code, tables, and data, and the U alias.
            (root(0x8000_0000), pte(0x8000_0000, RWX_AD)),
            (root(0x8000_0000 + USER_ALIAS), pte(0x8000_0000, RWX_AD | U)),
            // Level-1 cases.
            (root(VA_PTE_FAULT), pte(NOWHERE, V)),
            (root(l0va(0)), pte(L0, V)),
            (root(VA_RESERVED_POINTER), pte(L0, V | U | A | D)),
            (root(VA_R0W1_L1), pte(L0, V | W)),
            (root(VA_XONLY_MEGA), pte(0x8000_0000, V | X | A | D)),
            (root(VA_MISALIGNED_MEGA), pte(0x8000_0000, RWX_AD) | 1 << 10),
            (root(VA_MEGA), pte(0x8000_0000, V | R | W | A | D)),
            (root(VA_A0_MEGA), pte(0x8000_0000, V | R | W | X)),
            (root(VA_D0_MEGA), pte(0x8000_0000, V | R | W | X | A)),
            // l0 (C.1's second table, and 14–15 for U).
            (l0(0), pte(PG, V | R | W | A | D)),
            (l0(1), pte(FTARGET, V | R | X | A | D)),
            (l0(2), pte(PG, V)),
            (l0(4), pte(PG, V | W | A | D)),
            (l0(5), pte(PG, V | R | W | X)),
            (l0(6), pte(PG, V | R | W | X | A)),
            (l0(7), pte(NOWHERE, V | R | W | A | D)),
            (l0(8), pte(PG, V | R | W | U | A | D)),
            (l0(9), pte(FTARGET, V | R | X | U | A | D)),
            (l0(10), pte(PG, V | X | A | D)),
            (l0(11), pte(PG, V | R | W | G | A | D) | 3 << 8),
            (l0(12), pte(FTARGET, V | R | X)),
            (l0(13), pte(FTARGET, V | R | X | A)),
            (l0(14), pte(NOWHERE, V | R | W | A | D)),
            (l0(15), pte(NOWHERE, V | R | W | U | A | D)),
            // The data and the fetch target.
            (PG, PG_WORD),
            (PG2, PG2_WORD),
            (FTARGET, i(OP_IMM, 0, 13, 0, 0x77)),
            (FTARGET + 4, i(JALR, 0, 0, RA, 0)),
        ]
    }
}

/// Where the two handlers are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Labels {
    s_handler: u32,
    m_handler: u32,
}

/// Code from [`CODE_BASE`]. Every `li` is two words, so a layout never depends on the
/// values loaded.
#[derive(Default)]
struct Asm(Vec<u32>);

impl Asm {
    fn pc(&self) -> u32 {
        CODE_BASE + 4 * self.0.len() as u32
    }

    fn push(&mut self, word: u32) -> &mut Asm {
        self.0.push(word);
        self
    }

    fn li(&mut self, rd: u8, value: u32) -> &mut Asm {
        self.0.extend(li(rd, value));
        self
    }

    fn csr(&mut self, funct3: u32, rd: u8, rs1: u8, csr: u16) -> &mut Asm {
        self.push(csr_word(funct3, rd, rs1, csr))
    }

    fn read(&mut self, rd: u8, csr: u16) -> &mut Asm {
        self.csr(CSRRS, rd, 0, csr)
    }

    fn write(&mut self, csr: u16, rs1: u8) -> &mut Asm {
        self.csr(CSRRW, 0, rs1, csr)
    }

    /// `li t0, bits`, `csrrs x0, sstatus, t0`.
    fn set_sstatus(&mut self, bits: u32) -> &mut Asm {
        self.li(T0, bits).csr(CSRRS, 0, T0, SSTATUS)
    }

    /// `li t0, bits`, `csrrc x0, sstatus, t0`.
    fn clear_sstatus(&mut self, bits: u32) -> &mut Asm {
        self.li(T0, bits).csr(CSRRC, 0, T0, SSTATUS)
    }

    /// `li t1, va`, `lw rd, 0(t1)`.
    fn load(&mut self, rd: u8, va: u32) -> &mut Asm {
        self.li(T1, va).push(i(LOAD, 2, rd, T1, 0))
    }

    /// `li t1, va`, `li a2, value`, `sw a2, 0(t1)`.
    fn store(&mut self, va: u32, value: u32) -> &mut Asm {
        self.li(T1, va).li(A2, value).push(s(2, T1, A2, 0))
    }

    /// `li t1, va`, `jalr ra, 0(t1)`.
    fn fetch(&mut self, va: u32) -> &mut Asm {
        self.li(T1, va).push(i(JALR, 0, RA, T1, 0))
    }

    /// The M setup: `stvec`, `medeleg`, the tables, `satp`, then `MRET` into `mode` at
    /// the word after it (plus `alias` for U).
    fn boot(&mut self, labels: &Labels, medeleg: u32, mstatus: u32, alias: u32) -> &mut Asm {
        self.li(T0, labels.s_handler).write(STVEC, T0);
        self.li(T0, medeleg).write(MEDELEG, T0);
        for (addr, value) in Tables::words() {
            self.li(T0, addr).li(T2, value).push(s(2, T0, T2, 0));
        }
        self.li(T0, SATP_SV32).write(SATP, T0);
        self.li(T0, mstatus).write(MSTATUS, T0);
        let target = self.pc() + 4 * (2 + 1 + 1) + alias;
        self.li(T0, target).write(MEPC, T0).push(MRET)
    }

    /// The exception taken in M that ends the program: `word` at the current `pc` (plus
    /// `alias` for U), raising `cause` with `tval`.
    fn ending(&mut self, alias: u32, word: u32, cause: &'static str, tval: u32) -> ExpectedTrap {
        let pc = self.pc() + alias;
        self.push(word);
        ExpectedTrap {
            pc,
            insn: word,
            cause,
            tval,
        }
    }
}

/// `beq rs1, rs2, offset`.
fn beq(rs1: u8, rs2: u8, offset: i32) -> u32 {
    let imm = offset as u32;
    (imm >> 12 & 1) << 31
        | (imm >> 5 & 0x3f) << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | (imm >> 1 & 0xf) << 8
        | (imm >> 11 & 1) << 7
        | BRANCH
}

/// The program `m3-<name>`: the frame around `body`.
fn build(name: &str, body: impl Fn(&mut Asm, &Labels) -> ExpectedTrap) -> (Program, ExpectedTrap) {
    let assemble = |labels: &Labels| {
        let mut a = Asm::default();
        a.li(T0, labels.m_handler).write(MTVEC, T0);
        let expected = body(&mut a, labels);
        let s_handler = a.pc();
        a.read(T3, SCAUSE)
            .read(T4, SEPC)
            .read(T5, STVAL)
            .read(T6, SSTATUS)
            .push(i(OP_IMM, 0, T0, 0, 12))
            .push(beq(T3, T0, 24))
            .push(i(OP_IMM, 0, T0, 0, 1))
            .push(beq(T3, T0, 16))
            .push(i(OP_IMM, 0, T4, T4, 4))
            .write(SEPC, T4)
            .push(SRET)
            .write(SEPC, RA)
            .push(SRET);
        let m_handler = a.pc();
        a.0.extend(pass_sequence(m_handler));
        let labels = Labels {
            s_handler,
            m_handler,
        };
        (a.0, expected, labels)
    };
    let (_, _, labels) = assemble(&Labels {
        s_handler: 0,
        m_handler: 0,
    });
    let (code, expected, again) = assemble(&labels);
    assert_eq!(again, labels, "the layout does not depend on the labels");
    let program = Program {
        name: format!("m3-{name}"),
        elf: elf(&code, &[0; WINDOW_SIZE as usize]),
        code,
    };
    (program, expected)
}

const FROM_S: &str = "EnvironmentCallFromS";
const FROM_U: &str = "EnvironmentCallFromU";

/// C.2 Q6–Q9, Q20: a 4 KiB page through both levels for load, store, and fetch; `G`
/// and RSW ignored; `satp` read back.
fn vm_4k() -> (Program, ExpectedTrap) {
    build("vm-4k", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        a.read(A0, SATP);
        a.load(A0, l0va(0));
        a.store(l0va(0) + 4, 0x5a5a_5a5a).load(A1, l0va(0) + 4);
        a.fetch(l0va(1));
        a.load(A0, l0va(11));
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// A megapage: loads and a store, the last word of the 4 MiB, the unmapped next one, and
/// a misaligned megapage for load, store, and fetch (C.2).
fn vm_megapage() -> (Program, ExpectedTrap) {
    build("vm-megapage", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        a.load(A0, VA_MEGA + (PG - 0x8000_0000));
        a.store(VA_MEGA + (PG - 0x8000_0000) + 8, 0x0bad_cafe)
            .load(A1, PG + 8);
        a.store(VA_MEGA + 0x3F_FFFC, 0x600d_f00d)
            .load(A1, 0x803F_FFFC);
        a.load(A0, VA_MEGA + 0x40_0000);
        a.load(A0, VA_MISALIGNED_MEGA + 0x20_0000);
        a.store(VA_MISALIGNED_MEGA + 0x20_0000, 1);
        a.fetch(VA_MISALIGNED_MEGA + (FTARGET - 0x8000_0000));
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// Invalid PTEs (C.2 Q10–Q12, Q23, Q24): a pointer at level 0, `V` = 0 at either level,
/// `R` = 0 with `W` = 1 at either level, and a pointer with `U`, `A`, `D` set.
fn vm_invalid() -> (Program, ExpectedTrap) {
    build("vm-invalid", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        for va in [
            l0va(2),
            l0va(3),
            l0va(4),
            VA_RESERVED_POINTER,
            VA_R0W1_L1,
            VA_INVALID_L1,
        ] {
            a.load(A0, va);
        }
        a.store(l0va(4), 1).store(l0va(3), 1);
        a.fetch(l0va(3)).fetch(VA_INVALID_L1);
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// S permissions: X-only pages with and without `MXR`, U pages with and without `SUM`,
/// and fetches from a U page, which fault either way.
fn vm_perm_s() -> (Program, ExpectedTrap) {
    build("vm-perm-s", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        let xonly = VA_XONLY_MEGA + (PG - 0x8000_0000);
        a.load(A0, xonly).load(A0, l0va(10));
        a.store(xonly, 1);
        a.fetch(VA_XONLY_MEGA + (FTARGET - 0x8000_0000));
        a.set_sstatus(MXR);
        a.load(A0, xonly).load(A1, l0va(10));
        a.store(l0va(10), 1);
        a.clear_sstatus(MXR);
        a.load(A0, l0va(8)).store(l0va(8), 2).fetch(l0va(9));
        a.set_sstatus(SUM);
        a.load(A0, l0va(8))
            .store(l0va(8) + 4, 3)
            .load(A1, l0va(8) + 4);
        a.fetch(l0va(9));
        a.clear_sstatus(SUM);
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// Svade: `A` = 0 faults every access, `D` = 0 faults a store only, at both levels; then
/// S reads the PTEs back to show the CPU wrote none.
fn vm_svade() -> (Program, ExpectedTrap) {
    build("vm-svade", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        a.load(A0, l0va(5)).store(l0va(5), 1);
        a.load(A0, l0va(6)).store(l0va(6), 1);
        a.fetch(l0va(12)).fetch(l0va(13));
        a.load(A0, VA_A0_MEGA + 0x20_0000);
        a.load(A0, VA_D0_MEGA + 0x20_0000);
        a.store(VA_D0_MEGA + 0x20_0000, 1);
        for n in [5, 6, 12, 13] {
            a.load(A1, L0 + n * 4);
        }
        a.load(A1, ROOT + (VA_A0_MEGA >> 22) * 4)
            .load(A1, ROOT + (VA_D0_MEGA >> 22) * 4);
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// Fault priority and access faults: a PTE read the bus refuses is the access's access
/// fault with the VA, the final access to no memory is too, and a misaligned access
/// traps before any walk (C.2 Q1–Q5, Q17–Q19).
fn vm_faults() -> (Program, ExpectedTrap) {
    build("vm-faults", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        a.load(A0, VA_PTE_FAULT)
            .store(VA_PTE_FAULT, 1)
            .fetch(VA_PTE_FAULT);
        a.load(A0, l0va(7)).store(l0va(7), 1).fetch(l0va(7));
        a.load(A0, VA_PTE_FAULT + 1)
            .store(l0va(7) + 2, 1)
            .load(A0, VA_INVALID_L1 + 2);
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// U through Sv32 (C.3): U pages load, store, and fetch; S pages fault, a megapage too; a
/// denied leaf to no memory is a page fault, an allowed one an access fault. It ends with
/// an `ECALL` from U, which goes to M.
fn vm_user() -> (Program, ExpectedTrap) {
    build("vm-user", |a, labels| {
        a.boot(labels, DELEGATE, MPP_U, USER_ALIAS);
        a.load(A0, l0va(8))
            .store(l0va(8) + 8, 4)
            .load(A1, l0va(8) + 8);
        a.fetch(l0va(9));
        a.load(A0, l0va(0)).store(l0va(0), 5).fetch(l0va(1));
        a.load(A0, l0va(14)).load(A0, l0va(15));
        a.load(A0, VA_MEGA + 0x20_0000);
        a.ending(USER_ALIAS, ECALL, FROM_U, 0)
    })
}

/// SFENCE.VMA with Sv32 on: S rewrites `l0[0]` to the second data page, fences, and
/// loads the new page's word.
fn vm_sfence() -> (Program, ExpectedTrap) {
    build("vm-sfence", |a, labels| {
        a.boot(labels, DELEGATE, MPP_S, 0);
        a.load(A0, l0va(0));
        a.store(L0, pte(PG2, V | R | W | A | D));
        a.push(SFENCE_VMA);
        a.load(A1, l0va(0));
        a.push(SFENCE_VMA);
        a.ending(0, ECALL, FROM_S, 0)
    })
}

/// A page fault whose `medeleg` bit is clear is taken in M, and ends the program.
fn vm_undelegated(name: &str, cause: &'static str, bit: u32) -> (Program, ExpectedTrap) {
    build(name, |a, labels| {
        a.boot(labels, DELEGATE & !(1 << bit), MPP_S, 0);
        a.load(A0, l0va(0));
        let va = l0va(3);
        a.li(T1, va);
        let word = match bit {
            12 => i(JALR, 0, RA, T1, 0),
            13 => i(LOAD, 2, A0, T1, 0),
            _ => s(2, T1, A0, 0),
        };
        let mut expected = a.ending(0, word, cause, va);
        if bit == 12 {
            expected.pc = va;
            expected.insn = 0;
        }
        expected
    })
}

/// Every directed M3.3 program, with the exception taken in M that ends it.
pub fn programs() -> Vec<(Program, ExpectedTrap)> {
    vec![
        vm_4k(),
        vm_megapage(),
        vm_invalid(),
        vm_perm_s(),
        vm_svade(),
        vm_faults(),
        vm_user(),
        vm_sfence(),
        vm_undelegated("vm-undelegated-fetch", "InstructionPageFault", 12),
        vm_undelegated("vm-undelegated-load", "LoadPageFault", 13),
        vm_undelegated("vm-undelegated-store", "StorePageFault", 15),
    ]
}

#[cfg(test)]
mod tests {
    use systemscope_contracts::trace::Value;
    use systemscope_rv32i::Rv32iProfile;

    use super::*;
    use crate::manifest::load;
    use crate::runner::{self, CPU, End, Finished, Start};

    fn run(program: &Program) -> Finished {
        let image = load(&program.name, &program.elf).unwrap();
        runner::execute(
            runner::platform_with_profile(&image, false, runner::SEED, Rv32iProfile::M3),
            Start::Init { traced: false },
            Vec::new(),
        )
    }

    fn reg(finished: &Finished, name: &str) -> u32 {
        match finished.views[CPU.0 as usize].get(name) {
            Some(Value::U64(v)) => *v as u32,
            other => panic!("{name}: {other:?}"),
        }
    }

    fn find(name: &str) -> Program {
        programs()
            .into_iter()
            .find(|(p, _)| p.name == name)
            .unwrap()
            .0
    }

    /// Every program ends on SystemScope exactly with its expected exception taken in M.
    #[test]
    fn every_program_ends_with_its_exception_taken_in_m() {
        let programs = programs();
        assert_eq!(programs.len(), 11);
        for (p, expected) in &programs {
            let finished = run(p);
            assert_eq!(
                finished.outcome.end,
                End::Trap {
                    cause: expected.cause.to_owned(),
                    pc: expected.pc,
                    tval: expected.tval,
                },
                "{}",
                p.name
            );
        }
    }

    /// The last value each program leaves, as the appendix measured it.
    #[test]
    fn the_programs_end_with_the_measured_values() {
        // vm-4k: the last load reads pg through the G/RSW PTE.
        let f = run(&find("m3-vm-4k"));
        assert_eq!((reg(&f, "x10"), reg(&f, "x11")), (PG_WORD, 0x5a5a_5a5a));
        assert_eq!(reg(&f, "x13"), 0x77, "the fetch target ran");
        // vm-svade: the PTEs read back unchanged.
        let f = run(&find("m3-vm-svade"));
        assert_eq!(reg(&f, "x11"), pte(0x8000_0000, V | R | W | X | A));
        // vm-sfence: the load after the fence reads the new page.
        let f = run(&find("m3-vm-sfence"));
        assert_eq!((reg(&f, "x10"), reg(&f, "x11")), (PG_WORD, PG2_WORD));
        // vm-user: the last load was an S page from U: a page fault, delegated.
        let f = run(&find("m3-vm-user"));
        assert_eq!((reg(&f, "x28"), reg(&f, "x30")), (13, VA_MEGA + 0x20_0000));
        // vm-faults: the last delegated trap is the misaligned load, before the walk.
        let f = run(&find("m3-vm-faults"));
        assert_eq!((reg(&f, "x28"), reg(&f, "x30")), (4, VA_INVALID_L1 + 2));
    }

    /// The programs never write `satp` with an `ASID` or read `mip`, and every `medeleg`
    /// stays inside `0xB1FF` (m3-2-spike-appendix B.6).
    #[test]
    fn no_program_reaches_a_recorded_divergence() {
        use systemscope_rv32i::PrivInstr;
        use systemscope_rv32i::csr::decode_privileged;
        use systemscope_rv32i::privilege::{MIDELEG, SIE, SIP};
        assert_eq!(SATP_SV32 & 0x7fc0_0000, 0);
        assert_eq!(DELEGATE & !0xB1FF, 0);
        for (p, _) in programs() {
            for &word in &p.code {
                if let Some(instr @ PrivInstr::Csr { csr, .. }) = decode_privileged(word) {
                    assert_ne!(csr, 0x344, "{}: mip", p.name);
                    if instr.writes() {
                        assert!(![MIDELEG, SIE, SIP].contains(&csr), "{}", p.name);
                    }
                }
            }
        }
    }
}
