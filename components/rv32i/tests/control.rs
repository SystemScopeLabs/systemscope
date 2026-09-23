//! Branch and jump semantics, including precise misaligned-target traps, against
//! hand-computed vectors and an independent interpreter (`docs/m1-design.md` §5.3, §6).
//!
//! The oracle below shares no code with `execute_control`. It works on mathematical
//! integers: addresses are reduced modulo 2^32, signed readings are computed from the
//! value, conditions are decided from an `Ordering`, `JALR` clears bit 0 by subtracting the
//! remainder mod 2, and alignment is a remainder mod 4.

use std::cmp::Ordering;

use proptest::prelude::*;
use systemscope_rv32i::{
    BranchOp, ExecOutcome, Instr, NotControl, PendingEffect, PendingTrap, Reg, RegWrite, TrapCause,
    decode, execute_alu, execute_control,
};

fn x(i: u8) -> Reg {
    Reg::new(i).unwrap()
}

// ---------------------------------------------------------------------------------------
// The oracle.

const MOD: i64 = 1 << 32;

fn wrap(v: i64) -> u32 {
    u32::try_from(v.rem_euclid(MOD)).unwrap()
}

fn signed(v: u32) -> i64 {
    let v = i64::from(v);
    if v >= MOD / 2 { v - MOD } else { v }
}

/// What a control-transfer instruction does, in the oracle's own terms.
#[derive(Debug, PartialEq)]
enum Oracle {
    /// Retires, writing `(rd, value)` if the instruction links.
    Retire {
        write: Option<(u8, u32)>,
        next_pc: u32,
    },
    /// Traps on a target that is not a multiple of 4.
    Misaligned { target: u32 },
    /// Not a branch or jump.
    Other,
}

fn decide(op: BranchOp, a: u32, b: u32) -> bool {
    let (order, want): (Ordering, &[Ordering]) = match op {
        BranchOp::Eq => (a.cmp(&b), &[Ordering::Equal]),
        BranchOp::Ne => (a.cmp(&b), &[Ordering::Less, Ordering::Greater]),
        BranchOp::Lt => (signed(a).cmp(&signed(b)), &[Ordering::Less]),
        BranchOp::Ge => (
            signed(a).cmp(&signed(b)),
            &[Ordering::Equal, Ordering::Greater],
        ),
        BranchOp::Ltu => (i64::from(a).cmp(&i64::from(b)), &[Ordering::Less]),
        BranchOp::Geu => {
            let order = i64::from(a).cmp(&i64::from(b));
            (order, &[Ordering::Equal, Ordering::Greater])
        }
    };
    want.contains(&order)
}

fn jump(write: Option<(u8, u32)>, target: u32) -> Oracle {
    if i64::from(target) % 4 == 0 {
        Oracle::Retire {
            write,
            next_pc: target,
        }
    } else {
        Oracle::Misaligned { target }
    }
}

fn oracle(instr: &Instr, pc: u32, a: u32, b: u32) -> Oracle {
    let pc = i64::from(pc);
    let link = |rd: Reg| Some((rd.index(), wrap(pc + 4)));
    match *instr {
        Instr::Branch { op, offset, .. } => {
            if decide(op, a, b) {
                jump(None, wrap(pc + i64::from(offset)))
            } else {
                Oracle::Retire {
                    write: None,
                    next_pc: wrap(pc + 4),
                }
            }
        }
        Instr::Jal { rd, offset } => jump(link(rd), wrap(pc + i64::from(offset))),
        Instr::Jalr { rd, offset, .. } => {
            let sum = i64::from(wrap(i64::from(a) + i64::from(offset)));
            jump(link(rd), wrap(sum - sum % 2))
        }
        _ => Oracle::Other,
    }
}

/// What `execute_control` must return.
fn expected(instr: &Instr, pc: u32, a: u32, b: u32) -> Result<ExecOutcome, NotControl> {
    match oracle(instr, pc, a, b) {
        Oracle::Retire { write, next_pc } => Ok(ExecOutcome::Effect(PendingEffect {
            reg_write: write.map(|(rd, value)| RegWrite { rd: x(rd), value }),
            next_pc,
        })),
        Oracle::Misaligned { target } => Ok(ExecOutcome::Trap(PendingTrap {
            cause: TrapCause::InstructionAddressMisaligned,
            tval: target,
        })),
        Oracle::Other => Err(NotControl),
    }
}

// ---------------------------------------------------------------------------------------
// Hand-computed vectors.

const PC: u32 = 0x8000_0100;

fn run(instr: Instr, pc: u32, a: u32, b: u32) -> ExecOutcome {
    execute_control(&instr, pc, a, b).expect("a control-transfer instruction")
}

fn branch(op: BranchOp, offset: i32) -> Instr {
    Instr::Branch {
        op,
        rs1: x(6),
        rs2: x(7),
        offset,
    }
}

fn jal(rd: u8, offset: i32) -> Instr {
    Instr::Jal { rd: x(rd), offset }
}

fn jalr(rd: u8, rs1: u8, offset: i32) -> Instr {
    Instr::Jalr {
        rd: x(rd),
        rs1: x(rs1),
        offset,
    }
}

fn retire(next_pc: u32) -> ExecOutcome {
    ExecOutcome::Effect(PendingEffect {
        reg_write: None,
        next_pc,
    })
}

fn retire_linked(rd: u8, link: u32, next_pc: u32) -> ExecOutcome {
    ExecOutcome::Effect(PendingEffect {
        reg_write: Some(RegWrite {
            rd: x(rd),
            value: link,
        }),
        next_pc,
    })
}

fn misaligned(target: u32) -> ExecOutcome {
    ExecOutcome::Trap(PendingTrap {
        cause: TrapCause::InstructionAddressMisaligned,
        tval: target,
    })
}

/// Whether a branch with an aligned offset is taken.
fn is_taken(op: BranchOp, a: u32, b: u32) -> bool {
    match run(branch(op, 16), PC, a, b) {
        ExecOutcome::Effect(e) if e.next_pc == PC + 16 => true,
        ExecOutcome::Effect(e) if e.next_pc == PC + 4 => false,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn beq_and_bne_compare_for_equality() {
    for (a, b) in [(5, 5), (0, 0), (u32::MAX, u32::MAX)] {
        assert!(is_taken(BranchOp::Eq, a, b));
        assert!(!is_taken(BranchOp::Ne, a, b));
    }
    for (a, b) in [(5, 6), (0, 0x8000_0000), (u32::MAX, 0x7fff_ffff)] {
        assert!(!is_taken(BranchOp::Eq, a, b));
        assert!(is_taken(BranchOp::Ne, a, b));
    }
}

#[test]
fn blt_and_bge_compare_signed() {
    let neg1 = u32::MAX;
    // -1 < 1, and i32::MIN < i32::MAX.
    for (lo, hi) in [
        (neg1, 1),
        (0x8000_0000, 0x7fff_ffff),
        (neg1, 0),
        (0xffff_fffe, neg1),
    ] {
        assert!(is_taken(BranchOp::Lt, lo, hi), "{lo:#x} < {hi:#x}");
        assert!(!is_taken(BranchOp::Ge, lo, hi), "{lo:#x} < {hi:#x}");
        assert!(!is_taken(BranchOp::Lt, hi, lo), "{hi:#x} > {lo:#x}");
        assert!(is_taken(BranchOp::Ge, hi, lo), "{hi:#x} > {lo:#x}");
    }
    // Equal operands: BGE is taken, BLT is not.
    for v in [0, neg1, 0x8000_0000] {
        assert!(!is_taken(BranchOp::Lt, v, v));
        assert!(is_taken(BranchOp::Ge, v, v));
    }
}

#[test]
fn bltu_and_bgeu_compare_unsigned() {
    // Operands on both sides of the sign bit, ordered as unsigned values.
    for (lo, hi) in [(0, u32::MAX), (1, u32::MAX), (0x7fff_ffff, 0x8000_0000)] {
        assert!(is_taken(BranchOp::Ltu, lo, hi), "{lo:#x} <u {hi:#x}");
        assert!(!is_taken(BranchOp::Geu, lo, hi), "{lo:#x} <u {hi:#x}");
        assert!(!is_taken(BranchOp::Ltu, hi, lo), "{hi:#x} >u {lo:#x}");
        assert!(is_taken(BranchOp::Geu, hi, lo), "{hi:#x} >u {lo:#x}");
    }
    for v in [0, u32::MAX] {
        assert!(!is_taken(BranchOp::Ltu, v, v));
        assert!(is_taken(BranchOp::Geu, v, v));
    }
}

#[test]
fn signed_and_unsigned_branches_disagree_across_the_sign_bit() {
    let (a, b) = (u32::MAX, 0);
    assert!(is_taken(BranchOp::Lt, a, b));
    assert!(!is_taken(BranchOp::Ltu, a, b));
    assert!(!is_taken(BranchOp::Ge, a, b));
    assert!(is_taken(BranchOp::Geu, a, b));
}

#[test]
fn taken_branch_jumps_relative_to_its_own_pc() {
    assert_eq!(run(branch(BranchOp::Eq, 8), PC, 1, 1), retire(PC + 8));
    assert_eq!(run(branch(BranchOp::Eq, -8), PC, 1, 1), retire(PC - 8));
    assert_eq!(run(branch(BranchOp::Eq, 0), PC, 1, 1), retire(PC));
    assert_eq!(run(branch(BranchOp::Eq, 4092), PC, 1, 1), retire(PC + 4092));
    assert_eq!(
        run(branch(BranchOp::Eq, -4096), PC, 1, 1),
        retire(PC - 4096)
    );
}

#[test]
fn not_taken_branch_falls_through() {
    assert_eq!(run(branch(BranchOp::Eq, 8), PC, 1, 2), retire(PC + 4));
    assert_eq!(run(branch(BranchOp::Eq, -8), PC, 1, 2), retire(PC + 4));
}

#[test]
fn taken_branch_to_misaligned_target_traps() {
    for offset in [2, -2, 6, 4094, -4094] {
        let target = PC.wrapping_add_signed(offset);
        assert_eq!(
            run(branch(BranchOp::Ne, offset), PC, 1, 2),
            misaligned(target),
            "{offset}"
        );
    }
    // Straight from the decoder: `beq x0, x0, +2`.
    let instr = decode(0x0000_0163).unwrap();
    assert_eq!(run(instr, PC, 0, 0), misaligned(PC + 2));
}

#[test]
fn not_taken_branch_never_checks_its_target() {
    for offset in [2, -2, 6, 4094, -4094] {
        for op in [BranchOp::Eq, BranchOp::Lt, BranchOp::Ltu] {
            assert_eq!(
                run(branch(op, offset), PC, 2, 1),
                retire(PC + 4),
                "{op:?} {offset}"
            );
        }
    }
    let instr = decode(0x0000_0163).unwrap(); // beq x0, x0, +2
    assert_eq!(run(instr, PC, 0, 1), retire(PC + 4));
}

#[test]
fn branch_targets_wrap() {
    assert_eq!(run(branch(BranchOp::Eq, 4), 0xffff_fffc, 0, 0), retire(0));
    assert_eq!(run(branch(BranchOp::Eq, -4), 0, 0, 0), retire(0xffff_fffc));
    assert_eq!(run(branch(BranchOp::Eq, 8), 0xffff_fffc, 0, 1), retire(0));
    assert_eq!(
        run(branch(BranchOp::Eq, 6), 0xffff_fffc, 0, 0),
        misaligned(2)
    );
}

#[test]
fn jal_links_pc_plus_4_and_jumps() {
    assert_eq!(
        run(jal(1, 0x800), PC, 0, 0),
        retire_linked(1, PC + 4, PC + 0x800)
    );
    assert_eq!(
        run(jal(1, -0x800), PC, 0, 0),
        retire_linked(1, PC + 4, PC - 0x800)
    );
    assert_eq!(run(jal(1, 0), PC, 0, 0), retire_linked(1, PC + 4, PC));
    assert_eq!(
        run(jal(5, 0x000f_fffc), 0, 0, 0),
        retire_linked(5, 4, 0x000f_fffc)
    );
    assert_eq!(
        run(jal(5, -0x0010_0000), 0x0010_0000, 0, 0),
        retire_linked(5, 0x0010_0004, 0)
    );
}

#[test]
fn jal_to_x0_keeps_the_write() {
    // `j` is `jal x0`: the write stays in the effect for the register file to discard.
    let j = decode(0x0080_006f).unwrap(); // jal x0, +8
    assert_eq!(run(j, PC, 0, 0), retire_linked(0, PC + 4, PC + 8));
}

#[test]
fn jal_target_and_link_wrap() {
    assert_eq!(run(jal(1, 8), 0xffff_fffc, 0, 0), retire_linked(1, 0, 4));
    assert_eq!(run(jal(1, -8), 4, 0, 0), retire_linked(1, 8, 0xffff_fffc));
}

#[test]
fn misaligned_jal_traps_without_linking() {
    for offset in [2, -2, 6, 0x000f_fffe, -0x000f_fffe] {
        let target = PC.wrapping_add_signed(offset);
        assert_eq!(
            run(jal(1, offset), PC, 0, 0),
            misaligned(target),
            "{offset}"
        );
    }
    let instr = decode(0x0020_00ef).unwrap(); // jal x1, +2
    assert_eq!(run(instr, PC, 0, 0), misaligned(PC + 2));
}

#[test]
fn jalr_jumps_to_rs1_plus_offset() {
    let base = 0x0000_1000;
    assert_eq!(
        run(jalr(1, 6, 8), PC, base, 0),
        retire_linked(1, PC + 4, 0x1008)
    );
    assert_eq!(
        run(jalr(1, 6, -8), PC, base, 0),
        retire_linked(1, PC + 4, 0x0ff8)
    );
    assert_eq!(
        run(jalr(1, 6, 2044), PC, base, 0),
        retire_linked(1, PC + 4, 0x17fc)
    );
    assert_eq!(
        run(jalr(1, 6, -2048), PC, base, 0),
        retire_linked(1, PC + 4, 0x0800)
    );
    // The target depends on rs1, not on pc.
    assert_eq!(run(jalr(1, 6, 0), 0, base, 0), retire_linked(1, 4, base));
}

#[test]
fn jalr_target_wraps() {
    assert_eq!(
        run(jalr(1, 6, 8), PC, 0xffff_fffc, 0),
        retire_linked(1, PC + 4, 4)
    );
    assert_eq!(
        run(jalr(1, 6, -4), PC, 0, 0),
        retire_linked(1, PC + 4, 0xffff_fffc)
    );
    assert_eq!(
        run(jalr(1, 6, 1), PC, u32::MAX, 0),
        retire_linked(1, PC + 4, 0)
    );
}

#[test]
fn jalr_link_wraps() {
    assert_eq!(
        run(jalr(1, 6, 0), 0xffff_fffc, 0x100, 0),
        retire_linked(1, 0, 0x100)
    );
}

#[test]
fn jalr_to_x0_keeps_the_write() {
    let ret = decode(0x0000_8067).unwrap(); // jalr x0, 0(x1)
    assert_eq!(run(ret, PC, 0x2000, 0), retire_linked(0, PC + 4, 0x2000));
}

#[test]
fn jalr_with_rd_equal_to_rs1_uses_the_old_rs1() {
    // jalr x1, 8(x1): the target comes from x1 before the instruction, the link is pc + 4.
    let instr = decode(0x0080_80e7).unwrap();
    assert_eq!(instr, jalr(1, 1, 8));
    assert_eq!(run(instr, PC, 0x4000, 0), retire_linked(1, PC + 4, 0x4008));
}

#[test]
fn jalr_clears_bit_0_before_checking_alignment() {
    // An odd sum ending in 01 becomes aligned: no trap.
    assert_eq!(
        run(jalr(1, 6, 1), PC, 0x1000, 0),
        retire_linked(1, PC + 4, 0x1000)
    );
    assert_eq!(
        run(jalr(1, 6, 0), PC, 0x1001, 0),
        retire_linked(1, PC + 4, 0x1000)
    );
    assert_eq!(
        run(jalr(1, 6, -3), PC, 0x1004, 0),
        retire_linked(1, PC + 4, 0x1000)
    );
    // A sum ending in 10 keeps bit 1 set: trap, with bit 0 already clear in tval.
    assert_eq!(run(jalr(1, 6, 2), PC, 0x1000, 0), misaligned(0x1002));
    // A sum ending in 11 clears to 10: still a trap.
    assert_eq!(run(jalr(1, 6, 3), PC, 0x1000, 0), misaligned(0x1002));
    assert_eq!(run(jalr(1, 6, 0), PC, 0x1003, 0), misaligned(0x1002));
}

#[test]
fn misaligned_jalr_traps_without_linking() {
    for (rs1, offset, target) in [
        (0x1000, 2, 0x1002),
        (0, -2, 0xffff_fffe),
        (u32::MAX, -1, 0xffff_fffe),
    ] {
        assert_eq!(run(jalr(1, 6, offset), PC, rs1, 0), misaligned(target));
        assert_eq!(run(jalr(0, 6, offset), PC, rs1, 0), misaligned(target));
    }
}

#[test]
fn other_instructions_are_not_control() {
    // LUI, AUIPC, ADDI, SLLI, ADD, LW, SW, FENCE, ECALL, EBREAK.
    for word in [
        0x1234_52b7,
        0x0000_1297,
        0x0010_0293,
        0x0013_1293,
        0x0073_02b3,
        0x0000_2003,
        0x0000_2023,
        0x0ff0_000f,
        0x0000_0073,
        0x0010_0073,
    ] {
        let instr = decode(word).unwrap();
        assert_eq!(
            execute_control(&instr, PC, 1, 2),
            Err(NotControl),
            "{word:#010x}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Properties.

/// Addresses and operands, biased toward the boundaries.
fn value() -> impl Strategy<Value = u32> {
    prop_oneof![
        2 => any::<u32>(),
        1 => prop::sample::select(vec![
            0, 1, 2, 3, 4, 0x7fff_ffff, 0x8000_0000, 0xffff_fffc, 0xffff_fffe, u32::MAX,
        ]),
    ]
}

fn any_reg() -> impl Strategy<Value = Reg> {
    (0u8..32).prop_map(x)
}

fn branch_op() -> impl Strategy<Value = BranchOp> {
    prop::sample::select(vec![
        BranchOp::Eq,
        BranchOp::Ne,
        BranchOp::Lt,
        BranchOp::Ge,
        BranchOp::Ltu,
        BranchOp::Geu,
    ])
}

/// An even offset in `-limit..limit`, biased toward small and extreme values.
fn even_offset(limit: i32) -> impl Strategy<Value = i32> {
    prop_oneof![
        2 => (-limit / 2..limit / 2).prop_map(|h| h * 2),
        1 => prop::sample::select(vec![0, 2, -2, 4, -4, 6, limit - 2, limit - 4, -limit]),
    ]
}

fn jalr_offset() -> impl Strategy<Value = i32> {
    prop_oneof![
        2 => -2048i32..2048,
        1 => prop::sample::select(vec![0, 1, 2, 3, 4, -1, -2, -3, -4, 2047, -2048]),
    ]
}

/// Every control-transfer instruction shape, with offsets in the ranges `decode` produces.
fn control_instr() -> impl Strategy<Value = Instr> {
    prop_oneof![
        (branch_op(), any_reg(), any_reg(), even_offset(4096)).prop_map(
            |(op, rs1, rs2, offset)| Instr::Branch {
                op,
                rs1,
                rs2,
                offset
            }
        ),
        (any_reg(), even_offset(1 << 20)).prop_map(|(rd, offset)| Instr::Jal { rd, offset }),
        (any_reg(), any_reg(), jalr_offset()).prop_map(|(rd, rs1, offset)| Instr::Jalr {
            rd,
            rs1,
            offset
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn control_instructions_match_the_oracle(
        instr in control_instr(),
        pc in value(),
        a in value(),
        b in value(),
    ) {
        prop_assert_eq!(execute_control(&instr, pc, a, b), expected(&instr, pc, a, b));
    }

    /// Straight from the decoder: every decoded word, control or not, executes as the
    /// oracle says, and no instruction is both an ALU and a control instruction.
    #[test]
    fn decoded_words_match_the_oracle(
        word in any::<u32>(),
        pc in value(),
        a in value(),
        b in value(),
    ) {
        if let Ok(instr) = decode(word) {
            let control = execute_control(&instr, pc, a, b);
            prop_assert_eq!(control, expected(&instr, pc, a, b));
            prop_assert!(control.is_err() || execute_alu(&instr, pc, a, b).is_err());
        }
    }

    /// Execution reads only the sources the instruction names.
    #[test]
    fn unused_sources_are_ignored(
        instr in control_instr(),
        pc in value(),
        a in value(),
        b in value(),
        other in value(),
    ) {
        let (uses_rs1, uses_rs2) = match instr {
            Instr::Jal { .. } => (false, false),
            Instr::Jalr { .. } => (true, false),
            _ => (true, true),
        };
        let outcome = execute_control(&instr, pc, a, b);
        if !uses_rs1 {
            prop_assert_eq!(execute_control(&instr, pc, other, b), outcome);
        }
        if !uses_rs2 {
            prop_assert_eq!(execute_control(&instr, pc, a, other), outcome);
        }
    }

    /// From an aligned `pc`, a retiring instruction always leaves `pc` aligned, and a trap
    /// never carries a register write (the type has nowhere to put one).
    #[test]
    fn aligned_pc_stays_aligned(
        instr in control_instr(),
        pc in value(),
        a in value(),
        b in value(),
    ) {
        let pc = pc & !3;
        match execute_control(&instr, pc, a, b).unwrap() {
            ExecOutcome::Effect(e) => prop_assert_eq!(e.next_pc % 4, 0),
            ExecOutcome::Trap(t) => {
                prop_assert_eq!(t.cause, TrapCause::InstructionAddressMisaligned);
                prop_assert_ne!(t.tval % 4, 0);
            }
        }
    }

    /// Comparing a value with itself: EQ, GE, and GEU always branch; NE, LT, and LTU
    /// never do. The decision is checked with an aligned offset, and the final outcome
    /// with any offset, where an always-taken branch may trap and a never-taken one never
    /// does.
    #[test]
    fn self_comparisons(v in value(), pc in value(), offset in even_offset(4096)) {
        let pc = pc & !3;
        let aligned = offset & !3;
        for (op, always) in [
            (BranchOp::Eq, true),
            (BranchOp::Ne, false),
            (BranchOp::Lt, false),
            (BranchOp::Ge, true),
            (BranchOp::Ltu, false),
            (BranchOp::Geu, true),
        ] {
            let decision = execute_control(&branch(op, aligned), pc, v, v).unwrap();
            let next_pc = if always { pc.wrapping_add_signed(aligned) } else { pc.wrapping_add(4) };
            prop_assert_eq!(decision, retire(next_pc));

            let outcome = execute_control(&branch(op, offset), pc, v, v).unwrap();
            let target = pc.wrapping_add_signed(offset);
            let want = if !always {
                retire(pc.wrapping_add(4))
            } else if target % 4 == 0 {
                retire(target)
            } else {
                misaligned(target)
            };
            prop_assert_eq!(outcome, want);
        }
    }

    /// `JALR` targets always have bit 0 clear, in effects and traps alike, and two sums
    /// that differ only in bit 0 give the same outcome.
    #[test]
    fn jalr_target_bit_0_is_clear(
        rd in any_reg(),
        rs1 in any_reg(),
        offset in jalr_offset(),
        pc in value(),
        a in value(),
    ) {
        let instr = Instr::Jalr { rd, rs1, offset };
        let outcome = execute_control(&instr, pc, a, 0).unwrap();
        let target = match outcome {
            ExecOutcome::Effect(e) => e.next_pc,
            ExecOutcome::Trap(t) => t.tval,
        };
        prop_assert_eq!(target & 1, 0);

        let sum = a.wrapping_add_signed(offset);
        let twin = (sum ^ 1).wrapping_sub_signed(offset);
        prop_assert_eq!(execute_control(&instr, pc, twin, 0).unwrap(), outcome);
    }
}
