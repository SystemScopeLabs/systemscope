//! `parse_user_elf32` on a real executable: `tests/fixtures/user/user.elf`, built by
//! `build-user.sh` with the pinned toolchain and the linker's default script.
//!
//! The ELF and its inputs are checked against the BLAKE3 hashes in the fixture's
//! `manifest.json` first, so a changed fixture fails here rather than in the layout
//! assertions. The manifest is searched as text: the crate has no JSON dependency.

use std::fs;
use std::path::PathBuf;

use systemscope_elf::{
    FileCopy, PagePlan, Perms, UserElfError, UserImage, UserSegment, parse_user_elf32,
};

const USER: std::ops::Range<u32> = 0x0001_0000..0x7fff_b000;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/user")
        .join(name);
    fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn manifest() -> String {
    String::from_utf8(fixture("manifest.json")).unwrap()
}

#[test]
fn the_fixture_matches_its_manifest() {
    let manifest = manifest();
    for name in ["build-user.sh", "user.S"] {
        let line = format!(
            r#"{{ "path": "elf/tests/fixtures/user/{name}", "blake3": "{}" }}"#,
            blake3::hash(&fixture(name)).to_hex()
        );
        assert!(manifest.contains(&line), "manifest.json lacks {line}");
    }
    let elf = format!(
        r#""blake3": "{}","#,
        blake3::hash(&fixture("user.elf")).to_hex()
    );
    assert!(manifest.contains(&elf), "manifest.json lacks {elf}");
    assert!(manifest.contains(r#""entry": "0x00010094""#));
}

#[test]
fn the_fixture_parses_from_a_header_sized_prefix_and_a_retry() {
    let bytes = fixture("user.elf");
    let len = bytes.len() as u32;
    // A loader that knows nothing of the file reads the ELF header first...
    let needed = match parse_user_elf32(&bytes[..52], len, USER) {
        Err(UserElfError::PrefixTooShort { needed }) => needed,
        other => panic!("expected PrefixTooShort, got {other:?}"),
    };
    // ...which asks for the header and the three program headers.
    assert_eq!(needed, 52 + 3 * 32);
    let image = parse_user_elf32(&bytes[..needed as usize], len, USER).unwrap();
    assert_eq!(parse_user_elf32(&bytes, len, USER), Ok(image.clone()));

    let rx = Perms {
        read: true,
        write: false,
        execute: true,
    };
    let rw = Perms {
        read: true,
        write: true,
        execute: false,
    };
    // The layout `readelf -l` reports: the ELF headers, .text, and .rodata in one R+X
    // PT_LOAD (index 1, after PT_RISCV_ATTRIBUTES); .data and .bss in one R+W PT_LOAD on
    // the next page.
    assert_eq!(
        image,
        UserImage {
            entry: 0x0001_0094,
            segments: vec![
                UserSegment {
                    index: 1,
                    vaddr: 0x0001_0000,
                    memsz: 0xe5,
                    perms: rx,
                    pages: vec![PagePlan {
                        va: 0x0001_0000,
                        perms: rx,
                        copy: Some(FileCopy {
                            file_offset: 0,
                            page_offset: 0,
                            len: 0xe5
                        }),
                    }],
                },
                UserSegment {
                    index: 2,
                    vaddr: 0x0001_10e8,
                    memsz: 0x44,
                    perms: rw,
                    pages: vec![PagePlan {
                        va: 0x0001_1000,
                        perms: rw,
                        copy: Some(FileCopy {
                            file_offset: 0xe8,
                            page_offset: 0xe8,
                            len: 4
                        }),
                    }],
                },
            ],
        }
    );
}

#[test]
fn the_fixture_is_also_accepted_by_the_m1_loader_rules_it_shares() {
    // The M1 loader requires p_vaddr == p_paddr, which the default script gives; it places
    // segments in a RAM region, here one covering the user image.
    let bytes = fixture("user.elf");
    let image = systemscope_elf::load_elf32(&bytes, 0x0001_0000, 0x2000).unwrap();
    assert_eq!(image.entry, 0x0001_0094);
    assert_eq!(image.segments.len(), 2);
}
