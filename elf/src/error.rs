//! Why an ELF file cannot be loaded.

use std::fmt;

/// Why [`load_elf32`](crate::load_elf32) rejected an ELF file or a RAM region.
///
/// Segment errors name the segment by its index in the program-header table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElfError {
    /// The file is shorter than an ELF32 header, or `e_ehsize` is not 52.
    MalformedHeader,
    /// The file does not start with `\x7fELF`.
    InvalidMagic,
    /// `EI_CLASS` is not `ELFCLASS32` (1).
    UnsupportedClass(u8),
    /// `EI_DATA` is not `ELFDATA2LSB` (1).
    UnsupportedEndian(u8),
    /// `EI_VERSION` or `e_version` is not `EV_CURRENT` (1).
    UnsupportedVersion(u32),
    /// `e_type` is not `ET_EXEC` (2).
    UnsupportedType(u16),
    /// `e_machine` is not `EM_RISCV` (243).
    UnsupportedMachine(u16),
    /// The program-header table does not fit in the file, its entries are not 32 bytes,
    /// or `e_phnum` is `PN_XNUM`, which would need the section headers.
    MalformedProgramHeaders,
    /// A `PT_LOAD` segment is malformed.
    InvalidSegment {
        /// The segment's program-header index.
        index: u16,
        /// What is wrong with it.
        reason: SegmentError,
    },
    /// The RAM region is empty or reaches past the 32-bit address space.
    InvalidRamRegion,
    /// A `PT_LOAD` segment is not entirely inside the RAM region.
    SegmentOutsideRam {
        /// The segment's program-header index.
        index: u16,
    },
    /// Two `PT_LOAD` segments share at least one byte of memory.
    OverlappingSegments {
        /// The lower of the two program-header indices.
        first: u16,
        /// The higher of the two program-header indices.
        second: u16,
    },
    /// `e_entry` is not 4-byte aligned.
    MisalignedEntry(u32),
    /// `e_entry` is not inside any loaded segment.
    EntryOutsideSegments(u32),
}

/// What is wrong with a `PT_LOAD` segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentError {
    /// `p_filesz` is larger than `p_memsz`.
    FileSizeExceedsMemSize,
    /// `[p_offset, p_offset + p_filesz)` is not inside the file.
    FileRangeOutsideFile,
    /// `p_vaddr` and `p_paddr` differ.
    AddressMismatch,
    /// `p_vaddr + p_memsz` is past the 32-bit address space.
    AddressOverflow,
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SegmentError::FileSizeExceedsMemSize => "p_filesz is larger than p_memsz",
            SegmentError::FileRangeOutsideFile => "its file range is outside the file",
            SegmentError::AddressMismatch => "p_vaddr and p_paddr differ",
            SegmentError::AddressOverflow => "it reaches past the 32-bit address space",
        })
    }
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElfError::MalformedHeader => f.write_str("malformed ELF header"),
            ElfError::InvalidMagic => f.write_str("not an ELF file"),
            ElfError::UnsupportedClass(c) => write!(f, "ELF class {c} is not ELFCLASS32"),
            ElfError::UnsupportedEndian(d) => {
                write!(f, "ELF data encoding {d} is not little-endian")
            }
            ElfError::UnsupportedVersion(v) => write!(f, "ELF version {v} is not EV_CURRENT"),
            ElfError::UnsupportedType(t) => write!(f, "ELF type {t} is not ET_EXEC"),
            ElfError::UnsupportedMachine(m) => write!(f, "ELF machine {m} is not EM_RISCV"),
            ElfError::MalformedProgramHeaders => f.write_str("malformed program-header table"),
            ElfError::InvalidSegment { index, reason } => {
                write!(f, "PT_LOAD segment {index} is invalid: {reason}")
            }
            ElfError::InvalidRamRegion => f.write_str("the RAM region is empty or too large"),
            ElfError::SegmentOutsideRam { index } => {
                write!(f, "PT_LOAD segment {index} is outside the RAM region")
            }
            ElfError::OverlappingSegments { first, second } => {
                write!(f, "PT_LOAD segments {first} and {second} overlap")
            }
            ElfError::MisalignedEntry(e) => write!(f, "entry point {e:#010x} is misaligned"),
            ElfError::EntryOutsideSegments(e) => {
                write!(f, "entry point {e:#010x} is not in a loaded segment")
            }
        }
    }
}

impl std::error::Error for ElfError {}
