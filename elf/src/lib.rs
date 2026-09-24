//! Host-side ELF loader for SystemScope M1 (`docs/m1-design.md` §8).
//!
//! [`load_elf32`] turns the bytes of an ELF file into a [`LoadImage`]: the initial
//! contents of the RAM and the CPU's entry point. It runs before a simulation is built and
//! is not a component: it has no ports, events, snapshot, or simulation context.
//!
//! ```text
//! ELF bytes ──▶ load_elf32(elf, ram_base, ram_size) ──▶ LoadImage
//!                                                         ├─ segments, image_hash ──▶ RAM image
//!                                                         └─ entry ─────────────────▶ CPU entry point
//! ```
//!
//! # Supported subset
//!
//! Only 32-bit, little-endian, RISC-V, statically linked executables: `ELFCLASS32`,
//! `ELFDATA2LSB`, `EV_CURRENT`, `EM_RISCV`, `ET_EXEC`. Everything else is rejected, including
//! ELF64, big-endian files, `ET_DYN` (PIE), and `ET_REL`. There is no relocation, dynamic
//! linking, or symbol lookup, and section headers are never read.
//!
//! # Loading rules
//!
//! - **Only `PT_LOAD` segments are loaded;** other program headers are ignored.
//! - Every `PT_LOAD` must have `p_filesz <= p_memsz` and a file range inside the file. One
//!   with `p_memsz == 0` places nothing and is otherwise ignored, as in Spike's loader.
//! - **Load address:** `p_vaddr`, which must equal `p_paddr`. The CPU has no MMU and runs
//!   at the addresses the program was linked for (`p_vaddr`), while bare-metal loaders
//!   such as Spike place segments at `p_paddr`; requiring both to agree keeps one meaning.
//!   GNU ld and LLD make them equal unless a linker script gives a separate load address.
//! - **The segment's memory is `p_memsz` bytes:** the `p_filesz` file bytes, then zeros up
//!   to `p_memsz` (`.bss`).
//! - Each segment's memory must fit in the 32-bit address space and lie entirely inside
//!   the RAM region `[ram_base, ram_base + ram_size)`. Segments must not overlap; adjacent
//!   ones are fine.
//! - **Output offsets are RAM-relative:** `offset = address - ram_base`. Segments are sorted
//!   by offset, never empty, and never overlap, whatever the program-header order.
//! - **The entry point** must be 4-byte aligned (no C extension) and inside a loaded
//!   segment's memory.
//! - **`image_hash`** is the BLAKE3 hash of the original ELF bytes, not of the load image,
//!   so it identifies exactly which file was run.
//!
//! All arithmetic on file values is checked or done in `u64`, so the result does not depend
//! on the host, and malformed input returns an [`ElfError`], never a panic. Memory use is
//! bounded by `ram_size`, since segments are checked against the RAM before any is built.

mod error;
mod parse;

pub use error::{ElfError, SegmentError};

use parse::{PT_LOAD, file_len, header, program_header};

/// A segment of the initial RAM contents.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoadSegment {
    /// Offset of the first byte from the start of the RAM region, not an absolute address.
    pub offset: u32,
    /// The segment's memory: its file bytes, then zeros for the rest of `p_memsz`. Never
    /// empty.
    pub bytes: Vec<u8>,
}

/// A program ready to run: what the RAM holds at reset, and where the CPU starts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoadImage {
    /// The loaded segments, sorted by offset, non-empty, and non-overlapping. RAM bytes
    /// outside them are zero.
    pub segments: Vec<LoadSegment>,
    /// The entry point, as an absolute address: 4-byte aligned and inside a segment.
    pub entry: u32,
    /// BLAKE3 of the original ELF bytes. Identical files give identical hashes; files that
    /// differ in any byte give different ones.
    pub image_hash: [u8; 32],
}

/// A validated segment before sorting.
struct Placed {
    index: u16,
    offset: u32,
    bytes: Vec<u8>,
}

impl Placed {
    /// One past the last RAM offset, in `u64` so it cannot overflow.
    fn end(&self) -> u64 {
        u64::from(self.offset) + self.bytes.len() as u64
    }
}

/// Loads an ELF32 RISC-V executable into a RAM region of `ram_size` bytes at `ram_base`.
///
/// Only the subset described in the [crate documentation](crate) is accepted. Segment
/// offsets in the result are relative to `ram_base`, and `image_hash` hashes `elf` itself.
///
/// # Errors
///
/// Returns an [`ElfError`] for an unsupported or malformed file, a segment outside the
/// RAM region or overlapping another, an invalid entry point, or a RAM region that is
/// empty or reaches past `2^32`.
pub fn load_elf32(elf: &[u8], ram_base: u32, ram_size: u32) -> Result<LoadImage, ElfError> {
    let ram_start = u64::from(ram_base);
    let ram_end = ram_start + u64::from(ram_size);
    if ram_size == 0 || ram_end > 1 << 32 {
        return Err(ElfError::InvalidRamRegion);
    }
    let header = header(elf)?;
    let mut placed = Vec::new();
    for index in 0..header.phnum {
        let ph = program_header(elf, &header, index)?;
        if ph.p_type != PT_LOAD {
            continue;
        }
        let invalid = |reason| ElfError::InvalidSegment { index, reason };
        if ph.filesz > ph.memsz {
            return Err(invalid(SegmentError::FileSizeExceedsMemSize));
        }
        let file_start = u64::from(ph.offset);
        let file_end = file_start + u64::from(ph.filesz);
        if file_end > file_len(elf) {
            return Err(invalid(SegmentError::FileRangeOutsideFile));
        }
        if ph.memsz == 0 {
            continue;
        }
        if ph.vaddr != ph.paddr {
            return Err(invalid(SegmentError::AddressMismatch));
        }
        let start = u64::from(ph.vaddr);
        let end = start + u64::from(ph.memsz);
        if end > 1 << 32 {
            return Err(invalid(SegmentError::AddressOverflow));
        }
        if start < ram_start || end > ram_end {
            return Err(ElfError::SegmentOutsideRam { index });
        }
        // Both ranges were checked above, so these conversions and the slice succeed on
        // every target that can hold the file and the RAM; they are still checked.
        let mut bytes = usize::try_from(file_start)
            .ok()
            .zip(usize::try_from(file_end).ok())
            .and_then(|(from, to)| elf.get(from..to))
            .ok_or(invalid(SegmentError::FileRangeOutsideFile))?
            .to_vec();
        let memsz = usize::try_from(ph.memsz).map_err(|_| ElfError::SegmentOutsideRam { index })?;
        bytes.resize(memsz, 0);
        placed.push(Placed {
            index,
            offset: ph.vaddr - ram_base,
            bytes,
        });
    }
    placed.sort_by_key(|p| p.offset);
    for pair in placed.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if u64::from(b.offset) < a.end() {
            return Err(ElfError::OverlappingSegments {
                first: a.index.min(b.index),
                second: a.index.max(b.index),
            });
        }
    }
    let entry = header.entry;
    if !entry.is_multiple_of(4) {
        return Err(ElfError::MisalignedEntry(entry));
    }
    let inside = |p: &Placed| {
        let start = ram_start + u64::from(p.offset);
        (start..ram_start + p.end()).contains(&u64::from(entry))
    };
    if !placed.iter().any(inside) {
        return Err(ElfError::EntryOutsideSegments(entry));
    }
    Ok(LoadImage {
        segments: placed
            .into_iter()
            .map(|p| LoadSegment {
                offset: p.offset,
                bytes: p.bytes,
            })
            .collect(),
        entry,
        image_hash: *blake3::hash(elf).as_bytes(),
    })
}
