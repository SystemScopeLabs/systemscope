//! The M1 `rv32ui` fixtures and their runner (`docs/m1-design.md` §10.2, §10.6).
//!
//! ```text
//! pinned riscv-tests + env/ ──build-fixtures.sh (Linux)──▶ fixtures/rv32ui-*.elf + manifest.json
//! fixtures ──systemscope-elf──▶ Rv32iCpu + AddressBus + Ram ──▶ ECALL ──▶ PASS / FAIL
//! ```
//!
//! - The constants here pin everything a fixture is built from, and name exactly which
//!   tests are selected and which are excluded. `tests/fixtures.rs` checks that the build
//!   script pins the same versions.
//! - [`manifest`] writes and checks `fixtures/manifest.json`, the acceptance contract: the
//!   selected ELFs, their hashes, and the pins they were built with.
//! - [`runner`] runs a fixture on the reference platform and applies the pass rule.
//! - [`hello`] is M1-A5: `hello.elf`, built from `hello/hello.S` with the same toolchain,
//!   pinned by its own `hello/manifest.json`, and run on `m1-reference` with the UART.
//! - [`spike`] is M1-A3: every selected fixture's retirements against the pinned Spike's
//!   commit log. Only `cargo xtask spike diff` runs Spike; tests use committed Spike logs.
//!
//! - [`act4`] is M1-A4: the ACT4 RV32I corpus under `tests/act4`, self-checking ELFs with
//!   expected values from the Sail reference model, pinned by `tests/act4/manifest.json`.
//!   Only `cargo xtask act4 build` runs ACT4 and Sail.
//!
//! Tests only read the committed fixtures. `cargo xtask rv32-fixtures build` rebuilds
//! them on Linux, and `cargo xtask rv32-fixtures verify` checks them without a network or
//! a compiler.

use std::path::{Path, PathBuf};

pub mod act4;
pub mod hello;
pub mod manifest;
pub mod runner;
pub mod spike;
pub mod upstream;

/// The upstream repository the tests come from.
pub const RISCV_TESTS_REPO: &str = "https://github.com/riscv-software-src/riscv-tests.git";
/// The pinned `riscv-tests` commit.
pub const RISCV_TESTS_COMMIT: &str = "793a5ff2d99a6d9fbd91e84c34b9a0437e313b88";
/// The `env` submodule commit at [`RISCV_TESTS_COMMIT`], which `env/riscv_test.h` and
/// `env/linker.ld` are derived from. The build does not fetch it.
pub const RISCV_TEST_ENV_COMMIT: &str = "6de71edb142be36319e380ce782c3d1830c65d68";

/// The distribution the toolchain packages come from.
pub const TOOLCHAIN_DISTRO: &str = "Ubuntu 24.04 (noble)";

/// A pinned toolchain package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Package {
    /// The package name.
    pub name: &'static str,
    /// The exact package version.
    pub version: &'static str,
    /// SHA-256 of the `.deb` file, as the archive's package index lists it.
    pub deb_sha256: &'static str,
    /// The first line of the tool's `--version` output, which the build script checks.
    pub version_line: &'static str,
}

/// The compiler and the assembler and linker.
pub const TOOLCHAIN: [Package; 2] = [
    Package {
        name: "gcc-riscv64-unknown-elf",
        version: "13.2.0-11ubuntu1+12",
        deb_sha256: "47d4670801c391c65513e2002a19280c6bbdba158f5a467ac5100bff813de0e3",
        version_line: "riscv64-unknown-elf-gcc (13.2.0-11ubuntu1+12) 13.2.0",
    },
    Package {
        name: "binutils-riscv64-unknown-elf",
        version: "2.42-1ubuntu1+6",
        deb_sha256: "d42d42efdaa0563155d0e1263675852255b20864cc525db9265247a5dec2ce30",
        version_line: "GNU assembler (2.42-1ubuntu1+6) 2.42",
    },
];

/// The compiler flags: upstream's for the `p` environment, narrowed to RV32I, without a
/// build-id note. The build adds the include paths and `env/linker.ld`.
pub const FLAGS: [&str; 8] = [
    "-march=rv32i",
    "-mabi=ilp32",
    "-static",
    "-mcmodel=medany",
    "-fvisibility=hidden",
    "-nostdlib",
    "-nostartfiles",
    "-Wl,--build-id=none",
];

/// The RAM of `m1-reference` (§9): its base is every fixture's entry point.
pub const RAM_BASE: u32 = 0x8000_0000;
/// 16 MiB.
pub const RAM_SIZE: u32 = 0x0100_0000;
/// The base of `m1-reference`'s `SimpleUart` (§9); its window is
/// [`systemscope_platform::uart::SIZE`] bytes.
pub const UART_BASE: u32 = 0x1000_0000;
/// `m1-reference`'s instruction limit (§9). The longest test retires a few thousand
/// instructions, so reaching it means a test is stuck, and fails it.
pub const MAX_INSTRUCTIONS: u64 = 10_000_000;

/// The selected upstream `rv32ui` tests, in name order: all 42 but [`EXCLUDED`].
pub const SELECTED: [&str; 40] = [
    "add", "addi", "and", "andi", "auipc", "beq", "bge", "bgeu", "blt", "bltu", "bne", "jal",
    "jalr", "lb", "lbu", "ld_st", "lh", "lhu", "lui", "lw", "or", "ori", "sb", "sh", "simple",
    "sll", "slli", "slt", "slti", "sltiu", "sltu", "sra", "srai", "srl", "srli", "st_ld", "sub",
    "sw", "xor", "xori",
];

/// The upstream `rv32ui` tests M1 does not run, with the reason (§10.2).
pub const EXCLUDED: [(&str, &str); 2] = [
    (
        "fence_i",
        "needs Zifencei (FENCE.I), which M1 does not implement",
    ),
    (
        "ma_data",
        "requires misaligned loads and stores to return data; SystemScope traps on them (§6)",
    ),
];

/// The fixture directory, relative to the workspace root.
pub const FIXTURE_DIR: &str = "tests/rv32/fixtures";
/// The manifest, relative to the workspace root.
pub const MANIFEST_PATH: &str = "tests/rv32/fixtures/manifest.json";
/// The build script, relative to the workspace root.
pub const BUILD_SCRIPT: &str = "tests/rv32/build-fixtures.sh";
/// The files a fixture build depends on besides the upstream sources and the toolchain,
/// relative to the workspace root. The manifest records their hashes, so changing one
/// without rebuilding fails `verify`.
pub const INPUTS: [&str; 5] = [
    "tests/rv32/build-fixtures.sh",
    "tests/rv32/env/LICENSE.riscv-test-env",
    "tests/rv32/env/linker.ld",
    "tests/rv32/env/riscv_test.h",
    "tests/rv32/fixtures/LICENSE.riscv-tests",
];
/// The upstream license kept next to the fixtures.
pub const FIXTURE_LICENSE: &str = "LICENSE.riscv-tests";

/// The workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tests/rv32 sits two levels below the workspace root")
        .to_path_buf()
}

/// The fixture file name for test `name`.
pub fn elf_name(name: &str) -> String {
    format!("rv32ui-{name}.elf")
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads 64 hex digits.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_forty_tests_are_selected_in_name_order_and_none_is_excluded() {
        assert_eq!(SELECTED.len(), 40);
        assert!(
            SELECTED.windows(2).all(|w| w[0] < w[1]),
            "sorted, no duplicates"
        );
        for (name, reason) in EXCLUDED {
            assert!(
                !SELECTED.contains(&name),
                "{name} is both selected and excluded"
            );
            assert!(!reason.is_empty());
        }
        assert_eq!(
            SELECTED.len() + EXCLUDED.len(),
            42,
            "the upstream rv32ui list"
        );
    }

    #[test]
    fn hex_round_trips() {
        let bytes: [u8; 32] = std::array::from_fn(|i| (i * 9) as u8);
        assert_eq!(unhex32(&hex(&bytes)), Some(bytes));
        assert_eq!(unhex32("00"), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
    }
}
