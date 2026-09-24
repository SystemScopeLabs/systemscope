//! The committed fixtures match their manifest and the pins (`docs/m1-design.md` §10.6).
//! No network and no compiler: these read committed files only.

use std::fs;

use systemscope_rv32::manifest::{Manifest, verify};
use systemscope_rv32::{
    BUILD_SCRIPT, FIXTURE_DIR, FIXTURE_LICENSE, FLAGS, RAM_BASE, RISCV_TESTS_COMMIT,
    RISCV_TESTS_REPO, SELECTED, TOOLCHAIN, workspace_root,
};
use systemscope_rv32i::{Instr, decode};

#[test]
fn the_committed_fixtures_match_the_manifest() {
    let manifest = verify(&workspace_root()).unwrap_or_else(|errors| panic!("{errors:#?}"));
    assert_eq!(manifest.selected.len(), 40);
    let names: Vec<&str> = manifest.selected.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, SELECTED);
    for f in &manifest.selected {
        assert_eq!(f.entry, RAM_BASE, "{}", f.name);
        // The loader names a program by the BLAKE3 of the whole file.
        assert_eq!(f.image_hash, f.blake3, "{}", f.name);
    }
}

/// The value of `NAME=value` or `NAME='value'` in the build script.
fn script_value<'a>(script: &'a str, name: &str) -> &'a str {
    let line = script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{BUILD_SCRIPT} sets no {name}"));
    line.trim_matches('\'')
}

#[test]
fn the_build_script_pins_what_the_manifest_records() {
    let script = fs::read_to_string(workspace_root().join(BUILD_SCRIPT)).unwrap();
    assert_eq!(script_value(&script, "RISCV_TESTS_REPO"), RISCV_TESTS_REPO);
    assert_eq!(
        script_value(&script, "RISCV_TESTS_COMMIT"),
        RISCV_TESTS_COMMIT
    );
    assert_eq!(
        script_value(&script, "GCC_VERSION"),
        TOOLCHAIN[0].version_line
    );
    assert_eq!(
        script_value(&script, "AS_VERSION"),
        TOOLCHAIN[1].version_line
    );
    for package in TOOLCHAIN {
        assert!(
            package.version_line.contains(package.version),
            "{package:?}"
        );
    }
    let flags = script
        .split_once("FLAGS=(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(flags, _)| flags.split_whitespace().collect::<Vec<_>>())
        .expect("a FLAGS array");
    assert_eq!(flags, FLAGS);
}

#[test]
fn the_upstream_license_is_kept_next_to_the_fixtures_and_the_environment() {
    let root = workspace_root();
    let fixtures = fs::read(root.join(FIXTURE_DIR).join(FIXTURE_LICENSE)).unwrap();
    let env = fs::read(root.join("tests/rv32/env/LICENSE.riscv-test-env")).unwrap();
    assert_eq!(
        fixtures, env,
        "riscv-tests and riscv-test-env share one license text"
    );
    assert!(fixtures.starts_with(
        b"Copyright (c) 2012-2015, The Regents of the University of California (Regents)."
    ));
}

/// A second opinion on the build script's objdump check, with SystemScope's own decoder:
/// every nonzero word of the code segment (the one at the entry) is an RV32I instruction,
/// none is `EBREAK`, and the environment's `FENCE` and `ECALL` are there (one `ECALL` in
/// `simple`, which has only `RVTEST_PASS`; two elsewhere). The words the decoder rejects
/// are all zero: `RVTEST_CODE_END` and alignment padding.
#[test]
fn the_code_uses_only_rv32i_and_no_system_instruction_but_ecall() {
    let root = workspace_root();
    let manifest = Manifest::read(&root).unwrap();
    for fixture in &manifest.selected {
        let image = fixture.read(&root).unwrap();
        let code = image
            .segments
            .iter()
            .find(|s| s.offset == 0)
            .expect("a segment at the entry");
        let (mut fences, mut ecalls, mut zeros) = (0, 0, 0);
        for &word in code.bytes.as_chunks::<4>().0 {
            let word = u32::from_le_bytes(word);
            match decode(word) {
                Ok(Instr::Fence) => fences += 1,
                Ok(Instr::Ecall) => ecalls += 1,
                Ok(Instr::Ebreak) => panic!("{}: EBREAK", fixture.name),
                Ok(_) => {}
                Err(_) if word == 0 => zeros += 1,
                Err(e) => panic!("{}: {e:?} in the code segment", fixture.name),
            }
        }
        let expected_ecalls = if fixture.name == "simple" { 1 } else { 2 };
        assert_eq!(ecalls, expected_ecalls, "{}", fixture.name);
        assert!(
            fences >= ecalls,
            "{}: FENCE before each ECALL",
            fixture.name
        );
        assert!(zeros >= 1, "{}: RVTEST_CODE_END", fixture.name);
    }
}
