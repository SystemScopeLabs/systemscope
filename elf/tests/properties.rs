//! Property tests of `load_elf32` (`docs/m1-design.md` §8).
//!
//! - Valid models: random layouts in random RAM regions load to exactly the image the
//!   independent writer's oracle predicts, whatever the program-header order.
//! - Invalid models: one targeted defect in an otherwise valid file gives its error.
//! - Arbitrary input: random bytes, and random corruptions of valid files, never panic,
//!   and anything accepted satisfies every guarantee of a `LoadImage`.
//!
//! RAM regions are at most 64 KiB, so no case allocates much memory.

mod common;

use common::*;
use proptest::prelude::*;
use systemscope_elf::{ElfError, LoadImage, SegmentError, load_elf32};

/// A valid file description with the RAM region it targets.
#[derive(Clone, Debug)]
struct Model {
    elf: Elf,
    ram_base: u32,
    ram_size: u32,
}

/// Word-aligned RAM regions of 4..=64 KiB anywhere in the address space.
fn ram() -> impl Strategy<Value = (u32, u32)> {
    (1u32..=16).prop_flat_map(|kib4| {
        let size = kib4 * 0x1000;
        let max_base = (1u64 << 32) - u64::from(size);
        ((0..=max_base / 4).prop_map(|b| (b * 4) as u32), Just(size))
    })
}

/// (gap before, file bytes, extra bss) for one loaded segment. Gaps and total sizes are
/// word multiples so there is always an aligned entry in each segment.
fn segment_shape() -> impl Strategy<Value = (u32, Vec<u8>, u32)> {
    (
        0u32..64,
        prop::collection::vec(any::<u8>(), 0..48),
        0u32..96,
    )
        .prop_map(|(gap, data, bss)| {
            let len = data.len() as u32 + bss;
            let memsz = (len.max(1)).div_ceil(4) * 4;
            let bss = memsz - data.len() as u32;
            (gap * 4, data, bss)
        })
}

fn noise() -> impl Strategy<Value = Phdr> {
    (
        prop::sample::select(vec![PT_NULL, PT_NOTE, PT_GNU_STACK, PT_RISCV_ATTRIBUTES]),
        any::<u32>(),
        prop::collection::vec(any::<u8>(), 0..16),
        any::<u32>(),
    )
        .prop_map(|(t, addr, data, memsz)| {
            let memsz = memsz.max(data.len() as u32);
            other(t, addr, &data, memsz)
        })
}

fn empty_load() -> impl Strategy<Value = Phdr> {
    (any::<u32>(), any::<u32>()).prop_map(|(vaddr, paddr)| Phdr {
        p_type: PT_LOAD,
        vaddr,
        paddr,
        data: Vec::new(),
        memsz: 0,
    })
}

fn model() -> impl Strategy<Value = Model> {
    (
        ram(),
        prop::collection::vec(segment_shape(), 1..5),
        prop::collection::vec(noise(), 0..3),
        prop::collection::vec(empty_load(), 0..2),
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
        any::<u64>(),
    )
        .prop_filter_map(
            "segments do not fit in the RAM",
            |((ram_base, ram_size), shapes, noise, empties, which, word, order)| {
                let mut addr = u64::from(ram_base);
                let mut phdrs = Vec::new();
                for (gap, data, bss) in shapes {
                    addr += u64::from(gap);
                    let memsz = data.len() as u32 + bss;
                    phdrs.push(load(addr as u32, &data, memsz));
                    addr += u64::from(memsz);
                }
                if addr > u64::from(ram_base) + u64::from(ram_size) {
                    return None;
                }
                let target = which.get(&phdrs);
                let entry = target.vaddr + 4 * word.index((target.memsz / 4) as usize) as u32;
                phdrs.extend(noise);
                phdrs.extend(empties);
                shuffle(&mut phdrs, order);
                Some(Model {
                    elf: Elf::new(entry, phdrs),
                    ram_base,
                    ram_size,
                })
            },
        )
}

/// A deterministic Fisher-Yates shuffle driven by `seed`.
fn shuffle<T>(v: &mut [T], mut seed: u64) {
    for i in (1..v.len()).rev() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.swap(i, (seed >> 33) as usize % (i + 1));
    }
}

/// Indices of the loaded (`PT_LOAD`, `memsz > 0`) headers.
fn loaded(elf: &Elf) -> Vec<usize> {
    (0..elf.phdrs.len())
        .filter(|&i| elf.phdrs[i].p_type == PT_LOAD && elf.phdrs[i].memsz > 0)
        .collect()
}

/// Every guarantee a returned image makes, checked against the file and RAM region.
fn check_invariants(image: &LoadImage, file: &[u8], ram_base: u32, ram_size: u32) {
    assert_eq!(image.image_hash, *blake3::hash(file).as_bytes());
    let mut prev_end = 0u64;
    for (i, s) in image.segments.iter().enumerate() {
        assert!(!s.bytes.is_empty());
        let start = u64::from(s.offset);
        let end = start + s.bytes.len() as u64;
        assert!(end <= u64::from(ram_size));
        if i > 0 {
            assert!(start >= prev_end, "sorted and non-overlapping");
        }
        prev_end = end;
    }
    assert_eq!(image.entry % 4, 0);
    let entry = u64::from(image.entry);
    assert!(image.segments.iter().any(|s| {
        let start = u64::from(ram_base) + u64::from(s.offset);
        start <= entry && entry < start + s.bytes.len() as u64
    }));
}

/// A defect to introduce into a valid model, and the error it must cause.
#[derive(Clone, Debug)]
enum Defect {
    Class(u8),
    Endian(u8),
    Type(u16),
    Machine(u16),
    Version(u32),
    MisalignEntry(u32),
    EntryInGap,
    FileSizeExceedsMemSize(prop::sample::Index),
    FileRangeOutsideFile(prop::sample::Index),
    AddressMismatch(prop::sample::Index, u32),
    PastRamEnd(prop::sample::Index),
    BelowRamBase(prop::sample::Index),
    Overlap(prop::sample::Index, prop::sample::Index),
    TableOutsideFile,
}

fn defect() -> impl Strategy<Value = Defect> {
    prop_oneof![
        any::<u8>()
            .prop_filter("valid", |&c| c != 1)
            .prop_map(Defect::Class),
        any::<u8>()
            .prop_filter("valid", |&d| d != 1)
            .prop_map(Defect::Endian),
        any::<u16>()
            .prop_filter("valid", |&t| t != 2)
            .prop_map(Defect::Type),
        any::<u16>()
            .prop_filter("valid", |&m| m != 243)
            .prop_map(Defect::Machine),
        any::<u32>()
            .prop_filter("valid", |&v| v != 1)
            .prop_map(Defect::Version),
        (1u32..4).prop_map(Defect::MisalignEntry),
        Just(Defect::EntryInGap),
        any::<prop::sample::Index>().prop_map(Defect::FileSizeExceedsMemSize),
        any::<prop::sample::Index>().prop_map(Defect::FileRangeOutsideFile),
        (any::<prop::sample::Index>(), 1u32..).prop_map(|(i, d)| Defect::AddressMismatch(i, d)),
        any::<prop::sample::Index>().prop_map(Defect::PastRamEnd),
        any::<prop::sample::Index>().prop_map(Defect::BelowRamBase),
        (any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b)| Defect::Overlap(a, b)),
        Just(Defect::TableOutsideFile),
    ]
}

/// What a defective file must be rejected with.
#[derive(Debug)]
enum Expected {
    Exact(ElfError),
    /// Some pair of overlapping segments: moving one segment onto another may make it
    /// overlap others too, and the loader names the first pair in address order.
    AnyOverlap,
}

/// Applies `defect` to `m`, returning the file and the expected error, or `None` if the
/// defect does not apply to this model (for example, overlap with a single segment).
fn apply(m: &Model, defect: &Defect) -> Option<(Vec<u8>, Expected)> {
    let loads = loaded(&m.elf);
    let seg = |i: &prop::sample::Index| loads[i.index(loads.len())];
    let ram_start = u64::from(m.ram_base);
    let ram_end = ram_start + u64::from(m.ram_size);
    let mut f = m.elf.build();
    let err = match *defect {
        Defect::Class(c) => {
            put_u8(&mut f, EI_CLASS, c);
            ElfError::UnsupportedClass(c)
        }
        Defect::Endian(d) => {
            put_u8(&mut f, EI_DATA, d);
            ElfError::UnsupportedEndian(d)
        }
        Defect::Type(t) => {
            put_u16(&mut f, E_TYPE, t);
            ElfError::UnsupportedType(t)
        }
        Defect::Machine(machine) => {
            put_u16(&mut f, E_MACHINE, machine);
            ElfError::UnsupportedMachine(machine)
        }
        Defect::Version(v) => {
            put_u32(&mut f, E_VERSION, v);
            ElfError::UnsupportedVersion(v)
        }
        Defect::MisalignEntry(d) => {
            let entry = m.elf.entry + d;
            put_u32(&mut f, E_ENTRY, entry);
            ElfError::MisalignedEntry(entry)
        }
        Defect::EntryInGap => {
            // The first aligned address in the RAM that no segment covers, if any.
            let covered = |a: u64| {
                loads.iter().any(|&i| {
                    let p = &m.elf.phdrs[i];
                    u64::from(p.vaddr) <= a && a < u64::from(p.vaddr) + u64::from(p.memsz)
                })
            };
            let gap = (ram_start..ram_end).step_by(4).find(|&a| !covered(a))? as u32;
            put_u32(&mut f, E_ENTRY, gap);
            ElfError::EntryOutsideSegments(gap)
        }
        Defect::FileSizeExceedsMemSize(ref i) => {
            let i = seg(i);
            put_u32(&mut f, Elf::ph(i, P_FILESZ), m.elf.phdrs[i].memsz + 1);
            ElfError::InvalidSegment {
                index: i as u16,
                reason: SegmentError::FileSizeExceedsMemSize,
            }
        }
        Defect::FileRangeOutsideFile(ref i) => {
            let i = seg(i);
            let p = &m.elf.phdrs[i];
            let offset = (f.len() + 1 - p.data.len()) as u32;
            put_u32(&mut f, Elf::ph(i, P_OFFSET), offset);
            // With no file bytes the range is empty, and an empty range one past the end
            // is still outside the file.
            ElfError::InvalidSegment {
                index: i as u16,
                reason: SegmentError::FileRangeOutsideFile,
            }
        }
        Defect::AddressMismatch(ref i, d) => {
            let i = seg(i);
            put_u32(
                &mut f,
                Elf::ph(i, P_PADDR),
                m.elf.phdrs[i].paddr.wrapping_add(d),
            );
            ElfError::InvalidSegment {
                index: i as u16,
                reason: SegmentError::AddressMismatch,
            }
        }
        Defect::PastRamEnd(ref i) => {
            let i = seg(i);
            let memsz = u64::from(m.elf.phdrs[i].memsz);
            let addr = ram_end - memsz + 1;
            if addr + memsz > 1 << 32 {
                return None; // would be AddressOverflow, covered by the targeted tests
            }
            put_u32(&mut f, Elf::ph(i, P_VADDR), addr as u32);
            put_u32(&mut f, Elf::ph(i, P_PADDR), addr as u32);
            ElfError::SegmentOutsideRam { index: i as u16 }
        }
        Defect::BelowRamBase(ref i) => {
            let i = seg(i);
            let addr = u32::try_from(ram_start.checked_sub(1)?).ok()?;
            put_u32(&mut f, Elf::ph(i, P_VADDR), addr);
            put_u32(&mut f, Elf::ph(i, P_PADDR), addr);
            ElfError::SegmentOutsideRam { index: i as u16 }
        }
        Defect::Overlap(ref a, ref b) => {
            let (a, b) = (seg(a), seg(b));
            if a == b {
                return None;
            }
            let addr = m.elf.phdrs[a].vaddr;
            if u64::from(addr) + u64::from(m.elf.phdrs[b].memsz) > ram_end {
                return None;
            }
            put_u32(&mut f, Elf::ph(b, P_VADDR), addr);
            put_u32(&mut f, Elf::ph(b, P_PADDR), addr);
            return Some((f, Expected::AnyOverlap));
        }
        Defect::TableOutsideFile => {
            let phoff = (f.len() - m.elf.phdrs.len() * PHDR_LEN + 1) as u32;
            put_u32(&mut f, E_PHOFF, phoff);
            ElfError::MalformedProgramHeaders
        }
    };
    Some((f, Expected::Exact(err)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn valid_models_load_to_the_oracle_image(m in model()) {
        let file = m.elf.build();
        let image = load_elf32(&file, m.ram_base, m.ram_size).unwrap();
        prop_assert_eq!(&image, &m.elf.expected(m.ram_base));
        check_invariants(&image, &file, m.ram_base, m.ram_size);
    }

    #[test]
    fn each_defect_gives_its_error(m in model(), d in defect()) {
        if let Some((file, expected)) = apply(&m, &d) {
            let result = load_elf32(&file, m.ram_base, m.ram_size);
            match expected {
                Expected::Exact(e) => prop_assert_eq!(result, Err(e)),
                Expected::AnyOverlap => prop_assert!(
                    matches!(result, Err(ElfError::OverlappingSegments { first, second }) if first < second),
                    "{:?}", result
                ),
            }
        }
    }

    #[test]
    fn corrupted_valid_files_never_panic(
        m in model(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        cut in any::<prop::sample::Index>(),
        truncate in any::<bool>(),
    ) {
        let mut file = m.elf.build();
        for (at, byte) in edits {
            let at = at.index(file.len());
            file[at] = byte;
        }
        if truncate {
            file.truncate(cut.index(file.len() + 1));
        }
        if let Ok(image) = load_elf32(&file, m.ram_base, m.ram_size) {
            check_invariants(&image, &file, m.ram_base, m.ram_size);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        prefix in any::<bool>(),
        mut bytes in prop::collection::vec(any::<u8>(), 0..512),
        (ram_base, ram_size) in ram(),
    ) {
        // Half the cases start with a valid ELF32 RISC-V identification, so they get past
        // the first checks.
        if prefix && bytes.len() >= 20 {
            bytes[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
            bytes[16..20].copy_from_slice(&[2, 0, 243, 0]);
        }
        if let Ok(image) = load_elf32(&bytes, ram_base, ram_size) {
            check_invariants(&image, &bytes, ram_base, ram_size);
        }
    }

    #[test]
    fn arbitrary_ram_regions_never_panic(
        m in model(),
        ram_base in any::<u32>(),
        ram_size in any::<u32>(),
    ) {
        // Any region: accepted only if valid, and then the image fits in it. Segments are
        // at most a few hundred bytes, so a large region costs nothing.
        let file = m.elf.build();
        let result = load_elf32(&file, ram_base, ram_size);
        if ram_size == 0 || u64::from(ram_base) + u64::from(ram_size) > 1 << 32 {
            prop_assert_eq!(result, Err(ElfError::InvalidRamRegion));
        } else if let Ok(image) = result {
            check_invariants(&image, &file, ram_base, ram_size);
        }
    }
}
