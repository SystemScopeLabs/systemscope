//! Booting from disk (`docs/m3-design.md` §8.1, §8.2): the block controller registers the
//! kernel drives and where boot is on the disk.
//!
//! The kernel runs the M2 block controller (m2-design §9) exactly as a polling driver
//! would, through its own bus accesses: it programs one READ into staging, reads `STATUS`
//! once per kernel cycle until `DONE`, and writes `ACK`. `IRQ_ENABLE` is never written, so
//! the controller's line stays low. The first transfer is block 0, the executable table;
//! then each entry's blocks in table order, each read overwriting the last one's staging
//! bytes.
//!
//! The register offsets and bits are the controller's own (m2-design §9.2), repeated here
//! because this crate depends only on `contracts` and `elf`; the `m3-reference` tests
//! check them against `systemscope-platform`'s.

use systemscope_elf::{BLOCK_SIZE, ExecEntry, ExecTable};

/// Offset of `COMMAND`.
pub const COMMAND: u64 = 0x00;
/// Offset of `STATUS`.
pub const STATUS: u64 = 0x04;
/// Offset of `LBA`.
pub const LBA: u64 = 0x08;
/// Offset of `MEM_ADDR`.
pub const MEM_ADDR: u64 = 0x0C;
/// Offset of `BLOCK_COUNT`.
pub const BLOCK_COUNT: u64 = 0x10;
/// Offset of `ACK`.
pub const ACK: u64 = 0x18;
/// `COMMAND` value of a READ.
pub const OP_READ: u32 = 1;
/// `STATUS.DONE`.
pub const STATUS_DONE: u32 = 1 << 1;
/// The shift of `STATUS.ERROR`, bits `[15:8]`.
pub const STATUS_ERROR_SHIFT: u32 = 8;
/// The value written to `ACK`: bit 0 acknowledges `DONE`.
pub const ACK_DONE: u32 = 1;
/// The size of an ELF32 header: the first bytes of an executable the kernel reads before
/// it knows where the program-header table is (§8.3).
pub const ELF_HEADER_BYTES: u32 = 52;

/// Where boot is on the disk: block 0, or the entry being loaded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cursor {
    /// The validated executable table, or `None` while block 0 is being read. It is the
    /// boot operation's working data and is dropped when boot ends (§6.8).
    pub table: Option<ExecTable>,
    /// The entry being loaded: its index in the table. 0 while block 0 is being read.
    pub index: usize,
}

impl Cursor {
    /// The first transfer: block 0.
    pub fn table() -> Cursor {
        Cursor {
            table: None,
            index: 0,
        }
    }

    /// The entry being loaded, or `None` while block 0 is being read.
    pub fn entry(&self) -> Option<ExecEntry> {
        self.table.as_ref().map(|t| t.entries[self.index])
    }

    /// The PID the entry being loaded becomes: its index + 1 (§6.4).
    pub fn pid(&self) -> u32 {
        self.index as u32 + 1
    }

    /// The transfer to program: the first block and the number of blocks.
    pub fn transfer(&self) -> (u32, u32) {
        self.entry().map_or((0, 1), |e| (e.start_lba, e.blocks()))
    }

    /// The number of staging bytes to read first after the transfer: block 0, or the
    /// executable's ELF header (the whole file, if it is shorter).
    pub fn first_read(&self) -> u32 {
        self.entry()
            .map_or(BLOCK_SIZE as u32, |e| e.byte_len.min(ELF_HEADER_BYTES))
    }
}
