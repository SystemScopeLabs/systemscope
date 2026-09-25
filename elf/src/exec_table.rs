//! Boot execution table parser. Not part of ELF format.
//!
//! The M3 boot disk holds this table in block 0 (LBA 0); it lists where each user
//! executable is on the disk (`docs/m3-design.md` §8.1). The modeled kernel reads it once
//! at boot and keeps the result. It lives in this crate because it is the other half of
//! the kernel's loading input, next to [`parse_user_elf32`](crate::parse_user_elf32), not
//! because it is related to ELF.
//!
//! # Layout
//!
//! All fields are little-endian `u32`s.
//!
//! | Offset          | Field      | Value                                   |
//! |-----------------|------------|-----------------------------------------|
//! | `0x00`          | magic      | [`MAGIC`] (`"SSX0"` in byte order)      |
//! | `0x04`          | version    | [`VERSION`]                             |
//! | `0x08`          | count      | `1..=`[`MAX_ENTRIES`]                   |
//! | `0x0C`          | reserved   | 0                                       |
//! | `0x10 + 16·i`   | start_lba  | first block of executable `i`, `>= 1`   |
//! | `0x14 + 16·i`   | byte_len   | file length in bytes, `>= 1`            |
//! | `0x18 + 16·i`   | flags      | 0                                       |
//! | `0x1C + 16·i`   | reserved   | 0                                       |
//!
//! Entry slots at and after `count`, and every byte from `0x90` to the end of the block,
//! are zero. An executable occupies `ceil(byte_len / 512)` blocks from `start_lba`; those
//! blocks lie inside the disk, never include block 0, and never overlap another entry's.
//! `byte_len` is at most the kernel's staging size.
//!
//! Checks run in a fixed order: magic, version, count, the header's reserved word, each
//! entry's flags and reserved words, the unused slots, the tail of the block, then each
//! entry's range in index order, then overlap.

use std::fmt;

/// The block size, in bytes.
pub const BLOCK_SIZE: usize = 512;
/// The table's magic number, the bytes `"SSX0"` read as a little-endian `u32`.
pub const MAGIC: u32 = 0x3058_5353;
/// The only supported table version.
pub const VERSION: u32 = 0;
/// The largest number of entries.
pub const MAX_ENTRIES: usize = 8;

const HEADER_SIZE: usize = 0x10;
const ENTRY_SIZE: usize = 16;
const TABLE_END: usize = HEADER_SIZE + MAX_ENTRIES * ENTRY_SIZE;

/// One executable's place on the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExecEntry {
    /// The first block.
    pub start_lba: u32,
    /// The file length, in bytes.
    pub byte_len: u32,
}

impl ExecEntry {
    /// The number of blocks the file occupies, `ceil(byte_len / 512)`.
    pub fn blocks(&self) -> u32 {
        self.byte_len.div_ceil(BLOCK_SIZE as u32)
    }

    /// One past the last block, in `u64` so it cannot overflow.
    fn end_lba(&self) -> u64 {
        u64::from(self.start_lba) + u64::from(self.blocks())
    }
}

/// A validated execution table: the executables in table order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExecTable {
    /// The entries, `1..=`[`MAX_ENTRIES`] of them.
    pub entries: Vec<ExecEntry>,
}

/// Why [`parse_exec_table`] rejected block 0, or [`ExecTable::encode`] a table. Entry
/// errors name the entry by its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecTableError {
    /// The magic number is not [`MAGIC`].
    BadMagic(u32),
    /// The version is not [`VERSION`].
    BadVersion(u32),
    /// The count is 0 or larger than [`MAX_ENTRIES`].
    BadCount(u32),
    /// A reserved word or an entry's flags word is not zero.
    NonZeroReserved {
        /// The word's byte offset in the block.
        offset: usize,
    },
    /// An entry slot at or after `count`, or a byte after the last slot, is not zero.
    NonZeroUnusedEntry {
        /// The slot index; [`MAX_ENTRIES`] for the bytes after the last slot.
        slot: usize,
    },
    /// An entry starts at block 0, the table's own block.
    LbaZero {
        /// The entry's index.
        entry: usize,
    },
    /// An entry's file is empty.
    EmptyFile {
        /// The entry's index.
        entry: usize,
    },
    /// An entry's file is larger than the staging size.
    TooLarge {
        /// The entry's index.
        entry: usize,
    },
    /// An entry's blocks go past the end of the disk.
    OutsideDisk {
        /// The entry's index.
        entry: usize,
    },
    /// Two entries share a block.
    Overlap {
        /// The lower of the two entry indices.
        first: usize,
        /// The higher of the two entry indices.
        second: usize,
    },
}

impl fmt::Display for ExecTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecTableError::BadMagic(m) => write!(f, "bad execution table magic {m:#010x}"),
            ExecTableError::BadVersion(v) => write!(f, "unsupported execution table version {v}"),
            ExecTableError::BadCount(n) => {
                write!(f, "execution table count {n} is not in 1..={MAX_ENTRIES}")
            }
            ExecTableError::NonZeroReserved { offset } => {
                write!(
                    f,
                    "reserved execution table word at {offset:#x} is not zero"
                )
            }
            ExecTableError::NonZeroUnusedEntry { slot } => {
                write!(f, "unused execution table slot {slot} is not zero")
            }
            ExecTableError::LbaZero { entry } => {
                write!(f, "execution table entry {entry} starts at block 0")
            }
            ExecTableError::EmptyFile { entry } => {
                write!(f, "execution table entry {entry} is empty")
            }
            ExecTableError::TooLarge { entry } => {
                write!(
                    f,
                    "execution table entry {entry} is larger than the staging area"
                )
            }
            ExecTableError::OutsideDisk { entry } => {
                write!(
                    f,
                    "execution table entry {entry} goes past the end of the disk"
                )
            }
            ExecTableError::Overlap { first, second } => {
                write!(f, "execution table entries {first} and {second} overlap")
            }
        }
    }
}

impl std::error::Error for ExecTableError {}

fn word(block: &[u8; BLOCK_SIZE], at: usize) -> u32 {
    u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
}

fn put(block: &mut [u8; BLOCK_SIZE], at: usize, value: u32) {
    block[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Checks each entry's range, then overlap, for a disk of `capacity_blocks` blocks and a
/// staging area of `staging_size` bytes.
fn check_entries(
    entries: &[ExecEntry],
    capacity_blocks: u32,
    staging_size: u32,
) -> Result<(), ExecTableError> {
    for (entry, e) in entries.iter().enumerate() {
        if e.start_lba == 0 {
            return Err(ExecTableError::LbaZero { entry });
        }
        if e.byte_len == 0 {
            return Err(ExecTableError::EmptyFile { entry });
        }
        if e.byte_len > staging_size {
            return Err(ExecTableError::TooLarge { entry });
        }
        if e.end_lba() > u64::from(capacity_blocks) {
            return Err(ExecTableError::OutsideDisk { entry });
        }
    }
    for (second, b) in entries.iter().enumerate() {
        for (first, a) in entries[..second].iter().enumerate() {
            if u64::from(a.start_lba) < b.end_lba() && u64::from(b.start_lba) < a.end_lba() {
                return Err(ExecTableError::Overlap { first, second });
            }
        }
    }
    Ok(())
}

/// Parses and validates block 0 of a boot disk of `capacity_blocks` blocks, for a kernel
/// whose staging area holds `staging_size` bytes. The layout is `docs/m3-design.md` §8.1,
/// and each rule is described with its error in [`ExecTableError`].
///
/// # Errors
///
/// Returns an [`ExecTableError`] naming the first check that fails.
pub fn parse_exec_table(
    block: &[u8; BLOCK_SIZE],
    capacity_blocks: u32,
    staging_size: u32,
) -> Result<ExecTable, ExecTableError> {
    let magic = word(block, 0x00);
    if magic != MAGIC {
        return Err(ExecTableError::BadMagic(magic));
    }
    let version = word(block, 0x04);
    if version != VERSION {
        return Err(ExecTableError::BadVersion(version));
    }
    let count = word(block, 0x08);
    let used = usize::try_from(count)
        .ok()
        .filter(|n| (1..=MAX_ENTRIES).contains(n))
        .ok_or(ExecTableError::BadCount(count))?;
    if word(block, 0x0C) != 0 {
        return Err(ExecTableError::NonZeroReserved { offset: 0x0C });
    }
    let slot = |i: usize| HEADER_SIZE + i * ENTRY_SIZE;
    for i in 0..used {
        for offset in [slot(i) + 8, slot(i) + 12] {
            if word(block, offset) != 0 {
                return Err(ExecTableError::NonZeroReserved { offset });
            }
        }
    }
    for i in used..MAX_ENTRIES {
        if block[slot(i)..slot(i) + ENTRY_SIZE].iter().any(|&b| b != 0) {
            return Err(ExecTableError::NonZeroUnusedEntry { slot: i });
        }
    }
    if block[TABLE_END..].iter().any(|&b| b != 0) {
        return Err(ExecTableError::NonZeroUnusedEntry { slot: MAX_ENTRIES });
    }
    let entries: Vec<ExecEntry> = (0..used)
        .map(|i| ExecEntry {
            start_lba: word(block, slot(i)),
            byte_len: word(block, slot(i) + 4),
        })
        .collect();
    check_entries(&entries, capacity_blocks, staging_size)?;
    Ok(ExecTable { entries })
}

impl ExecTable {
    /// Encodes the table as block 0, for building test disks and fixtures; the kernel only
    /// parses. Only the count is checked here, so tests can also build invalid tables: an
    /// encoded table parses back to `self` exactly when its entries pass
    /// [`parse_exec_table`]'s range and overlap checks for the given disk.
    ///
    /// # Errors
    ///
    /// Returns [`ExecTableError::BadCount`] if there are no entries or more than
    /// [`MAX_ENTRIES`].
    pub fn encode(&self) -> Result<[u8; BLOCK_SIZE], ExecTableError> {
        let count = self.entries.len();
        if !(1..=MAX_ENTRIES).contains(&count) {
            return Err(ExecTableError::BadCount(
                u32::try_from(count).unwrap_or(u32::MAX),
            ));
        }
        let mut block = [0; BLOCK_SIZE];
        put(&mut block, 0x00, MAGIC);
        put(&mut block, 0x04, VERSION);
        // count <= MAX_ENTRIES, so the cast is exact.
        put(&mut block, 0x08, count as u32);
        for (i, e) in self.entries.iter().enumerate() {
            let at = HEADER_SIZE + i * ENTRY_SIZE;
            put(&mut block, at, e.start_lba);
            put(&mut block, at + 4, e.byte_len);
        }
        Ok(block)
    }
}
