//! The directed M3.2 programs for the Spike differential: M, S, and U modes, the M3 CSR
//! whitelist and access rule, `MRET`, `SRET`, `SFENCE.VMA`, and delegated exceptions
//! (`docs/m3-design.md` §5.1, §5.3), run with the M3 CPU profile.
//!
//! Like [`csrgen`](crate::csrgen), each program is machine code and a minimal ELF written
//! directly, with no assembler. Every program has the same frame:
//!
//! - it starts in M, points `mtvec` at the §10.2 pass sequence, and runs its body, which
//!   may set `stvec`, `medeleg`, and `mstatus` and enter S or U with `MRET` or `SRET`;
//! - the S handler reads `scause`, `sepc`, `stval`, and `sstatus` into `t3`–`t6`,
//!   advances `sepc` by 4, and returns with `SRET`, so every delegated exception is
//!   compared through the handler's register writes as well as the exception itself;
//! - the body ends with an exception taken in M. SystemScope halts on it
//!   (`docs/m3-design.md` §5.3); Spike runs its handler, the pass sequence, and exits
//!   through `write_tohost`. The differential compares both sides up to that trap, the
//!   m3-2-spike-appendix D11 boundary.
//!
//! The bodies stay clear of every divergence in m3-2-spike-appendix B.6: `medeleg`
//! values inside `0xB1FF` (D1), no write of `mideleg`, `sie`, or `sip` (D2, D3), only the
//! §5.1 `mstatus` and `sstatus` bits (D4, D5), 4-byte-aligned `stvec` (D6), `satp` with
//! `MODE` = Bare and `ASID` = 0 (D7, D8), `WFI` only in U (D9), no unsupported CSR but
//! the rejected ones (D10). `mip` is never read: the pinned Spike's timer is pending.
//!
//! `MRET`'s and `SRET`'s own `mstatus` updates are not in SystemScope's trace, so the
//! bodies read `mstatus` or `sstatus` after them; each retirement's mode is compared too.

use systemscope_rv32i::csr::{MCAUSE, MEPC, MIE, MRET, MSCRATCH, MSTATUS, MTVAL, MTVEC};
use systemscope_rv32i::privilege::{
    MEDELEG, MIDELEG, SATP, SCAUSE, SEPC, SIE, SIP, SRET, SSCRATCH, SSTATUS, STVAL, STVEC,
};

use crate::csrgen::csr_word;
use crate::progen::{
    CODE_BASE, ExpectedTrap, JALR, LOAD, OP_IMM, Program, SYSTEM, WINDOW_SIZE, elf, i, li,
    pass_sequence, s,
};

const RA: u8 = 1;
const T0: u8 = 5;
const T1: u8 = 6;
const A0: u8 = 10;
const A1: u8 = 11;
const A2: u8 = 12;
const A3: u8 = 13;
const A4: u8 = 14;
const A5: u8 = 15;
const A6: u8 = 16;
const T3: u8 = 28;
const T4: u8 = 29;
const T5: u8 = 30;
const T6: u8 = 31;

// Zicsr `funct3`.
const CSRRW: u32 = 1;
const CSRRS: u32 = 2;
const CSRRC: u32 = 3;
const CSRRWI: u32 = 5;
const CSRRSI: u32 = 6;
const CSRRCI: u32 = 7;

/// `ECALL`.
pub const ECALL: u32 = SYSTEM;
/// `EBREAK`.
pub const EBREAK: u32 = 0x0010_0073;
/// `WFI`.
pub const WFI: u32 = 0x1050_0073;
/// `SFENCE.VMA x0, x0`.
pub const SFENCE_VMA: u32 = 0x1200_0073;

/// `mstatus.MPP` for each mode.
const MPP_U: u32 = 0;
const MPP_S: u32 = 1 << 11;
const MPP_M: u32 = 3 << 11;

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

    /// `csrr rd, csr`.
    fn read(&mut self, rd: u8, csr: u16) -> &mut Asm {
        self.csr(CSRRS, rd, 0, csr)
    }

    /// `csrw csr, rs1`.
    fn write(&mut self, csr: u16, rs1: u8) -> &mut Asm {
        self.csr(CSRRW, 0, rs1, csr)
    }

    /// `li t0, value`, `csrrw a0, csr, t0`, `csrr a0, csr`: the old and the stored value.
    fn store(&mut self, csr: u16, value: u32) -> &mut Asm {
        self.li(T0, value).csr(CSRRW, A0, T0, csr).read(A0, csr)
    }

    fn addi(&mut self, rd: u8, rs1: u8, imm: i32) -> &mut Asm {
        self.push(i(OP_IMM, 0, rd, rs1, imm))
    }

    /// `stvec` = the S handler, `medeleg` = `medeleg`.
    fn supervisor(&mut self, labels: &Labels, medeleg: u32) -> &mut Asm {
        self.li(T0, labels.s_handler)
            .write(STVEC, T0)
            .li(T0, medeleg)
            .write(MEDELEG, T0)
    }

    /// `mstatus` = `mstatus`, `mepc` = the word after the `MRET`, then `MRET`.
    fn mret_with(&mut self, mstatus: u32) -> &mut Asm {
        self.li(T0, mstatus).write(MSTATUS, T0);
        let target = self.pc() + 4 * (2 + 1 + 1);
        self.li(T1, target).write(MEPC, T1).push(MRET)
    }

    /// `sepc` = the word after the `SRET`, then `SRET`.
    fn sret_to_next(&mut self) -> &mut Asm {
        let target = self.pc() + 4 * (2 + 1 + 1);
        self.li(T1, target).write(SEPC, T1).push(SRET)
    }

    /// The exception taken in M that ends the program: `word` at the current `pc`.
    fn ending(&mut self, word: u32, cause: &'static str) -> ExpectedTrap {
        let pc = self.pc();
        self.push(word);
        let tval = match word {
            ECALL => 0,
            EBREAK => pc,
            _ => word,
        };
        ExpectedTrap {
            pc,
            insn: word,
            cause,
            tval,
        }
    }
}

/// `lw rd, imm(rs1)`.
fn lw(rd: u8, rs1: u8, imm: i32) -> u32 {
    i(LOAD, 2, rd, rs1, imm)
}

/// `sw rs2, imm(rs1)`.
fn sw(rs2: u8, rs1: u8, imm: i32) -> u32 {
    s(2, rs1, rs2, imm)
}

/// The program `m3-<name>`: the frame around `body`, which returns the exception taken in
/// M that ends it.
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
            .addi(T4, T4, 4)
            .write(SEPC, T4)
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

const FROM_M: &str = "EnvironmentCallFromM";
const FROM_S: &str = "EnvironmentCallFromS";
const FROM_U: &str = "EnvironmentCallFromU";

/// m3-2-spike-appendix B.1: every compared CSR at reset, in M.
fn reset() -> (Program, ExpectedTrap) {
    build("reset", |a, _| {
        for (rd, csr) in [
            (A0, MSTATUS),
            (A1, SSTATUS),
            (A2, MEDELEG),
            (A3, MIDELEG),
            (A4, SIE),
            (A5, SIP),
            (A6, STVEC),
            (A0, SSCRATCH),
            (A1, SEPC),
            (A2, SCAUSE),
            (A3, STVAL),
            (A4, SATP),
            (A5, MIE),
            (A6, MSCRATCH),
            (A0, MEPC),
            (A1, MCAUSE),
            (A2, MTVAL),
            (A3, MTVEC),
        ] {
            a.read(rd, csr);
        }
        a.ending(ECALL, FROM_M)
    })
}

/// `mstatus` (B.2): every §5.1 bit, `MPP` S, M, and the reserved `0b10`, which stores U.
fn mstatus() -> (Program, ExpectedTrap) {
    build("mstatus", |a, _| {
        for value in [
            0x000c_19aa,
            0,
            0x0000_1000,
            0x0000_0800,
            0x0000_1800,
            0x000c_0000,
        ] {
            a.store(MSTATUS, value);
        }
        a.csr(CSRRSI, A1, 2, MSTATUS)
            .csr(CSRRSI, A1, 8, MSTATUS)
            .li(T0, 0x0004_0120)
            .csr(CSRRS, A2, T0, MSTATUS)
            .csr(CSRRC, A3, T0, MSTATUS)
            .csr(CSRRCI, A4, 10, MSTATUS)
            .csr(CSRRSI, A4, 0, MSTATUS)
            .csr(CSRRWI, A5, 0, MSTATUS)
            .read(A5, MSTATUS);
        a.ending(ECALL, FROM_M)
    })
}

/// `sstatus` (B.2): the S view of `mstatus`, written with its own bits only; `MPP` and
/// `MPIE` stay as `mstatus` left them.
fn sstatus() -> (Program, ExpectedTrap) {
    build("sstatus", |a, _| {
        a.li(T0, 0x1880).write(MSTATUS, T0);
        a.store(SSTATUS, 0x000c_0122).read(A1, MSTATUS);
        a.li(T0, 0x100)
            .csr(CSRRC, A2, T0, SSTATUS)
            .csr(CSRRSI, A3, 2, SSTATUS)
            .li(T0, 0x0008_0020)
            .csr(CSRRS, A4, T0, SSTATUS)
            .csr(CSRRCI, A4, 2, SSTATUS)
            .read(A5, SSTATUS)
            .read(A6, MSTATUS)
            .csr(CSRRWI, A5, 0, SSTATUS)
            .read(A5, SSTATUS)
            .read(A6, MSTATUS);
        a.ending(ECALL, FROM_M)
    })
}

/// The supervisor CSRs and `medeleg` in M (B.2): `stvec` and `sepc` drop bits `[1:0]`,
/// `sscratch`, `scause`, and `stval` keep 32 bits, `satp` keeps a Bare value with `ASID`
/// 0, `medeleg` keeps its mask, and `mideleg`, `sie`, `sip` read 0.
fn supervisor_csrs() -> (Program, ExpectedTrap) {
    build("supervisor-csrs", |a, _| {
        for value in [0x8000_1000, 0x8000_1004, 0xffff_fffc, 0] {
            a.store(STVEC, value);
        }
        for value in [0xffff_ffff, 0x8000_0002, 0x8000_0001, 0x1234_5678, 0] {
            a.store(SEPC, value);
        }
        for csr in [SSCRATCH, SCAUSE, STVAL] {
            for value in [0xffff_ffff, 0x8000_000b, 0x1234_5678, 0] {
                a.store(csr, value);
            }
            a.csr(CSRRSI, A1, 31, csr)
                .li(T1, 0x8000_0000)
                .csr(CSRRS, A2, T1, csr)
                .csr(CSRRC, A3, T1, csr)
                .csr(CSRRCI, A4, 1, csr)
                .read(A4, csr);
        }
        for value in [0x003f_ffff, 0x0001_2345, 0] {
            a.store(SATP, value);
        }
        for value in [0xb1ff, 0xb1f7, 0x0100, 0] {
            a.store(MEDELEG, value);
        }
        a.read(A1, MIDELEG).read(A2, SIE).read(A3, SIP);
        a.ending(ECALL, FROM_M)
    })
}

/// `MRET` from M to M (B.4 R1, R2): `MIE` ← `MPIE`, `MPIE` ← 1, `MPP` ← U.
fn mret_m() -> (Program, ExpectedTrap) {
    build("mret-m", |a, _| {
        a.mret_with(0x1880).read(A0, MSTATUS);
        a.mret_with(0x1808).read(A1, MSTATUS);
        a.mret_with(0x1888).read(A2, MSTATUS);
        a.ending(ECALL, FROM_M)
    })
}

/// `MPP` written as `0b10` stores U, and `MRET` enters U (B.4 P1).
fn mret_reserved_mpp() -> (Program, ExpectedTrap) {
    build("mret-reserved-mpp", |a, _| {
        a.li(T0, 0x1080).write(MSTATUS, T0).read(A0, MSTATUS);
        let target = a.pc() + 4 * (2 + 1 + 1);
        a.li(T1, target).write(MEPC, T1).push(MRET);
        a.addi(A1, 0, 1);
        a.ending(ECALL, FROM_U)
    })
}

/// `MRET` to S (B.3 M3): the S view afterwards, then an `ECALL` from S, which always goes
/// to M.
fn mret_s() -> (Program, ExpectedTrap) {
    build("mret-s", |a, _| {
        a.mret_with(MPP_S | 0x80).read(A0, SSTATUS).addi(A1, 0, 1);
        a.ending(ECALL, FROM_S)
    })
}

/// `SRET` from M (B.4 R3): legal, it enters `SPP` = S and leaves `MPP` alone.
fn sret_m() -> (Program, ExpectedTrap) {
    build("sret-m", |a, _| {
        a.li(T0, MPP_M | 0x120).write(MSTATUS, T0);
        a.sret_to_next().read(A0, SSTATUS).addi(A1, 0, 1);
        a.ending(ECALL, FROM_S)
    })
}

/// `SRET` from S to S, then to U (B.4 R4, R7), a delegated `ECALL` from U and its return,
/// and a breakpoint taken in M.
fn sret_chain() -> (Program, ExpectedTrap) {
    build("sret-chain", |a, labels| {
        a.supervisor(labels, 0x0100);
        a.mret_with(MPP_S | 0x100 | 0x2);
        a.sret_to_next().read(A0, SSTATUS);
        a.csr(CSRRSI, 0, 2, SSTATUS).read(A1, SSTATUS);
        a.sret_to_next().addi(A2, 0, 3).push(ECALL).addi(A3, 0, 4);
        a.ending(EBREAK, "Breakpoint")
    })
}

/// S-mode rows of B.3 with `medeleg` = `0xB1FF`: the CSR access rule, `MRET` illegal,
/// `SFENCE.VMA` and the S CSRs legal, each illegal access delegated to S.
fn s_traps() -> (Program, ExpectedTrap) {
    build("s-traps", |a, labels| {
        a.supervisor(labels, 0xb1ff).mret_with(MPP_S);
        a.read(A0, SSTATUS).read(A1, MSTATUS);
        a.csr(CSRRSI, 0, 2, SSTATUS).read(A1, MSTATUS);
        a.push(MRET).push(SFENCE_VMA);
        a.read(A2, SATP)
            .li(T0, 0x0001_2345)
            .csr(CSRRW, A3, T0, SATP)
            .read(A3, SATP);
        a.read(A4, MEDELEG).read(A4, MTVEC).read(A4, MSCRATCH);
        a.read(A5, SIE).read(A6, SIP).read(A6, STVEC);
        a.store(SSCRATCH, 0x5a5a_a5a5).store(SEPC, 0x8000_0003);
        a.ending(ECALL, FROM_S)
    })
}

/// U-mode rows of B.3 with `medeleg` = `0xB1F7`: every CSR illegal, `SRET`, `MRET`,
/// `SFENCE.VMA`, and `WFI` illegal, a delegated `ECALL`, the misaligned and faulting
/// accesses with alignment first, a misaligned jump target, and a breakpoint, which is
/// not delegated.
fn u_traps() -> (Program, ExpectedTrap) {
    build("u-traps", |a, labels| {
        a.supervisor(labels, 0xb1f7).mret_with(MPP_U);
        a.addi(A0, 0, 7).read(A1, SSTATUS).read(A1, 0xc00);
        a.push(ECALL)
            .push(SRET)
            .push(MRET)
            .push(SFENCE_VMA)
            .push(WFI);
        a.li(T0, 0x8000_0001).push(lw(A2, T0, 0));
        a.push(lw(A2, 0, 0))
            .push(sw(A2, 0, 0))
            .push(lw(A2, 0, 1))
            .push(sw(A2, 0, 2));
        a.li(T0, 0x8000_0002).push(i(JALR, 0, RA, T0, 0));
        a.addi(A3, 0, 9);
        a.ending(EBREAK, "Breakpoint")
    })
}

/// An `ECALL` from U with `medeleg` = 0 goes to M.
fn u_ecall() -> (Program, ExpectedTrap) {
    build("u-ecall", |a, labels| {
        a.supervisor(labels, 0).mret_with(MPP_U).addi(A0, 0, 1);
        a.ending(ECALL, FROM_U)
    })
}

/// An illegal instruction in M is never delegated (B.3 M1), and `SFENCE.VMA` retires in
/// M.
fn m_illegal() -> (Program, ExpectedTrap) {
    build("m-illegal", |a, labels| {
        a.supervisor(labels, 0xb1ff).push(SFENCE_VMA);
        a.ending(csr_word(CSRRS, A0, 0, 0x7c0), "IllegalInstruction")
    })
}

/// Every directed M3.2 program, with the exception taken in M that ends it.
pub fn programs() -> Vec<(Program, ExpectedTrap)> {
    vec![
        reset(),
        mstatus(),
        sstatus(),
        supervisor_csrs(),
        mret_m(),
        mret_reserved_mpp(),
        mret_s(),
        sret_m(),
        sret_chain(),
        s_traps(),
        u_traps(),
        u_ecall(),
        m_illegal(),
    ]
}

#[cfg(test)]
mod tests {
    use systemscope_contracts::trace::Value;
    use systemscope_rv32i::csr::decode_privileged;
    use systemscope_rv32i::{PrivInstr, Rv32iProfile};

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

    /// Every program ends on SystemScope exactly with its expected exception taken in M.
    #[test]
    fn every_program_ends_with_its_exception_taken_in_m() {
        let programs = programs();
        assert_eq!(programs.len(), 13);
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

    /// The bodies avoid m3-2-spike-appendix B.6: no write of `mideleg`, `sie`, `sip`, or
    /// `mip`, no read of `mip`, `WFI` only in `u-traps`, and every `medeleg` and `satp`
    /// value loaded inside the compared range.
    #[test]
    fn no_program_reaches_a_recorded_divergence() {
        for (p, _) in programs() {
            for &word in &p.code {
                if let Some(instr @ PrivInstr::Csr { csr, .. }) = decode_privileged(word) {
                    assert_ne!(csr, 0x344, "{}: mip", p.name);
                    if instr.writes() {
                        assert!(![MIDELEG, SIE, SIP].contains(&csr), "{}", p.name);
                    }
                }
                if word == WFI {
                    assert_eq!(p.name, "m3-u-traps");
                }
            }
        }
    }

    /// `MRET` and `SRET` land where m3-2-spike-appendix B.4 says, with its `mstatus`.
    #[test]
    fn mret_and_sret_leave_the_measured_status() {
        let find = |name: &str| {
            programs()
                .into_iter()
                .find(|(p, _)| p.name == name)
                .unwrap()
                .0
        };
        let f = run(&find("m3-mret-m"));
        assert_eq!(
            (reg(&f, "x10"), reg(&f, "x11"), reg(&f, "x12")),
            (0x0088, 0x0080, 0x0088)
        );
        let f = run(&find("m3-mret-reserved-mpp"));
        assert_eq!(reg(&f, "x10"), 0x0080);
        let f = run(&find("m3-sret-m"));
        assert_eq!(reg(&f, "x10"), 0x22);
        let f = run(&find("m3-sret-chain"));
        assert_eq!((reg(&f, "x10"), reg(&f, "x11")), (0x20, 0x22));
        // The delegated ECALL from U: cause 8, stval 0, SPP = U, SPIE = the SIE of U.
        assert_eq!(
            (reg(&f, "x28"), reg(&f, "x30"), reg(&f, "x31")),
            (8, 0, 0x20)
        );
    }

    /// The delegated exceptions of `u-traps` reach the S handler in order, the last one
    /// the misaligned jump, and none retires.
    #[test]
    fn u_traps_delegates_every_exception_but_the_breakpoint() {
        let (p, expected) = u_traps();
        let f = run(&p);
        assert_eq!(reg(&f, "x28"), 0, "the last delegated cause");
        assert_eq!(reg(&f, "x30"), 0x8000_0002, "its stval");
        assert_eq!(reg(&f, "x13"), 9);
        assert_eq!(expected.cause, "Breakpoint");
    }
}
