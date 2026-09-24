//! Targeted tests of `load_elf32` (`docs/m1-design.md` §8): the accepted subset, each
//! rejection, and the edges of every range check.
//!
//! Files come from the independent writer in `common`; expected load images come from its
//! oracle, never from the file bytes.

mod common;

use common::*;
use systemscope_elf::{ElfError, LoadImage, LoadSegment, SegmentError, load_elf32};

const BASE: u32 = 0x8000_0000;
const SIZE: u32 = 0x1_0000;
const END: u32 = BASE + SIZE;

fn words(n: usize) -> Vec<u8> {
    (0..n as u32 * 4).map(|i| (i as u8) ^ 0xa5).collect()
}

fn ok(elf: &Elf) -> LoadImage {
    let image = load_elf32(&elf.build(), BASE, SIZE).unwrap();
    assert_eq!(image, elf.expected(BASE));
    image
}

fn err(file: &[u8]) -> ElfError {
    load_elf32(file, BASE, SIZE).unwrap_err()
}

fn seg_err(index: u16, reason: SegmentError) -> ElfError {
    ElfError::InvalidSegment { index, reason }
}

/// A one-segment program: 16 bytes of code at the base of RAM.
fn minimal() -> Elf {
    Elf::new(BASE, vec![load(BASE, &words(4), 16)])
}

/// Code, then initialized data followed by `.bss` in a second segment.
fn text_data_bss() -> Elf {
    Elf::new(
        BASE + 0x100,
        vec![
            load(BASE + 0x100, &words(8), 32),
            load(BASE + 0x2000, &[1, 2, 3, 4, 5, 6], 0x40),
        ],
    )
}

fn patched(elf: &Elf, edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut f = elf.build();
    edit(&mut f);
    f
}

// ---- Valid files (B28) ----

#[test]
fn a_minimal_executable_loads_at_a_ram_relative_offset() {
    let image = ok(&minimal());
    assert_eq!(image.entry, BASE);
    assert_eq!(
        image.segments,
        vec![LoadSegment {
            offset: 0,
            bytes: words(4)
        }]
    );
}

#[test]
fn text_data_and_bss_load_with_bss_zero_filled() {
    let image = ok(&text_data_bss());
    assert_eq!(image.entry, BASE + 0x100);
    assert_eq!(image.segments.len(), 2);
    assert_eq!(image.segments[0].offset, 0x100);
    assert_eq!(image.segments[0].bytes, words(8));
    let data = &image.segments[1];
    assert_eq!(data.offset, 0x2000);
    assert_eq!(data.bytes.len(), 0x40);
    assert_eq!(&data.bytes[..6], &[1, 2, 3, 4, 5, 6]);
    assert!(data.bytes[6..].iter().all(|&b| b == 0), "bss must be zero");
}

#[test]
fn bss_is_zero_even_where_the_file_holds_other_bytes() {
    // The file bytes right after the data segment's p_filesz are the next segment's code,
    // not zeros: the loader must zero-fill, not copy p_memsz bytes from the file.
    let elf = Elf::new(
        BASE,
        vec![
            load(BASE + 0x1000, &[0x11; 4], 0x20),
            load(BASE, &[0xff; 64], 64),
        ],
    );
    let file = elf.build();
    assert_eq!(file[elf.data_offset(1)], 0xff);
    let image = ok(&elf);
    let bss = &image.segments[1].bytes;
    assert_eq!(&bss[..4], &[0x11; 4]);
    assert_eq!(&bss[4..], &[0; 0x1c]);
}

#[test]
fn a_segment_with_no_file_bytes_is_all_zeros() {
    let elf = Elf::new(
        BASE,
        vec![load(BASE, &words(1), 4), load(BASE + 0x800, &[], 0x100)],
    );
    let image = ok(&elf);
    assert_eq!(image.segments[1].offset, 0x800);
    assert_eq!(image.segments[1].bytes, vec![0; 0x100]);
}

#[test]
fn segments_are_sorted_by_offset_whatever_the_header_order() {
    let elf = Elf::new(
        BASE + 0x40,
        vec![
            load(BASE + 0x3000, &[3; 4], 4),
            load(BASE + 0x40, &words(2), 8),
            load(BASE + 0x1000, &[1; 4], 8),
        ],
    );
    let image = ok(&elf);
    let offsets: Vec<u32> = image.segments.iter().map(|s| s.offset).collect();
    assert_eq!(offsets, vec![0x40, 0x1000, 0x3000]);
}

#[test]
fn headers_other_than_pt_load_are_ignored() {
    // Their addresses lie outside the RAM, their vaddr and paddr differ, and they overlap
    // the loaded segment: none of that matters.
    let elf = Elf::new(
        BASE,
        vec![
            other(PT_NULL, 0, &[], 0),
            other(PT_NOTE, 0x10, &[9; 12], 12),
            load(BASE, &words(4), 16),
            other(PT_GNU_STACK, 0, &[], 0x10_0000),
            other(PT_RISCV_ATTRIBUTES, BASE, &[7; 20], 0),
        ],
    );
    let image = ok(&elf);
    assert_eq!(image.segments.len(), 1);
}

#[test]
fn an_empty_pt_load_is_ignored_wherever_it_points() {
    // p_memsz == 0: nothing to place, so its address is not checked (as in Spike's loader),
    // even outside the RAM, with vaddr != paddr, or on top of another segment.
    for (vaddr, paddr) in [(0, 0), (0xffff_fff0, 0x10), (BASE, BASE + 4), (END, END)] {
        let empty = Phdr {
            p_type: PT_LOAD,
            vaddr,
            paddr,
            data: Vec::new(),
            memsz: 0,
        };
        let elf = Elf::new(BASE, vec![empty, load(BASE, &words(4), 16)]);
        let image = ok(&elf);
        assert_eq!(image.segments.len(), 1, "{vaddr:#x}/{paddr:#x}");
    }
}

#[test]
fn an_empty_pt_load_still_needs_a_valid_file_range() {
    let elf = Elf::new(BASE, vec![load(0, &[], 0), load(BASE, &words(4), 16)]);
    let f = patched(&elf, |f| put_u32(f, Elf::ph(0, P_FILESZ), 1));
    assert_eq!(err(&f), seg_err(0, SegmentError::FileSizeExceedsMemSize));
    let f = patched(&elf, |f| put_u32(f, Elf::ph(0, P_OFFSET), 0x10_0000));
    assert_eq!(err(&f), seg_err(0, SegmentError::FileRangeOutsideFile));
}

#[test]
fn adjacent_segments_are_allowed() {
    let elf = Elf::new(
        BASE,
        vec![load(BASE + 16, &[2; 16], 16), load(BASE, &words(4), 16)],
    );
    let image = ok(&elf);
    assert_eq!(image.segments[0].offset, 0);
    assert_eq!(image.segments[1].offset, 16);
}

#[test]
fn the_entry_may_be_anywhere_in_a_loaded_segment_including_bss() {
    let elf = text_data_bss();
    for entry in [BASE + 0x100, BASE + 0x11c, BASE + 0x2000, BASE + 0x203c] {
        let elf = Elf::new(entry, elf.phdrs.clone());
        assert_eq!(ok(&elf).entry, entry);
    }
}

#[test]
fn the_ram_region_may_start_anywhere_and_reach_the_top_of_the_address_space() {
    for base in [0, 0x1000, 0x8000_0000, 0xffff_0000] {
        let size = ((1u64 << 32) - u64::from(base)).min(0x1_0000) as u32;
        let elf = Elf::new(base + 8, vec![load(base + 8, &words(2), 8)]);
        let image = load_elf32(&elf.build(), base, size).unwrap();
        assert_eq!(image, elf.expected(base));
        assert_eq!(image.segments[0].offset, 8);
    }
    // The whole 4 GiB address space minus one byte (ram_size is a u32).
    let elf = Elf::new(0xffff_fff8, vec![load(0xffff_fff8, &words(2), 8)]);
    let image = load_elf32(&elf.build(), 1, u32::MAX).unwrap();
    assert_eq!(image.segments[0].offset, 0xffff_fff7);
}

#[test]
fn the_hash_is_blake3_of_the_original_file() {
    let elf = text_data_bss();
    let file = elf.build();
    let image = load_elf32(&file, BASE, SIZE).unwrap();
    assert_eq!(image.image_hash, *blake3::hash(&file).as_bytes());
    // Same load image, different file (a byte of padding no segment covers): different
    // hash. A hash of the load image would give the same one.
    let mut other_file = file.clone();
    other_file.extend_from_slice(&[0xee; 4]);
    let other = load_elf32(&other_file, BASE, SIZE).unwrap();
    assert_eq!(other.segments, image.segments);
    assert_eq!(other.entry, image.entry);
    assert_ne!(other.image_hash, image.image_hash);
    assert_eq!(other.image_hash, *blake3::hash(&other_file).as_bytes());
    // And the same RAM contents placed in a different RAM region give the same hash.
    let moved = load_elf32(&file, BASE - 0x1000, SIZE + 0x1000).unwrap();
    assert_eq!(moved.image_hash, image.image_hash);
}

#[test]
fn loading_is_deterministic() {
    let file = text_data_bss().build();
    assert_eq!(
        load_elf32(&file, BASE, SIZE),
        load_elf32(&file.clone(), BASE, SIZE)
    );
}

// ---- Invalid files (B29) ----

#[test]
fn files_shorter_than_a_header_are_malformed() {
    let file = minimal().build();
    for len in 0..EHDR_LEN {
        assert_eq!(err(&file[..len]), ElfError::MalformedHeader, "length {len}");
    }
}

#[test]
fn the_magic_must_be_elf() {
    for i in 0..4 {
        let f = patched(&minimal(), |f| f[i] ^= 0x20);
        assert_eq!(err(&f), ElfError::InvalidMagic);
    }
}

#[test]
fn only_elfclass32_is_accepted() {
    for class in [0, 2, 3, 0xff] {
        let f = patched(&minimal(), |f| put_u8(f, EI_CLASS, class));
        assert_eq!(err(&f), ElfError::UnsupportedClass(class));
    }
}

#[test]
fn an_elf64_header_is_rejected_as_elf64() {
    // A real ELF64 header: class 2, 64-byte header, 56-byte program headers.
    let mut f = vec![0u8; 64 + 56];
    f[0..4].copy_from_slice(b"\x7fELF");
    f[EI_CLASS] = 2;
    f[EI_DATA] = 1;
    f[EI_VERSION] = 1;
    put_u16(&mut f, E_TYPE, 2);
    put_u16(&mut f, E_MACHINE, 243);
    put_u32(&mut f, E_VERSION, 1);
    assert_eq!(err(&f), ElfError::UnsupportedClass(2));
}

#[test]
fn only_little_endian_is_accepted() {
    for data in [0, 2, 3] {
        let f = patched(&minimal(), |f| put_u8(f, EI_DATA, data));
        assert_eq!(err(&f), ElfError::UnsupportedEndian(data));
    }
}

#[test]
fn a_big_endian_riscv_file_is_rejected_as_big_endian() {
    // Every multi-byte field byte-swapped, as a big-endian writer would produce.
    let mut f = minimal().build();
    put_u8(&mut f, EI_DATA, 2);
    f[E_TYPE..E_TYPE + 2].copy_from_slice(&2u16.to_be_bytes());
    f[E_MACHINE..E_MACHINE + 2].copy_from_slice(&243u16.to_be_bytes());
    f[E_VERSION..E_VERSION + 4].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(err(&f), ElfError::UnsupportedEndian(2));
}

#[test]
fn both_versions_must_be_ev_current() {
    for v in [0, 2, 0xff] {
        let f = patched(&minimal(), |f| put_u8(f, EI_VERSION, v));
        assert_eq!(err(&f), ElfError::UnsupportedVersion(u32::from(v)));
    }
    for v in [0, 2, 0x0100_0000] {
        let f = patched(&minimal(), |f| put_u32(f, E_VERSION, v));
        assert_eq!(err(&f), ElfError::UnsupportedVersion(v));
    }
}

#[test]
fn only_et_exec_is_accepted() {
    // ET_NONE, ET_REL, ET_DYN (PIE), ET_CORE, a processor-specific type, and ET_EXEC
    // byte-swapped.
    for t in [0, 1, 3, 4, 0xff00, 0x0200] {
        let f = patched(&minimal(), |f| put_u16(f, E_TYPE, t));
        assert_eq!(err(&f), ElfError::UnsupportedType(t));
    }
}

#[test]
fn only_em_riscv_is_accepted() {
    // EM_NONE, EM_386, EM_ARM, EM_X86_64, EM_AARCH64, and EM_RISCV byte-swapped.
    for m in [0, 3, 40, 62, 183, 0xf300] {
        let f = patched(&minimal(), |f| put_u16(f, E_MACHINE, m));
        assert_eq!(err(&f), ElfError::UnsupportedMachine(m));
    }
}

#[test]
fn the_header_size_must_be_52() {
    for size in [0, 51, 53, 64] {
        let f = patched(&minimal(), |f| put_u16(f, E_EHSIZE, size));
        assert_eq!(err(&f), ElfError::MalformedHeader);
    }
}

#[test]
fn program_headers_must_be_32_bytes() {
    for size in [0, 31, 33, 56] {
        let f = patched(&minimal(), |f| put_u16(f, E_PHENTSIZE, size));
        assert_eq!(err(&f), ElfError::MalformedProgramHeaders);
    }
}

#[test]
fn pn_xnum_is_rejected() {
    let f = patched(&minimal(), |f| put_u16(f, E_PHNUM, 0xffff));
    assert_eq!(err(&f), ElfError::MalformedProgramHeaders);
}

#[test]
fn the_program_header_table_must_be_inside_the_file() {
    let elf = minimal();
    let len = elf.build().len() as u32;
    for phoff in [len, len - 31, 0xffff_ffff, 0xffff_ffe0] {
        let f = patched(&elf, |f| put_u32(f, E_PHOFF, phoff));
        assert_eq!(err(&f), ElfError::MalformedProgramHeaders, "{phoff:#x}");
    }
    let f = patched(&elf, |f| put_u16(f, E_PHNUM, 0x1000));
    assert_eq!(err(&f), ElfError::MalformedProgramHeaders);
}

#[test]
fn file_size_must_not_exceed_memory_size() {
    let f = patched(&text_data_bss(), |f| put_u32(f, Elf::ph(1, P_MEMSZ), 5));
    assert_eq!(err(&f), seg_err(1, SegmentError::FileSizeExceedsMemSize));
}

#[test]
fn the_file_range_must_be_inside_the_file() {
    let elf = text_data_bss();
    let len = elf.build().len() as u32;
    let f = patched(&elf, |f| put_u32(f, Elf::ph(1, P_OFFSET), len));
    assert_eq!(err(&f), seg_err(1, SegmentError::FileRangeOutsideFile));
    // p_offset + p_filesz wraps a u32.
    let f = patched(&elf, |f| put_u32(f, Elf::ph(1, P_OFFSET), 0xffff_fffe));
    assert_eq!(err(&f), seg_err(1, SegmentError::FileRangeOutsideFile));
    // p_filesz as large as p_memsz allows.
    let f = patched(&elf, |f| {
        put_u32(f, Elf::ph(1, P_FILESZ), 0x40);
    });
    assert_eq!(err(&f), seg_err(1, SegmentError::FileRangeOutsideFile));
}

#[test]
fn vaddr_and_paddr_must_agree() {
    for paddr in [0, BASE + 0x2004, 0x2000] {
        let f = patched(&text_data_bss(), |f| put_u32(f, Elf::ph(1, P_PADDR), paddr));
        assert_eq!(err(&f), seg_err(1, SegmentError::AddressMismatch));
    }
}

#[test]
fn a_segment_must_not_wrap_the_address_space() {
    let elf = Elf::new(0xffff_fff0, vec![load(0xffff_fff0, &words(4), 0x20)]);
    let f = elf.build();
    assert_eq!(
        load_elf32(&f, 0xffff_0000, 0x1_0000).unwrap_err(),
        seg_err(0, SegmentError::AddressOverflow)
    );
}

#[test]
fn segments_must_be_inside_the_ram() {
    for addr in [0, BASE - 4, BASE - 0x100, END, END - 8, 0xffff_ff00] {
        let elf = Elf::new(
            BASE,
            vec![load(BASE, &words(1), 4), load(addr, &words(4), 16)],
        );
        assert_eq!(
            err(&elf.build()),
            ElfError::SegmentOutsideRam { index: 1 },
            "{addr:#x}"
        );
    }
    // Only the bss part is outside.
    let elf = Elf::new(BASE, vec![load(BASE, &words(1), SIZE + 4)]);
    assert_eq!(err(&elf.build()), ElfError::SegmentOutsideRam { index: 0 });
}

#[test]
fn overlapping_segments_are_rejected_in_any_order() {
    let cases: [(u32, u32, u32, u32); 4] = [
        (0x100, 0x20, 0x100, 0x20), // identical
        (0x100, 0x20, 0x11c, 0x20), // last word shared
        (0x100, 0x40, 0x110, 0x08), // one inside the other
        (0x100, 0x20, 0x0f0, 0x14), // shared from below
    ];
    for (a, alen, b, blen) in cases {
        for swap in [false, true] {
            let (x, y) = (load(BASE + a, &[], alen), load(BASE + b, &[], blen));
            let mut phdrs = vec![load(BASE, &words(1), 4), x, y];
            if swap {
                phdrs.swap(1, 2);
            }
            let elf = Elf::new(BASE, phdrs);
            assert_eq!(
                err(&elf.build()),
                ElfError::OverlappingSegments {
                    first: 1,
                    second: 2
                },
                "{a:#x}+{alen:#x} vs {b:#x}+{blen:#x}, swapped {swap}"
            );
        }
    }
}

#[test]
fn the_entry_must_be_word_aligned() {
    for delta in 1..4 {
        let elf = Elf::new(BASE + 0x100 + delta, text_data_bss().phdrs);
        assert_eq!(
            err(&elf.build()),
            ElfError::MisalignedEntry(BASE + 0x100 + delta)
        );
    }
}

#[test]
fn the_entry_must_be_in_a_loaded_segment_not_just_in_ram() {
    let phdrs = text_data_bss().phdrs;
    // Before the code, right after it, in the gap between segments, right after the bss,
    // at the RAM base, outside the RAM, and in a non-loaded header's range.
    for entry in [
        BASE + 0xfc,
        BASE + 0x120,
        BASE + 0x1000,
        BASE + 0x2040,
        BASE,
        0,
        END,
    ] {
        let elf = Elf::new(entry, phdrs.clone());
        assert_eq!(
            err(&elf.build()),
            ElfError::EntryOutsideSegments(entry),
            "{entry:#x}"
        );
    }
    let mut phdrs = phdrs;
    phdrs.push(other(PT_NOTE, BASE + 0x3000, &[0; 16], 16));
    let elf = Elf::new(BASE + 0x3000, phdrs);
    assert_eq!(
        err(&elf.build()),
        ElfError::EntryOutsideSegments(BASE + 0x3000)
    );
}

#[test]
fn a_file_with_nothing_to_load_has_no_valid_entry() {
    let elf = Elf::new(BASE, vec![]);
    let mut f = elf.build();
    // With no program headers, e_phentsize does not matter.
    put_u16(&mut f, E_PHENTSIZE, 0);
    assert_eq!(err(&f), ElfError::EntryOutsideSegments(BASE));
    let elf = Elf::new(BASE, vec![load(BASE, &[], 0)]);
    assert_eq!(err(&elf.build()), ElfError::EntryOutsideSegments(BASE));
}

#[test]
fn the_ram_region_must_be_non_empty_and_inside_the_address_space() {
    let f = minimal().build();
    assert_eq!(
        load_elf32(&f, BASE, 0).unwrap_err(),
        ElfError::InvalidRamRegion
    );
    assert_eq!(
        load_elf32(&f, 0x8000_0000, 0x8000_0001).unwrap_err(),
        ElfError::InvalidRamRegion
    );
    assert_eq!(
        load_elf32(&f, u32::MAX, 2).unwrap_err(),
        ElfError::InvalidRamRegion
    );
    // Checked before the file is looked at.
    assert_eq!(
        load_elf32(&[], 0, 0).unwrap_err(),
        ElfError::InvalidRamRegion
    );
}

#[test]
fn errors_name_the_problem() {
    let cases = [
        (ElfError::UnsupportedClass(2), "ELFCLASS32"),
        (ElfError::UnsupportedEndian(2), "little-endian"),
        (ElfError::UnsupportedType(3), "ET_EXEC"),
        (ElfError::UnsupportedMachine(62), "EM_RISCV"),
        (
            seg_err(3, SegmentError::FileSizeExceedsMemSize),
            "segment 3",
        ),
        (
            ElfError::OverlappingSegments {
                first: 1,
                second: 2,
            },
            "1 and 2 overlap",
        ),
        (ElfError::MisalignedEntry(0x8000_0002), "0x80000002"),
        (ElfError::EntryOutsideSegments(0x10), "0x00000010"),
    ];
    for (e, needle) in cases {
        let text = e.to_string();
        assert!(text.contains(needle), "{text:?} lacks {needle:?}");
    }
    let _: &dyn std::error::Error = &ElfError::InvalidMagic;
}

// ---- Boundaries (B30) ----

#[test]
fn a_segment_may_end_exactly_at_the_end_of_ram_but_not_one_byte_later() {
    let elf = Elf::new(END - 16, vec![load(END - 16, &words(4), 16)]);
    assert_eq!(ok(&elf).segments[0].offset, SIZE - 16);
    let elf = Elf::new(END - 16, vec![load(END - 16, &words(4), 17)]);
    assert_eq!(err(&elf.build()), ElfError::SegmentOutsideRam { index: 0 });
    let elf = Elf::new(END - 12, vec![load(END - 15, &words(4), 16)]);
    assert_eq!(err(&elf.build()), ElfError::SegmentOutsideRam { index: 0 });
}

#[test]
fn a_segment_may_start_exactly_at_the_ram_base_but_not_one_byte_earlier() {
    assert_eq!(ok(&minimal()).segments[0].offset, 0);
    let elf = Elf::new(BASE, vec![load(BASE - 1, &words(4), 16)]);
    assert_eq!(err(&elf.build()), ElfError::SegmentOutsideRam { index: 0 });
}

#[test]
fn the_entry_may_be_the_last_word_of_a_segment_but_not_its_end() {
    let phdrs = vec![load(BASE, &words(4), 16)];
    assert_eq!(ok(&Elf::new(BASE + 12, phdrs.clone())).entry, BASE + 12);
    assert_eq!(
        err(&Elf::new(BASE + 16, phdrs).build()),
        ElfError::EntryOutsideSegments(BASE + 16)
    );
}

#[test]
fn segments_may_touch_but_not_share_one_byte() {
    let touching = Elf::new(
        BASE,
        vec![load(BASE, &words(4), 16), load(BASE + 16, &[1], 1)],
    );
    assert_eq!(ok(&touching).segments.len(), 2);
    let sharing = Elf::new(
        BASE,
        vec![load(BASE, &words(4), 16), load(BASE + 15, &[1], 1)],
    );
    assert_eq!(
        err(&sharing.build()),
        ElfError::OverlappingSegments {
            first: 0,
            second: 1
        }
    );
}

#[test]
fn file_size_may_equal_memory_size_but_not_exceed_it() {
    assert_eq!(ok(&minimal()).segments[0].bytes, words(4));
    let f = patched(&minimal(), |f| put_u32(f, Elf::ph(0, P_MEMSZ), 15));
    assert_eq!(err(&f), seg_err(0, SegmentError::FileSizeExceedsMemSize));
}

#[test]
fn a_file_range_may_end_exactly_at_the_end_of_the_file() {
    let elf = minimal();
    let file = elf.build();
    assert_eq!(elf.data_offset(0) + 16, file.len());
    assert!(load_elf32(&file, BASE, SIZE).is_ok());
    assert_eq!(
        err(&file[..file.len() - 1]),
        seg_err(0, SegmentError::FileRangeOutsideFile)
    );
}

#[test]
fn the_program_header_table_may_end_exactly_at_the_end_of_the_file() {
    let elf = Elf::new(BASE, vec![load(BASE, &[], 16)]);
    let file = elf.build();
    assert_eq!(file.len(), EHDR_LEN + PHDR_LEN);
    assert!(load_elf32(&file, BASE, SIZE).is_ok());
    assert_eq!(
        err(&file[..file.len() - 1]),
        ElfError::MalformedProgramHeaders
    );
}

#[test]
fn a_segment_may_end_exactly_at_the_top_of_the_address_space() {
    let top = 0u32.wrapping_sub(16);
    let elf = Elf::new(top, vec![load(top, &words(4), 16)]);
    let image = load_elf32(&elf.build(), top - 0x1000, 0x1010).unwrap();
    assert_eq!(image, elf.expected(top - 0x1000));
    let elf = Elf::new(top, vec![load(top, &words(4), 17)]);
    assert_eq!(
        load_elf32(&elf.build(), top - 0x1000, 0x1010).unwrap_err(),
        seg_err(0, SegmentError::AddressOverflow)
    );
}

#[test]
fn the_ram_region_may_reach_the_top_of_the_address_space_but_not_past_it() {
    let f = Elf::new(0xffff_f000, vec![load(0xffff_f000, &words(1), 4)]).build();
    assert!(load_elf32(&f, 0xffff_f000, 0x1000).is_ok());
    assert_eq!(
        load_elf32(&f, 0xffff_f000, 0x1001).unwrap_err(),
        ElfError::InvalidRamRegion
    );
}
