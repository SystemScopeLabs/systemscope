//! M1-A4: the committed ACT4 corpus matches its manifest and the pins, its adapter
//! configuration claims RV32I only, and every test passes on `m1-reference`
//! (`docs/m1-design.md` §10.4). No network, ACT4, Sail, or compiler: these read
//! committed files only.

use std::fs;

use serde_json::Value;
use systemscope_rv32::act4::{
    self, ACT4_CONFIG_DIR, ACT4_FIXTURE_DIR, ACT4_MANIFEST, ACT4_SCRIPT, ALLOWED_MNEMONICS,
    CANONICAL_PATH, EXTENSIONS, SM_SHIM,
};
use systemscope_rv32::{RAM_BASE, Rv32iProfile, workspace_root};

/// How many tests ACT4 selected for `EXTENSIONS=I` at the pinned commit, as the committed
/// manifest records it. Checked here so that a regenerated corpus that silently shrinks
/// or grows is a visible contract change.
const EXPECTED_TESTS: usize = 39;

fn config(file: &str) -> String {
    fs::read_to_string(workspace_root().join(ACT4_CONFIG_DIR).join(file)).unwrap()
}

#[test]
fn the_committed_corpus_matches_the_manifest() {
    let manifest = act4::verify(&workspace_root()).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(manifest.count, EXPECTED_TESTS);
    assert_eq!(manifest.tests.len(), EXPECTED_TESTS);
    for t in &manifest.tests {
        assert_eq!(t.entry, RAM_BASE, "{}", t.recorded.name);
        // The loader names a program by the BLAKE3 of the whole file.
        assert_eq!(t.image_hash, t.blake3, "{}", t.recorded.name);
        assert!(t.recorded.name.starts_with("I-"), "{}", t.recorded.name);
    }
}

#[test]
fn every_act4_test_passes_on_m1_reference() {
    let (manifest, report) = act4::run_corpus(&workspace_root()).unwrap();
    let lines: Vec<String> = report.results.iter().map(|r| r.line()).collect();
    assert_eq!(
        report.accept(manifest.count),
        Ok(()),
        "{}",
        lines.join("\n")
    );
    assert_eq!(
        (
            manifest.count,
            report.selected,
            report.executed(),
            report.passed()
        ),
        (
            EXPECTED_TESTS,
            EXPECTED_TESTS,
            EXPECTED_TESTS,
            EXPECTED_TESTS
        )
    );
}

/// The M2 CPU profile passes the same corpus (`docs/m2-design.md` §15.4).
#[test]
fn every_act4_test_passes_with_the_m2_cpu_profile() {
    let (manifest, report) = act4::run_corpus_with(&workspace_root(), Rv32iProfile::M2).unwrap();
    let lines: Vec<String> = report.results.iter().map(|r| r.line()).collect();
    assert_eq!(
        report.accept(manifest.count),
        Ok(()),
        "{}",
        lines.join(
            "
"
        )
    );
    assert_eq!(
        (report.selected, report.executed(), report.passed()),
        (EXPECTED_TESTS, EXPECTED_TESTS, EXPECTED_TESTS)
    );
}

/// The value of `NAME=value` or `NAME='value'` in the build script.
fn script_value<'a>(script: &'a str, name: &str) -> &'a str {
    let line = script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{ACT4_SCRIPT} sets no {name}"));
    line.trim_matches('\'')
}

#[test]
fn the_build_script_pins_what_the_manifest_records() {
    let script = fs::read_to_string(workspace_root().join(ACT4_SCRIPT)).unwrap();
    let pins = [
        ("ACT4_REPO", act4::ACT4_REPO),
        ("ACT4_COMMIT", act4::ACT4_COMMIT),
        ("ACT4_TESTPLAN_SHA256", act4::ACT4_TESTPLAN_SHA256),
        ("ACT4_UV_LOCK_SHA256", act4::ACT4_UV_LOCK_SHA256),
        ("ACT4_GEMFILE_LOCK_SHA256", act4::ACT4_GEMFILE_LOCK_SHA256),
        ("SAIL_VERSION", act4::SAIL_VERSION),
        ("SAIL_URL", act4::SAIL_URL),
        ("SAIL_TARBALL_SHA256", act4::SAIL_TARBALL_SHA256),
        ("SAIL_BINARY_SHA256", act4::SAIL_BINARY_SHA256),
        ("GCC_URL", act4::GCC_URL),
        ("GCC_TARBALL_SHA256", act4::GCC_TARBALL_SHA256),
        ("GCC_BINARY_SHA256", act4::GCC_BINARY_SHA256),
        ("AS_BINARY_SHA256", act4::AS_BINARY_SHA256),
        ("GCC_VERSION_LINE", act4::GCC_VERSION_LINE),
        ("AS_VERSION_LINE", act4::AS_VERSION_LINE),
        ("MISE_URL", act4::MISE_URL),
        ("MISE_SHA256", act4::MISE_SHA256),
        ("EXTENSIONS", EXTENSIONS),
        ("CANON", CANONICAL_PATH),
    ];
    for (name, value) in pins {
        assert_eq!(script_value(&script, name), value, "{name}");
    }
    assert_eq!(
        script_value(&script, "RUBY_VERSION_PREFIX"),
        format!("{} ", act4::RUBY_VERSION)
    );
    assert_eq!(
        script_value(&script, "UV_VERSION_PREFIX"),
        format!("{} ", act4::UV_VERSION)
    );
    assert!(act4::SAIL_URL.contains(&format!("/{}/", act4::SAIL_VERSION)));
    let mnemonics = script
        .split_once("RV32I_MNEMONICS=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(list, _)| list.split_whitespace().collect::<Vec<_>>())
        .expect("the script lists RV32I_MNEMONICS");
    assert_eq!(mnemonics, ALLOWED_MNEMONICS);
    // Every pin is spelled out in full.
    for (name, value) in act4::identity().into_iter().chain(act4::sources()) {
        assert!(!value.contains('…') && !value.contains("..."), "{name}");
    }
}

#[test]
fn only_rv32i_tests_are_requested_and_privileged_tests_are_off() {
    assert_eq!(EXTENSIONS, "I");
    const { assert!(!act4::INCLUDE_PRIV_TESTS) };
    let test_config = config("test_config.yaml");
    let setting = |key: &str| {
        test_config
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}: ")))
            .unwrap_or_else(|| panic!("test_config.yaml sets no {key}"))
    };
    assert_eq!(setting("include_priv_tests"), "False");
    assert_eq!(setting("compiler_exe"), "systemscope-act4-gcc");
    assert_eq!(setting("ref_model_exe"), "sail_riscv_sim");
    assert_eq!(setting("udb_config"), "systemscope-rv32i.yaml");
    assert_eq!(setting("linker_script"), "link.ld");
    let manifest = act4::Act4Manifest::read(&workspace_root()).unwrap();
    assert_eq!(
        manifest.identity[7],
        ("extensions".to_owned(), "I".to_owned())
    );
    assert!(!manifest.include_priv_tests);
}

#[test]
fn the_udb_configuration_declares_i_and_the_sm_shim_only() {
    let udb = config("systemscope-rv32i.yaml");
    let extensions: Vec<&str> = udb
        .lines()
        .skip_while(|l| *l != "implemented_extensions:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.trim().strip_prefix("- { name: "))
        .map(|l| l.split(',').next().unwrap())
        .collect();
    assert_eq!(extensions, act4::ADAPTER_EXTENSIONS);
    let header: String = udb
        .lines()
        .take_while(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        header.contains(
            "Sm is declared only because this pinned ACT4/UDB schema requires Sm-owned MXLEN \
             for an RV32I DUT configuration. SystemScope does not implement Sm or privileged \
             architecture. Privileged ACT tests remain disabled."
        ),
        "{header}"
    );
    assert!(udb.contains("\n  MXLEN: 32\n"));
    assert!(SM_SHIM.contains("does not describe a SystemScope CPU capability"));
}

#[test]
fn sail_runs_with_every_other_extension_off_and_the_platform_ram() {
    let sail: Value = serde_json::from_str(&config("sail.json")).unwrap();
    assert_eq!(sail["base"]["xlen"], 32);
    let extensions = sail["extensions"].as_object().unwrap();
    // Sail spells C as Zca and its relatives.
    for name in ["M", "A", "F", "D", "Zca", "Zicsr", "Zifencei", "S", "U"] {
        assert_eq!(extensions[name]["supported"], false, "{name}");
    }
    for (name, ext) in extensions {
        if let Some(supported) = ext.get("supported") {
            assert_eq!(supported, false, "{name}");
        }
    }
    let mstatus = &sail["base"]["mstatus"];
    for key in ["fs_legal_states", "vs_legal_states"] {
        let states = mstatus.get(key).map(|v| v.to_string()).unwrap_or_default();
        assert!(states.contains("ExtContext_Off"), "{key}: {states}");
        assert!(
            !states.contains("Dirty") && !states.contains("Clean"),
            "{key}: {states}"
        );
    }
    let regions = sail["memory"]["regions"].as_array().unwrap();
    assert!(regions.iter().any(|r| {
        r["base"]["value"] == format!("{RAM_BASE:#x}") && r["size"]["value"] == "0x01000000"
    }));
    let link = config("link.ld");
    assert!(link.contains("RAM_LENGTH = 0x01000000;"), "{link}");
}

#[test]
fn the_dut_macros_end_by_ecall_print_through_the_uart_and_have_no_tohost() {
    let macros = config("rvmodel_macros.h");
    let code: String = macros
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!code.to_lowercase().contains("tohost"));
    assert!(code.contains("#define SYSTEMSCOPE_UART_BASE 0x10000000"));
    for halt in ["RVMODEL_HALT_PASS", "RVMODEL_HALT_FAIL"] {
        let body = code.split_once(&format!("#define {halt}")).unwrap().1;
        let body = &body[..body.find("\n\n").unwrap()];
        assert!(
            body.contains("li gp, 1") && body.contains("ecall"),
            "{halt}"
        );
    }
    for irq in [
        "RVMODEL_SET_MEXT_INT",
        "RVMODEL_CLR_MEXT_INT",
        "RVMODEL_SET_MSW_INT",
        "RVMODEL_CLR_MSW_INT",
    ] {
        let line = code
            .lines()
            .find(|l| l.starts_with(&format!("#define {irq}(")))
            .unwrap();
        assert!(line.contains(".error"), "{line}");
    }
}

#[test]
fn nothing_committed_names_a_host_path() {
    let root = workspace_root();
    let manifest = fs::read(root.join(ACT4_MANIFEST)).unwrap();
    let elfs = act4::Act4Manifest::read(&root).unwrap().tests;
    let mut files = vec![("manifest.json".to_owned(), manifest)];
    for t in elfs {
        let bytes = fs::read(root.join(ACT4_FIXTURE_DIR).join(&t.elf)).unwrap();
        files.push((t.elf, bytes));
    }
    for (name, bytes) in &files {
        for host in [
            &b"/home/"[..],
            b"/mnt/",
            b"/root/",
            b"/Users/",
            b"C:\\",
            b"/runner/",
        ] {
            assert!(
                !bytes.windows(host.len()).any(|w| w == host),
                "{name} names {:?}",
                String::from_utf8_lossy(host)
            );
        }
    }
    // The ELFs' debug information names the canonical generation path instead.
    let canonical = CANONICAL_PATH.as_bytes();
    assert!(
        files[1..]
            .iter()
            .all(|(_, b)| b.windows(canonical.len()).any(|w| w == canonical))
    );
}
