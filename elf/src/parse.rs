//! ELF32 header and program-header parsing (System V gABI, "ELF Header" and "Program
//! Header"), for the subset [`load_elf32`](crate::load_elf32) accepts.
//!
//! The file is untrusted. Every read goes through [`Fields`], which checks bounds and
//! decodes little-endian explicitly, so nothing depends on the host's byte order or word
//! size, and malformed input is an error, never a panic.

use crate::error::ElfError;

/// Size of an ELF32 file header.
pub(crate) const EHDR_SIZE: usize = 52;
/// Size of an ELF32 program header.
pub(crate) const PHDR_SIZE: u16 = 32;

const ELFCLASS32: u8 = 1;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_RISCV: u16 = 243;
/// `e_phnum` value meaning "the count is in section header 0".
const PN_XNUM: u16 = 0xffff;
/// Program-header type of a loadable segment.
pub(crate) const PT_LOAD: u32 = 1;

/// Bounds-checked little-endian reads from a byte slice.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn bytes<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        self.0.get(at..at.checked_add(N)?)?.try_into().ok()
    }

    fn u8(&self, at: usize) -> Option<u8> {
        self.0.get(at).copied()
    }

    fn u16(&self, at: usize) -> Option<u16> {
        self.bytes(at).map(u16::from_le_bytes)
    }

    fn u32(&self, at: usize) -> Option<u32> {
        self.bytes(at).map(u32::from_le_bytes)
    }
}

/// The ELF header fields the loader uses.
pub(crate) struct Header {
    pub(crate) entry: u32,
    pub(crate) phoff: u32,
    pub(crate) phnum: u16,
}

/// Checks the file header and returns what the loader needs from it.
pub(crate) fn header(elf: &[u8]) -> Result<Header, ElfError> {
    let malformed = ElfError::MalformedHeader;
    if elf.len() < EHDR_SIZE {
        return Err(malformed);
    }
    let f = Fields(elf);
    if f.bytes::<4>(0) != Some(*b"\x7fELF") {
        return Err(ElfError::InvalidMagic);
    }
    let class = f.u8(4).ok_or(malformed)?;
    if class != ELFCLASS32 {
        return Err(ElfError::UnsupportedClass(class));
    }
    let data = f.u8(5).ok_or(malformed)?;
    if data != ELFDATA2LSB {
        return Err(ElfError::UnsupportedEndian(data));
    }
    let ident_version = u32::from(f.u8(6).ok_or(malformed)?);
    if ident_version != EV_CURRENT {
        return Err(ElfError::UnsupportedVersion(ident_version));
    }
    let e_type = f.u16(16).ok_or(malformed)?;
    if e_type != ET_EXEC {
        return Err(ElfError::UnsupportedType(e_type));
    }
    let machine = f.u16(18).ok_or(malformed)?;
    if machine != EM_RISCV {
        return Err(ElfError::UnsupportedMachine(machine));
    }
    let version = f.u32(20).ok_or(malformed)?;
    if version != EV_CURRENT {
        return Err(ElfError::UnsupportedVersion(version));
    }
    let entry = f.u32(24).ok_or(malformed)?;
    let phoff = f.u32(28).ok_or(malformed)?;
    let ehsize = f.u16(40).ok_or(malformed)?;
    if usize::from(ehsize) != EHDR_SIZE {
        return Err(malformed);
    }
    let phentsize = f.u16(42).ok_or(malformed)?;
    let phnum = f.u16(44).ok_or(malformed)?;
    // With no program headers, e_phentsize may be anything (gABI).
    if phnum == PN_XNUM || (phnum != 0 && phentsize != PHDR_SIZE) {
        return Err(ElfError::MalformedProgramHeaders);
    }
    // The whole table must be inside the file.
    let table_end = u64::from(phoff) + u64::from(phnum) * u64::from(PHDR_SIZE);
    if table_end > file_len(elf) {
        return Err(ElfError::MalformedProgramHeaders);
    }
    Ok(Header {
        entry,
        phoff,
        phnum,
    })
}

/// The program-header fields the loader uses.
pub(crate) struct ProgramHeader {
    pub(crate) p_type: u32,
    pub(crate) offset: u32,
    pub(crate) vaddr: u32,
    pub(crate) paddr: u32,
    pub(crate) filesz: u32,
    pub(crate) memsz: u32,
}

/// Program header `index`, which [`header`] has checked is inside the file.
pub(crate) fn program_header(
    elf: &[u8],
    header: &Header,
    index: u16,
) -> Result<ProgramHeader, ElfError> {
    let bad = ElfError::MalformedProgramHeaders;
    let at = u64::from(header.phoff) + u64::from(index) * u64::from(PHDR_SIZE);
    let at = usize::try_from(at).map_err(|_| bad)?;
    let f = Fields(elf);
    let field = |n: usize| at.checked_add(n).and_then(|a| f.u32(a)).ok_or(bad);
    Ok(ProgramHeader {
        p_type: field(0)?,
        offset: field(4)?,
        vaddr: field(8)?,
        paddr: field(12)?,
        filesz: field(16)?,
        memsz: field(20)?,
    })
}

/// The file length as a `u64`, for range checks independent of `usize`.
pub(crate) fn file_len(elf: &[u8]) -> u64 {
    // A slice is at most isize::MAX bytes, which fits in a u64 on every Rust target.
    u64::try_from(elf.len()).unwrap_or(u64::MAX)
}
