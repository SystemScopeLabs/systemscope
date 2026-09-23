//! ALU semantics against hand-computed vectors and an independent interpreter
//! (`docs/m1-design.md` §5.3, M1-A1).
//!
//! The oracle below shares no code with `execute_alu`. It works on mathematical integers:
//! values are reduced modulo 2^32, signed readings are computed from the value (not by
//! reinterpreting bits), shifts are multiplication and floor division by powers of two,
//! and bitwise operations are computed one bit at a time.

use proptest::prelude::*;
use systemscope_rv32i::{
    ImmOp, Instr, NotAlu, PendingEffect, Reg, RegOp, RegWrite, ShiftOp, decode, execute_alu,
};

fn x(i: u8) -> Reg {
    Reg::new(i).unwrap()
}

// ---------------------------------------------------------------------------------------
// The oracle.

const MOD: i64 = 1 << 32;

/// Reduces a mathematical integer to 32 bits.
fn wrap(v: i64) -> u32 {
    u32::try_from(v.rem_euclid(MOD)).unwrap()
}

/// The two's-complement reading of a 32-bit value.
fn signed(v: u32) -> i64 {
    let v = i64::from(v);
    if v >= MOD / 2 { v - MOD } else { v }
}

fn pow2(n: u32) -> i64 {
    (0..n).fold(1, |p, _| p * 2)
}

fn bitwise(a: u32, b: u32, f: fn(bool, bool) -> bool) -> u32 {
    let (a, b) = (i64::from(a), i64::from(b));
    let v = (0..32).fold(0, |acc, i| {
        let bit = |v: i64| (v / pow2(i)) % 2 == 1;
        acc + if f(bit(a), bit(b)) { pow2(i) } else { 0 }
    });
    u32::try_from(v).unwrap()
}

enum Shift {
    Left,
    Logical,
    Arithmetic,
}

/// Shifts by `amount mod 32`.
fn shift(kind: Shift, v: u32, amount: u32) -> u32 {
    let p = pow2(amount % 32);
    match kind {
        Shift::Left => wrap(i64::from(v) * p),
        Shift::Logical => wrap(i64::from(v) / p),
        Shift::Arithmetic => wrap(signed(v).div_euclid(p)),
    }
}

/// `rd` and the value an ALU instruction writes, or `None` for any other instruction.
fn oracle(instr: &Instr, pc: u32, a: u32, b: u32) -> Option<(Reg, u32)> {
    let flag = |c: bool| u32::from(c);
    Some(match *instr {
        Instr::Lui { rd, imm } => (rd, imm),
        Instr::Auipc { rd, imm } => (rd, wrap(i64::from(pc) + i64::from(imm))),
        Instr::OpImm { op, rd, imm, .. } => {
            let imm = i64::from(imm);
            let bits = wrap(imm);
            let v = match op {
                ImmOp::Addi => wrap(i64::from(a) + imm),
                ImmOp::Slti => flag(signed(a) < imm),
                ImmOp::Sltiu => flag(a < bits),
                ImmOp::Xori => bitwise(a, bits, |p, q| p != q),
                ImmOp::Ori => bitwise(a, bits, |p, q| p || q),
                ImmOp::Andi => bitwise(a, bits, |p, q| p && q),
            };
            (rd, v)
        }
        Instr::ShiftImm { op, rd, shamt, .. } => {
            let kind = match op {
                ShiftOp::Sll => Shift::Left,
                ShiftOp::Srl => Shift::Logical,
                ShiftOp::Sra => Shift::Arithmetic,
            };
            (rd, shift(kind, a, shamt))
        }
        Instr::Op { op, rd, .. } => {
            let v = match op {
                RegOp::Add => wrap(i64::from(a) + i64::from(b)),
                RegOp::Sub => wrap(i64::from(a) - i64::from(b)),
                RegOp::Sll => shift(Shift::Left, a, b),
                RegOp::Slt => flag(signed(a) < signed(b)),
                RegOp::Sltu => flag(a < b),
                RegOp::Xor => bitwise(a, b, |p, q| p != q),
                RegOp::Srl => shift(Shift::Logical, a, b),
                RegOp::Sra => shift(Shift::Arithmetic, a, b),
                RegOp::Or => bitwise(a, b, |p, q| p || q),
                RegOp::And => bitwise(a, b, |p, q| p && q),
            };
            (rd, v)
        }
        _ => return None,
    })
}

/// What `execute_alu` must return.
fn expected(instr: &Instr, pc: u32, a: u32, b: u32) -> Result<PendingEffect, NotAlu> {
    match oracle(instr, pc, a, b) {
        Some((rd, value)) => Ok(PendingEffect {
            reg_write: Some(RegWrite { rd, value }),
            next_pc: wrap(i64::from(pc) + 4),
        }),
        None => Err(NotAlu),
    }
}

// ---------------------------------------------------------------------------------------
// Hand-computed vectors.

const PC: u32 = 0x8000_0000;

/// The value written to `x5` by `instr` with `rs1` and `rs2` holding `a` and `b`.
fn run(instr: Instr, a: u32, b: u32) -> u32 {
    run_at(instr, PC, a, b)
}

fn run_at(instr: Instr, pc: u32, a: u32, b: u32) -> u32 {
    let effect = execute_alu(&instr, pc, a, b).expect("an ALU instruction");
    assert_eq!(effect.next_pc, pc.wrapping_add(4));
    let write = effect.reg_write.expect("ALU instructions write rd");
    assert_eq!(write.rd, x(5));
    write.value
}

fn imm(op: ImmOp, imm: i32) -> Instr {
    Instr::OpImm {
        op,
        rd: x(5),
        rs1: x(6),
        imm,
    }
}

fn shi(op: ShiftOp, shamt: u32) -> Instr {
    Instr::ShiftImm {
        op,
        rd: x(5),
        rs1: x(6),
        shamt,
    }
}

fn reg(op: RegOp) -> Instr {
    Instr::Op {
        op,
        rd: x(5),
        rs1: x(6),
        rs2: x(7),
    }
}

#[test]
fn lui_writes_the_immediate() {
    let lui = |imm| Instr::Lui { rd: x(5), imm };
    assert_eq!(run(lui(0x1234_5000), 7, 9), 0x1234_5000, "sources ignored");
    assert_eq!(run(lui(0xffff_f000), 0, 0), 0xffff_f000);
    assert_eq!(run(lui(0), u32::MAX, u32::MAX), 0);
}

#[test]
fn auipc_adds_the_raw_immediate_to_pc_with_wrap_around() {
    let auipc = |imm| Instr::Auipc { rd: x(5), imm };
    assert_eq!(run_at(auipc(0x0000_1000), 0x0000_2000, 0, 0), 0x0000_3000);
    // The immediate is added as bits: 0xfffff000 acts as -4096.
    assert_eq!(run_at(auipc(0xffff_f000), 0x0000_1000, 0, 0), 0);
    assert_eq!(run_at(auipc(0x8000_0000), 0x8000_0000, 0, 0), 0);
    assert_eq!(run_at(auipc(0), 0x1234_5678, 1, 2), 0x1234_5678);
}

#[test]
fn addi_wraps_and_uses_the_signed_immediate() {
    assert_eq!(run(imm(ImmOp::Addi, 1), u32::MAX, 0), 0, "0xffffffff + 1");
    assert_eq!(run(imm(ImmOp::Addi, -1), 0, 0), u32::MAX, "0 - 1");
    assert_eq!(run(imm(ImmOp::Addi, -2048), 0x800, 0), 0);
    assert_eq!(run(imm(ImmOp::Addi, 2047), 0x7fff_f801, 0), 0x8000_0000);
    assert_eq!(run(imm(ImmOp::Addi, 0), 0xdead_beef, 0), 0xdead_beef);
}

#[test]
fn slti_compares_signed() {
    assert_eq!(run(imm(ImmOp::Slti, 0), 0xffff_ffff, 0), 1, "-1 < 0");
    assert_eq!(run(imm(ImmOp::Slti, 0), 0x7fff_ffff, 0), 0);
    assert_eq!(
        run(imm(ImmOp::Slti, -1), 0x8000_0000, 0),
        1,
        "i32::MIN < -1"
    );
    assert_eq!(run(imm(ImmOp::Slti, -1), 0xffff_ffff, 0), 0, "-1 < -1");
    assert_eq!(
        run(imm(ImmOp::Slti, -2048), 0xffff_f7ff, 0),
        1,
        "-2049 < -2048"
    );
    assert_eq!(run(imm(ImmOp::Slti, 2047), 2046, 0), 1);
}

#[test]
fn sltiu_compares_unsigned_against_the_sign_extended_immediate() {
    // imm = -1 is 0xffffffff: everything but 0xffffffff is below it.
    assert_eq!(run(imm(ImmOp::Sltiu, -1), 0, 0), 1);
    assert_eq!(run(imm(ImmOp::Sltiu, -1), 0xffff_fffe, 0), 1);
    assert_eq!(run(imm(ImmOp::Sltiu, -1), 0xffff_ffff, 0), 0);
    // imm = -2048 is 0xfffff800.
    assert_eq!(run(imm(ImmOp::Sltiu, -2048), 0xffff_f7ff, 0), 1);
    assert_eq!(run(imm(ImmOp::Sltiu, -2048), 0xffff_f800, 0), 0);
    // SEQZ: sltiu rd, rs, 1.
    assert_eq!(run(imm(ImmOp::Sltiu, 1), 0, 0), 1);
    assert_eq!(run(imm(ImmOp::Sltiu, 1), 1, 0), 0);
    assert_eq!(run(imm(ImmOp::Sltiu, 0), 0, 0), 0, "nothing is below 0");
}

#[test]
fn bitwise_immediates_use_the_sign_extended_pattern() {
    assert_eq!(
        run(imm(ImmOp::Xori, -1), 0x1234_5678, 0),
        0xedcb_a987,
        "NOT"
    );
    assert_eq!(run(imm(ImmOp::Xori, 0x5a5), 0x0000_0fff, 0), 0x0000_0a5a);
    assert_eq!(run(imm(ImmOp::Ori, -2048), 0x0000_0001, 0), 0xffff_f801);
    assert_eq!(run(imm(ImmOp::Ori, 0x0f0), 0x0000_0f00, 0), 0x0000_0ff0);
    assert_eq!(run(imm(ImmOp::Andi, -1), 0xdead_beef, 0), 0xdead_beef);
    assert_eq!(run(imm(ImmOp::Andi, -2048), 0xdead_beef, 0), 0xdead_b800);
    assert_eq!(run(imm(ImmOp::Andi, 0xff), 0xdead_beef, 0), 0x0000_00ef);
}

#[test]
fn immediate_shifts_are_logical_or_arithmetic() {
    assert_eq!(run(shi(ShiftOp::Sll, 0), 0x8000_0001, 0), 0x8000_0001);
    assert_eq!(run(shi(ShiftOp::Sll, 1), 0x8000_0001, 0), 0x0000_0002);
    assert_eq!(run(shi(ShiftOp::Sll, 31), 0x0000_0003, 0), 0x8000_0000);
    assert_eq!(run(shi(ShiftOp::Srl, 0), 0x8000_0000, 0), 0x8000_0000);
    assert_eq!(run(shi(ShiftOp::Srl, 1), 0x8000_0000, 0), 0x4000_0000);
    assert_eq!(run(shi(ShiftOp::Srl, 31), 0x8000_0000, 0), 1);
    assert_eq!(run(shi(ShiftOp::Sra, 0), 0x8000_0000, 0), 0x8000_0000);
    assert_eq!(run(shi(ShiftOp::Sra, 1), 0x8000_0000, 0), 0xc000_0000);
    assert_eq!(run(shi(ShiftOp::Sra, 31), 0x8000_0000, 0), 0xffff_ffff);
    assert_eq!(
        run(shi(ShiftOp::Sra, 4), 0x7fff_fff0, 0),
        0x07ff_ffff,
        "positive"
    );
    // A hand-built shamt of 32 or more uses its low 5 bits, as register shifts do.
    assert_eq!(run(shi(ShiftOp::Sll, 32), 0x8000_0001, 0), 0x8000_0001);
    assert_eq!(
        run(shi(ShiftOp::Sra, u32::MAX), 0x8000_0000, 0),
        0xffff_ffff
    );
}

#[test]
fn add_and_sub_wrap() {
    assert_eq!(run(reg(RegOp::Add), u32::MAX, 1), 0);
    assert_eq!(run(reg(RegOp::Add), 0x8000_0000, 0x8000_0000), 0);
    assert_eq!(run(reg(RegOp::Add), 0x7fff_ffff, 1), 0x8000_0000);
    assert_eq!(run(reg(RegOp::Sub), 0, 1), u32::MAX);
    assert_eq!(run(reg(RegOp::Sub), 0x8000_0000, 1), 0x7fff_ffff);
    assert_eq!(run(reg(RegOp::Sub), 5, 3), 2);
}

#[test]
fn slt_is_signed_and_sltu_is_unsigned() {
    assert_eq!(run(reg(RegOp::Slt), 0xffff_ffff, 0), 1, "-1 < 0");
    assert_eq!(run(reg(RegOp::Sltu), 0xffff_ffff, 0), 0, "0xffffffff < 0");
    assert_eq!(run(reg(RegOp::Slt), 0, 0xffff_ffff), 0);
    assert_eq!(run(reg(RegOp::Sltu), 0, 0xffff_ffff), 1);
    assert_eq!(run(reg(RegOp::Slt), 0x8000_0000, 0x7fff_ffff), 1);
    assert_eq!(run(reg(RegOp::Sltu), 0x8000_0000, 0x7fff_ffff), 0);
    assert_eq!(run(reg(RegOp::Slt), 3, 3), 0);
    assert_eq!(run(reg(RegOp::Sltu), 3, 3), 0);
    // SNEZ: sltu rd, x0, rs.
    assert_eq!(run(reg(RegOp::Sltu), 0, 7), 1);
}

#[test]
fn register_bitwise_operations() {
    assert_eq!(run(reg(RegOp::Xor), 0xff00_ff00, 0x0ff0_0ff0), 0xf0f0_f0f0);
    assert_eq!(run(reg(RegOp::Or), 0xff00_ff00, 0x0ff0_0ff0), 0xfff0_fff0);
    assert_eq!(run(reg(RegOp::And), 0xff00_ff00, 0x0ff0_0ff0), 0x0f00_0f00);
}

#[test]
fn register_shifts_use_the_low_5_bits_of_rs2() {
    let v = 0x8000_0001;
    for (amount, sll, srl, sra) in [
        (0, 0x8000_0001, 0x8000_0001, 0x8000_0001),
        (1, 0x0000_0002, 0x4000_0000, 0xc000_0000),
        (31, 0x8000_0000, 0x0000_0001, 0xffff_ffff),
        (32, 0x8000_0001, 0x8000_0001, 0x8000_0001),
        (33, 0x0000_0002, 0x4000_0000, 0xc000_0000),
        (63, 0x8000_0000, 0x0000_0001, 0xffff_ffff),
        (u32::MAX, 0x8000_0000, 0x0000_0001, 0xffff_ffff),
        (0xffff_ffe0, 0x8000_0001, 0x8000_0001, 0x8000_0001),
    ] {
        assert_eq!(run(reg(RegOp::Sll), v, amount), sll, "sll {amount:#x}");
        assert_eq!(run(reg(RegOp::Srl), v, amount), srl, "srl {amount:#x}");
        assert_eq!(run(reg(RegOp::Sra), v, amount), sra, "sra {amount:#x}");
    }
}

#[test]
fn x0_writes_stay_in_the_effect() {
    let add = Instr::Op {
        op: RegOp::Add,
        rd: Reg::ZERO,
        rs1: x(1),
        rs2: x(2),
    };
    assert_eq!(
        execute_alu(&add, 0x100, 2, 3),
        Ok(PendingEffect {
            reg_write: Some(RegWrite {
                rd: Reg::ZERO,
                value: 5
            }),
            next_pc: 0x104,
        })
    );
    let lui = Instr::Lui {
        rd: Reg::ZERO,
        imm: 0x1000,
    };
    let write = execute_alu(&lui, 0, 0, 0).unwrap().reg_write.unwrap();
    assert_eq!((write.rd, write.value), (Reg::ZERO, 0x1000));
}

#[test]
fn next_pc_is_pc_plus_4_and_wraps() {
    let nop = decode(0x0000_0013).unwrap();
    assert_eq!(execute_alu(&nop, 0, 0, 0).unwrap().next_pc, 4);
    assert_eq!(execute_alu(&nop, 0xffff_fffc, 0, 0).unwrap().next_pc, 0);
}

#[test]
fn other_instructions_are_not_alu() {
    // JAL, JALR, BEQ, LW, SW, FENCE, ECALL, EBREAK.
    for word in [
        0xffdf_f0ef,
        0x0000_8067,
        0x0020_8063,
        0x0000_2003,
        0x0000_2023,
        0x0ff0_000f,
        0x0000_0073,
        0x0010_0073,
    ] {
        let instr = decode(word).unwrap();
        assert_eq!(execute_alu(&instr, PC, 1, 2), Err(NotAlu), "{word:#010x}");
    }
}

// ---------------------------------------------------------------------------------------
// Properties.

/// Operand values, biased toward the boundaries.
fn value() -> impl Strategy<Value = u32> {
    prop_oneof![
        3 => any::<u32>(),
        1 => prop::sample::select(vec![0, 1, 2, 31, 32, 0x7fff_ffff, 0x8000_0000, 0xffff_fffe, u32::MAX]),
    ]
}

fn any_reg() -> impl Strategy<Value = Reg> {
    (0u8..32).prop_map(x)
}

fn imm_op() -> impl Strategy<Value = ImmOp> {
    prop::sample::select(vec![
        ImmOp::Addi,
        ImmOp::Slti,
        ImmOp::Sltiu,
        ImmOp::Xori,
        ImmOp::Ori,
        ImmOp::Andi,
    ])
}

fn shift_op() -> impl Strategy<Value = ShiftOp> {
    prop::sample::select(vec![ShiftOp::Sll, ShiftOp::Srl, ShiftOp::Sra])
}

fn reg_op() -> impl Strategy<Value = RegOp> {
    prop::sample::select(vec![
        RegOp::Add,
        RegOp::Sub,
        RegOp::Sll,
        RegOp::Slt,
        RegOp::Sltu,
        RegOp::Xor,
        RegOp::Srl,
        RegOp::Sra,
        RegOp::Or,
        RegOp::And,
    ])
}

/// Every ALU instruction shape, with immediates in the ranges `decode` produces.
fn alu_instr() -> impl Strategy<Value = Instr> {
    prop_oneof![
        (any_reg(), any::<u32>()).prop_map(|(rd, i)| Instr::Lui {
            rd,
            imm: i & 0xffff_f000
        }),
        (any_reg(), any::<u32>()).prop_map(|(rd, i)| Instr::Auipc {
            rd,
            imm: i & 0xffff_f000
        }),
        (imm_op(), any_reg(), any_reg(), -2048i32..2048)
            .prop_map(|(op, rd, rs1, imm)| { Instr::OpImm { op, rd, rs1, imm } }),
        (shift_op(), any_reg(), any_reg(), 0u32..32)
            .prop_map(|(op, rd, rs1, shamt)| { Instr::ShiftImm { op, rd, rs1, shamt } }),
        (reg_op(), any_reg(), any_reg(), any_reg())
            .prop_map(|(op, rd, rs1, rs2)| { Instr::Op { op, rd, rs1, rs2 } }),
    ]
}

fn run_op(op: RegOp, a: u32, b: u32) -> u32 {
    let effect = execute_alu(&reg(op), 0, a, b).unwrap();
    effect.reg_write.unwrap().value
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn alu_instructions_match_the_oracle(
        instr in alu_instr(),
        pc in any::<u32>(),
        a in value(),
        b in value(),
    ) {
        prop_assert_eq!(execute_alu(&instr, pc, a, b), expected(&instr, pc, a, b));
    }

    /// Straight from the decoder: every decoded word, ALU or not, executes as the oracle
    /// says.
    #[test]
    fn decoded_words_match_the_oracle(
        word in any::<u32>(),
        pc in any::<u32>(),
        a in value(),
        b in value(),
    ) {
        if let Ok(instr) = decode(word) {
            prop_assert_eq!(execute_alu(&instr, pc, a, b), expected(&instr, pc, a, b));
        }
    }

    /// Execution reads only the sources the instruction names.
    #[test]
    fn unused_sources_are_ignored(
        instr in alu_instr(),
        pc in any::<u32>(),
        a in value(),
        b in value(),
        other in value(),
    ) {
        let (uses_rs1, uses_rs2) = match instr {
            Instr::Lui { .. } | Instr::Auipc { .. } => (false, false),
            Instr::OpImm { .. } | Instr::ShiftImm { .. } => (true, false),
            _ => (true, true),
        };
        let effect = execute_alu(&instr, pc, a, b);
        if !uses_rs1 {
            prop_assert_eq!(execute_alu(&instr, pc, other, b), effect);
        }
        if !uses_rs2 {
            prop_assert_eq!(execute_alu(&instr, pc, a, other), effect);
        }
    }

    #[test]
    fn algebraic_identities(v in value(), k in 0u32..32) {
        prop_assert_eq!(run_op(RegOp::Add, v, 0), v);
        prop_assert_eq!(run_op(RegOp::Sub, v, v), 0);
        prop_assert_eq!(run_op(RegOp::Xor, v, v), 0);
        prop_assert_eq!(run_op(RegOp::Or, v, 0), v);
        prop_assert_eq!(run_op(RegOp::And, v, u32::MAX), v);
        prop_assert_eq!(run_op(RegOp::Slt, v, v), 0);
        prop_assert_eq!(run_op(RegOp::Sltu, v, v), 0);
        prop_assert_eq!(run_op(RegOp::Add, v, run_op(RegOp::Sub, 0, v)), 0);
        // Register shift amounts are taken modulo 32.
        for op in [RegOp::Sll, RegOp::Srl, RegOp::Sra] {
            prop_assert_eq!(run_op(op, v, 32 + k), run_op(op, v, k));
        }
        // SRA and SRL agree exactly when the sign bit is clear.
        let positive = v & 0x7fff_ffff;
        prop_assert_eq!(run_op(RegOp::Sra, positive, k), run_op(RegOp::Srl, positive, k));
    }
}
