//! User-image validation for the M3 modeled kernel (`docs/m3-design.md` §8.3).
//!
//! [`parse_user_elf32`] checks an ELF32 RISC-V executable that a process will run in its
//! own Sv32 address space, and turns it into a [`UserImage`]: the entry point and, for
//! every loaded segment, a page-by-page mapping plan. It is a pure function. It reads only
//! a prefix of the file that holds the ELF header and the program-header table, never the
//! segment bytes, and it produces no physical address, frame, or RAM reference: the kernel
//! (M3.4b) allocates frames, copies the file ranges the plan names, and writes the PTEs.
//!
//! ```text
//! ELF prefix (header + program headers), file length, user range
//!   ──▶ parse_user_elf32 ──▶ UserImage { entry, segments: [UserSegment { pages: [PagePlan] }] }
//! ```
//!
//! # Rules
//!
//! The header checks are the M1 loader's (`docs/m1-design.md` §8), run by the same code:
//! `ELFCLASS32`, `ELFDATA2LSB`, `EV_CURRENT`, `EM_RISCV`, `ET_EXEC`, a 52-byte header, and
//! 32-byte program headers. An error there is [`UserElfError::Header`]. Beyond them:
//!
//! - At most [`MAX_PHDRS`] program headers.
//! - Only `PT_LOAD` segments are mapped. Each needs `p_filesz <= p_memsz` and a file range
//!   inside the file. One with `p_memsz == 0` maps nothing and is otherwise ignored, as in
//!   the M1 loader.
//! - **Addresses are virtual:** a segment is mapped at `p_vaddr`, and `p_paddr` is ignored.
//!   The segment's memory `[p_vaddr, p_vaddr + p_memsz)` must lie inside the user range.
//! - **Permissions** come from `p_flags`: `PF_R`, `PF_W`, `PF_X`. Every other bit is
//!   ignored. A segment with none of the three, or with `PF_W` but not `PF_R` (a reserved
//!   Sv32 PTE encoding), is rejected.
//! - **No two segments share a page,** so every page has one permission set and at most one
//!   file range. Overlapping bytes are [`UserElfError::SegmentOverlap`]; a page shared
//!   without overlapping bytes is [`UserElfError::SharedPage`].
//! - **The entry point** is 4-byte aligned and inside the memory of a segment with `PF_X`.
//!
//! Checks run in a fixed order: the prefix and the header, the program-header count, each
//! `PT_LOAD` in table order (sizes, file range, permissions, placement), then overlap and
//! page sharing, then the entry point. All arithmetic on file values is checked or done in
//! `u64`, and malformed input returns an error, never a panic.

use std::fmt;
use std::ops::Range;

use crate::error::ElfError;
use crate::parse::{EHDR_SIZE, PHDR_SIZE, PT_LOAD, header, program_header};

/// The largest `e_phnum` a user image may have (`docs/m3-design.md` §6.2, §8.3).
pub const MAX_PHDRS: u16 = 16;

/// The Sv32 page size.
pub const PAGE_SIZE: u32 = 4096;

// Program-header flags (System V gABI, "Segment Flag Bits").
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
/// Offset of `p_flags` in an ELF32 program header.
const P_FLAGS: u64 = 24;
/// `e_phnum` value meaning "the count is in section header 0".
const PN_XNUM: u16 = 0xffff;

/// A segment's or page's access permissions, from `PF_R`, `PF_W`, and `PF_X`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Perms {
    /// Readable (`PF_R`, PTE `R`).
    pub read: bool,
    /// Writable (`PF_W`, PTE `W`). Never set without `read`.
    pub write: bool,
    /// Executable (`PF_X`, PTE `X`).
    pub execute: bool,
}

/// File bytes to copy into one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileCopy {
    /// Offset of the first byte in the ELF file.
    pub file_offset: u32,
    /// Offset of the first byte in the page.
    pub page_offset: u32,
    /// Number of bytes, at least 1; `page_offset + len` is at most [`PAGE_SIZE`].
    pub len: u32,
}

/// How one virtual page of a segment is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PagePlan {
    /// The page's virtual address, a multiple of [`PAGE_SIZE`].
    pub va: u32,
    /// The segment's permissions.
    pub perms: Perms,
    /// The file bytes the page holds, if any. Every other byte of the page is zero:
    /// `.bss`, and the parts of the first and last page outside the segment.
    pub copy: Option<FileCopy>,
}

/// A loaded segment and its mapping plan.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserSegment {
    /// The segment's program-header index.
    pub index: u16,
    /// `p_vaddr`.
    pub vaddr: u32,
    /// `p_memsz`, never 0.
    pub memsz: u32,
    /// The permissions from `p_flags`.
    pub perms: Perms,
    /// Every page the segment's memory touches, in ascending address order.
    pub pages: Vec<PagePlan>,
}

/// A validated user executable: where it starts and how its segments are mapped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserImage {
    /// The entry point, a virtual address: 4-byte aligned and inside an executable
    /// segment.
    pub entry: u32,
    /// The loaded segments, in ascending address order. No two share a page.
    pub segments: Vec<UserSegment>,
}

/// Why [`parse_user_elf32`] rejected a user executable. Segment errors name the segment
/// by its program-header index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserElfError {
    /// A header check shared with the M1 loader failed: the file itself is malformed or
    /// unsupported.
    Header(ElfError),
    /// The prefix is longer than the file it claims to be a prefix of.
    PrefixLongerThanFile,
    /// The prefix ends before the ELF header or the program-header table does, although
    /// the file is long enough to hold them. Retrying with the first `needed` bytes of the
    /// file gets past this check.
    PrefixTooShort {
        /// The prefix length the header and the program-header table need.
        needed: u32,
    },
    /// The user range is empty, or its start or end is not page-aligned.
    InvalidUserRange,
    /// `e_phnum` is larger than [`MAX_PHDRS`].
    PhdrCountExceeded(u16),
    /// A `PT_LOAD` segment has `p_filesz > p_memsz`.
    InvalidSegmentSize {
        /// The segment's program-header index.
        index: u16,
    },
    /// A `PT_LOAD` segment's file range `[p_offset, p_offset + p_filesz)` is not inside
    /// the file.
    FileRangeOutsideFile {
        /// The segment's program-header index.
        index: u16,
    },
    /// A `PT_LOAD` segment has none of `PF_R`, `PF_W`, and `PF_X`.
    NoPermission {
        /// The segment's program-header index.
        index: u16,
    },
    /// A `PT_LOAD` segment has `PF_W` without `PF_R`.
    WriteWithoutRead {
        /// The segment's program-header index.
        index: u16,
    },
    /// A `PT_LOAD` segment's memory is not entirely inside the user range.
    SegmentOutsideUserRange {
        /// The segment's program-header index.
        index: u16,
    },
    /// Two `PT_LOAD` segments share at least one byte of memory.
    SegmentOverlap {
        /// The lower of the two program-header indices.
        first: u16,
        /// The higher of the two program-header indices.
        second: u16,
    },
    /// Two `PT_LOAD` segments do not overlap but touch the same page.
    SharedPage {
        /// The lower of the two program-header indices.
        first: u16,
        /// The higher of the two program-header indices.
        second: u16,
    },
    /// `e_entry` is not 4-byte aligned.
    MisalignedEntry(u32),
    /// `e_entry` is not inside the memory of a segment with `PF_X`.
    EntryNotExecutable(u32),
}

impl From<ElfError> for UserElfError {
    fn from(e: ElfError) -> UserElfError {
        UserElfError::Header(e)
    }
}

impl fmt::Display for UserElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserElfError::Header(e) => write!(f, "{e}"),
            UserElfError::PrefixLongerThanFile => {
                f.write_str("the ELF prefix is longer than the file")
            }
            UserElfError::PrefixTooShort { needed } => {
                write!(
                    f,
                    "the ELF prefix must hold the first {needed} bytes of the file"
                )
            }
            UserElfError::InvalidUserRange => {
                f.write_str("the user range is empty or not page-aligned")
            }
            UserElfError::PhdrCountExceeded(n) => {
                write!(f, "{n} program headers exceed the limit of {MAX_PHDRS}")
            }
            UserElfError::InvalidSegmentSize { index } => {
                write!(
                    f,
                    "PT_LOAD segment {index} has p_filesz larger than p_memsz"
                )
            }
            UserElfError::FileRangeOutsideFile { index } => {
                write!(
                    f,
                    "PT_LOAD segment {index} has a file range outside the file"
                )
            }
            UserElfError::NoPermission { index } => {
                write!(f, "PT_LOAD segment {index} has no permission")
            }
            UserElfError::WriteWithoutRead { index } => {
                write!(f, "PT_LOAD segment {index} is writable but not readable")
            }
            UserElfError::SegmentOutsideUserRange { index } => {
                write!(f, "PT_LOAD segment {index} is outside the user range")
            }
            UserElfError::SegmentOverlap { first, second } => {
                write!(f, "PT_LOAD segments {first} and {second} overlap")
            }
            UserElfError::SharedPage { first, second } => {
                write!(f, "PT_LOAD segments {first} and {second} share a page")
            }
            UserElfError::MisalignedEntry(e) => write!(f, "entry point {e:#010x} is misaligned"),
            UserElfError::EntryNotExecutable(e) => {
                write!(f, "entry point {e:#010x} is not in an executable segment")
            }
        }
    }
}

impl std::error::Error for UserElfError {}

/// A checked `PT_LOAD` segment before sorting.
struct Checked {
    index: u16,
    offset: u32,
    vaddr: u32,
    filesz: u32,
    memsz: u32,
    perms: Perms,
}

impl Checked {
    /// One past the last byte of memory, in `u64` so it cannot overflow.
    fn end(&self) -> u64 {
        u64::from(self.vaddr) + u64::from(self.memsz)
    }

    /// The page holding the last byte of memory.
    fn last_page(&self) -> u64 {
        (self.end() - 1) / u64::from(PAGE_SIZE)
    }

    fn first_page(&self) -> u64 {
        u64::from(self.vaddr) / u64::from(PAGE_SIZE)
    }

    /// The plan for every page the segment touches.
    fn pages(&self) -> Vec<PagePlan> {
        let page = u64::from(PAGE_SIZE);
        let file_start = u64::from(self.vaddr);
        let file_end = file_start + u64::from(self.filesz);
        (self.first_page()..=self.last_page())
            .map(|n| {
                let va = n * page;
                let from = file_start.max(va);
                let to = file_end.min(va + page);
                let copy = (from < to).then(|| FileCopy {
                    // Every value is below 2^32: the segment lies inside the user range,
                    // which ends at or below 2^32, and the file range was checked against
                    // the file length, a u32.
                    file_offset: narrow(u64::from(self.offset) + (from - file_start)),
                    page_offset: narrow(from - va),
                    len: narrow(to - from),
                });
                PagePlan {
                    va: narrow(va),
                    perms: self.perms,
                    copy,
                }
            })
            .collect()
    }
}

/// Narrows a value the caller has proved is below 2^32.
fn narrow(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Checks the prefix and the header with the M1 loader's code, and returns the header.
fn checked_header(prefix: &[u8], file_len: u32) -> Result<crate::parse::Header, UserElfError> {
    let file_len = u64::from(file_len);
    let prefix_len = u64::try_from(prefix.len()).unwrap_or(u64::MAX);
    if prefix_len > file_len {
        return Err(UserElfError::PrefixLongerThanFile);
    }
    let ehdr = EHDR_SIZE as u64;
    if prefix_len < ehdr {
        return Err(if file_len < ehdr {
            UserElfError::Header(ElfError::MalformedHeader)
        } else {
            UserElfError::PrefixTooShort {
                needed: EHDR_SIZE as u32,
            }
        });
    }
    match header(prefix) {
        Ok(h) if h.phnum > MAX_PHDRS => Err(UserElfError::PhdrCountExceeded(h.phnum)),
        Ok(h) => Ok(h),
        // `header` requires the whole program-header table inside the slice it is given.
        // Given a prefix, that fails also when the table is in the file but past the
        // prefix: tell the two apart from the raw fields.
        Err(ElfError::MalformedProgramHeaders) => {
            let field = |at: usize| {
                prefix
                    .get(at..at + 2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
            };
            let (phentsize, phnum) = (field(42).unwrap_or(0), field(44).unwrap_or(0));
            let phoff = prefix
                .get(28..32)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            let malformed = phnum == PN_XNUM || (phnum != 0 && phentsize != PHDR_SIZE);
            if malformed {
                return Err(UserElfError::Header(ElfError::MalformedProgramHeaders));
            }
            if phnum > MAX_PHDRS {
                return Err(UserElfError::PhdrCountExceeded(phnum));
            }
            let table_end = u64::from(phoff) + u64::from(phnum) * u64::from(PHDR_SIZE);
            if table_end <= file_len {
                Err(UserElfError::PrefixTooShort {
                    needed: narrow(table_end),
                })
            } else {
                Err(UserElfError::Header(ElfError::MalformedProgramHeaders))
            }
        }
        Err(e) => Err(UserElfError::Header(e)),
    }
}

/// Reads `p_flags` of program header `index`, which the header check placed inside the
/// prefix.
fn p_flags(prefix: &[u8], phoff: u32, index: u16) -> Result<u32, UserElfError> {
    let at = u64::from(phoff) + u64::from(index) * u64::from(PHDR_SIZE) + P_FLAGS;
    usize::try_from(at)
        .ok()
        .and_then(|at| prefix.get(at..at.checked_add(4)?))
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(UserElfError::Header(ElfError::MalformedProgramHeaders))
}

/// Validates a user executable and plans its mapping.
///
/// `elf_prefix` is the first bytes of the ELF file, at least through the ELF header and
/// the program-header table; `file_len` is the whole file's length. `user_range` is the
/// virtual range segments may occupy (`[USER_BASE, STACK_TOP − STACK_PAGES·4096)` in
/// `docs/m3-design.md` §11.2); both ends must be page-aligned. The rules are those of
/// `docs/m3-design.md` §8.3, each described with its error in [`UserElfError`].
///
/// The parser never needs the segment bytes. In M3 the kernel has the whole file in its
/// staging area (§8.2) and passes the start of it as the prefix: the ELF header and
/// program-header table (the design's `headers`); it then copies the [`FileCopy`] ranges
/// from staging itself. A caller that does not know the table's position can pass the
/// first 52 bytes and, on [`UserElfError::PrefixTooShort`], retry with the `needed`
/// bytes.
///
/// # Errors
///
/// Returns a [`UserElfError`] naming the first check that fails.
pub fn parse_user_elf32(
    elf_prefix: &[u8],
    file_len: u32,
    user_range: Range<u32>,
) -> Result<UserImage, UserElfError> {
    let aligned = |v: u32| v.is_multiple_of(PAGE_SIZE);
    if user_range.is_empty() || !aligned(user_range.start) || !aligned(user_range.end) {
        return Err(UserElfError::InvalidUserRange);
    }
    let header = checked_header(elf_prefix, file_len)?;
    let mut checked = Vec::new();
    for index in 0..header.phnum {
        let ph = program_header(elf_prefix, &header, index)?;
        if ph.p_type != PT_LOAD {
            continue;
        }
        if ph.filesz > ph.memsz {
            return Err(UserElfError::InvalidSegmentSize { index });
        }
        if u64::from(ph.offset) + u64::from(ph.filesz) > u64::from(file_len) {
            return Err(UserElfError::FileRangeOutsideFile { index });
        }
        if ph.memsz == 0 {
            continue;
        }
        let flags = p_flags(elf_prefix, header.phoff, index)?;
        let perms = Perms {
            read: flags & PF_R != 0,
            write: flags & PF_W != 0,
            execute: flags & PF_X != 0,
        };
        if !perms.read && !perms.write && !perms.execute {
            return Err(UserElfError::NoPermission { index });
        }
        if perms.write && !perms.read {
            return Err(UserElfError::WriteWithoutRead { index });
        }
        let segment = Checked {
            index,
            offset: ph.offset,
            vaddr: ph.vaddr,
            filesz: ph.filesz,
            memsz: ph.memsz,
            perms,
        };
        if ph.vaddr < user_range.start || segment.end() > u64::from(user_range.end) {
            return Err(UserElfError::SegmentOutsideUserRange { index });
        }
        checked.push(segment);
    }
    checked.sort_by_key(|s| s.vaddr);
    // Sorted by address, a segment that overlaps or shares a page with any later one also
    // does so with the next one, so adjacent pairs are enough.
    for pair in checked.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let (first, second) = (a.index.min(b.index), a.index.max(b.index));
        if u64::from(b.vaddr) < a.end() {
            return Err(UserElfError::SegmentOverlap { first, second });
        }
        if b.first_page() <= a.last_page() {
            return Err(UserElfError::SharedPage { first, second });
        }
    }
    let entry = header.entry;
    if !entry.is_multiple_of(4) {
        return Err(UserElfError::MisalignedEntry(entry));
    }
    let runs_entry =
        |s: &Checked| s.perms.execute && (u64::from(s.vaddr)..s.end()).contains(&u64::from(entry));
    if !checked.iter().any(runs_entry) {
        return Err(UserElfError::EntryNotExecutable(entry));
    }
    Ok(UserImage {
        entry,
        segments: checked
            .iter()
            .map(|s| UserSegment {
                index: s.index,
                vaddr: s.vaddr,
                memsz: s.memsz,
                perms: s.perms,
                pages: s.pages(),
            })
            .collect(),
    })
}
