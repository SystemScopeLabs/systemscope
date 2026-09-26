//! Test support for booting from disk (`docs/m3-design.md` §8.1, §8.2, §8.3). Test-only.
//!
//! - [`MockBlk`] is a block controller written from the register map and the command
//!   rules of m2-design §9 (`COMMAND`, `STATUS`, `LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `ACK`),
//!   independently of `systemscope-platform`'s: a READ copies whole blocks from its disk
//!   into the harness memory, `STATUS` reads `BUSY` for a chosen number of reads and then
//!   `DONE` with an error code, and `ACK` returns it to idle. Any other access, a second
//!   command, or `IRQ_ENABLE` fails the test: the kernel is a polling driver.
//! - [`table`] and [`disk`] write an `SSX0` table and a disk from the §8.1 layout,
//!   without `ExecTable::encode`.
//! - [`expect`] is the boot oracle: what boot must do with a disk, from §8.1–§8.3 and the
//!   frame model, using only the pure `systemscope-elf` validators and never the kernel's
//!   boot state machine.

#![allow(dead_code)]

use std::collections::BTreeMap;

use systemscope_elf::{BLOCK_SIZE, UserElfError, parse_exec_table, parse_user_elf32};
use systemscope_os::{BootImage, UserLayout};

use super::layout::*;
use super::procs::{PageMem, model};

/// The M2 controller's register offsets and bits (m2-design §9.2).
pub const COMMAND: u64 = 0x00;
pub const STATUS: u64 = 0x04;
pub const LBA: u64 = 0x08;
pub const MEM_ADDR: u64 = 0x0C;
pub const BLOCK_COUNT: u64 = 0x10;
pub const IRQ_ENABLE: u64 = 0x14;
pub const ACK: u64 = 0x18;
pub const BUSY: u32 = 1;
pub const DONE: u32 = 2;

/// One transfer as the mock saw its command: `(lba, blocks, mem_addr)`.
pub type Transfer = (u32, u32, u32);

/// The mock controller's command state.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Busy {
    Idle,
    /// `STATUS` reads left before `DONE`, and the error the transfer ends with.
    Reading {
        reads: u32,
        error: u8,
    },
    Done {
        error: u8,
    },
}

/// An independent block controller behind the harness (see the module docs).
#[derive(Clone, Debug)]
pub struct MockBlk {
    /// The disk, a whole number of blocks.
    pub disk: Vec<u8>,
    /// `STATUS` reads that return `BUSY` after each command.
    pub delay: u32,
    /// The error code transfer `i` (counted from 0) ends with; absent means 0.
    pub errors: BTreeMap<usize, u8>,
    /// Every command, in order.
    pub transfers: Vec<Transfer>,
    /// `STATUS` reads.
    pub polls: usize,
    lba: u32,
    mem_addr: u32,
    count: u32,
    busy: Busy,
}

impl MockBlk {
    pub fn new(disk: Vec<u8>) -> MockBlk {
        assert!(disk.len().is_multiple_of(BLOCK_SIZE));
        MockBlk {
            disk,
            delay: 0,
            errors: BTreeMap::new(),
            transfers: Vec::new(),
            polls: 0,
            lba: 0,
            mem_addr: 0,
            count: 0,
            busy: Busy::Idle,
        }
    }

    /// Whether the controller is between a command and its `ACK`.
    pub fn in_flight(&self) -> bool {
        self.busy != Busy::Idle
    }

    /// A read of `len` bytes at register `offset`.
    pub fn read(&mut self, offset: u64, len: usize, mem: &mut PageMem) -> Vec<u8> {
        assert_eq!((offset, len), (STATUS, 4), "the kernel reads only STATUS");
        self.polls += 1;
        let status = match self.busy.clone() {
            Busy::Idle => 0,
            Busy::Reading { reads, error } if reads > 0 => {
                self.busy = Busy::Reading {
                    reads: reads - 1,
                    error,
                };
                BUSY
            }
            Busy::Reading { error, .. } => {
                if error == 0 {
                    let from = self.lba as usize * BLOCK_SIZE;
                    let len = self.count as usize * BLOCK_SIZE;
                    mem.write(u64::from(self.mem_addr), &self.disk[from..from + len]);
                }
                self.busy = Busy::Done { error };
                DONE | u32::from(error) << 8
            }
            Busy::Done { error } => DONE | u32::from(error) << 8,
        };
        status.to_le_bytes().to_vec()
    }

    /// A write of `data` at register `offset`.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        assert_eq!(data.len(), 4, "the kernel writes whole registers");
        let value = u32::from_le_bytes(data.try_into().unwrap());
        match offset {
            LBA => self.lba = value,
            MEM_ADDR => self.mem_addr = value,
            BLOCK_COUNT => self.count = value,
            COMMAND => {
                assert_eq!(value, 1, "the kernel only reads");
                assert_eq!(self.busy, Busy::Idle, "a command while one is in flight");
                let error = self.errors.get(&self.transfers.len()).copied().unwrap_or(0);
                let fits = self.count >= 1
                    && (u64::from(self.lba) + u64::from(self.count)) * BLOCK_SIZE as u64
                        <= self.disk.len() as u64;
                assert!(fits || error != 0, "the kernel reads inside the disk");
                self.transfers.push((self.lba, self.count, self.mem_addr));
                self.busy = Busy::Reading {
                    reads: self.delay,
                    error,
                };
            }
            ACK => {
                assert_eq!(value, 1, "ACK acknowledges DONE");
                assert!(matches!(self.busy, Busy::Done { .. }), "ACK before DONE");
                self.busy = Busy::Idle;
            }
            IRQ_ENABLE => panic!("the kernel never enables the controller's interrupt"),
            other => panic!("a write to register {other:#x}"),
        }
    }
}

/// An `SSX0` table block for `entries` of `(start_lba, byte_len)`, from §8.1.
pub fn table(entries: &[(u32, u32)]) -> [u8; BLOCK_SIZE] {
    let mut b = [0u8; BLOCK_SIZE];
    b[0..4].copy_from_slice(&0x3058_5353u32.to_le_bytes());
    b[8..12].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    for (i, &(lba, len)) in entries.iter().enumerate() {
        let at = 0x10 + 16 * i;
        b[at..at + 4].copy_from_slice(&lba.to_le_bytes());
        b[at + 4..at + 8].copy_from_slice(&len.to_le_bytes());
    }
    b
}

/// A disk of `blocks` blocks: `files` packed from LBA 1, their table at LBA 0.
pub fn disk(files: &[Vec<u8>], blocks: u32) -> Vec<u8> {
    let mut entries = Vec::new();
    let mut lba = 1u32;
    for f in files {
        entries.push((lba, f.len() as u32));
        lba += (f.len() as u32).div_ceil(BLOCK_SIZE as u32);
    }
    disk_with(&table(&entries), &entries, files, blocks)
}

/// A disk of `blocks` blocks with `block0` at LBA 0 and each file at its entry's LBA.
pub fn disk_with(
    block0: &[u8; BLOCK_SIZE],
    entries: &[(u32, u32)],
    files: &[Vec<u8>],
    blocks: u32,
) -> Vec<u8> {
    let mut d = vec![0u8; blocks as usize * BLOCK_SIZE];
    d[..BLOCK_SIZE].copy_from_slice(block0);
    for (&(lba, _), f) in entries.iter().zip(files) {
        let at = lba as usize * BLOCK_SIZE;
        let end = (at + f.len()).min(d.len());
        d[at..end].copy_from_slice(&f[..end - at]);
    }
    d
}

/// What the oracle says boot does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// The table is unusable: shutdown reason 1 with a detail starting with the text.
    Fail(String),
    /// The table is valid.
    Booted {
        /// `entries` of `os.boot`.
        entries: usize,
        /// Each entry's `os.process.create`: `Ok(root frame address)` or `Err(error)`.
        creates: Vec<Result<u64, String>>,
        /// The transfers commanded: block 0, then every entry, in table order.
        transfers: Vec<Transfer>,
        /// The first PID dispatched, or `None` if no process was created.
        first: Option<u32>,
    },
}

/// The boot oracle: what a kernel with the M3 layout, staging of `staging_size` bytes,
/// and a pool of `pool` frames does with `disk` (`capacity` blocks), when transfer `i`
/// ends with error `errors[i]`.
pub fn expect(
    disk: &[u8],
    capacity: u32,
    staging_size: u32,
    pool: usize,
    errors: &BTreeMap<usize, u8>,
) -> Boot {
    let staging = STAGING as u32;
    let mut transfers = vec![(0, 1, staging)];
    if let Some(&e) = errors.get(&0) {
        return Boot::Fail(format!(
            "block controller error {e} reading the executable table"
        ));
    }
    let block0: [u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().unwrap();
    let table = match parse_exec_table(&block0, capacity, staging_size) {
        Err(e) => return Boot::Fail(format!("invalid executable table: {e}")),
        Ok(t) => t,
    };
    let layout = UserLayout::M3;
    let mut owners = vec![0u32; pool];
    let mut creates = Vec::new();
    for (i, e) in table.entries.iter().enumerate() {
        let pid = i as u32 + 1;
        transfers.push((e.start_lba, e.blocks(), staging));
        if let Some(&code) = errors.get(&(i + 1)) {
            creates.push(Err(format!("block controller error {code}")));
            continue;
        }
        let at = e.start_lba as usize * BLOCK_SIZE;
        let file = &disk[at..at + e.byte_len as usize];
        // The whole file is the longest prefix: a parse of all of it is what the
        // header-prefix retry converges to, with every error but PrefixTooShort.
        let image = match parse_user_elf32(file, e.byte_len, layout.image_range()) {
            Ok(image) => image,
            Err(UserElfError::PrefixTooShort { .. }) => unreachable!("the whole file"),
            Err(err) => {
                creates.push(Err(err.to_string()));
                continue;
            }
        };
        let mmio = (UART_BASE >> 22) as u32;
        let kernel = (RAM_BASE >> 22) as u32;
        if image
            .segments
            .iter()
            .flat_map(|s| &s.pages)
            .any(|p| p.va >> 22 == mmio || p.va >> 22 == kernel)
        {
            creates.push(Err("a segment is in a megapage slot".to_owned()));
            continue;
        }
        let boot = BootImage {
            staged: staging,
            file_len: e.byte_len,
            image,
        };
        let n = model::needed(&boot, &layout);
        let free: Vec<usize> = (0..pool).filter(|&f| owners[f] == 0).take(n).collect();
        if free.len() < n {
            creates.push(Err("frame pool exhausted".to_owned()));
            continue;
        }
        for &f in &free {
            owners[f] = pid;
        }
        creates.push(Ok(POOL + 4096 * free[0] as u64));
    }
    let first = creates.iter().position(Result::is_ok).map(|i| i as u32 + 1);
    Boot::Booted {
        entries: table.entries.len(),
        creates,
        transfers,
        first,
    }
}
