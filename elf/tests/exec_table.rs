//! Unit tests for the boot execution table (`docs/m3-design.md` §8.1). Blocks are built
//! here with `to_le_bytes` at the documented offsets, independently of `encode`.

use systemscope_elf::{
    BLOCK_SIZE, ExecEntry, ExecTable, ExecTableError, MAGIC, MAX_ENTRIES, parse_exec_table,
};

const CAPACITY: u32 = 64;
const STAGING: u32 = 16 * 1024;

fn put(block: &mut [u8; BLOCK_SIZE], at: usize, v: u32) {
    block[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// A block listing `entries` as `(start_lba, byte_len)`.
fn block(entries: &[(u32, u32)]) -> [u8; BLOCK_SIZE] {
    let mut b = [0; BLOCK_SIZE];
    b[..4].copy_from_slice(b"SSX0");
    put(&mut b, 0x08, entries.len() as u32);
    for (i, &(lba, len)) in entries.iter().enumerate() {
        put(&mut b, 0x10 + 16 * i, lba);
        put(&mut b, 0x14 + 16 * i, len);
    }
    b
}

fn parse(b: &[u8; BLOCK_SIZE]) -> Result<ExecTable, ExecTableError> {
    parse_exec_table(b, CAPACITY, STAGING)
}

fn entry(start_lba: u32, byte_len: u32) -> ExecEntry {
    ExecEntry {
        start_lba,
        byte_len,
    }
}

#[test]
fn the_magic_is_ssx0() {
    assert_eq!(MAGIC.to_le_bytes(), *b"SSX0");
}

#[test]
fn a_valid_table_parses_in_table_order() {
    let b = block(&[(10, 1000), (1, 512), (3, 1)]);
    let table = parse(&b).unwrap();
    assert_eq!(table.entries, [entry(10, 1000), entry(1, 512), entry(3, 1)]);
    assert_eq!(
        table
            .entries
            .iter()
            .map(ExecEntry::blocks)
            .collect::<Vec<_>>(),
        [2, 1, 1]
    );
    assert_eq!(table.encode(), Ok(b));
}

#[test]
fn eight_entries_fill_the_table() {
    let entries: Vec<(u32, u32)> = (0..8).map(|i| (1 + 2 * i, 1024)).collect();
    assert_eq!(parse(&block(&entries)).unwrap().entries.len(), MAX_ENTRIES);
}

#[test]
fn header_errors() {
    let mut b = block(&[(1, 1)]);
    b[3] = b'1';
    assert_eq!(parse(&b), Err(ExecTableError::BadMagic(0x3158_5353)));
    let mut b = block(&[(1, 1)]);
    put(&mut b, 0x04, 1);
    assert_eq!(parse(&b), Err(ExecTableError::BadVersion(1)));
    for count in [0, 9, u32::MAX] {
        let mut b = block(&[(1, 1)]);
        put(&mut b, 0x08, count);
        assert_eq!(parse(&b), Err(ExecTableError::BadCount(count)), "{count}");
    }
    let mut b = block(&[(1, 1)]);
    put(&mut b, 0x0C, 1);
    assert_eq!(
        parse(&b),
        Err(ExecTableError::NonZeroReserved { offset: 0x0C })
    );
}

#[test]
fn entry_flags_and_reserved_words_must_be_zero() {
    for offset in [0x28, 0x2C] {
        let mut b = block(&[(1, 1), (2, 1)]);
        put(&mut b, offset, 0x8000_0000);
        assert_eq!(
            parse(&b),
            Err(ExecTableError::NonZeroReserved { offset }),
            "{offset:#x}"
        );
    }
}

#[test]
fn unused_slots_and_the_tail_must_be_zero() {
    // A leftover entry after the count.
    let mut b = block(&[(1, 1), (2, 1)]);
    put(&mut b, 0x08, 1);
    assert_eq!(
        parse(&b),
        Err(ExecTableError::NonZeroUnusedEntry { slot: 1 })
    );
    let mut b = block(&[(1, 1)]);
    b[0x10 + 16 * 7 + 15] = 1;
    assert_eq!(
        parse(&b),
        Err(ExecTableError::NonZeroUnusedEntry { slot: 7 })
    );
    for at in [0x90, BLOCK_SIZE - 1] {
        let mut b = block(&[(1, 1)]);
        b[at] = 0xff;
        assert_eq!(
            parse(&b),
            Err(ExecTableError::NonZeroUnusedEntry { slot: MAX_ENTRIES }),
            "{at:#x}"
        );
    }
}

#[test]
fn an_entry_at_block_0_is_rejected() {
    assert_eq!(
        parse(&block(&[(1, 1), (0, 1)])),
        Err(ExecTableError::LbaZero { entry: 1 })
    );
}

#[test]
fn an_empty_file_is_rejected() {
    assert_eq!(
        parse(&block(&[(1, 0)])),
        Err(ExecTableError::EmptyFile { entry: 0 })
    );
}

#[test]
fn a_file_larger_than_the_staging_area_is_rejected() {
    assert_eq!(parse(&block(&[(1, STAGING)])).map(|_| ()), Ok(()));
    assert_eq!(
        parse(&block(&[(1, STAGING + 1)])),
        Err(ExecTableError::TooLarge { entry: 0 })
    );
}

#[test]
fn a_file_past_the_end_of_the_disk_is_rejected() {
    // Blocks 62 and 63 are the last two of a 64-block disk.
    assert_eq!(parse(&block(&[(62, 1024)])).map(|_| ()), Ok(()));
    assert_eq!(
        parse(&block(&[(62, 1025)])),
        Err(ExecTableError::OutsideDisk { entry: 0 })
    );
    // A start near 4 GiB must not wrap.
    assert_eq!(
        parse_exec_table(&block(&[(u32::MAX, 512)]), u32::MAX, STAGING),
        Err(ExecTableError::OutsideDisk { entry: 0 })
    );
}

#[test]
fn overlapping_files_are_rejected() {
    // 1000 bytes occupy blocks 5 and 6.
    assert_eq!(
        parse(&block(&[(5, 1000), (7, 1), (6, 1)])),
        Err(ExecTableError::Overlap {
            first: 0,
            second: 2
        })
    );
    assert_eq!(
        parse(&block(&[(9, 1), (5, 5000)])),
        Err(ExecTableError::Overlap {
            first: 0,
            second: 1
        })
    );
    // Adjacent files are fine.
    assert_eq!(parse(&block(&[(5, 1000), (7, 1)])).map(|_| ()), Ok(()));
}

#[test]
fn encode_checks_only_the_count() {
    assert_eq!(
        ExecTable { entries: vec![] }.encode(),
        Err(ExecTableError::BadCount(0))
    );
    assert_eq!(
        ExecTable {
            entries: vec![entry(1, 1); 9]
        }
        .encode(),
        Err(ExecTableError::BadCount(9))
    );
    // Invalid entries encode, and parsing rejects them.
    let b = ExecTable {
        entries: vec![entry(0, 0)],
    }
    .encode()
    .unwrap();
    assert_eq!(parse(&b), Err(ExecTableError::LbaZero { entry: 0 }));
}

#[test]
fn errors_display_their_cause() {
    assert!(
        ExecTableError::Overlap {
            first: 2,
            second: 5
        }
        .to_string()
        .contains("2 and 5")
    );
    assert!(
        ExecTableError::BadMagic(1)
            .to_string()
            .contains("0x00000001")
    );
}
