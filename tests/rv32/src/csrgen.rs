//! The directed M2 programs for the Spike differential: Zicsr, the eight whitelisted CSRs,
//! and `MRET` (`docs/m2-design.md` §4, §5.3), run with the M2 CPU profile.
//!
//! Like [`progen`](crate::progen), each program is machine code and a minimal ELF written
//! directly, with no assembler. They are a fixed list, apart from the generated programs,
//! whose digest they leave alone.
//!
//! - **Pass programs** ([`pass_programs`]) exercise every Zicsr form, write suppression,
//!   `uimm` 0, 1, and 31, each CSR's write rule, and the three `MRET` cases. Every
//!   value written is one on which the pinned Spike's CSR agrees with M2's: `mie` only
//!   `MEIE` or all-but-the-standard-bits, `mtvec` `MODE` 0 or 2. Each ends with the §10.2
//!   pass sequence, so Spike exits on `write_tohost` and no instruction limit is needed.
//! - **Trap programs** ([`illegal_programs`]) end in an access to a CSR outside the
//!   whitelist that the pinned Spike rejects too (m2-design §4.5): each first points
//!   `mtvec` at a pass sequence, so Spike's handler exits through `write_tohost` rather
//!   than at the instruction limit. SystemScope halts on the trap; the differential
//!   compares both sides up to it.
//!
//! `MRET`'s own `mstatus` update is not in SystemScope's trace (§6.5), so every `MRET`
//! program reads `mstatus` right after it: the register write carries the comparison.

use systemscope_rv32i::csr::{MCAUSE, MEPC, MIE, MIP, MRET, MSCRATCH, MSTATUS, MTVAL, MTVEC};

use crate::progen::{
    CODE_BASE, ExpectedTrap, OP_IMM, Program, SYSTEM, WINDOW_SIZE, elf, i, li, pass_sequence,
};

const T0: u8 = 5;
const T1: u8 = 6;
const T2: u8 = 7;
const A0: u8 = 10;
const A1: u8 = 11;
const A2: u8 = 12;
const A3: u8 = 13;
const A4: u8 = 14;
const A5: u8 = 15;
const A6: u8 = 16;
const A7: u8 = 17;
const S1: u8 = 9;

// Zicsr `funct3`.
const CSRRW: u32 = 1;
const CSRRS: u32 = 2;
const CSRRC: u32 = 3;
const CSRRWI: u32 = 5;
const CSRRSI: u32 = 6;
const CSRRCI: u32 = 7;

/// A Zicsr instruction: `rs1` is the register, or the `uimm` of an immediate form.
pub fn csr_word(funct3: u32, rd: u8, rs1: u8, csr: u16) -> u32 {
    i(SYSTEM, funct3, rd, rs1, i32::from(csr))
}

/// Code from [`CODE_BASE`].
#[derive(Default)]
struct Asm(Vec<u32>);

impl Asm {
    fn pc(&self) -> u32 {
        CODE_BASE + 4 * self.0.len() as u32
    }

    fn li(&mut self, rd: u8, value: u32) -> &mut Asm {
        self.0.extend(li(rd, value));
        self
    }

    fn csr(&mut self, funct3: u32, rd: u8, rs1: u8, csr: u16) -> &mut Asm {
        self.0.push(csr_word(funct3, rd, rs1, csr));
        self
    }

    /// `csrr rd, csr`: `CSRRS` with `rs1 = x0`, a read with the write suppressed.
    fn read(&mut self, rd: u8, csr: u16) -> &mut Asm {
        self.csr(CSRRS, rd, 0, csr)
    }

    /// `csrw csr, rs1`.
    fn write(&mut self, csr: u16, rs1: u8) -> &mut Asm {
        self.csr(CSRRW, 0, rs1, csr)
    }

    /// `addi rd, x0, imm`: a word that must never run where it is skipped.
    fn addi(&mut self, rd: u8, imm: i32) -> &mut Asm {
        self.0.push(i(OP_IMM, 0, rd, 0, imm));
        self
    }

    /// The program, ending with the pass sequence.
    fn pass(mut self, name: &str) -> Program {
        let at = self.pc();
        self.0.extend(pass_sequence(at));
        program(name, self.0)
    }
}

fn program(name: &str, code: Vec<u32>) -> Program {
    Program {
        name: format!("m2-{name}"),
        elf: elf(&code, &[0; WINDOW_SIZE as usize]),
        code,
    }
}

/// Every form on `mscratch`: reads, writes, suppression, `uimm` 0, 1, and 31, a write of
/// an unchanged value, and `rd = rs1`, whose operand is read before `rd` is written.
fn forms() -> Program {
    let mut a = Asm::default();
    a.li(T0, 0x1234_5678)
        .write(MSCRATCH, T0)
        .csr(CSRRW, A0, 0, MSCRATCH)
        .li(T1, 0xf0f0_f0f0)
        .csr(CSRRS, A1, T1, MSCRATCH)
        .csr(CSRRS, A2, 0, MSCRATCH)
        .li(T1, 0x00ff_00ff)
        .csr(CSRRC, A3, T1, MSCRATCH)
        .csr(CSRRC, A4, 0, MSCRATCH)
        .csr(CSRRWI, A5, 31, MSCRATCH)
        .csr(CSRRSI, A6, 0, MSCRATCH)
        .csr(CSRRSI, A6, 1, MSCRATCH)
        .csr(CSRRSI, A6, 31, MSCRATCH)
        .csr(CSRRCI, A7, 0, MSCRATCH)
        .csr(CSRRCI, A7, 1, MSCRATCH)
        .csr(CSRRCI, A7, 31, MSCRATCH)
        .csr(CSRRWI, A0, 0, MSCRATCH)
        .csr(CSRRWI, A0, 1, MSCRATCH)
        .csr(CSRRSI, 0, 30, MSCRATCH)
        .csr(CSRRCI, 0, 2, MSCRATCH)
        .li(T2, 0xabcd)
        .csr(CSRRW, T2, T2, MSCRATCH)
        .li(T2, 0x0f00)
        .csr(CSRRS, T2, T2, MSCRATCH)
        .csr(CSRRC, T2, T2, MSCRATCH)
        .read(A1, MSCRATCH);
    a.pass("csr-forms")
}

/// `mstatus`: MIE and MPIE by every form, `MPP` always `0b11`.
fn mstatus() -> Program {
    let mut a = Asm::default();
    a.read(A0, MSTATUS)
        .li(T0, 0x1888)
        .write(MSTATUS, T0)
        .read(A0, MSTATUS)
        .write(MSTATUS, 0)
        .read(A0, MSTATUS)
        .csr(CSRRSI, A1, 8, MSTATUS)
        .li(T0, 0x80)
        .csr(CSRRS, A1, T0, MSTATUS)
        .csr(CSRRCI, A2, 8, MSTATUS)
        .csr(CSRRC, A2, T0, MSTATUS)
        .li(T0, 0x88)
        .csr(CSRRW, A3, T0, MSTATUS)
        .csr(CSRRSI, A4, 0, MSTATUS)
        .csr(CSRRCI, A4, 0, MSTATUS)
        .csr(CSRRWI, A4, 0, MSTATUS)
        .read(A5, MSTATUS);
    a.pass("csr-mstatus")
}

/// `mie`: only `MEIE` is kept.
fn mie() -> Program {
    let mut a = Asm::default();
    a.read(A0, MIE)
        .li(T0, 0x800)
        .write(MIE, T0)
        .read(A0, MIE)
        .write(MIE, 0)
        .li(T1, 0xffff_f777)
        .csr(CSRRW, A1, T1, MIE)
        .read(A1, MIE)
        .csr(CSRRC, A2, T0, MIE)
        .csr(CSRRS, A3, T0, MIE)
        .csr(CSRRSI, A4, 0, MIE)
        .csr(CSRRCI, A4, 0, MIE)
        .csr(CSRRWI, A5, 0, MIE)
        .read(A5, MIE);
    a.pass("csr-mie")
}

/// `mip`: read-only here, every write ignored.
fn mip() -> Program {
    let mut a = Asm::default();
    a.read(A0, MIP)
        .li(T0, 0xffff_ffff)
        .csr(CSRRW, A1, T0, MIP)
        .li(T0, 0x800)
        .csr(CSRRS, A2, T0, MIP)
        .csr(CSRRC, A3, T0, MIP)
        .csr(CSRRWI, A4, 31, MIP)
        .csr(CSRRSI, A4, 31, MIP)
        .csr(CSRRCI, A4, 31, MIP)
        .read(A5, MIP);
    a.pass("csr-mip")
}

/// `mtvec`: `MODE` 0 and 2 (bits `[1:0]` dropped).
fn mtvec() -> Program {
    let mut a = Asm::default();
    for value in [0x8000_1000, 0x8000_1002, 0xffff_fffe, 0x4, 0] {
        a.li(T0, value).write(MTVEC, T0).read(A0, MTVEC);
    }
    a.li(T0, 0x8000_2000)
        .csr(CSRRW, A1, T0, MTVEC)
        .li(T1, 0x102)
        .csr(CSRRS, A2, T1, MTVEC)
        .csr(CSRRC, A3, T1, MTVEC)
        .csr(CSRRSI, A4, 30, MTVEC)
        .csr(CSRRCI, A4, 12, MTVEC)
        .csr(CSRRWI, A5, 2, MTVEC)
        .read(A5, MTVEC);
    a.pass("csr-mtvec")
}

/// `mepc`: bits `[1:0]` dropped.
fn mepc() -> Program {
    let mut a = Asm::default();
    for value in [0xffff_ffff, 0x8000_0002, 0x8000_0001, 0x1234_5678, 0] {
        a.li(T0, value).write(MEPC, T0).read(A0, MEPC);
    }
    a.csr(CSRRSI, A1, 31, MEPC)
        .csr(CSRRCI, A2, 5, MEPC)
        .csr(CSRRWI, A3, 3, MEPC)
        .read(A3, MEPC);
    a.pass("csr-mepc")
}

/// `mscratch`, `mcause`, and `mtval`: all 32 bits.
fn full_width() -> Program {
    let mut a = Asm::default();
    for csr in [MSCRATCH, MCAUSE, MTVAL] {
        for value in [0xffff_ffff, 0x8000_000b, 0x1234_5678, 0] {
            a.li(T0, value).write(csr, T0).read(A0, csr);
        }
        a.csr(CSRRSI, A1, 31, csr)
            .li(T1, 0x8000_0000)
            .csr(CSRRS, A2, T1, csr)
            .csr(CSRRC, A3, T1, csr)
            .csr(CSRRCI, A4, 1, csr)
            .read(A4, csr);
    }
    a.pass("csr-full-width")
}

/// `MRET` with `mstatus` = `before`: `pc` becomes `mepc`, written `offset` bytes past the
/// target to show its dropped bits; the skipped words would write `x9`. At the target,
/// `mstatus` and `mepc` are read.
fn mret(name: &str, before: u32, offset: u32) -> Program {
    let mut a = Asm::default();
    a.li(T0, before).write(MSTATUS, T0);
    // li, csrw, mret, then three skipped words.
    let target = a.pc() + 4 * (2 + 1 + 1 + 3);
    a.li(T1, target + offset).write(MEPC, T1);
    a.0.push(MRET);
    a.addi(S1, 0x111).addi(S1, 0x222).addi(S1, 0x333);
    debug_assert_eq!(a.pc(), target);
    a.read(A2, MSTATUS).read(A3, MEPC);
    a.pass(name)
}

/// The directed pass programs, run to `write_tohost` on both sides.
pub fn pass_programs() -> Vec<Program> {
    vec![
        forms(),
        mstatus(),
        mie(),
        mip(),
        mtvec(),
        mepc(),
        full_width(),
        mret("mret-mpie", 0x1880, 0),
        mret("mret-mie", 0x1808, 0),
        mret("mret-both", 0x1888, 0),
        mret("mret-pc", 0x1800, 2),
    ]
}

/// The CSRs outside the whitelist that both SystemScope and the pinned Spike reject, with
/// the instruction that accesses each: reads have their write suppressed.
pub const ILLEGAL: [(&str, u32); 9] = [
    ("medeleg", csr_const(CSRRS, A1, 0, 0x302)),
    ("mideleg", csr_const(CSRRS, A1, 0, 0x303)),
    ("mcounteren", csr_const(CSRRW, A1, T0, 0x306)),
    ("satp", csr_const(CSRRS, A1, 0, 0x180)),
    ("cycle", csr_const(CSRRS, A1, 0, 0xc00)),
    ("time", csr_const(CSRRS, A1, 0, 0xc01)),
    ("instret", csr_const(CSRRS, A1, 0, 0xc02)),
    ("custom-7c0", csr_const(CSRRS, A1, 0, 0x7c0)),
    ("mhartid-write", csr_const(CSRRW, A1, T0, 0xf14)),
];

/// [`csr_word`], in a constant.
const fn csr_const(funct3: u32, rd: u8, rs1: u8, csr: u16) -> u32 {
    (csr as u32) << 20 | (rs1 as u32) << 15 | funct3 << 12 | (rd as u32) << 7 | SYSTEM
}

/// The trap program for `word`: `mtvec` = a pass sequence, then `word`, which must trap
/// as an illegal instruction with `tval` = the word.
pub fn illegal(name: &str, word: u32) -> (Program, ExpectedTrap) {
    let mut a = Asm::default();
    // li, csrw, li, the word, then the handler.
    let handler = a.pc() + 4 * (2 + 1 + 2 + 1);
    a.li(T0, handler).write(MTVEC, T0).li(T0, 0x5a5);
    let pc = a.pc();
    a.0.push(word);
    debug_assert_eq!(a.pc(), handler);
    let expected = ExpectedTrap {
        pc,
        insn: word,
        cause: "IllegalInstruction",
        tval: word,
    };
    (a.pass(&format!("illegal-{name}")), expected)
}

/// Every [`ILLEGAL`] trap program.
pub fn illegal_programs() -> Vec<(Program, ExpectedTrap)> {
    ILLEGAL
        .iter()
        .map(|&(name, word)| illegal(name, word))
        .collect()
}

#[cfg(test)]
mod tests {
    use systemscope_contracts::trace::Value;
    use systemscope_rv32i::csr::is_supported;
    use systemscope_rv32i::{PrivInstr, Rv32iProfile, decode_privileged};

    use super::*;
    use crate::manifest::load;
    use crate::runner::{self, CPU, End, Finished, Start};

    fn run(program: &Program, profile: Rv32iProfile) -> Finished {
        let image = load(&program.name, &program.elf).unwrap();
        runner::execute(
            runner::platform_with_profile(&image, false, runner::SEED, profile),
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

    #[test]
    fn the_encoder_is_the_constant_encoder() {
        for (funct3, rd, rs1, csr) in [(CSRRW, A1, T0, 0xf14), (CSRRCI, 0, 31, 0xfff)] {
            assert_eq!(
                csr_word(funct3, rd, rs1, csr),
                csr_const(funct3, rd, rs1, csr)
            );
        }
        assert_eq!(csr_word(CSRRS, A0, 0, MSTATUS), 0x3000_2573); // csrr a0, mstatus
        assert_eq!(csr_word(CSRRW, 0, T0, MTVEC), 0x3052_9073); // csrw mtvec, t0
        assert_eq!(csr_word(CSRRWI, A2, 31, MSCRATCH), 0x340f_d673);
    }

    /// Every pass program ends at the pass rule with the M2 profile, and stops at its
    /// first CSR instruction, as an illegal one, with M1's.
    #[test]
    fn every_pass_program_passes_with_m2_and_traps_with_m1() {
        let programs = pass_programs();
        assert_eq!(programs.len(), 11);
        for p in &programs {
            let finished = run(p, Rv32iProfile::M2);
            runner::judge(&finished.outcome).unwrap_or_else(|e| panic!("{}: {e}", p.name));
            let ecall = CODE_BASE + 4 * (p.code.len() as u32 - 1);
            assert!(
                matches!(finished.outcome.end, End::Trap { pc, .. } if pc == ecall),
                "{}",
                p.name
            );
            let first = p
                .code
                .iter()
                .position(|w| decode_privileged(*w).is_some())
                .unwrap();
            let m1 = run(p, Rv32iProfile::M1);
            assert_eq!(
                m1.outcome.end,
                End::Trap {
                    cause: "IllegalInstruction".to_owned(),
                    pc: CODE_BASE + 4 * first as u32,
                    tval: p.code[first],
                },
                "{}",
                p.name
            );
        }
    }

    /// The pass programs use only whitelisted CSRs, and together every form, with and
    /// without suppression, `uimm` 0, 1, and 31, and `MRET`.
    #[test]
    fn the_pass_programs_cover_every_form() {
        let mut seen = std::collections::BTreeSet::new();
        for p in pass_programs() {
            for &word in &p.code {
                match decode_privileged(word) {
                    Some(instr @ PrivInstr::Csr { csr, .. }) => {
                        assert!(is_supported(csr), "{}: {word:#010x}", p.name);
                        let funct3 = word >> 12 & 7;
                        let field = word >> 15 & 0x1f;
                        seen.insert((
                            funct3,
                            instr.writes(),
                            field.min(2) + u32::from(field == 31),
                        ));
                    }
                    Some(PrivInstr::Mret) => {
                        seen.insert((0, true, 0));
                    }
                    None => {}
                }
            }
        }
        assert!(seen.contains(&(0, true, 0)));
        for funct3 in [CSRRW, CSRRS, CSRRC, CSRRWI, CSRRSI, CSRRCI] {
            assert!(seen.iter().any(|s| s.0 == funct3 && s.1), "{funct3}");
            if funct3 != CSRRW && funct3 != CSRRWI {
                assert!(seen.contains(&(funct3, false, 0)), "{funct3} suppressed");
            }
        }
        for funct3 in [CSRRWI, CSRRSI, CSRRCI] {
            for uimm in [0, 1, 3] {
                // 3 stands for 31.
                assert!(
                    seen.iter().any(|s| s.0 == funct3 && s.2 == uimm),
                    "{funct3} {uimm}"
                );
            }
        }
    }

    /// Each `MRET` program lands on its target with the §5.3 `mstatus`, and skips the
    /// words in between.
    #[test]
    fn every_mret_program_lands_on_its_target() {
        for (name, after) in [
            ("m2-mret-mpie", 0x1888),
            ("m2-mret-mie", 0x1880),
            ("m2-mret-both", 0x1888),
            ("m2-mret-pc", 0x1880),
        ] {
            let p = pass_programs()
                .into_iter()
                .find(|p| p.name == name)
                .unwrap();
            let finished = run(&p, Rv32iProfile::M2);
            assert_eq!(reg(&finished, "x12"), after, "{name}");
            assert_eq!(reg(&finished, "x9"), 0, "{name}");
            assert_eq!(reg(&finished, "x13") % 4, 0, "{name}");
        }
    }

    #[test]
    fn every_illegal_program_traps_where_expected() {
        let programs = illegal_programs();
        assert_eq!(programs.len(), 9);
        for (p, expected) in &programs {
            let PrivInstr::Csr { csr, .. } = decode_privileged(expected.insn).unwrap() else {
                panic!("{}", p.name);
            };
            assert!(!is_supported(csr), "{}", p.name);
            let finished = run(p, Rv32iProfile::M2);
            assert_eq!(
                finished.outcome.end,
                End::Trap {
                    cause: expected.cause.to_owned(),
                    pc: expected.pc,
                    tval: expected.tval
                },
                "{}",
                p.name
            );
            assert_eq!(finished.outcome.instret, 5, "{}", p.name);
        }
    }
}
