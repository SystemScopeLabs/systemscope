//! Immediate extraction, the only place immediate bits are assembled
//! (`docs/m1-design.md` §5.2).
//!
//! Each function takes the whole instruction word and ignores every bit outside its
//! format's immediate fields. Signed immediates come back sign-extended to `i32`; branch
//! and jump offsets are byte offsets with bit 0 clear.

/// The I-type immediate: `word[31:20]`, a 12-bit signed value in `-2048..2048`.
pub const fn imm_i(word: u32) -> i32 {
    (word as i32) >> 20
}

/// The S-type immediate: `word[31:25]` then `word[11:7]`, a 12-bit signed value in
/// `-2048..2048`.
pub const fn imm_s(word: u32) -> i32 {
    ((word as i32) >> 25 << 5) | ((word >> 7) & 0x1f) as i32
}

/// The B-type immediate: a 13-bit signed, even byte offset in `-4096..4096`.
///
/// `imm[12]` is `word[31]`, `imm[11]` is `word[7]`, `imm[10:5]` is `word[30:25]`, and
/// `imm[4:1]` is `word[11:8]`.
pub const fn imm_b(word: u32) -> i32 {
    ((word as i32) >> 31 << 12)
        | (((word >> 7) & 0x1) << 11) as i32
        | (((word >> 25) & 0x3f) << 5) as i32
        | (((word >> 8) & 0xf) << 1) as i32
}

/// The U-type immediate: `word[31:12]` in place, with the low 12 bits zero.
pub const fn imm_u(word: u32) -> u32 {
    word & 0xffff_f000
}

/// The J-type immediate: a 21-bit signed, even byte offset in `-2^20..2^20`.
///
/// `imm[20]` is `word[31]`, `imm[19:12]` is `word[19:12]`, `imm[11]` is `word[20]`, and
/// `imm[10:1]` is `word[30:21]`.
pub const fn imm_j(word: u32) -> i32 {
    ((word as i32) >> 31 << 20)
        | (word & 0x000f_f000) as i32
        | (((word >> 20) & 0x1) << 11) as i32
        | (((word >> 21) & 0x3ff) << 1) as i32
}
