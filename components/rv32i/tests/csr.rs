//! The M2 privileged subset, pure (`docs/m2-design.md` §4, §5.3): Zicsr decoding and
//! legality, each whitelisted CSR's read and write rule, and `MRET`, against an oracle
//! written from the design's tables that shares no code with the crate.

use proptest::prelude::*;
use systemscope_rv32i::csr::{
    self, CsrFile, CsrOp, CsrSource, MCAUSE, MEPC, MIE, MIP, MRET, MSCRATCH, MSTATUS, MTVAL, MTVEC,
    PrivInstr, decode_privileged, is_supported,
};
use systemscope_rv32i::{Reg, decode};

/// The whitelist, from §4.3.
const WHITELIST: [u16; 8] = [0x300, 0x304, 0x305, 0x340, 0x341, 0x342, 0x343, 0x344];

/// CSRs real hardware (and Spike) implement that M2 does not (§4.5).
const UNSUPPORTED: [u16; 22] = [
    0x301, 0xf14, 0xf11, 0xf12, 0xf13, 0xf15, 0x310, 0x302, 0x303, 0x306, 0xb00, 0xb02, 0xc00,
    0xc01, 0xc02, 0x180, 0x3a0, 0x3b0, 0x7c0, 0x7a5, 0x320, 0x000,
];

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}

/// What the oracle reads from a word: `(funct3, rd, rs1/uimm field, csr)` for the six
/// Zicsr forms, `Mret`, or nothing.
#[derive(Debug, PartialEq, Eq)]
enum OracleInstr {
    Csr(u32, u32, u32, u16),
    Mret,
}

fn oracle_decode(word: u32) -> Option<OracleInstr> {
    if word == 0x3020_0073 {
        return Some(OracleInstr::Mret);
    }
    let funct3 = word >> 12 & 7;
    if word & 0x7f != 0x73 || funct3 == 0 || funct3 == 4 {
        return None;
    }
    Some(OracleInstr::Csr(
        funct3,
        word >> 7 & 31,
        word >> 15 & 31,
        (word >> 20) as u16,
    ))
}

fn as_oracle(instr: PrivInstr) -> OracleInstr {
    match instr {
        PrivInstr::Mret => OracleInstr::Mret,
        PrivInstr::Csr { op, rd, src, csr } => {
            let (imm, field) = match src {
                CsrSource::Reg(r) => (0, u32::from(r.index())),
                CsrSource::Imm(u) => (4, u32::from(u)),
            };
            let op = match op {
                CsrOp::Write => 1,
                CsrOp::Set => 2,
                CsrOp::Clear => 3,
            };
            OracleInstr::Csr(imm | op, u32::from(rd.index()), field, csr)
        }
    }
}

/// The CSRs as software reads them, the oracle's whole state.
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

const RESET: Model = Model {
    mstatus: 0x1800,
    mie: 0,
    mip: 0,
    mtvec: 0,
    mscratch: 0,
    mepc: 0,
    mcause: 0,
    mtval: 0,
};

fn model_read(m: &Model, csr: u16) -> Option<u32> {
    Some(match csr {
        0x300 => m.mstatus,
        0x304 => m.mie,
        0x344 => m.mip,
        0x305 => m.mtvec,
        0x340 => m.mscratch,
        0x341 => m.mepc,
        0x342 => m.mcause,
        0x343 => m.mtval,
        _ => return None,
    })
}

/// §4.3's "Write (value `v`)" column.
fn model_write(m: &mut Model, csr: u16, v: u32) {
    match csr {
        0x300 => m.mstatus = 0x1800 | (v & 0x0000_0088),
        0x304 => m.mie = v & 0x0000_0800,
        0x344 => {}
        0x305 => m.mtvec = v & 0xffff_fffc,
        0x340 => m.mscratch = v,
        0x341 => m.mepc = v & 0xffff_fffc,
        0x342 => m.mcause = v,
        0x343 => m.mtval = v,
        _ => panic!("unsupported {csr:#x}"),
    }
}

/// §5.3: MIE ← MPIE, MPIE ← 1, MPP stays 0b11.
fn model_mret(m: &mut Model) -> u32 {
    let mpie = m.mstatus >> 7 & 1;
    m.mstatus = 0x1800 | mpie << 3 | 0x80;
    m.mepc
}

/// The crate's CSR file holding the oracle's state.
fn file_of(m: &Model) -> CsrFile {
    CsrFile {
        mie: m.mstatus & 8 != 0,
        mpie: m.mstatus & 0x80 != 0,
        meie: m.mie != 0,
        mtvec: m.mtvec,
        mscratch: m.mscratch,
        mepc: m.mepc,
        mcause: m.mcause,
        mtval: m.mtval,
        irq_level: m.mip != 0,
    }
}

fn model_of(f: &CsrFile) -> Model {
    let r = |csr| f.read(csr).unwrap();
    Model {
        mstatus: r(MSTATUS),
        mie: r(MIE),
        mip: r(MIP),
        mtvec: r(MTVEC),
        mscratch: r(MSCRATCH),
        mepc: r(MEPC),
        mcause: r(MCAUSE),
        mtval: r(MTVAL),
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

fn any_csr() -> impl Strategy<Value = u16> {
    prop_oneof![
        3 => proptest::sample::select(WHITELIST.to_vec()),
        1 => proptest::sample::select(UNSUPPORTED.to_vec()),
        1 => 0u16..0x1000,
    ]
}

/// A Zicsr word: any form, registers, and CSR.
fn any_csr_word() -> impl Strategy<Value = u32> {
    (
        proptest::sample::select(vec![1u32, 2, 3, 5, 6, 7]),
        0u32..32,
        0u32..32,
        any_csr(),
    )
        .prop_map(|(f3, rd, field, csr)| csr_word(f3, rd, field, csr))
}

// ---------------------------------------------------------------------------------------
// Decoding and legality (§4.2, §4.5).

#[test]
fn the_six_forms_decode_with_their_fields() {
    let x = |i| Reg::new(i).unwrap();
    let cases = [
        (1, CsrOp::Write, false),
        (2, CsrOp::Set, false),
        (3, CsrOp::Clear, false),
        (5, CsrOp::Write, true),
        (6, CsrOp::Set, true),
        (7, CsrOp::Clear, true),
    ];
    for (funct3, op, imm) in cases {
        let word = csr_word(funct3, 7, 19, 0x340);
        let src = if imm {
            CsrSource::Imm(19)
        } else {
            CsrSource::Reg(x(19))
        };
        assert_eq!(
            decode_privileged(word),
            Some(PrivInstr::Csr {
                op,
                rd: x(7),
                src,
                csr: 0x340
            }),
            "funct3 {funct3}"
        );
    }
    assert_eq!(decode_privileged(0x3020_0073), Some(PrivInstr::Mret));
}

#[test]
fn every_other_system_encoding_stays_illegal() {
    for word in [
        0x1050_0073u32, // WFI
        0x1020_0073,    // SRET
        0x0020_0073,    // URET
        0x1200_0073,    // SFENCE.VMA x0, x0
        0x1230_0073,    // SFENCE.VMA x0, x3
        0x7b20_0073,    // DRET
        0x3020_0173,    // MRET with rs1 = 2
        0x3020_00f3,    // MRET with rd = 1
        0x3030_0073,    // MRET with rs2 = 3
        csr_word(4, 1, 2, 0x340),
        csr_word(4, 0, 0, 0x300),
        0x0000_0073, // ECALL: M1's, not a privileged addition
        0x0010_0073, // EBREAK
    ] {
        assert_eq!(decode_privileged(word), None, "{word:#010x}");
    }
    // ECALL and EBREAK keep their M1 decoding.
    assert!(decode(0x0000_0073).is_ok() && decode(0x0010_0073).is_ok());
}

#[test]
fn the_whitelist_is_exactly_eight_csrs() {
    for csr in 0..0x1000u16 {
        assert_eq!(is_supported(csr), WHITELIST.contains(&csr), "{csr:#x}");
    }
    assert_eq!(csr::SUPPORTED.len(), 8);
}

#[test]
fn unsupported_csrs_read_and_write_nothing() {
    let mut f = CsrFile::new();
    for csr in UNSUPPORTED {
        assert_eq!(f.read(csr), None, "{csr:#x}");
        assert_eq!(f.write(csr, u32::MAX), None, "{csr:#x}");
        assert_eq!(f, CsrFile::new());
    }
}

#[test]
fn write_suppression_depends_only_on_the_form_and_the_rs1_field() {
    for (funct3, field, writes) in [
        (1, 0, true),
        (1, 5, true),
        (5, 0, true),
        (5, 5, true),
        (2, 0, false),
        (2, 5, true),
        (3, 0, false),
        (3, 5, true),
        (6, 0, false),
        (6, 1, true),
        (6, 31, true),
        (7, 0, false),
        (7, 1, true),
        (7, 31, true),
    ] {
        for rd in [0, 9] {
            let instr = decode_privileged(csr_word(funct3, rd, field, 0x340)).unwrap();
            assert_eq!(
                instr.writes(),
                writes,
                "funct3 {funct3} field {field} rd {rd}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn any_word_decodes_as_the_oracle_says(word in any::<u32>()) {
        prop_assert_eq!(decode_privileged(word).map(as_oracle), oracle_decode(word));
    }

    #[test]
    fn any_system_word_decodes_as_the_oracle_says(high in any::<u32>()) {
        let word = high << 7 | 0x73;
        prop_assert_eq!(decode_privileged(word).map(as_oracle), oracle_decode(word));
    }

    /// The M1 decoder is untouched: every Zicsr word and `MRET` stay illegal there.
    #[test]
    fn m1_decoding_rejects_every_privileged_addition(word in any_csr_word()) {
        prop_assert!(decode(word).is_err());
        prop_assert!(decode(MRET).is_err());
    }
}

// ---------------------------------------------------------------------------------------
// Per-CSR rules (§4.3, §4.6).

#[test]
fn reset_values_are_mstatus_0x1800_and_zero() {
    let f = CsrFile::new();
    assert_eq!(model_of(&f), RESET);
    assert_eq!(CsrFile::default(), f);
}

#[test]
fn mstatus_keeps_mie_and_mpie_and_hardwires_mpp() {
    let mut f = CsrFile::new();
    for (write, read) in [
        (0xffff_ffff, 0x1888),
        (0x0000_0000, 0x1800),
        (0x0000_0008, 0x1808),
        (0x0000_0080, 0x1880),
        (0x0000_1800, 0x1800),
        (0xffff_e777, 0x1800),
        (0x0000_0088, 0x1888),
    ] {
        f.write(MSTATUS, write).unwrap();
        assert_eq!(f.read(MSTATUS), Some(read), "{write:#x}");
    }
}

#[test]
fn mie_keeps_only_meie() {
    let mut f = CsrFile::new();
    for (write, read) in [
        (0xffff_ffff, 0x800),
        (0x0000_0888, 0x800),
        (0xffff_f7ff, 0),
        (0x0000_0800, 0x800),
        (0, 0),
    ] {
        f.write(MIE, write).unwrap();
        assert_eq!(f.read(MIE), Some(read), "{write:#x}");
    }
}

#[test]
fn mip_reads_the_irq_level_and_ignores_writes() {
    for level in [false, true] {
        let mut f = CsrFile {
            irq_level: level,
            ..CsrFile::new()
        };
        for write in [0, 0x800, u32::MAX] {
            f.write(MIP, write).unwrap();
            assert_eq!(f.read(MIP), Some(u32::from(level) << 11));
            assert_eq!(f.irq_level, level);
        }
    }
}

#[test]
fn mtvec_clears_mode_in_every_mode() {
    let mut f = CsrFile::new();
    for (write, read) in [
        (0x8000_1000, 0x8000_1000), // MODE 0
        (0x8000_1001, 0x8000_1000), // MODE 1: SystemScope-only, Spike keeps it
        (0x8000_1002, 0x8000_1000), // MODE 2
        (0x8000_1003, 0x8000_1000), // MODE 3: SystemScope-only
        (0x8000_1004, 0x8000_1004), // BASE only 4-byte aligned
        (0x8000_1005, 0x8000_1004),
        (0xffff_ffff, 0xffff_fffc),
        (0, 0),
    ] {
        f.write(MTVEC, write).unwrap();
        assert_eq!(f.read(MTVEC), Some(read), "{write:#x}");
    }
}

#[test]
fn mepc_is_always_aligned() {
    let mut f = CsrFile::new();
    for (write, read) in [
        (0xffff_ffff, 0xffff_fffc),
        (0x8000_0002, 0x8000_0000),
        (0x8000_0001, 0x8000_0000),
        (0x8000_0004, 0x8000_0004),
    ] {
        f.write(MEPC, write).unwrap();
        assert_eq!(f.read(MEPC), Some(read), "{write:#x}");
    }
}

#[test]
fn mscratch_mcause_and_mtval_keep_every_bit() {
    let mut f = CsrFile::new();
    for csr in [MSCRATCH, MCAUSE, MTVAL] {
        for v in [0xffff_ffff, 0x8000_000b, 0x1234_5678, 0x0000_0003, 0] {
            f.write(csr, v).unwrap();
            assert_eq!(f.read(csr), Some(v), "{csr:#x} {v:#x}");
        }
    }
}

#[test]
fn mret_restores_mie_from_mpie() {
    for (before, after) in [
        (0x1880, 0x1888),
        (0x1808, 0x1880),
        (0x1888, 0x1888),
        (0x1800, 0x1880),
    ] {
        let mut f = CsrFile::new();
        f.write(MSTATUS, before).unwrap();
        f.write(MEPC, 0x8000_0124).unwrap();
        let unchanged = CsrFile {
            mie: false,
            mpie: false,
            ..f
        };
        assert_eq!(f.mret(), 0x8000_0124);
        assert_eq!(f.read(MSTATUS), Some(after), "{before:#x}");
        // Nothing else changes.
        assert_eq!(
            CsrFile {
                mie: false,
                mpie: false,
                ..f
            },
            unchanged
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    /// Any CSR operation on any state reads, stores, and returns what the oracle says.
    #[test]
    fn csr_operations_match_the_oracle(
        m in any_model(),
        word in any_csr_word(),
        rs1_value in any::<u32>(),
    ) {
        let Some(OracleInstr::Csr(funct3, _, field, csr)) = oracle_decode(word) else {
            unreachable!("a Zicsr word");
        };
        let instr = decode_privileged(word).unwrap();
        let PrivInstr::Csr { op, src, .. } = instr else { unreachable!() };
        let mut f = file_of(&m);
        prop_assert_eq!(model_of(&f), m);
        if !WHITELIST.contains(&csr) {
            prop_assert!(!is_supported(csr));
            prop_assert_eq!(f.read(csr), None);
            return Ok(());
        }
        let operand = match src {
            CsrSource::Reg(_) => rs1_value,
            CsrSource::Imm(u) => u32::from(u),
        };
        let old = f.read(csr).unwrap();
        prop_assert_eq!(Some(old), model_read(&m, csr));
        if instr.writes() {
            f.write(csr, op.apply(old, operand)).unwrap();
        }

        let mut expected = m;
        let oracle_operand = if funct3 >= 5 { field } else { rs1_value };
        let oracle_writes = funct3 & 3 == 1 || field != 0;
        if oracle_writes {
            let new = match funct3 & 3 {
                1 => oracle_operand,
                2 => old | oracle_operand,
                _ => old & !oracle_operand,
            };
            model_write(&mut expected, csr, new);
        }
        prop_assert_eq!(model_of(&f), expected);
    }

    #[test]
    fn mret_matches_the_oracle(m in any_model()) {
        let mut f = file_of(&m);
        let mut expected = m;
        prop_assert_eq!(f.mret(), model_mret(&mut expected));
        prop_assert_eq!(model_of(&f), expected);
    }
}
