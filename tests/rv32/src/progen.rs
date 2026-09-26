//! Deterministic RV32I programs for the Spike differential (`docs/m1-design.md` §10.3).
//!
//! ```text
//! seed ──derive_key("SystemScope 2026-09 rv32 progen v1")──▶ xoshiro256** ──▶ RV32I words ──▶ ELF32
//! ```
//!
//! [`generate`] writes machine code and a minimal ELF directly, with no assembler, so a
//! program is a function of its seed and nothing else:
//!
//! - The stream is xoshiro256\*\* seeded as in m0-design §5.2, with its own context
//!   ([`PROGEN_CONTEXT`]) and the path [`PROGEN_PATH`].
//! - The code is straight-line groups of instructions. Branches and jumps only go
//!   forward, to the start of a later group, so every program terminates.
//! - A load or store first sets its base register with `LUI` and `ADDI`, then accesses an
//!   address aligned to its width inside the data window, so it never traps.
//! - Every program ends with the §10.2 pass sequence: `gp = 1`, `a7 = 93`, `a0 = 0`, the
//!   `write_tohost` stores, then `ECALL`.
//!
//! [`misaligned`] builds the dedicated trap programs: each ends in one load or store
//! whose address is not aligned to its width. SystemScope traps on it (§6), and the
//! differential requires the pinned Spike to trap at the same instruction too.
//!
//! Changing what a seed produces changes every program: it needs a new
//! [`PROGEN_CONTEXT`], and [`FIXED_SEEDS_DIGEST`] changes with it.

use systemscope_contracts::rng::SimRng;
use systemscope_runtime::rng::{Xoshiro256StarStar, seed_material};

use crate::RAM_BASE;

/// BLAKE3 `derive_key` context for program seeds, in m0-design §5.2's form.
pub const PROGEN_CONTEXT: &str = "SystemScope 2026-09 rv32 progen v1";
/// The path in the seed material (§5.2), after the seed.
pub const PROGEN_PATH: &str = "rv32.progen";
/// The seeds every CI run compares with Spike.
pub const FIXED_SEEDS: [u64; 64] = {
    let mut seeds = [0; 64];
    let mut i = 0;
    while i < 64 {
        seeds[i] = i as u64;
        i += 1;
    }
    seeds
};
/// BLAKE3 over the ELF BLAKE3s of [`FIXED_SEEDS`], in order. A test pins it, so the
/// generator cannot change what a seed means unnoticed.
pub const FIXED_SEEDS_DIGEST: &str =
    "d37b5894b713b2da83ad96fa5d30c2dce468f4830b05f7a6280a2b2e6c4a396f";
/// Environment variable holding the nightly seed, in decimal or `0x` hex.
pub const SEED_VAR: &str = "M1_PROGEN_SEED";

/// Instruction groups per program, before the pass sequence.
pub const GROUPS: usize = 256;
/// Where the code starts: the entry, the RAM base.
pub const CODE_BASE: u32 = RAM_BASE;
/// The data segment: `tohost`, `fromhost`, then the data window.
pub const DATA_BASE: u32 = RAM_BASE + 0x1_0000;
/// The `tohost` symbol Spike's HTIF watches.
pub const TOHOST: u32 = DATA_BASE;
/// The `fromhost` symbol.
pub const FROMHOST: u32 = DATA_BASE + 0x40;
/// The data window every load and store accesses.
pub const WINDOW: u32 = DATA_BASE + 0x100;
/// The data window's size in bytes. Its initial bytes are random.
pub const WINDOW_SIZE: u32 = 0x100;

/// The stream for `seed`: `derive_key(PROGEN_CONTEXT, seed_material(seed, PROGEN_PATH))`
/// as the xoshiro256** state, exactly as m0-design §5.2 seeds a component.
fn stream(seed: u64) -> Xoshiro256StarStar {
    let key = blake3::derive_key(PROGEN_CONTEXT, &seed_material(seed, PROGEN_PATH));
    Xoshiro256StarStar::from_seed_bytes(key)
}

/// Draws for one program.
struct Draw(Xoshiro256StarStar);

impl Draw {
    /// A value in `0..n`. The modulo bias is irrelevant here, and the rule is fixed.
    fn below(&mut self, n: u32) -> u32 {
        (self.0.next_u64() % u64::from(n)) as u32
    }

    /// Any register, `x0` included.
    fn reg(&mut self) -> u8 {
        self.below(32) as u8
    }

    /// A register other than `x0`.
    fn reg_nonzero(&mut self) -> u8 {
        1 + self.below(31) as u8
    }

    /// A 12-bit signed immediate, one time in four from the boundary values.
    fn imm12(&mut self) -> i32 {
        const EDGES: [i32; 8] = [0, 1, -1, 2, 2047, -2048, 0x555, -0x556];
        if self.below(4) == 0 {
            EDGES[self.below(8) as usize]
        } else {
            self.below(4096) as i32 - 2048
        }
    }

    /// A 20-bit upper immediate, one time in four from the boundary values.
    fn imm20(&mut self) -> u32 {
        const EDGES: [u32; 6] = [0, 1, 0x7ffff, 0x80000, 0xfffff, 0x80001];
        if self.below(4) == 0 {
            EDGES[self.below(6) as usize]
        } else {
            self.below(1 << 20)
        }
    }
}

// Encoders, RV32I base formats.
const OP: u32 = 0x33;
pub(crate) const OP_IMM: u32 = 0x13;
pub(crate) const LOAD: u32 = 0x03;
const STORE: u32 = 0x23;
const BRANCH: u32 = 0x63;
const JAL: u32 = 0x6f;
pub(crate) const JALR: u32 = 0x67;
const LUI: u32 = 0x37;
const AUIPC: u32 = 0x17;
const MISC_MEM: u32 = 0x0f;
pub(crate) const SYSTEM: u32 = 0x73;

fn r(funct7: u32, rs2: u8, rs1: u8, funct3: u32, rd: u8) -> u32 {
    funct7 << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | u32::from(rd) << 7
        | OP
}

pub(crate) fn i(opcode: u32, funct3: u32, rd: u8, rs1: u8, imm: i32) -> u32 {
    (imm as u32 & 0xfff) << 20 | u32::from(rs1) << 15 | funct3 << 12 | u32::from(rd) << 7 | opcode
}

pub(crate) fn s(funct3: u32, rs1: u8, rs2: u8, imm: i32) -> u32 {
    let imm = imm as u32;
    (imm >> 5 & 0x7f) << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | (imm & 0x1f) << 7
        | STORE
}

fn b(funct3: u32, rs1: u8, rs2: u8, offset: u32) -> u32 {
    (offset >> 12 & 1) << 31
        | (offset >> 5 & 0x3f) << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | (offset >> 1 & 0xf) << 8
        | (offset >> 11 & 1) << 7
        | BRANCH
}

fn j(rd: u8, offset: u32) -> u32 {
    (offset >> 20 & 1) << 31
        | (offset >> 1 & 0x3ff) << 21
        | (offset >> 11 & 1) << 20
        | (offset >> 12 & 0xff) << 12
        | u32::from(rd) << 7
        | JAL
}

fn u(opcode: u32, rd: u8, imm20: u32) -> u32 {
    imm20 << 12 | u32::from(rd) << 7 | opcode
}

/// `LUI` and `ADDI` that leave `value` in `rd`.
pub(crate) fn li(rd: u8, value: u32) -> [u32; 2] {
    let lo = ((value & 0xfff) as i32) << 20 >> 20;
    let hi = value.wrapping_sub(lo as u32) >> 12;
    [u(LUI, rd, hi), i(OP_IMM, 0, rd, rd, lo)]
}

/// `AUIPC` at `pc` and the low part that reach `target` from it.
fn pcrel(pc: u32, target: u32) -> (u32, i32) {
    let delta = target.wrapping_sub(pc);
    let lo = ((delta & 0xfff) as i32) << 20 >> 20;
    (delta.wrapping_sub(lo as u32) >> 12, lo)
}

/// One group of instructions: a branch or jump target is always a group's first word.
enum Group {
    /// Words that need no layout.
    Plain(Vec<u32>),
    /// A conditional branch `funct3` to the start of the group `skip` groups ahead.
    Branch {
        funct3: u32,
        rs1: u8,
        rs2: u8,
        skip: usize,
    },
    /// `JAL rd` to the group `skip` groups ahead.
    Jal { rd: u8, skip: usize },
    /// `AUIPC base, 0`, then `JALR rd, base` to the group `skip` groups ahead, with bit 0
    /// of the sum set if `odd` (JALR clears it).
    Jalr {
        base: u8,
        rd: u8,
        skip: usize,
        odd: bool,
    },
}

impl Group {
    fn len(&self) -> u32 {
        match self {
            Group::Plain(words) => words.len() as u32,
            Group::Branch { .. } | Group::Jal { .. } => 1,
            Group::Jalr { .. } => 2,
        }
    }
}

fn group(d: &mut Draw) -> Group {
    let roll = d.below(100);
    match roll {
        // Register-register ALU: funct3 and funct7 of the ten R-type operations.
        0..25 => {
            const OPS: [(u32, u32); 10] = [
                (0, 0),
                (0x20, 0),
                (0, 1),
                (0, 2),
                (0, 3),
                (0, 4),
                (0, 5),
                (0x20, 5),
                (0, 6),
                (0, 7),
            ];
            let (funct7, funct3) = OPS[d.below(10) as usize];
            Group::Plain(vec![r(funct7, d.reg(), d.reg(), funct3, d.reg())])
        }
        // Register-immediate ALU.
        25..37 => {
            const FUNCT3: [u32; 6] = [0, 2, 3, 4, 6, 7];
            let funct3 = FUNCT3[d.below(6) as usize];
            Group::Plain(vec![i(OP_IMM, funct3, d.reg(), d.reg(), d.imm12())])
        }
        // Shifts by an immediate.
        37..45 => {
            const SHIFTS: [(u32, u32); 3] = [(0, 1), (0, 5), (0x20, 5)];
            let (funct7, funct3) = SHIFTS[d.below(3) as usize];
            let shamt = d.below(32) as i32;
            let (rd, rs1) = (d.reg(), d.reg());
            Group::Plain(vec![i(
                OP_IMM,
                funct3,
                rd,
                rs1,
                (funct7 << 5) as i32 | shamt,
            )])
        }
        45..53 => {
            let opcode = if d.below(2) == 0 { LUI } else { AUIPC };
            Group::Plain(vec![u(opcode, d.reg(), d.imm20())])
        }
        // A load: the base register, then an aligned address in the window.
        53..68 => {
            const LOADS: [(u32, u32); 5] = [(0, 1), (1, 2), (2, 4), (4, 1), (5, 2)];
            let (funct3, width) = LOADS[d.below(5) as usize];
            let (base, rd) = (d.reg_nonzero(), d.reg());
            let (set, imm) = based(d, base, width);
            Group::Plain(vec![set[0], set[1], i(LOAD, funct3, rd, base, imm)])
        }
        // A store: the base register, then an aligned address in the window.
        68..80 => {
            const STORES: [(u32, u32); 3] = [(0, 1), (1, 2), (2, 4)];
            let (funct3, width) = STORES[d.below(3) as usize];
            let (base, src) = (d.reg_nonzero(), d.reg());
            let (set, imm) = based(d, base, width);
            Group::Plain(vec![set[0], set[1], s(funct3, base, src, imm)])
        }
        80..92 => {
            const FUNCT3: [u32; 6] = [0, 1, 4, 5, 6, 7];
            Group::Branch {
                funct3: FUNCT3[d.below(6) as usize],
                rs1: d.reg(),
                rs2: d.reg(),
                skip: 1 + d.below(4) as usize,
            }
        }
        92..95 => Group::Jal {
            rd: d.reg(),
            skip: 1 + d.below(4) as usize,
        },
        95..98 => Group::Jalr {
            base: d.reg_nonzero(),
            rd: d.reg(),
            skip: 1 + d.below(4) as usize,
            odd: d.below(2) == 0,
        },
        // FENCE with fm = 0, rs1 = rd = 0, and nonzero predecessor and successor sets.
        _ => {
            let (pred, succ) = (1 + d.below(15), 1 + d.below(15));
            Group::Plain(vec![pred << 24 | succ << 20 | MISC_MEM])
        }
    }
}

/// `LUI` and `ADDI` that set `base` so that `base + imm` is a random address in the window
/// aligned to `width`, and that `imm`.
fn based(d: &mut Draw, base: u8, width: u32) -> ([u32; 2], i32) {
    let target = WINDOW + d.below(WINDOW_SIZE / width) * width;
    let imm = d.imm12();
    (li(base, target.wrapping_sub(imm as u32)), imm)
}

/// The §10.2 pass sequence at `pc`: `FENCE`, `gp = 1`, `a7 = 93`, `a0 = 0`, then
/// `write_tohost` (a word store of `gp` to `tohost`, and of zero to `tohost + 4`, each
/// addressed through `t5`), then `ECALL`.
pub(crate) fn pass_sequence(pc: u32) -> Vec<u32> {
    const T5: u8 = 30;
    let mut words = vec![
        0x0ff0_000f, // fence iorw, iorw
        i(OP_IMM, 0, 3, 0, 1),
        i(OP_IMM, 0, 17, 0, 93),
        i(OP_IMM, 0, 10, 0, 0),
    ];
    for (src, offset) in [(3u8, 0u32), (0, 4)] {
        let at = pc + 4 * (words.len() as u32);
        let (hi, lo) = pcrel(at, TOHOST + offset);
        words.push(u(AUIPC, T5, hi));
        words.push(s(2, T5, src, lo));
    }
    words.push(SYSTEM); // ecall
    words
}

/// A generated or trap program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// `progen-<seed>` or `misaligned-<case>`.
    pub name: String,
    /// The code, from [`CODE_BASE`].
    pub code: Vec<u32>,
    /// The ELF file.
    pub elf: Vec<u8>,
}

/// The program for `seed`.
pub fn generate(seed: u64) -> Program {
    let mut d = Draw(stream(seed));
    let groups: Vec<Group> = (0..GROUPS).map(|_| group(&mut d)).collect();
    // Group k starts at starts[k]; starts[GROUPS] is the pass sequence.
    let mut starts = Vec::with_capacity(GROUPS + 1);
    let mut pc = CODE_BASE;
    for g in &groups {
        starts.push(pc);
        pc += 4 * g.len();
    }
    starts.push(pc);
    let target = |k: usize, skip: usize| starts[(k + skip).min(GROUPS)];
    let mut code = Vec::new();
    for (k, g) in groups.iter().enumerate() {
        let at = starts[k];
        match *g {
            Group::Plain(ref words) => code.extend(words),
            Group::Branch {
                funct3,
                rs1,
                rs2,
                skip,
            } => code.push(b(funct3, rs1, rs2, target(k, skip) - at)),
            Group::Jal { rd, skip } => code.push(j(rd, target(k, skip) - at)),
            Group::Jalr {
                base,
                rd,
                skip,
                odd,
            } => {
                let offset = (target(k, skip) - at) as i32 + i32::from(odd);
                code.push(u(AUIPC, base, 0));
                code.push(i(JALR, 0, rd, base, offset));
            }
        }
    }
    code.extend(pass_sequence(pc));
    let mut window = vec![0; WINDOW_SIZE as usize];
    for chunk in window.chunks_mut(8) {
        chunk.copy_from_slice(&d.0.next_u64().to_le_bytes());
    }
    Program {
        name: format!("progen-{seed:016x}"),
        elf: elf(&code, &window),
        code,
    }
}

/// A dedicated trap program's case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Misaligned {
    /// `lh-1` and so on: the access and the offset from an aligned address.
    pub name: &'static str,
    /// The load or store's `funct3`.
    pub funct3: u32,
    /// A store, or a load.
    pub store: bool,
    /// Bytes past the aligned window start.
    pub offset: u32,
    /// The trap cause SystemScope reports.
    pub cause: &'static str,
}

/// Every misaligned case: each halfword and word access at each misaligned offset.
pub const MISALIGNED: [Misaligned; 9] = {
    const fn load(name: &'static str, funct3: u32, offset: u32) -> Misaligned {
        Misaligned {
            name,
            funct3,
            store: false,
            offset,
            cause: "LoadAddressMisaligned",
        }
    }
    const fn store(name: &'static str, funct3: u32, offset: u32) -> Misaligned {
        Misaligned {
            name,
            funct3,
            store: true,
            offset,
            cause: "StoreAddressMisaligned",
        }
    }
    [
        load("lh-1", 1, 1),
        load("lhu-1", 5, 1),
        load("lw-1", 2, 1),
        load("lw-2", 2, 2),
        load("lw-3", 2, 3),
        store("sh-1", 1, 1),
        store("sw-1", 2, 1),
        store("sw-2", 2, 2),
        store("sw-3", 2, 3),
    ]
};

/// The trap each [`misaligned`] program must end with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedTrap {
    /// The trapping instruction's address.
    pub pc: u32,
    /// Its word.
    pub insn: u32,
    /// SystemScope's cause name.
    pub cause: &'static str,
    /// The misaligned effective address.
    pub tval: u32,
}

/// The index of the misaligned access in every [`misaligned`] program.
const TRAP_INDEX: u32 = 5;

/// The trap program for `case`: `x5` = the window, an aligned word store and load that
/// must not trap, then the misaligned access, which must. The words after it never run.
pub fn misaligned(case: &Misaligned) -> (Program, ExpectedTrap) {
    const X5: u8 = 5;
    let [hi, lo] = li(X5, WINDOW);
    let access = if case.store {
        s(case.funct3, X5, 6, case.offset as i32)
    } else {
        i(LOAD, case.funct3, 7, X5, case.offset as i32)
    };
    let code = vec![
        hi,
        lo,
        i(OP_IMM, 0, 6, 0, 0x5a5),
        s(2, X5, 6, 0),
        i(LOAD, 2, 8, X5, 0),
        access,
        i(OP_IMM, 0, 3, 0, 1),
        SYSTEM,
    ];
    debug_assert_eq!(code[TRAP_INDEX as usize], access);
    let expected = ExpectedTrap {
        pc: CODE_BASE + 4 * TRAP_INDEX,
        insn: access,
        cause: case.cause,
        tval: WINDOW + case.offset,
    };
    let program = Program {
        name: format!("misaligned-{}", case.name),
        elf: elf(&code, &[0; WINDOW_SIZE as usize]),
        code,
    };
    (program, expected)
}

/// A minimal ELF32 RISC-V executable: the code at [`CODE_BASE`] and the data segment at
/// [`DATA_BASE`] (`tohost`, `fromhost`, then `window` at [`WINDOW`]), as two `PT_LOAD`
/// segments, with section headers and a symbol table naming `tohost` and `fromhost` for
/// Spike's HTIF. Every offset is fixed by the code's length.
pub(crate) fn elf(code: &[u32], window: &[u8]) -> Vec<u8> {
    const EHDR: u32 = 52;
    const PHDR: u32 = 32;
    const SHDR: u32 = 40;
    let text: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut data = vec![0; (WINDOW - DATA_BASE) as usize];
    data.extend_from_slice(window);
    let strtab = b"\0tohost\0fromhost\0";
    let shstrtab = b"\0.text\0.data\0.symtab\0.strtab\0.shstrtab\0";
    let (n_text, n_data, n_symtab, n_strtab, n_shstrtab) = (1u32, 7, 13, 21, 29);

    let align = |at: usize, to: usize| at.div_ceil(to) * to;
    let text_off = align((EHDR + 2 * PHDR) as usize, 16);
    let data_off = align(text_off + text.len(), 16);
    let sym_off = align(data_off + data.len(), 4);
    let symbol = |name: u32, value: u32| {
        let mut e = Vec::with_capacity(16);
        e.extend_from_slice(&name.to_le_bytes());
        e.extend_from_slice(&value.to_le_bytes());
        e.extend_from_slice(&8u32.to_le_bytes()); // st_size
        e.push(0x11); // STB_GLOBAL, STT_OBJECT
        e.push(0); // STV_DEFAULT
        e.extend_from_slice(&2u16.to_le_bytes()); // .data
        e
    };
    let mut symtab = vec![0; 16];
    symtab.extend(symbol(1, TOHOST));
    symtab.extend(symbol(8, FROMHOST));
    let str_off = sym_off + symtab.len();
    let shstr_off = str_off + strtab.len();
    let sh_off = align(shstr_off + shstrtab.len(), 4);

    let mut f = vec![0; sh_off + 6 * SHDR as usize];
    let put =
        |f: &mut Vec<u8>, at: usize, bytes: &[u8]| f[at..at + bytes.len()].copy_from_slice(bytes);
    let words =
        |values: &[u32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };
    // ELF header.
    put(&mut f, 0, b"\x7fELF\x01\x01\x01");
    put(&mut f, 16, &[2, 0, 243, 0]); // ET_EXEC, EM_RISCV
    put(&mut f, 20, &words(&[1, CODE_BASE, EHDR, sh_off as u32, 0]));
    let halves: Vec<u8> = [EHDR, PHDR, 2, SHDR, 6, 5]
        .iter()
        .flat_map(|h| (*h as u16).to_le_bytes())
        .collect();
    put(&mut f, 40, &halves);
    // Program headers: PT_LOAD R+X, then PT_LOAD R+W.
    let len = |b: usize| b as u32;
    put(
        &mut f,
        EHDR as usize,
        &words(&[
            1,
            len(text_off),
            CODE_BASE,
            CODE_BASE,
            len(text.len()),
            len(text.len()),
            5,
            4,
        ]),
    );
    put(
        &mut f,
        (EHDR + PHDR) as usize,
        &words(&[
            1,
            len(data_off),
            DATA_BASE,
            DATA_BASE,
            len(data.len()),
            len(data.len()),
            6,
            4,
        ]),
    );
    put(&mut f, text_off, &text);
    put(&mut f, data_off, &data);
    put(&mut f, sym_off, &symtab);
    put(&mut f, str_off, strtab);
    put(&mut f, shstr_off, shstrtab);
    // Section headers: null, .text, .data, .symtab, .strtab, .shstrtab.
    let sections: [[u32; 10]; 6] = [
        [0; 10],
        [
            n_text,
            1,
            6,
            CODE_BASE,
            len(text_off),
            len(text.len()),
            0,
            0,
            4,
            0,
        ],
        [
            n_data,
            1,
            3,
            DATA_BASE,
            len(data_off),
            len(data.len()),
            0,
            0,
            4,
            0,
        ],
        [
            n_symtab,
            2,
            0,
            0,
            len(sym_off),
            len(symtab.len()),
            4,
            1,
            4,
            16,
        ],
        [
            n_strtab,
            3,
            0,
            0,
            len(str_off),
            len(strtab.len()),
            0,
            0,
            1,
            0,
        ],
        [
            n_shstrtab,
            3,
            0,
            0,
            len(shstr_off),
            len(shstrtab.len()),
            0,
            0,
            1,
            0,
        ],
    ];
    for (k, section) in sections.iter().enumerate() {
        put(&mut f, sh_off + k * SHDR as usize, &words(section));
    }
    f
}

#[cfg(test)]
mod tests {
    use systemscope_rv32i::decode::decode;
    use systemscope_rv32i::instr::Instr;

    use super::*;
    use crate::hex;
    use crate::manifest::load;
    use crate::runner::{self, End};
    use crate::spike::elf_symbol;

    /// The derivation, pinned: the context, the seed material, and the first draws.
    #[test]
    fn the_stream_is_the_m0_derivation_with_its_own_context() {
        assert_eq!(PROGEN_CONTEXT, "SystemScope 2026-09 rv32 progen v1");
        let mut material = 7u64.to_le_bytes().to_vec();
        material.extend_from_slice(&11u32.to_le_bytes());
        material.extend_from_slice(b"rv32.progen");
        assert_eq!(seed_material(7, PROGEN_PATH), material);
        let key = blake3::derive_key(PROGEN_CONTEXT, &material);
        let mut a = stream(7);
        let mut b = Xoshiro256StarStar::from_seed_bytes(key);
        for _ in 0..4 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(stream(7).next_u64(), stream(8).next_u64());
    }

    #[test]
    fn a_program_is_a_function_of_its_seed() {
        assert_eq!(generate(5), generate(5));
        assert_ne!(generate(5).elf, generate(6).elf);
        assert_eq!(generate(0x2a).name, "progen-000000000000002a");
    }

    #[test]
    fn the_fixed_seeds_produce_the_pinned_programs() {
        let mut all = blake3::Hasher::new();
        for seed in FIXED_SEEDS {
            all.update(blake3::hash(&generate(seed).elf).as_bytes());
        }
        assert_eq!(hex(all.finalize().as_bytes()), FIXED_SEEDS_DIGEST);
        assert!(FIXED_SEEDS.windows(2).all(|w| w[0] < w[1]));
    }

    /// Every word decodes to an RV32I instruction; the only `ECALL` is the last word;
    /// branches and jumps go forward to a group start; loads and stores stay aligned in
    /// the window (checked by running them, below).
    #[test]
    fn generated_code_is_rv32i_forward_only_and_ends_with_the_pass_sequence() {
        for seed in FIXED_SEEDS {
            let p = generate(seed);
            let n = p.code.len();
            let pass = pass_sequence(0).len();
            let start = CODE_BASE + 4 * (n - pass) as u32;
            assert_eq!(&p.code[n - pass..], pass_sequence(start).as_slice());
            for (k, &word) in p.code.iter().enumerate() {
                let pc = CODE_BASE + 4 * k as u32;
                let instr = decode(word).unwrap_or_else(|e| panic!("{}: {pc:#x}: {e}", p.name));
                let is_ecall = matches!(instr, Instr::Ecall);
                assert_eq!(is_ecall, k == n - 1, "{}: {pc:#x}", p.name);
                assert!(!matches!(instr, Instr::Ebreak), "{}: {pc:#x}", p.name);
            }
        }
    }

    /// Every fixed seed runs on `m1-reference` to the pass rule, retiring every
    /// `write_tohost` store, and none traps before its `ECALL`.
    #[test]
    fn every_fixed_seed_passes_on_systemscope() {
        let mut total = 0;
        let mut kinds = std::collections::BTreeSet::new();
        for seed in FIXED_SEEDS {
            let p = generate(seed);
            assert_eq!(elf_symbol(&p.elf, "tohost"), Some(TOHOST), "{}", p.name);
            assert_eq!(elf_symbol(&p.elf, "fromhost"), Some(FROMHOST), "{}", p.name);
            let image = load(&p.name, &p.elf).unwrap();
            let outcome = runner::run(&image, false, Vec::new());
            runner::judge(&outcome).unwrap_or_else(|e| panic!("{}: {e}", p.name));
            let ecall = CODE_BASE + 4 * (p.code.len() as u32 - 1);
            assert!(
                matches!(outcome.end, End::Trap { pc, .. } if pc == ecall),
                "{}",
                p.name
            );
            assert!(outcome.instret > 200, "{}: {}", p.name, outcome.instret);
            total += outcome.instret;
            for word in &p.code {
                let instr = format!("{:?}", decode(*word).unwrap());
                kinds.insert(instr.split([' ', '{', '(']).next().unwrap().to_owned());
            }
        }
        println!(
            "{} programs, {total} instructions retired, {} kinds: {kinds:?}",
            FIXED_SEEDS.len(),
            kinds.len()
        );
    }

    /// The nightly seed from [`SEED_VAR`], in decimal or `0x` hex, printed first so a
    /// failing run names it: its program is the same twice, passes, and ends the same
    /// traced and untraced, run after run. Spike runs it in `cargo xtask spike random`.
    #[test]
    #[ignore = "nightly: needs M1_PROGEN_SEED"]
    fn the_nightly_seed_passes_on_systemscope() {
        let text = std::env::var(SEED_VAR).unwrap_or_else(|_| {
            panic!("set {SEED_VAR} to the seed to test, e.g. {SEED_VAR}=0x1234")
        });
        let text = text.trim().replace('_', "");
        let seed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(digits) => u64::from_str_radix(digits, 16),
            None => text.parse(),
        }
        .unwrap_or_else(|_| panic!("{SEED_VAR}={text:?} is not a seed"));
        println!("{SEED_VAR}={seed:#x}");
        let p = generate(seed);
        assert_eq!(generate(seed), p);
        let image = load(&p.name, &p.elf).unwrap();
        let outcome = runner::run(&image, false, Vec::new());
        runner::judge(&outcome).unwrap_or_else(|e| panic!("{SEED_VAR}={seed:#x}: {e}"));
        let ecall = CODE_BASE + 4 * (p.code.len() as u32 - 1);
        assert!(matches!(outcome.end, End::Trap { pc, .. } if pc == ecall));
        assert_eq!(runner::run(&image, false, Vec::new()), outcome);
        let traced = runner::run(&image, true, Vec::new());
        assert!(traced.trace.is_some());
        assert_eq!(
            runner::Outcome {
                trace: None,
                ..traced
            },
            outcome
        );
    }

    #[test]
    fn every_misaligned_program_traps_where_expected() {
        let names: Vec<&str> = MISALIGNED.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), 9);
        for case in &MISALIGNED {
            let (p, expected) = misaligned(case);
            assert_ne!(expected.tval % if case.funct3 & 3 == 1 { 2 } else { 4 }, 0);
            let image = load(&p.name, &p.elf).unwrap();
            let outcome = runner::run(&image, false, Vec::new());
            assert_eq!(
                outcome.end,
                End::Trap {
                    cause: expected.cause.to_owned(),
                    pc: expected.pc,
                    tval: expected.tval
                },
                "{}",
                p.name
            );
            assert_eq!(outcome.instret, u64::from(TRAP_INDEX), "{}", p.name);
            assert_eq!(p.code[TRAP_INDEX as usize], expected.insn);
        }
    }

    #[test]
    fn the_elf_is_the_minimal_layout() {
        let p = generate(0);
        let image = load(&p.name, &p.elf).unwrap();
        assert_eq!(image.entry, CODE_BASE);
        assert_eq!(image.segments.len(), 2);
        assert_eq!(image.segments[0].offset, 0);
        assert_eq!(image.segments[0].bytes.len(), 4 * p.code.len());
        assert_eq!(image.segments[1].offset, DATA_BASE - RAM_BASE);
        assert_eq!(
            image.segments[1].bytes.len() as u32,
            WINDOW - DATA_BASE + WINDOW_SIZE
        );
        assert!(4 * p.code.len() < (DATA_BASE - CODE_BASE) as usize);
    }
}
