//! Property tests for `parse_user_elf32` and the boot execution table: no input panics,
//! valid files parse to the writer's oracle, one injected defect gives exactly its error,
//! and a table round-trips through `encode`.

mod user_common;

use proptest::prelude::*;
use systemscope_elf::{
    ElfError, ExecEntry, ExecTable, MAX_ENTRIES, MAX_PHDRS, UserElfError, parse_exec_table,
    parse_user_elf32,
};
use user_common::*;

const LEGAL_FLAGS: [u32; 5] = [PF_R, PF_X, RX, RW, RW | PF_X];

/// A valid user executable: 1 to 6 `PT_LOAD`s on distinct pages near the bottom of the
/// user range, up to 3 `PT_NOTE`s, all in random table order, and an entry in the first
/// segment built (which is executable).
fn valid_elf() -> impl Strategy<Value = UserElf> {
    let segment = (
        0..4u32,
        0..1024u32,
        1..=3 * PAGE,
        0..=600u32,
        0..LEGAL_FLAGS.len(),
    );
    (
        0..16u32,
        prop::collection::vec(segment, 1..=6),
        0..=3usize,
        any::<u32>(),
    )
        .prop_flat_map(|(base, segments, notes, entry_pick)| {
            let mut page = USER.start / PAGE + base;
            let mut phdrs = Vec::new();
            let mut entry = 0;
            for (i, (gap, word, memsz, filesz, flags)) in segments.into_iter().enumerate() {
                let vaddr = (page + gap) * PAGE + word * 4;
                let memsz = memsz.max(1);
                let data: Vec<u8> = (0..filesz.min(memsz)).map(|b| b as u8).collect();
                let mut flags = LEGAL_FLAGS[flags];
                if i == 0 {
                    flags |= PF_X;
                    entry = vaddr + ((entry_pick % memsz) & !3);
                }
                phdrs.push(load(vaddr, flags, &data, memsz));
                page = (vaddr + memsz - 1) / PAGE + 1;
            }
            phdrs.extend((0..notes).map(|n| other(PT_NOTE, n as u32, &[n as u8; 3])));
            Just(phdrs)
                .prop_shuffle()
                .prop_map(move |phdrs| UserElf::new(entry, phdrs))
        })
}

fn parse(elf: &UserElf, bytes: &[u8]) -> Result<systemscope_elf::UserImage, UserElfError> {
    parse_user_elf32(&bytes[..elf.prefix_len()], bytes.len() as u32, USER)
}

/// The table indices of `elf`'s `PT_LOAD` headers.
fn loads(elf: &UserElf) -> Vec<usize> {
    (0..elf.phdrs.len())
        .filter(|&i| elf.phdrs[i].p_type == PT_LOAD)
        .collect()
}

/// A valid execution table and a disk it fits on, with its staging size.
fn valid_table() -> impl Strategy<Value = (ExecTable, u32, u32)> {
    (
        prop::collection::vec((0..4u32, 1..=64 * 1024u32), 1..=MAX_ENTRIES),
        0..8u32,
    )
        .prop_flat_map(|(files, spare)| {
            let mut lba = 1;
            let mut entries = Vec::new();
            for (gap, byte_len) in files {
                let e = ExecEntry {
                    start_lba: lba + gap,
                    byte_len,
                };
                lba = e.start_lba + e.blocks();
                entries.push(e);
            }
            let staging = entries.iter().map(|e| e.byte_len).max().unwrap_or(1);
            let capacity = lba + spare;
            Just(entries)
                .prop_shuffle()
                .prop_map(move |entries| (ExecTable { entries }, capacity, staging))
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..400),
        header in any::<bool>(),
        extra in 0..10_000u32,
        cut in any::<prop::sample::Index>(),
        range in (any::<u32>(), any::<u32>()),
    ) {
        let mut bytes = bytes;
        // Half the cases start with a valid identification, so they reach the later checks.
        if header && bytes.len() >= 52 {
            let valid = typical().bytes();
            bytes[..20].copy_from_slice(&valid[..20]);
        }
        let prefix = cut.index(bytes.len() + 1);
        let file_len = bytes.len() as u32 + extra;
        let _ = parse_user_elf32(&bytes[..prefix], file_len, USER);
        let _ = parse_user_elf32(&bytes[..prefix], file_len, range.0..range.1);
        let mut block = [0u8; 512];
        let n = bytes.len().min(512);
        block[..n].copy_from_slice(&bytes[..n]);
        let _ = parse_exec_table(&block, range.0, range.1);
    }

    #[test]
    fn a_valid_file_parses_to_the_writers_image(elf in valid_elf()) {
        let bytes = elf.bytes();
        prop_assert_eq!(parse(&elf, &bytes), Ok(elf.expected()));
        // Changing p_paddr changes nothing.
        let mut moved = bytes.clone();
        for i in loads(&elf) {
            put_u32(&mut moved, ph(i, P_PADDR), !elf.phdrs[i].paddr);
        }
        prop_assert_eq!(parse(&elf, &moved), Ok(elf.expected()));
    }

    #[test]
    fn one_injected_defect_gives_exactly_its_error(
        elf in valid_elf(),
        pick in any::<prop::sample::Index>(),
        other_pick in any::<prop::sample::Index>(),
        defect in 0..8u8,
        value in any::<u16>(),
    ) {
        let mut bytes = elf.bytes();
        let loads = loads(&elf);
        let i = loads[pick.index(loads.len())];
        let index = i as u16;
        let expected = match defect {
            0 => {
                put_u32(&mut bytes, ph(i, P_FLAGS), 0);
                UserElfError::NoPermission { index }
            }
            1 => {
                put_u32(&mut bytes, ph(i, P_FLAGS), PF_W | (u32::from(value) & PF_X));
                UserElfError::WriteWithoutRead { index }
            }
            2 => {
                // p_filesz one past p_memsz, still inside the file.
                let memsz = elf.phdrs[i].memsz;
                put_u32(&mut bytes, ph(i, P_FILESZ), memsz + 1);
                bytes.resize(bytes.len() + memsz as usize + 1, 0);
                UserElfError::InvalidSegmentSize { index }
            }
            3 => {
                put_u32(&mut bytes, ph(i, P_VADDR), USER.start - 1 - u32::from(value));
                UserElfError::SegmentOutsideUserRange { index }
            }
            4 => {
                let machine = if value == 243 { 244 } else { value };
                put_u16(&mut bytes, E_MACHINE, machine);
                UserElfError::Header(ElfError::UnsupportedMachine(machine))
            }
            5 => {
                let phnum = (MAX_PHDRS + 1).max(value).min(0xfffe);
                put_u16(&mut bytes, E_PHNUM, phnum);
                UserElfError::PhdrCountExceeded(phnum)
            }
            6 => {
                let entry = elf.entry + 1 + u32::from(value % 3);
                put_u32(&mut bytes, E_ENTRY, entry);
                UserElfError::MisalignedEntry(entry)
            }
            _ => {
                // Another segment moved onto this one's address.
                prop_assume!(loads.len() >= 2);
                let j = loads[other_pick.index(loads.len())];
                prop_assume!(j != i);
                put_u32(&mut bytes, ph(j, P_VADDR), elf.phdrs[i].vaddr);
                UserElfError::SegmentOverlap {
                    first: i.min(j) as u16,
                    second: i.max(j) as u16,
                }
            }
        };
        prop_assert_eq!(parse(&elf, &bytes), Err(expected));
    }

    #[test]
    fn an_exec_table_round_trips((table, capacity, staging) in valid_table()) {
        let block = table.encode().unwrap();
        prop_assert_eq!(parse_exec_table(&block, capacity, staging), Ok(table));
    }
}
