//! A test-only writer for M3 user executables, independent of the crate's parser.
//!
//! [`UserElf`] lays out a well-formed little-endian RISC-V `ET_EXEC` file whose program
//! headers carry `p_flags`, which the M1 writer in `tests/common` does not write. Every
//! field is written with `to_le_bytes` at the offsets given in the System V gABI ("ELF
//! Header", "Program Header"), spelled out here rather than taken from the crate. The
//! program-header table follows the ELF header, and the segments' file bytes follow the
//! table in header order. Malformed files are made by building a valid one and patching
//! fields with [`put_u16`] and [`put_u32`].
//!
//! [`UserElf::expected`] is the oracle: the image the parser must return, computed from
//! the builder's description of the segments, never from the file bytes.

#![allow(dead_code)]

use std::ops::Range;

use systemscope_elf::{FileCopy, PagePlan, Perms, UserImage, UserSegment};

// ELF header field offsets (ELF32).
pub const EI_CLASS: usize = 4;
pub const EI_DATA: usize = 5;
pub const E_TYPE: usize = 16;
pub const E_MACHINE: usize = 18;
pub const E_ENTRY: usize = 24;
pub const E_PHOFF: usize = 28;
pub const E_PHENTSIZE: usize = 42;
pub const E_PHNUM: usize = 44;

// Program header field offsets (ELF32).
pub const P_TYPE: usize = 0;
pub const P_OFFSET: usize = 4;
pub const P_VADDR: usize = 8;
pub const P_PADDR: usize = 12;
pub const P_FILESZ: usize = 16;
pub const P_MEMSZ: usize = 20;
pub const P_FLAGS: usize = 24;

pub const EHDR_LEN: usize = 52;
pub const PHDR_LEN: usize = 32;
pub const PAGE: u32 = 4096;

pub const PT_LOAD: u32 = 1;
pub const PT_NOTE: u32 = 4;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;
pub const RX: u32 = PF_R | PF_X;
pub const RW: u32 = PF_R | PF_W;

/// The M3 user range, `[USER_BASE, STACK_TOP − STACK_PAGES·4096)` (`docs/m3-design.md`
/// §8.3, §11.2).
pub const USER: Range<u32> = 0x0001_0000..0x7fff_b000;

/// One program header, with its file bytes.
#[derive(Clone, Debug)]
pub struct Phdr {
    pub p_type: u32,
    pub vaddr: u32,
    pub paddr: u32,
    pub flags: u32,
    /// The file bytes; `p_filesz` is their length.
    pub data: Vec<u8>,
    pub memsz: u32,
}

/// A `PT_LOAD` at `vaddr` (and `p_paddr`) with `flags`, holding `data`, then zeros up to
/// `memsz`.
pub fn load(vaddr: u32, flags: u32, data: &[u8], memsz: u32) -> Phdr {
    Phdr {
        p_type: PT_LOAD,
        vaddr,
        paddr: vaddr,
        flags,
        data: data.to_vec(),
        memsz,
    }
}

/// A non-loadable header with arbitrary addresses and flags.
pub fn other(p_type: u32, vaddr: u32, data: &[u8]) -> Phdr {
    Phdr {
        p_type,
        vaddr,
        paddr: vaddr ^ 0x1234_5678,
        flags: 0,
        data: data.to_vec(),
        memsz: 0,
    }
}

/// A user executable to write.
#[derive(Clone, Debug)]
pub struct UserElf {
    pub entry: u32,
    pub phdrs: Vec<Phdr>,
}

impl UserElf {
    pub fn new(entry: u32, phdrs: Vec<Phdr>) -> UserElf {
        UserElf { entry, phdrs }
    }

    /// The length of the header and the program-header table: the prefix the parser
    /// needs.
    pub fn prefix_len(&self) -> usize {
        EHDR_LEN + PHDR_LEN * self.phdrs.len()
    }

    /// The file offset of header `i`'s bytes.
    pub fn data_offset(&self, i: usize) -> usize {
        self.prefix_len() + self.phdrs[..i].iter().map(|p| p.data.len()).sum::<usize>()
    }

    /// The whole file.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.prefix_len()];
        out[..4].copy_from_slice(b"\x7fELF");
        out[EI_CLASS] = 1;
        out[EI_DATA] = 1;
        out[6] = 1; // EI_VERSION
        put_u16(&mut out, E_TYPE, 2); // ET_EXEC
        put_u16(&mut out, E_MACHINE, 243); // EM_RISCV
        put_u32(&mut out, 20, 1); // e_version
        put_u32(&mut out, E_ENTRY, self.entry);
        put_u32(&mut out, E_PHOFF, EHDR_LEN as u32);
        put_u16(&mut out, 40, EHDR_LEN as u16); // e_ehsize
        put_u16(&mut out, E_PHENTSIZE, PHDR_LEN as u16);
        put_u16(&mut out, E_PHNUM, self.phdrs.len() as u16);
        for (i, p) in self.phdrs.iter().enumerate() {
            let at = EHDR_LEN + PHDR_LEN * i;
            put_u32(&mut out, at + P_TYPE, p.p_type);
            put_u32(&mut out, at + P_OFFSET, self.data_offset(i) as u32);
            put_u32(&mut out, at + P_VADDR, p.vaddr);
            put_u32(&mut out, at + P_PADDR, p.paddr);
            put_u32(&mut out, at + P_FILESZ, p.data.len() as u32);
            put_u32(&mut out, at + P_MEMSZ, p.memsz);
            put_u32(&mut out, at + P_FLAGS, p.flags);
            put_u32(&mut out, at + 28, PAGE); // p_align
        }
        for p in &self.phdrs {
            out.extend_from_slice(&p.data);
        }
        out
    }

    /// The image the parser must return: every non-empty `PT_LOAD`, sorted by address,
    /// with one plan per page its memory touches.
    pub fn expected(&self) -> UserImage {
        let mut segments: Vec<UserSegment> = self
            .phdrs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.p_type == PT_LOAD && p.memsz != 0)
            .map(|(i, p)| {
                let perms = Perms {
                    read: p.flags & PF_R != 0,
                    write: p.flags & PF_W != 0,
                    execute: p.flags & PF_X != 0,
                };
                let offset = self.data_offset(i) as u64;
                let start = u64::from(p.vaddr);
                let file_end = start + p.data.len() as u64;
                let mem_end = start + u64::from(p.memsz);
                let mut pages = Vec::new();
                let mut va = start - start % u64::from(PAGE);
                while va < mem_end {
                    let lo = start.max(va);
                    let hi = file_end.min(va + u64::from(PAGE));
                    let copy = (lo < hi).then(|| FileCopy {
                        file_offset: (offset + lo - start) as u32,
                        page_offset: (lo - va) as u32,
                        len: (hi - lo) as u32,
                    });
                    pages.push(PagePlan {
                        va: va as u32,
                        perms,
                        copy,
                    });
                    va += u64::from(PAGE);
                }
                UserSegment {
                    index: i as u16,
                    vaddr: p.vaddr,
                    memsz: p.memsz,
                    perms,
                    pages,
                }
            })
            .collect();
        segments.sort_by_key(|s| s.vaddr);
        UserImage {
            entry: self.entry,
            segments,
        }
    }
}

pub fn put_u16(elf: &mut [u8], at: usize, v: u16) {
    elf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn put_u32(elf: &mut [u8], at: usize, v: u32) {
    elf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The offset of field `field` of program header `i`.
pub fn ph(i: usize, field: usize) -> usize {
    EHDR_LEN + PHDR_LEN * i + field
}

/// A typical two-segment program: text and rodata (RX) at `0x10000`, data and `.bss` (RW)
/// on the next page.
pub fn typical() -> UserElf {
    UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 0x40], 0x40),
            load(0x0001_1000, RW, &[0xaa; 0x10], 0x1800),
        ],
    )
}
