//! `decode` against hand-written machine words and an independent table-driven decoder
//! (`docs/m1-design.md` §5.2).
//!
//! The oracle below shares no code with the production decoder: it finds instructions by
//! scanning a table of `(opcode, funct3, funct7)` patterns and assembles immediates bit by
//! bit from the format diagrams. The hand-written vectors check both of them.

use proptest::prelude::*;
use systemscope_rv32i::immediate::{imm_b, imm_i, imm_j, imm_s, imm_u};
use systemscope_rv32i::{
    BranchOp, Illegal, ImmOp, Instr, LoadOp, Reg, RegOp, ShiftOp, StoreOp, decode,
};

fn x(i: u8) -> Reg {
    Reg::new(i).unwrap()
}

// ---------------------------------------------------------------------------------------
// The oracle.

/// `(immediate bit, word bit)` pairs for each format.
fn map_i() -> Vec<(u32, u32)> {
    (0..12).map(|i| (i, 20 + i)).collect()
}
fn map_s() -> Vec<(u32, u32)> {
    (0..5)
        .map(|i| (i, 7 + i))
        .chain((5..12).map(|i| (i, 20 + i)))
        .collect()
}
fn map_b() -> Vec<(u32, u32)> {
    let mut m: Vec<_> = (1..5).map(|i| (i, 7 + i)).collect();
    m.extend((5..11).map(|i| (i, 20 + i)));
    m.extend([(11, 7), (12, 31)]);
    m
}
fn map_j() -> Vec<(u32, u32)> {
    let mut m: Vec<_> = (1..11).map(|i| (i, 20 + i)).collect();
    m.push((11, 20));
    m.extend((12..20).map(|i| (i, i)));
    m.push((20, 31));
    m
}

/// Gathers the immediate bits and sign-extends from bit `sign`.
fn gather(word: u32, map: &[(u32, u32)], sign: u32) -> i32 {
    let mut v = 0u32;
    for &(imm, bit) in map {
        v |= ((word >> bit) & 1) << imm;
    }
    let unused = 31 - sign;
    ((v << unused) as i32) >> unused
}

fn oi(w: u32) -> i32 {
    gather(w, &map_i(), 11)
}
fn os(w: u32) -> i32 {
    gather(w, &map_s(), 11)
}
fn ob(w: u32) -> i32 {
    gather(w, &map_b(), 12)
}
fn oj(w: u32) -> i32 {
    gather(w, &map_j(), 20)
}
fn ou(w: u32) -> u32 {
    let mut v = 0;
    for bit in 12..32 {
        v |= w & (1 << bit);
    }
    v
}

fn rd(w: u32) -> Reg {
    x(((w >> 7) % 32) as u8)
}
fn rs1(w: u32) -> Reg {
    x(((w >> 15) % 32) as u8)
}
fn rs2(w: u32) -> Reg {
    x(((w >> 20) % 32) as u8)
}

/// How a row matches: by fields, or by the whole word.
enum Pattern {
    Fields {
        opcode: u32,
        funct3: Option<u32>,
        funct7: Option<u32>,
    },
    Word(u32),
}

struct Row {
    name: &'static str,
    pattern: Pattern,
    build: fn(u32) -> Instr,
}

fn row(
    name: &'static str,
    opcode: u32,
    funct3: Option<u32>,
    funct7: Option<u32>,
    build: fn(u32) -> Instr,
) -> Row {
    Row {
        name,
        pattern: Pattern::Fields {
            opcode,
            funct3,
            funct7,
        },
        build,
    }
}

/// The 40 instructions, straight from the RV32I opcode map.
fn table() -> Vec<Row> {
    use Instr::*;
    let (b, l, s, i, sh, r) = (0x63, 0x03, 0x23, 0x13, 0x13, 0x33);
    vec![
        row("LUI", 0x37, None, None, |w| Lui {
            rd: rd(w),
            imm: ou(w),
        }),
        row("AUIPC", 0x17, None, None, |w| Auipc {
            rd: rd(w),
            imm: ou(w),
        }),
        row("JAL", 0x6f, None, None, |w| Jal {
            rd: rd(w),
            offset: oj(w),
        }),
        row("JALR", 0x67, Some(0), None, |w| Jalr {
            rd: rd(w),
            rs1: rs1(w),
            offset: oi(w),
        }),
        row("BEQ", b, Some(0), None, |w| br(BranchOp::Eq, w)),
        row("BNE", b, Some(1), None, |w| br(BranchOp::Ne, w)),
        row("BLT", b, Some(4), None, |w| br(BranchOp::Lt, w)),
        row("BGE", b, Some(5), None, |w| br(BranchOp::Ge, w)),
        row("BLTU", b, Some(6), None, |w| br(BranchOp::Ltu, w)),
        row("BGEU", b, Some(7), None, |w| br(BranchOp::Geu, w)),
        row("LB", l, Some(0), None, |w| ld(LoadOp::B, w)),
        row("LH", l, Some(1), None, |w| ld(LoadOp::H, w)),
        row("LW", l, Some(2), None, |w| ld(LoadOp::W, w)),
        row("LBU", l, Some(4), None, |w| ld(LoadOp::Bu, w)),
        row("LHU", l, Some(5), None, |w| ld(LoadOp::Hu, w)),
        row("SB", s, Some(0), None, |w| st(StoreOp::B, w)),
        row("SH", s, Some(1), None, |w| st(StoreOp::H, w)),
        row("SW", s, Some(2), None, |w| st(StoreOp::W, w)),
        row("ADDI", i, Some(0), None, |w| opi(ImmOp::Addi, w)),
        row("SLTI", i, Some(2), None, |w| opi(ImmOp::Slti, w)),
        row("SLTIU", i, Some(3), None, |w| opi(ImmOp::Sltiu, w)),
        row("XORI", i, Some(4), None, |w| opi(ImmOp::Xori, w)),
        row("ORI", i, Some(6), None, |w| opi(ImmOp::Ori, w)),
        row("ANDI", i, Some(7), None, |w| opi(ImmOp::Andi, w)),
        row("SLLI", sh, Some(1), Some(0x00), |w| shi(ShiftOp::Sll, w)),
        row("SRLI", sh, Some(5), Some(0x00), |w| shi(ShiftOp::Srl, w)),
        row("SRAI", sh, Some(5), Some(0x20), |w| shi(ShiftOp::Sra, w)),
        row("ADD", r, Some(0), Some(0x00), |w| op(RegOp::Add, w)),
        row("SUB", r, Some(0), Some(0x20), |w| op(RegOp::Sub, w)),
        row("SLL", r, Some(1), Some(0x00), |w| op(RegOp::Sll, w)),
        row("SLT", r, Some(2), Some(0x00), |w| op(RegOp::Slt, w)),
        row("SLTU", r, Some(3), Some(0x00), |w| op(RegOp::Sltu, w)),
        row("XOR", r, Some(4), Some(0x00), |w| op(RegOp::Xor, w)),
        row("SRL", r, Some(5), Some(0x00), |w| op(RegOp::Srl, w)),
        row("SRA", r, Some(5), Some(0x20), |w| op(RegOp::Sra, w)),
        row("OR", r, Some(6), Some(0x00), |w| op(RegOp::Or, w)),
        row("AND", r, Some(7), Some(0x00), |w| op(RegOp::And, w)),
        row("FENCE", 0x0f, Some(0), None, |_| Fence),
        Row {
            name: "ECALL",
            pattern: Pattern::Word(0x0000_0073),
            build: |_| Ecall,
        },
        Row {
            name: "EBREAK",
            pattern: Pattern::Word(0x0010_0073),
            build: |_| Ebreak,
        },
    ]
}

fn br(op: BranchOp, w: u32) -> Instr {
    Instr::Branch {
        op,
        rs1: rs1(w),
        rs2: rs2(w),
        offset: ob(w),
    }
}
fn ld(op: LoadOp, w: u32) -> Instr {
    Instr::Load {
        op,
        rd: rd(w),
        rs1: rs1(w),
        offset: oi(w),
    }
}
fn st(op: StoreOp, w: u32) -> Instr {
    Instr::Store {
        op,
        rs1: rs1(w),
        rs2: rs2(w),
        offset: os(w),
    }
}
fn opi(op: ImmOp, w: u32) -> Instr {
    Instr::OpImm {
        op,
        rd: rd(w),
        rs1: rs1(w),
        imm: oi(w),
    }
}
fn shi(op: ShiftOp, w: u32) -> Instr {
    Instr::ShiftImm {
        op,
        rd: rd(w),
        rs1: rs1(w),
        shamt: (w >> 20) % 32,
    }
}
fn op(op: RegOp, w: u32) -> Instr {
    Instr::Op {
        op,
        rd: rd(w),
        rs1: rs1(w),
        rs2: rs2(w),
    }
}

fn matches(p: &Pattern, w: u32) -> bool {
    match *p {
        Pattern::Word(word) => w == word,
        Pattern::Fields {
            opcode,
            funct3,
            funct7,
        } => {
            w % 128 == opcode
                && funct3.is_none_or(|f| (w >> 12) % 8 == f)
                && funct7.is_none_or(|f| w >> 25 == f)
        }
    }
}

/// The row that decodes `w`, if any. At most one row may match.
fn oracle_row(table: &[Row], w: u32) -> Option<&Row> {
    let mut hits = table.iter().filter(|r| matches(&r.pattern, w));
    let hit = hits.next();
    assert!(hits.next().is_none(), "the table is ambiguous at {w:#010x}");
    hit
}

/// What `decode` must return for `w`.
fn oracle(table: &[Row], w: u32) -> Result<Instr, Illegal> {
    oracle_row(table, w)
        .map(|r| (r.build)(w))
        .ok_or(Illegal { word: w })
}

// ---------------------------------------------------------------------------------------
// Hand-written words.

/// One or more words per instruction, each with its operands written out.
fn vectors() -> Vec<(u32, Instr)> {
    use Instr::*;
    let br = |op, a, b, offset| Branch {
        op,
        rs1: x(a),
        rs2: x(b),
        offset,
    };
    let ld = |op, d, a, offset| Load {
        op,
        rd: x(d),
        rs1: x(a),
        offset,
    };
    let st = |op, a, b, offset| Store {
        op,
        rs1: x(a),
        rs2: x(b),
        offset,
    };
    let opi = |op, d, a, imm| OpImm {
        op,
        rd: x(d),
        rs1: x(a),
        imm,
    };
    let shi = |op, d, a, shamt| ShiftImm {
        op,
        rd: x(d),
        rs1: x(a),
        shamt,
    };
    let rr = |op, d, a, b| Op {
        op,
        rd: x(d),
        rs1: x(a),
        rs2: x(b),
    };
    vec![
        (
            0xffff_ffb7,
            Lui {
                rd: x(31),
                imm: 0xffff_f000,
            },
        ),
        (
            0x8000_00b7,
            Lui {
                rd: x(1),
                imm: 0x8000_0000,
            },
        ),
        (
            0x1234_5297,
            Auipc {
                rd: x(5),
                imm: 0x1234_5000,
            },
        ),
        (0x0000_0017, Auipc { rd: x(0), imm: 0 }),
        (
            0xffdf_f0ef,
            Jal {
                rd: x(1),
                offset: -4,
            },
        ),
        (
            0x7fff_f06f,
            Jal {
                rd: x(0),
                offset: 1_048_574,
            },
        ),
        (
            0x8000_0fef,
            Jal {
                rd: x(31),
                offset: -1_048_576,
            },
        ),
        (
            0x54a5_516f,
            Jal {
                rd: x(2),
                offset: 0x5_554a,
            },
        ),
        (
            0x800f_80e7,
            Jalr {
                rd: x(1),
                rs1: x(31),
                offset: -2048,
            },
        ),
        (
            0x7ff0_8067,
            Jalr {
                rd: x(0),
                rs1: x(1),
                offset: 2047,
            },
        ),
        (
            0x0000_8067,
            Jalr {
                rd: x(0),
                rs1: x(1),
                offset: 0,
            },
        ),
        (0x8020_8063, br(BranchOp::Eq, 1, 2, -4096)),
        (0x7e0f_9fe3, br(BranchOp::Ne, 31, 0, 4094)),
        (0xfe41_cfe3, br(BranchOp::Lt, 3, 4, -2)),
        (0x0062_d0e3, br(BranchOp::Ge, 5, 6, 2048)),
        (0x2a83_e5e3, br(BranchOp::Ltu, 7, 8, 0xaaa)),
        (0x00a4_f463, br(BranchOp::Geu, 9, 10, 8)),
        (0xfff1_0083, ld(LoadOp::B, 1, 2, -1)),
        (0x7ff2_1183, ld(LoadOp::H, 3, 4, 2047)),
        (0x800f_af83, ld(LoadOp::W, 31, 31, -2048)),
        (0x5a53_4283, ld(LoadOp::Bu, 5, 6, 0x5a5)),
        (0x0000_5383, ld(LoadOp::Hu, 7, 0, 0)),
        (0xfe20_8fa3, st(StoreOp::B, 1, 2, -1)),
        (0x7fef_9fa3, st(StoreOp::H, 31, 30, 2047)),
        (0x8050_2023, st(StoreOp::W, 0, 5, -2048)),
        (0x8001_0093, opi(ImmOp::Addi, 1, 2, -2048)),
        (0x7ff2_2193, opi(ImmOp::Slti, 3, 4, 2047)),
        (0xfff3_3293, opi(ImmOp::Sltiu, 5, 6, -1)),
        (0x5a54_4393, opi(ImmOp::Xori, 7, 8, 0x5a5)),
        (0xaaa5_6493, opi(ImmOp::Ori, 9, 10, -1366)),
        (0x001f_ff93, opi(ImmOp::Andi, 31, 31, 1)),
        (0x01f1_1093, shi(ShiftOp::Sll, 1, 2, 31)),
        (0x0002_5193, shi(ShiftOp::Srl, 3, 4, 0)),
        (0x41f3_5293, shi(ShiftOp::Sra, 5, 6, 31)),
        (0x4014_5393, shi(ShiftOp::Sra, 7, 8, 1)),
        (0x0031_00b3, rr(RegOp::Add, 1, 2, 3)),
        (0x4062_8233, rr(RegOp::Sub, 4, 5, 6)),
        (0x0094_13b3, rr(RegOp::Sll, 7, 8, 9)),
        (0x00c5_a533, rr(RegOp::Slt, 10, 11, 12)),
        (0x00f7_36b3, rr(RegOp::Sltu, 13, 14, 15)),
        (0x0128_c833, rr(RegOp::Xor, 16, 17, 18)),
        (0x015a_59b3, rr(RegOp::Srl, 19, 20, 21)),
        (0x418b_db33, rr(RegOp::Sra, 22, 23, 24)),
        (0x01bd_6cb3, rr(RegOp::Or, 25, 26, 27)),
        (0x01df_7fb3, rr(RegOp::And, 31, 30, 29)),
        (0x0ff0_000f, Fence),
        (0x0000_0073, Ecall),
        (0x0010_0073, Ebreak),
    ]
}

#[test]
fn every_instruction_decodes_from_hand_written_words() {
    let table = table();
    assert_eq!(table.len(), 40);
    let mut seen = std::collections::BTreeSet::new();
    for (word, expected) in vectors() {
        assert_eq!(decode(word), Ok(expected), "{word:#010x}");
        // The vectors also check the oracle.
        assert_eq!(oracle(&table, word), Ok(expected), "oracle at {word:#010x}");
        seen.insert(oracle_row(&table, word).unwrap().name);
    }
    let all: std::collections::BTreeSet<_> = table.iter().map(|r| r.name).collect();
    assert_eq!(seen, all, "every instruction has a hand-written word");
}

#[test]
fn decoded_immediates_are_the_extractors() {
    for (word, instr) in vectors() {
        let imm = match instr {
            Instr::Lui { imm, .. } | Instr::Auipc { imm, .. } => {
                assert_eq!(imm, imm_u(word));
                continue;
            }
            Instr::Jal { offset, .. } => (offset, imm_j(word)),
            Instr::Branch { offset, .. } => (offset, imm_b(word)),
            Instr::Store { offset, .. } => (offset, imm_s(word)),
            Instr::Jalr { offset, .. } | Instr::Load { offset, .. } => (offset, imm_i(word)),
            Instr::OpImm { imm, .. } => (imm, imm_i(word)),
            _ => continue,
        };
        assert_eq!(imm.0, imm.1, "{word:#010x}");
    }
}

#[test]
fn neighbouring_encodings_are_told_apart() {
    let ok = |w: u32| decode(w).unwrap();
    let rr = |op| Instr::Op {
        op,
        rd: x(1),
        rs1: x(2),
        rs2: x(3),
    };
    // Only funct7 bit 30 separates these pairs.
    assert_eq!(ok(0x0031_00b3), rr(RegOp::Add));
    assert_eq!(ok(0x4031_00b3), rr(RegOp::Sub));
    assert_eq!(ok(0x0031_50b3), rr(RegOp::Srl));
    assert_eq!(ok(0x4031_50b3), rr(RegOp::Sra));
    let shi = |op| Instr::ShiftImm {
        op,
        rd: x(1),
        rs1: x(2),
        shamt: 3,
    };
    assert_eq!(ok(0x0031_5093), shi(ShiftOp::Srl));
    assert_eq!(ok(0x4031_5093), shi(ShiftOp::Sra));
    // Only funct3 bit 12 separates SLT and SLTU.
    assert_eq!(ok(0x0031_20b3), rr(RegOp::Slt));
    assert_eq!(ok(0x0031_30b3), rr(RegOp::Sltu));
    // Every funct3 of LOAD, STORE, and BRANCH.
    let funct3 = |base: u32| (0..8).map(move |f| decode(base | (f << 12)).ok());
    let ld = |op| {
        Some(Instr::Load {
            op,
            rd: x(1),
            rs1: x(2),
            offset: 0,
        })
    };
    assert_eq!(
        funct3(0x0001_0083).collect::<Vec<_>>(),
        [
            ld(LoadOp::B),
            ld(LoadOp::H),
            ld(LoadOp::W),
            None,
            ld(LoadOp::Bu),
            ld(LoadOp::Hu),
            None,
            None
        ]
    );
    let st = |op| {
        Some(Instr::Store {
            op,
            rs1: x(2),
            rs2: x(3),
            offset: 0,
        })
    };
    assert_eq!(
        funct3(0x0031_0023).collect::<Vec<_>>(),
        [
            st(StoreOp::B),
            st(StoreOp::H),
            st(StoreOp::W),
            None,
            None,
            None,
            None,
            None
        ]
    );
    let br = |op| {
        Some(Instr::Branch {
            op,
            rs1: x(2),
            rs2: x(3),
            offset: 0,
        })
    };
    assert_eq!(
        funct3(0x0031_0063).collect::<Vec<_>>(),
        [
            br(BranchOp::Eq),
            br(BranchOp::Ne),
            None,
            None,
            br(BranchOp::Lt),
            br(BranchOp::Ge),
            br(BranchOp::Ltu),
            br(BranchOp::Geu)
        ]
    );
}

#[test]
fn every_fence_configuration_is_a_fence() {
    for word in [
        0x0ff0_000f, // FENCE iorw, iorw
        0x8330_000f, // FENCE.TSO
        0x0100_000f, // PAUSE
        0x0000_000f, // no predecessor or successor set
        0xffff_8f8f, // every field but funct3 set, including reserved fm and rs1/rd
        0x0ff5_850f, // non-zero rs1 and rd
    ] {
        assert_eq!(decode(word), Ok(Instr::Fence), "{word:#010x}");
    }
}

#[test]
fn hints_decode_as_ordinary_instructions() {
    assert_eq!(
        decode(0x0050_0013),
        Ok(Instr::OpImm {
            op: ImmOp::Addi,
            rd: x(0),
            rs1: x(0),
            imm: 5
        })
    );
    assert_eq!(
        decode(0x01ff_8033),
        Ok(Instr::Op {
            op: RegOp::Add,
            rd: x(0),
            rs1: x(31),
            rs2: x(31)
        })
    );
    assert_eq!(
        decode(0x0000_0013),
        Ok(Instr::OpImm {
            op: ImmOp::Addi,
            rd: x(0),
            rs1: x(0),
            imm: 0
        }),
        "NOP"
    );
    assert_eq!(
        decode(0x1234_5037),
        Ok(Instr::Lui {
            rd: x(0),
            imm: 0x1234_5000
        })
    );
    assert_eq!(
        decode(0x0031_6033),
        Ok(Instr::Op {
            op: RegOp::Or,
            rd: x(0),
            rs1: x(2),
            rs2: x(3)
        })
    );
}

#[test]
fn illegal_words_are_rejected_with_the_word() {
    for word in [
        0x0000_0000, // all zero
        0xffff_ffff, // opcode 1111111
        0x0000_0010, // ADDI with low bits 00, 01, 10: 16-bit encodings
        0x0000_0011,
        0x0000_0012,
        0x0000_4501, // a compressed instruction
        0x0000_002f, // AMO
        0x0000_0007, // LOAD-FP
        0x0000_003b, // OP-32
        0x0000_001b, // OP-IMM-32
        0x0000_005b, // custom-2
        0x0000_9067, // JALR with funct3 = 001
        0x0000_3003, // LOAD funct3 011, 110, 111
        0x0000_6003,
        0x0000_7003,
        0x0000_3023, // STORE funct3 011 to 111
        0x0000_4023,
        0x0000_5023,
        0x0000_6023,
        0x0000_7023,
        0x0000_2063, // BRANCH funct3 010, 011
        0x0000_3063,
        0x0200_1013, // SLLI with shamt[5] = 1
        0x4000_1013, // SLLI with imm[11:5] = 0100000
        0xfe00_1013, // SLLI with imm[11:5] = 1111111
        0x0200_5013, // SRLI with shamt[5] = 1
        0x4200_5013, // SRAI with shamt[5] = 1
        0x6000_5013, // SRLI/SRAI with imm[11:5] = 0110000
        0x8000_5013, // imm[11:5] = 1000000
        0x0200_0033, // MUL (funct7 0000001)
        0x4000_1033, // SLL with funct7 0100000
        0x4000_7033, // AND with funct7 0100000
        0x8000_0033, // funct7 1000000
        0x0000_100f, // FENCE.I
        0x0000_200f, // MISC-MEM funct3 010
        0x0000_700f,
        0x3401_1173, // CSRRW sp, mscratch, sp
        0x3000_2573, // CSRRS a0, mstatus, x0
        0xc000_2573, // RDCYCLE a0
        0x0000_5073, // CSRRWI x0, 0, 0
        0x3020_0073, // MRET
        0x1050_0073, // WFI
        0x1020_0073, // SRET
        0x0000_00f3, // ECALL with rd = x1
        0x0000_8073, // ECALL with rs1 = x1
        0x0010_8073, // EBREAK with rs1 = x1
        0x0020_0073, // SYSTEM funct12 = 2
    ] {
        assert_eq!(decode(word), Err(Illegal { word }), "{word:#010x}");
    }
}

/// Every `(opcode, funct3, funct7)` combination, under several settings of the register
/// fields, decodes exactly as the oracle says. The fillers set each register field alone,
/// so `SYSTEM` words one field away from `ECALL` are covered, and include `rs2 = 1`, so
/// `EBREAK` is reached.
#[test]
fn exhaustive_over_opcode_funct3_and_funct7() {
    const REGS: u32 = 0x01ff_8f80;
    let table = table();
    let fillers = [
        0,
        0x0000_0080, // rd = 1
        0x0000_8000, // rs1 = 1
        0x0010_0000, // rs2 = 1
        REGS,
        0x0123_4567 & REGS,
        0x0089_abcd & REGS,
    ];
    let mut legal = 0;
    for opcode in 0..128 {
        for funct3 in 0..8 {
            for funct7 in 0..128 {
                for filler in fillers {
                    let word = (funct7 << 25) | filler | (funct3 << 12) | opcode;
                    let expected = oracle(&table, word);
                    assert_eq!(decode(word), expected, "{word:#010x}");
                    legal += usize::from(expected.is_ok());
                }
            }
        }
    }
    assert!(legal > 0);
}

/// Any word with a known opcode, and its other bits random.
fn with_known_opcode() -> impl Strategy<Value = u32> {
    let opcodes = [
        0x37, 0x17, 0x6f, 0x67, 0x63, 0x03, 0x23, 0x13, 0x33, 0x0f, 0x73,
    ];
    (any::<u32>(), prop::sample::select(opcodes.to_vec())).prop_map(|(w, op)| (w & !0x7f) | op)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn any_word_decodes_as_the_oracle_says(word in any::<u32>()) {
        prop_assert_eq!(decode(word), oracle(&table(), word));
    }

    #[test]
    fn any_word_with_a_known_opcode_decodes_as_the_oracle_says(word in with_known_opcode()) {
        prop_assert_eq!(decode(word), oracle(&table(), word));
    }

    #[test]
    fn system_words_other_than_ecall_and_ebreak_are_illegal(high in any::<u32>()) {
        let word = (high & !0x7f) | 0x73;
        let expected = match word {
            0x0000_0073 => Ok(Instr::Ecall),
            0x0010_0073 => Ok(Instr::Ebreak),
            _ => Err(Illegal { word }),
        };
        prop_assert_eq!(decode(word), expected);
    }

    #[test]
    fn extractors_match_the_bit_maps(word in any::<u32>()) {
        prop_assert_eq!(imm_i(word), oi(word));
        prop_assert_eq!(imm_s(word), os(word));
        prop_assert_eq!(imm_b(word), ob(word));
        prop_assert_eq!(imm_j(word), oj(word));
        prop_assert_eq!(imm_u(word), ou(word));
    }
}
