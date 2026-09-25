//! Unit tests for `parse_user_elf32` (`docs/m3-design.md` §8.3): one test per rule, each
//! on a file written by the independent writer in `tests/user_common`.

mod user_common;

use systemscope_elf::{
    ElfError, FileCopy, MAX_PHDRS, PagePlan, Perms, UserElfError, UserImage, parse_user_elf32,
};
use user_common::*;

/// Parses `bytes` given its first `prefix` bytes, over the M3 user range.
fn parse_prefix(bytes: &[u8], prefix: usize) -> Result<UserImage, UserElfError> {
    parse_user_elf32(&bytes[..prefix], bytes.len() as u32, USER)
}

/// Parses `bytes`, whose header and table are those of `elf`.
fn parse_as(elf: &UserElf, bytes: &[u8]) -> Result<UserImage, UserElfError> {
    parse_prefix(bytes, elf.prefix_len())
}

fn parse(elf: &UserElf) -> Result<UserImage, UserElfError> {
    parse_as(elf, &elf.bytes())
}

const RX_PERMS: Perms = Perms {
    read: true,
    write: false,
    execute: true,
};
const RW_PERMS: Perms = Perms {
    read: true,
    write: true,
    execute: false,
};

#[test]
fn a_typical_program_is_planned_page_by_page() {
    let elf = typical();
    let image = parse(&elf).unwrap();
    assert_eq!(image, elf.expected());
    // Spelled out once, independently of the oracle.
    let text = elf.data_offset(0) as u32;
    let data = elf.data_offset(1) as u32;
    assert_eq!(image.entry, 0x0001_0000);
    assert_eq!(image.segments.len(), 2);
    assert_eq!(image.segments[0].perms, RX_PERMS);
    assert_eq!(
        image.segments[0].pages,
        [PagePlan {
            va: 0x0001_0000,
            perms: RX_PERMS,
            copy: Some(FileCopy {
                file_offset: text,
                page_offset: 0,
                len: 0x40
            }),
        }]
    );
    assert_eq!(
        image.segments[1].pages,
        [
            PagePlan {
                va: 0x0001_1000,
                perms: RW_PERMS,
                copy: Some(FileCopy {
                    file_offset: data,
                    page_offset: 0,
                    len: 0x10
                }),
            },
            // .bss only: zero-filled, nothing to copy.
            PagePlan {
                va: 0x0001_2000,
                perms: RW_PERMS,
                copy: None,
            },
        ]
    );
}

#[test]
fn segments_come_out_sorted_by_address_with_their_header_index() {
    let elf = UserElf::new(
        0x0002_0000,
        vec![
            load(0x0003_0000, RW, &[1; 4], 4),
            other(PT_NOTE, 0, &[9; 8]),
            load(0x0002_0000, RX, &[2; 8], 8),
        ],
    );
    let image = parse(&elf).unwrap();
    assert_eq!(image, elf.expected());
    let order: Vec<(u16, u32)> = image.segments.iter().map(|s| (s.index, s.vaddr)).collect();
    assert_eq!(order, [(2, 0x0002_0000), (0, 0x0003_0000)]);
}

#[test]
fn an_unaligned_segment_copies_into_the_middle_of_its_pages() {
    // 0x1ff0..0x2010 of file bytes straddles a page boundary.
    let elf = UserElf::new(
        0x0001_fff0,
        vec![load(0x0001_fff0, RX | PF_W, &[7; 0x20], 0x20)],
    );
    let image = parse(&elf).unwrap();
    assert_eq!(image, elf.expected());
    let off = elf.data_offset(0) as u32;
    let copies: Vec<Option<FileCopy>> = image.segments[0].pages.iter().map(|p| p.copy).collect();
    assert_eq!(
        copies,
        [
            Some(FileCopy {
                file_offset: off,
                page_offset: 0xff0,
                len: 0x10
            }),
            Some(FileCopy {
                file_offset: off + 0x10,
                page_offset: 0,
                len: 0x10
            }),
        ]
    );
}

#[test]
fn filesz_below_memsz_plans_a_zero_filled_tail() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 4], 4),
            load(0x0001_1ffc, RW, &[1; 8], 0x3000),
        ],
    );
    let image = parse(&elf).unwrap();
    assert_eq!(image, elf.expected());
    let pages = &image.segments[1].pages;
    assert_eq!(pages.len(), 4); // 0x11000, 0x12000, 0x13000, 0x14000
    assert_eq!(pages[0].copy.map(|c| c.len), Some(4));
    assert_eq!(pages[1].copy.map(|c| (c.page_offset, c.len)), Some((0, 4)));
    assert!(pages[2..].iter().all(|p| p.copy.is_none()));
}

#[test]
fn p_paddr_is_ignored() {
    let elf = typical();
    let mut bytes = elf.bytes();
    let before = parse_as(&elf, &bytes).unwrap();
    for i in 0..2 {
        put_u32(&mut bytes, ph(i, P_PADDR), 0xdead_0000 + i as u32);
    }
    assert_eq!(parse_as(&elf, &bytes), Ok(before));
}

#[test]
fn unknown_flag_bits_are_ignored() {
    let elf = typical();
    let mut bytes = elf.bytes();
    let before = parse_as(&elf, &bytes).unwrap();
    put_u32(&mut bytes, ph(0, P_FLAGS), RX | 0xf0f0_fff8);
    put_u32(&mut bytes, ph(1, P_FLAGS), RW | 0x0ff0_0008);
    assert_eq!(parse_as(&elf, &bytes), Ok(before));
}

#[test]
fn segments_at_both_ends_of_the_user_range_are_accepted() {
    let elf = UserElf::new(
        USER.start,
        vec![
            load(USER.start, RX, &[0x13; 4], PAGE),
            load(USER.end - PAGE, RW, &[], PAGE),
        ],
    );
    assert_eq!(parse(&elf), Ok(elf.expected()));
}

#[test]
fn a_segment_one_byte_past_the_user_range_is_rejected() {
    let low = UserElf::new(
        USER.start,
        vec![
            load(USER.start, RX, &[0x13; 4], 4),
            load(USER.start - 1, RW, &[], 1),
        ],
    );
    assert_eq!(
        parse(&low),
        Err(UserElfError::SegmentOutsideUserRange { index: 1 })
    );
    let high = UserElf::new(
        USER.start,
        vec![
            load(USER.start, RX, &[0x13; 4], 4),
            load(USER.end - PAGE, RW, &[], PAGE + 1),
        ],
    );
    assert_eq!(
        parse(&high),
        Err(UserElfError::SegmentOutsideUserRange { index: 1 })
    );
}

#[test]
fn a_segment_wrapping_past_4_gib_is_outside_the_user_range() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 4], 4),
            load(0x7000_0000, RW, &[], u32::MAX),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::SegmentOutsideUserRange { index: 1 })
    );
}

#[test]
fn zero_size_segments_are_ignored_wherever_they_are() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 4], 4),
            // Outside the range, overlapping, and without permission: all ignored.
            load(0, 0, &[], 0),
            load(0x0001_0000, RW, &[], 0),
        ],
    );
    let image = parse(&elf).unwrap();
    assert_eq!(image, elf.expected());
    assert_eq!(image.segments.len(), 1);
}

#[test]
fn non_load_headers_are_ignored() {
    let mut elf = typical();
    elf.phdrs.push(other(PT_NOTE, 0xffff_f000, &[1, 2, 3]));
    assert_eq!(parse(&elf), Ok(elf.expected()));
}

#[test]
fn header_errors_are_the_m1_loaders() {
    let elf = typical();
    let cases: [(usize, &[u8], ElfError); 4] = [
        (0, b"\x7fELG", ElfError::InvalidMagic),
        (EI_CLASS, &[2], ElfError::UnsupportedClass(2)),
        (E_TYPE, &3u16.to_le_bytes(), ElfError::UnsupportedType(3)),
        (
            E_MACHINE,
            &62u16.to_le_bytes(),
            ElfError::UnsupportedMachine(62),
        ),
    ];
    for (at, patch, error) in cases {
        let mut bytes = elf.bytes();
        bytes[at..at + patch.len()].copy_from_slice(patch);
        assert_eq!(
            parse_as(&elf, &bytes),
            Err(UserElfError::Header(error)),
            "{error}"
        );
    }
}

#[test]
fn big_endian_is_rejected() {
    let elf = typical();
    let mut bytes = elf.bytes();
    bytes[EI_DATA] = 2;
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::Header(ElfError::UnsupportedEndian(2)))
    );
}

#[test]
fn more_than_max_phdrs_is_rejected() {
    let mut phdrs = vec![load(0x0001_0000, RX, &[0x13; 4], 4)];
    phdrs.extend((1..=MAX_PHDRS).map(|_| other(PT_NOTE, 0, &[])));
    let elf = UserElf::new(0x0001_0000, phdrs);
    assert_eq!(
        parse(&elf),
        Err(UserElfError::PhdrCountExceeded(MAX_PHDRS + 1))
    );
    // Exactly MAX_PHDRS is fine.
    let mut elf = elf;
    elf.phdrs.pop();
    assert_eq!(parse(&elf), Ok(elf.expected()));
}

#[test]
fn a_huge_phnum_is_a_count_error_even_past_the_file() {
    let elf = typical();
    let mut bytes = elf.bytes();
    put_u16(&mut bytes, E_PHNUM, 0x1000);
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::PhdrCountExceeded(0x1000))
    );
}

#[test]
fn pn_xnum_and_a_bad_phentsize_are_malformed_headers() {
    let elf = typical();
    let malformed = Err(UserElfError::Header(ElfError::MalformedProgramHeaders));
    let mut bytes = elf.bytes();
    put_u16(&mut bytes, E_PHNUM, 0xffff);
    assert_eq!(parse_as(&elf, &bytes), malformed);
    let mut bytes = elf.bytes();
    put_u16(&mut bytes, E_PHENTSIZE, 56);
    assert_eq!(parse_as(&elf, &bytes), malformed);
}

#[test]
fn a_table_past_the_end_of_the_file_is_malformed() {
    let elf = typical();
    let mut bytes = elf.bytes();
    let len = bytes.len() as u32;
    put_u32(&mut bytes, E_PHOFF, len - 32);
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::Header(ElfError::MalformedProgramHeaders))
    );
}

#[test]
fn a_truncated_prefix_asks_for_the_bytes_it_needs() {
    let elf = typical();
    let bytes = elf.bytes();
    let needed = elf.prefix_len() as u32;
    for prefix in [0, 1, 51] {
        assert_eq!(
            parse_prefix(&bytes, prefix),
            Err(UserElfError::PrefixTooShort { needed: 52 }),
            "{prefix}"
        );
    }
    for prefix in [52, 53, elf.prefix_len() - 1] {
        assert_eq!(
            parse_prefix(&bytes, prefix),
            Err(UserElfError::PrefixTooShort { needed }),
            "{prefix}"
        );
    }
    // A longer prefix, up to the whole file, gives the same image.
    for prefix in [elf.prefix_len(), elf.prefix_len() + 1, bytes.len()] {
        assert_eq!(parse_prefix(&bytes, prefix), Ok(elf.expected()));
    }
}

#[test]
fn a_file_shorter_than_the_header_is_malformed() {
    let bytes = typical().bytes();
    assert_eq!(
        parse_user_elf32(&bytes[..20], 20, USER),
        Err(UserElfError::Header(ElfError::MalformedHeader))
    );
}

#[test]
fn a_prefix_longer_than_the_file_is_rejected() {
    let bytes = typical().bytes();
    assert_eq!(
        parse_user_elf32(&bytes, bytes.len() as u32 - 1, USER),
        Err(UserElfError::PrefixLongerThanFile)
    );
}

#[test]
fn an_invalid_user_range_is_rejected() {
    let elf = typical();
    let bytes = elf.bytes();
    let prefix = &bytes[..elf.prefix_len()];
    let len = bytes.len() as u32;
    let ranges = [
        0x0001_0000..0x0001_0000,
        std::ops::Range {
            start: 0x0002_0000,
            end: 0x0001_0000,
        },
        0x0001_0001..0x7fff_b000,
        0x0001_0000..0x7fff_b001,
    ];
    for range in ranges {
        assert_eq!(
            parse_user_elf32(prefix, len, range.clone()),
            Err(UserElfError::InvalidUserRange),
            "{range:?}"
        );
    }
}

#[test]
fn filesz_above_memsz_is_rejected() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 4], 4),
            load(0x0001_1000, RW, &[1; 8], 4),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::InvalidSegmentSize { index: 1 })
    );
}

#[test]
fn a_file_range_past_the_file_is_rejected() {
    let elf = typical();
    let mut bytes = elf.bytes();
    let len = bytes.len() as u32;
    put_u32(&mut bytes, ph(1, P_OFFSET), len - 8);
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::FileRangeOutsideFile { index: 1 })
    );
    // An offset near 4 GiB must not wrap.
    put_u32(&mut bytes, ph(1, P_OFFSET), u32::MAX - 4);
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::FileRangeOutsideFile { index: 1 })
    );
}

#[test]
fn a_segment_without_permission_is_rejected() {
    let elf = typical();
    let mut bytes = elf.bytes();
    // Unknown bits alone are no permission.
    put_u32(&mut bytes, ph(1, P_FLAGS), 0xff00_0008);
    assert_eq!(
        parse_as(&elf, &bytes),
        Err(UserElfError::NoPermission { index: 1 })
    );
}

#[test]
fn write_without_read_is_rejected() {
    for flags in [PF_W, PF_W | PF_X] {
        let elf = typical();
        let mut bytes = elf.bytes();
        put_u32(&mut bytes, ph(1, P_FLAGS), flags);
        assert_eq!(
            parse_as(&elf, &bytes),
            Err(UserElfError::WriteWithoutRead { index: 1 }),
            "{flags:#x}"
        );
    }
}

#[test]
fn every_legal_permission_set_is_accepted() {
    for flags in [PF_R, PF_X, RX, RW, RW | PF_X] {
        let elf = UserElf::new(
            0x0001_0000,
            vec![load(0x0001_0000, flags | PF_X, &[0; 4], 4)],
        );
        assert_eq!(parse(&elf), Ok(elf.expected()), "{flags:#x}");
        let elf = UserElf::new(
            0x0001_0000,
            vec![
                load(0x0001_0000, RX, &[0; 4], 4),
                load(0x0002_0000, flags, &[0; 4], 4),
            ],
        );
        assert_eq!(parse(&elf), Ok(elf.expected()), "{flags:#x}");
    }
}

#[test]
fn overlapping_segments_are_rejected() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_1000, RW, &[], 0x1000),
            load(0x0001_0000, RX, &[0x13; 4], 0x1001),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::SegmentOverlap {
            first: 0,
            second: 1
        })
    );
}

#[test]
fn a_segment_inside_another_is_an_overlap() {
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 4], 0x3000),
            load(0x0001_1000, RW, &[], 0x10),
            load(0x0001_2000, RW, &[], 0x10),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::SegmentOverlap {
            first: 0,
            second: 1
        })
    );
}

#[test]
fn segments_sharing_a_page_are_rejected() {
    // Adjacent bytes, same page: the usual text/data layout without page alignment.
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0000, RX, &[0x13; 0x40], 0x40),
            load(0x0001_0040, RW, &[1; 4], 4),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::SharedPage {
            first: 0,
            second: 1
        })
    );
    // Even with the same permissions.
    let elf = UserElf::new(
        0x0001_0000,
        vec![
            load(0x0001_0ff0, RX, &[0x13; 4], 0x14),
            load(0x0001_0000, RX, &[0x13; 4], 4),
        ],
    );
    assert_eq!(
        parse(&elf),
        Err(UserElfError::SharedPage {
            first: 0,
            second: 1
        })
    );
}

#[test]
fn segments_on_neighbouring_pages_do_not_share() {
    let elf = UserElf::new(
        0x0001_0ffc,
        vec![
            load(0x0001_0ffc, RX, &[0x13; 4], 4),
            load(0x0001_1000, RW, &[1; 4], 4),
        ],
    );
    assert_eq!(parse(&elf), Ok(elf.expected()));
}

#[test]
fn a_misaligned_entry_is_rejected() {
    let mut elf = typical();
    elf.entry = 0x0001_0002;
    assert_eq!(parse(&elf), Err(UserElfError::MisalignedEntry(0x0001_0002)));
}

#[test]
fn an_entry_outside_executable_segments_is_rejected() {
    for entry in [
        0x0001_1000, // in the RW segment
        0x0001_0040, // just past the RX segment's memory
        0x0000_fffc, // below it
        0x0002_0000, // in no segment
    ] {
        let mut elf = typical();
        elf.entry = entry;
        assert_eq!(
            parse(&elf),
            Err(UserElfError::EntryNotExecutable(entry)),
            "{entry:#x}"
        );
    }
}

#[test]
fn an_entry_in_bss_of_an_executable_segment_is_accepted() {
    let elf = UserElf::new(0x0001_0ffc, vec![load(0x0001_0000, RX, &[0x13; 4], 0x1000)]);
    assert_eq!(parse(&elf), Ok(elf.expected()));
}

#[test]
fn a_file_without_loadable_segments_has_no_executable_entry() {
    let elf = UserElf::new(0x0001_0000, vec![other(PT_NOTE, 0, &[1])]);
    assert_eq!(
        parse(&elf),
        Err(UserElfError::EntryNotExecutable(0x0001_0000))
    );
}

#[test]
fn errors_display_their_cause() {
    let cases = [
        (
            UserElfError::Header(ElfError::InvalidMagic),
            ElfError::InvalidMagic.to_string(),
        ),
        (
            UserElfError::PrefixTooShort { needed: 116 },
            "116".to_string(),
        ),
        (UserElfError::PhdrCountExceeded(17), "17".to_string()),
        (
            UserElfError::SharedPage {
                first: 1,
                second: 3,
            },
            "share a page".to_string(),
        ),
        (
            UserElfError::EntryNotExecutable(0x1234),
            "0x00001234".to_string(),
        ),
    ];
    for (error, needle) in cases {
        assert!(error.to_string().contains(&needle), "{error}");
    }
    assert_eq!(
        UserElfError::from(ElfError::MalformedHeader),
        UserElfError::Header(ElfError::MalformedHeader)
    );
}
