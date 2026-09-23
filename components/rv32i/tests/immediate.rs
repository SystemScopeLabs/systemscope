//! Immediate extraction against hand-computed vectors (`docs/m1-design.md` §5.2).
//!
//! Every word below is written from the instruction-format diagrams, not computed by the
//! extractors. Words whose immediate is zero have every other bit set, so a mask that
//! leaks a neighbouring field shows up.

use systemscope_rv32i::immediate::{imm_b, imm_i, imm_j, imm_s, imm_u};

#[test]
fn i_type_is_12_bit_signed() {
    assert_eq!(imm_i(0x000f_ffff), 0, "zero, every other bit set");
    assert_eq!(imm_i(0x7ff0_0000), 2047, "maximum");
    assert_eq!(imm_i(0xfff0_0000), -1);
    assert_eq!(imm_i(0x8000_0000), -2048, "minimum");
    assert_eq!(imm_i(0x5a50_0000), 0x5a5);
    assert_eq!(imm_i(0xaaa0_0000), -1366, "0xaaa sign-extended");
    assert_eq!(imm_i(0x0010_0000), 1, "imm[0] is word[20]");
}

#[test]
fn s_type_is_12_bit_signed_from_two_fields() {
    // imm[11:5] = word[31:25], imm[4:0] = word[11:7].
    assert_eq!(imm_s(0x01ff_f07f), 0, "zero, every other bit set");
    assert_eq!(imm_s(0x7e00_0f80), 2047, "maximum");
    assert_eq!(imm_s(0xfe00_0f80), -1);
    assert_eq!(imm_s(0x8000_0000), -2048, "minimum");
    assert_eq!(imm_s(0x5a00_0280), 0x5a5, "0b1011010 then 0b00101");
    assert_eq!(imm_s(0x0000_0080), 1, "imm[0] is word[7]");
    assert_eq!(imm_s(0x0000_0800), 16, "imm[4] is word[11]");
    assert_eq!(imm_s(0x0200_0000), 32, "imm[5] is word[25]");
}

#[test]
fn b_type_is_13_bit_signed_and_even() {
    // imm[12] = word[31], imm[10:5] = word[30:25], imm[4:1] = word[11:8], imm[11] = word[7].
    assert_eq!(imm_b(0x01ff_f07f), 0, "zero, every other bit set");
    assert_eq!(imm_b(0x7e00_0f80), 4094, "maximum");
    assert_eq!(
        imm_b(0xfe00_0f80),
        -2,
        "the negative offset closest to zero"
    );
    assert_eq!(imm_b(0x8000_0000), -4096, "minimum");
    // One word bit at a time catches a scrambled field.
    assert_eq!(imm_b(0x0000_0100), 2, "imm[1] is word[8]");
    assert_eq!(imm_b(0x0000_0800), 16, "imm[4] is word[11]");
    assert_eq!(imm_b(0x0200_0000), 32, "imm[5] is word[25]");
    assert_eq!(imm_b(0x4000_0000), 1024, "imm[10] is word[30]");
    assert_eq!(imm_b(0x0000_0080), 2048, "imm[11] is word[7]");
    assert_eq!(imm_b(0x2a00_0580), 0xaaa, "mixed");
    // Bit 0 is never set: word[7] is imm[11], not imm[0].
    for word in [0x0000_0080, 0xffff_ffff, 0x1234_5678] {
        assert_eq!(imm_b(word) & 1, 0, "{word:#x}");
    }
}

#[test]
fn u_type_keeps_the_upper_20_bits_in_place() {
    assert_eq!(imm_u(0x0000_0fff), 0, "zero, every other bit set");
    assert_eq!(imm_u(0x7fff_f000), 0x7fff_f000, "largest positive");
    assert_eq!(imm_u(0xffff_ffff), 0xffff_f000, "-4096 as bits");
    assert_eq!(imm_u(0x8000_0000), 0x8000_0000, "most negative");
    assert_eq!(imm_u(0x1234_5abc), 0x1234_5000);
    assert_eq!(imm_u(0x0000_1000), 0x1000, "imm[12] is word[12]");
}

#[test]
fn j_type_is_21_bit_signed_and_even() {
    // imm[20] = word[31], imm[10:1] = word[30:21], imm[11] = word[20], imm[19:12] = word[19:12].
    assert_eq!(imm_j(0x0000_0fff), 0, "zero, every other bit set");
    assert_eq!(imm_j(0x7fff_f000), 1_048_574, "maximum");
    assert_eq!(
        imm_j(0xffff_f000),
        -2,
        "the negative offset closest to zero"
    );
    assert_eq!(imm_j(0x8000_0000), -1_048_576, "minimum");
    assert_eq!(imm_j(0x0020_0000), 2, "imm[1] is word[21]");
    assert_eq!(imm_j(0x4000_0000), 1024, "imm[10] is word[30]");
    assert_eq!(imm_j(0x0010_0000), 2048, "imm[11] is word[20]");
    assert_eq!(imm_j(0x0000_1000), 4096, "imm[12] is word[12]");
    assert_eq!(imm_j(0x0008_0000), 0x8_0000, "imm[19] is word[19]");
    assert_eq!(imm_j(0xffdf_f000), -4, "JAL ra, -4 (0xffdff0ef)");
    assert_eq!(imm_j(0x54a5_5000), 0x5_554a, "mixed");
}
