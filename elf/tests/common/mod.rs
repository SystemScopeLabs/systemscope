//! A test-only ELF32 writer, independent of the crate's parser.
//!
//! [`Elf`] lays out a well-formed little-endian RISC-V `ET_EXEC` file from a list of
//! program headers, writing every field with `to_le_bytes` at the offsets given in the
//! System V gABI ("ELF Header", "Program Header"). The offsets below are written out here
//! again rather than taken from the crate, so a wrong offset or byte order in the parser
//! cannot be mirrored by the tests. Malformed files are made by building a valid one and
//! patching fields with [`put_u8`], [`put_u16`], and [`put_u32`].
//!
//! [`Elf::expected`] is the oracle: the load image the loader must return, computed from
//! the builder's own description of the segments, never from the file bytes.

#![allow(dead_code)]

use systemscope_elf::{LoadImage, LoadSegment};

// ELF header field offsets (ELF32).
pub const EI_CLASS: usize = 4;
pub const EI_DATA: usize = 5;
pub const EI_VERSION: usize = 6;
pub const E_TYPE: usize = 16;
pub const E_MACHINE: usize = 18;
pub const E_VERSION: usize = 20;
pub const E_ENTRY: usize = 24;
pub const E_PHOFF: usize = 28;
pub const E_EHSIZE: usize = 40;
pub const E_PHENTSIZE: usize = 42;
pub const E_PHNUM: usize = 44;

// Program header field offsets (ELF32).
pub const P_TYPE: usize = 0;
pub const P_OFFSET: usize = 4;
pub const P_VADDR: usize = 8;
pub const P_PADDR: usize = 12;
pub const P_FILESZ: usize = 16;
pub const P_MEMSZ: usize = 20;

pub const EHDR_LEN: usize = 52;
pub const PHDR_LEN: usize = 32;

pub const PT_NULL: u32 = 0;
pub const PT_LOAD: u32 = 1;
pub const PT_NOTE: u32 = 4;
pub const PT_GNU_STACK: u32 = 0x6474_e551;
pub const PT_RISCV_ATTRIBUTES: u32 = 0x7000_0003;

/// One program header, with its file bytes.
#[derive(Clone, Debug)]
pub struct Phdr {
    pub p_type: u32,
    pub vaddr: u32,
    pub paddr: u32,
    /// The file bytes; `p_filesz` is their length.
    pub data: Vec<u8>,
    pub memsz: u32,
}

/// A `PT_LOAD` at `addr` (as both `p_vaddr` and `p_paddr`) holding `data`, then zeros up
/// to `memsz`.
pub fn load(addr: u32, data: &[u8], memsz: u32) -> Phdr {
    Phdr {
        p_type: PT_LOAD,
        vaddr: addr,
        paddr: addr,
        data: data.to_vec(),
        memsz,
    }
}

/// A non-loadable header with arbitrary addresses and contents.
pub fn other(p_type: u32, addr: u32, data: &[u8], memsz: u32) -> Phdr {
    Phdr {
        p_type,
        vaddr: addr,
        paddr: addr ^ 0x1234_5678,
        data: data.to_vec(),
        memsz,
    }
}

/// An ELF file description.
#[derive(Clone, Debug)]
pub struct Elf {
    pub entry: u32,
    pub phdrs: Vec<Phdr>,
}

impl Elf {
    pub fn new(entry: u32, phdrs: Vec<Phdr>) -> Elf {
        Elf { entry, phdrs }
    }

    /// File offset of program header `index`.
    pub fn ph(index: usize, field: usize) -> usize {
        EHDR_LEN + index * PHDR_LEN + field
    }

    /// File offset of the data of program header `index` in [`Elf::build`]'s layout.
    pub fn data_offset(&self, index: usize) -> usize {
        let mut at = EHDR_LEN + self.phdrs.len() * PHDR_LEN;
        for ph in &self.phdrs[..index] {
            at = align4(at + ph.data.len());
        }
        at
    }

    /// The file: header, program-header table right after it, then each header's data in
    /// table order, 4-byte aligned.
    pub fn build(&self) -> Vec<u8> {
        let mut f = vec![0u8; EHDR_LEN];
        f[0..4].copy_from_slice(b"\x7fELF");
        f[EI_CLASS] = 1; // ELFCLASS32
        f[EI_DATA] = 1; // ELFDATA2LSB
        f[EI_VERSION] = 1; // EV_CURRENT
        put_u16(&mut f, E_TYPE, 2); // ET_EXEC
        put_u16(&mut f, E_MACHINE, 243); // EM_RISCV
        put_u32(&mut f, E_VERSION, 1);
        put_u32(&mut f, E_ENTRY, self.entry);
        put_u32(&mut f, E_PHOFF, EHDR_LEN as u32);
        put_u16(&mut f, E_EHSIZE, EHDR_LEN as u16);
        put_u16(&mut f, E_PHENTSIZE, PHDR_LEN as u16);
        put_u16(&mut f, E_PHNUM, self.phdrs.len() as u16);
        f.resize(EHDR_LEN + self.phdrs.len() * PHDR_LEN, 0);
        for (i, ph) in self.phdrs.iter().enumerate() {
            let offset = self.data_offset(i);
            put_u32(&mut f, Elf::ph(i, P_TYPE), ph.p_type);
            put_u32(&mut f, Elf::ph(i, P_OFFSET), offset as u32);
            put_u32(&mut f, Elf::ph(i, P_VADDR), ph.vaddr);
            put_u32(&mut f, Elf::ph(i, P_PADDR), ph.paddr);
            put_u32(&mut f, Elf::ph(i, P_FILESZ), ph.data.len() as u32);
            put_u32(&mut f, Elf::ph(i, P_MEMSZ), ph.memsz);
            put_u32(&mut f, Elf::ph(i, 24), 7); // p_flags: RWX, ignored
            put_u32(&mut f, Elf::ph(i, 28), 4); // p_align, ignored
            f.resize(offset, 0);
            f.extend_from_slice(&ph.data);
        }
        f.resize(align4(f.len()), 0);
        f
    }

    /// The load image a correct loader returns for this file in a RAM at `ram_base`:
    /// every `PT_LOAD` with `memsz > 0`, at `vaddr - ram_base`, holding its data then zeros,
    /// sorted by offset. Assumes the description is valid.
    pub fn expected(&self, ram_base: u32) -> LoadImage {
        let mut segments: Vec<LoadSegment> = self
            .phdrs
            .iter()
            .filter(|ph| ph.p_type == PT_LOAD && ph.memsz > 0)
            .map(|ph| {
                let mut bytes = ph.data.clone();
                bytes.resize(ph.memsz as usize, 0);
                LoadSegment {
                    offset: ph.vaddr - ram_base,
                    bytes,
                }
            })
            .collect();
        segments.sort_by_key(|s| s.offset);
        LoadImage {
            segments,
            entry: self.entry,
            image_hash: *blake3::hash(&self.build()).as_bytes(),
        }
    }
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

pub fn put_u8(f: &mut [u8], at: usize, v: u8) {
    f[at] = v;
}

pub fn put_u16(f: &mut [u8], at: usize, v: u16) {
    f[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u32(f: &mut [u8], at: usize, v: u32) {
    f[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// RV32I encodings used by the integration programs (ISA manual, "Base Instruction
/// Formats"), independent of the CPU crate's decoder.
pub mod asm {
    pub fn lui(rd: u32, imm20: u32) -> u32 {
        imm20 << 12 | rd << 7 | 0x37
    }

    pub fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) & 0xfff) << 20 | rs1 << 15 | rd << 7 | 0x13
    }

    pub fn add(rd: u32, rs1: u32, rs2: u32) -> u32 {
        rs2 << 20 | rs1 << 15 | rd << 7 | 0x33
    }

    pub fn lw(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) & 0xfff) << 20 | rs1 << 15 | 2 << 12 | rd << 7 | 0x03
    }

    pub fn sw(rs2: u32, rs1: u32, imm: i32) -> u32 {
        let imm = imm as u32;
        (imm >> 5 & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (imm & 0x1f) << 7 | 0x23
    }

    /// Little-endian bytes of a program.
    pub fn code(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}
