//! M1-A4: the ACT4 RV32I corpus, with expected values from the Sail reference model, run
//! on `m1-reference` (`docs/m1-design.md` §10.4).
//!
//! ```text
//! pinned ACT4 + Sail + GCC ──build-act4.sh (Linux)──▶ <out>/elfs/*.elf + <out>/record.txt
//!   ACT4 source ─(Sail macros)─▶ .sig.elf ─Sail─▶ .sig ─▶ .results ─(DUT macros)─▶ .elf
//! <out> ──cargo xtask act4──▶ tests/act4/fixtures/*.elf + tests/act4/manifest.json
//! fixtures ──systemscope-elf──▶ Ram ──▶ Rv32iCpu ──▶ AddressBus ──▶ SimpleUart ──▶ PASS / FAIL
//! ```
//!
//! Each final ELF checks itself: it compares every result with the value Sail computed
//! when the corpus was generated, prints a summary through the UART, and ends with ECALL.
//! A test passes when the run ends in `Trap(EnvironmentCall)` with `gp == 1` and
//! `a0 == 0`, and the UART printed exactly [`pass_output`] for that test.
//!
//! SystemScope's architectural capability is [`ARCHITECTURAL_CAPABILITY`], RV32I. The
//! ACT4 adapter configuration declares [`ADAPTER_EXTENSIONS`], I and Sm, only because
//! the pinned ACT4/UDB schema requires Sm to express MXLEN = 32 ([`SM_SHIM`]). No
//! privileged test is selected ([`INCLUDE_PRIV_TESTS`]), Sail runs with every other
//! extension off, and every final ELF is audited to hold RV32I instructions and ECALL
//! only ([`ALLOWED_MNEMONICS`]).
//!
//! Only `cargo xtask act4 build` runs ACT4 and Sail. Tests, `act4 verify`, and `act4 run`
//! read the committed files only.

use std::fs;
use std::path::Path;

use serde_json::Value;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::Value as TraceValue;
use systemscope_elf::LoadImage;
use systemscope_platform::uart;
use systemscope_rv32i::Rv32iProfile;

use crate::hello::uart_traffic;
use crate::manifest::{addr, array, digest, dir_entries, load, q, s};
use crate::runner::{self, FixtureResult, Outcome, Report, Start, UART};
use crate::{RAM_BASE, RAM_SIZE, hex, unhex32};

/// The ACT4 repository.
pub const ACT4_REPO: &str = "https://github.com/riscv/riscv-arch-test.git";
/// The pinned ACT4 commit.
pub const ACT4_COMMIT: &str = "54cfe21bb70ecc0609ab5a70588f8ecfc3e4bf88";
/// SHA-256 of `testplans/I.csv` at [`ACT4_COMMIT`]: the I testplan the tests cover.
pub const ACT4_TESTPLAN_SHA256: &str =
    "12fc316e0fbacbebde91c3d730871929ec235f22e3a060aafed4734e1e4c0272";
/// SHA-256 of ACT4's `uv.lock`, which pins its Python dependencies.
pub const ACT4_UV_LOCK_SHA256: &str =
    "4f5740cd6457b3c4bff96bd2b7b42b1785c987a3ead6c70d0b89b1493bd8a93b";
/// SHA-256 of ACT4's `framework/src/act/data/Gemfile.lock`, which pins UDB's gems.
pub const ACT4_GEMFILE_LOCK_SHA256: &str =
    "5e06729627ddcfdb70ed37855c9309245b868db4f8461c4f63430c6b3d6891c5";
/// The Sail reference model release.
pub const SAIL_VERSION: &str = "0.14.1";
/// The `sail-riscv` commit the [`SAIL_VERSION`] tag names.
pub const SAIL_COMMIT: &str = "e4b243f4eb5d1ed05bbbc030ad338c2a32c45d72";
/// The Sail release tarball.
pub const SAIL_URL: &str =
    "https://github.com/riscv/sail-riscv/releases/download/0.14.1/sail-riscv-Linux-x86_64.tar.gz";
/// SHA-256 of [`SAIL_URL`].
pub const SAIL_TARBALL_SHA256: &str =
    "de45a89748ca67a8a522b3ac0924c303b5609a16bb50d759bbd08c4d440df0eb";
/// SHA-256 of the tarball's `bin/sail_riscv_sim`.
pub const SAIL_BINARY_SHA256: &str =
    "4ccc3bb600387165323f8812bf534e91740f51d1c05ebf6184afb4f6f2a9d2a9";
/// The riscv-collab GCC release tarball.
pub const GCC_URL: &str = "https://github.com/riscv-collab/riscv-gnu-toolchain/releases/download/2026.08.27/riscv64-elf-ubuntu-24.04-gcc.tar.xz";
/// SHA-256 of [`GCC_URL`].
pub const GCC_TARBALL_SHA256: &str =
    "fe7dadf99dfaee59855b4be5f8d491dc66593bec295090e155a3ec51f0d14f56";
/// SHA-256 of the tarball's `bin/riscv64-unknown-elf-gcc`.
pub const GCC_BINARY_SHA256: &str =
    "fa4616e3aa8b2abddeb95e1b13be57a1ec425203278bf20debb02ce6c63385c3";
/// SHA-256 of the tarball's `bin/riscv64-unknown-elf-as`.
pub const AS_BINARY_SHA256: &str =
    "aad8812815e58b1727e31cf7d50302df3807e8c74847cfa29f24711f0895407c";
/// The first line of `riscv64-unknown-elf-gcc --version`.
pub const GCC_VERSION_LINE: &str = "riscv64-unknown-elf-gcc (g6afcc4f6d) 16.1.0";
/// The first line of `riscv64-unknown-elf-as --version`.
pub const AS_VERSION_LINE: &str = "GNU assembler (GNU Binutils) 2.47.20260726";
/// The `mise` release that installs ACT4's Ruby and uv.
pub const MISE_URL: &str =
    "https://github.com/jdx/mise/releases/download/v2026.9.12/mise-v2026.9.12-linux-x64";
/// SHA-256 of [`MISE_URL`].
pub const MISE_SHA256: &str = "e79ae57945034903aee8aa2ea66b4c7ca9cd4f4edd5a8a78a589cbae6d0f428a";
/// Ruby, as ACT4's `.mise.toml` pins it.
pub const RUBY_VERSION: &str = "ruby 3.4.10";
/// uv, as ACT4's `.mise.toml` pins it.
pub const UV_VERSION: &str = "uv 0.11.33";
/// The Python uv chooses for ACT4's `.python-version`.
pub const PYTHON_VERSION: &str = "Python 3.14.6";
/// The extensions whose tests ACT4 selects (`make elfs EXTENSIONS=I`).
pub const EXTENSIONS: &str = "I";
/// Where every generation runs. The ELFs' debug information names paths under it, so
/// a fixed path makes them the same on every machine.
pub const CANONICAL_PATH: &str = "/tmp/systemscope-act4";

/// What SystemScope's CPU implements.
pub const ARCHITECTURAL_CAPABILITY: &str = "RV32I";
/// The extensions the ACT4 UDB adapter configuration declares.
pub const ADAPTER_EXTENSIONS: [&str; 2] = ["I", "Sm"];
/// Why [`ADAPTER_EXTENSIONS`] names Sm.
pub const SM_SHIM: &str = "Sm appears only in the ACT4 UDB adapter configuration because the \
                           pinned ACT4/UDB schema requires Sm in order to express MXLEN=32. It \
                           does not describe a SystemScope CPU capability.";
/// The adapter's `include_priv_tests`: no privileged test is selected.
pub const INCLUDE_PRIV_TESTS: bool = false;

/// The mnemonics a final ELF may hold, by GNU objdump `-d -M no-aliases` over its
/// executable sections: RV32I, and ECALL. FENCE.TSO is the RV32I FENCE encoding with
/// `fm = 1000`. No CSR, MRET, SRET, WFI, M, A, C, or Zifencei instruction is allowed.
pub const ALLOWED_MNEMONICS: [&str; 40] = [
    "lui",
    "auipc",
    "jal",
    "jalr",
    "beq",
    "bne",
    "blt",
    "bge",
    "bltu",
    "bgeu",
    "lb",
    "lh",
    "lw",
    "lbu",
    "lhu",
    "sb",
    "sh",
    "sw",
    "addi",
    "slti",
    "sltiu",
    "xori",
    "ori",
    "andi",
    "slli",
    "srli",
    "srai",
    "add",
    "sub",
    "sll",
    "slt",
    "sltu",
    "xor",
    "srl",
    "sra",
    "or",
    "and",
    "fence",
    "fence.tso",
    "ecall",
];

/// The ACT4 directory, relative to the workspace root.
pub const ACT4_DIR: &str = "tests/act4";
/// The build script, relative to the workspace root.
pub const ACT4_SCRIPT: &str = "tests/act4/build-act4.sh";
/// The adapter configuration, relative to the workspace root.
pub const ACT4_CONFIG_DIR: &str = "tests/act4/config/systemscope-rv32i";
/// The fixture directory, relative to the workspace root.
pub const ACT4_FIXTURE_DIR: &str = "tests/act4/fixtures";
/// The manifest, relative to the workspace root.
pub const ACT4_MANIFEST: &str = "tests/act4/manifest.json";
/// ACT4's license (Apache-2.0, `COPYING.APACHE` at [`ACT4_COMMIT`]), kept next to the ELFs.
pub const ACT4_LICENSE: &str = "LICENSE.riscv-arch-test";
/// The record the build script writes next to the ELFs.
pub const RECORD: &str = "record.txt";
/// The ELF directory in a build's output directory.
pub const OUT_ELFS: &str = "elfs";
/// The files a generation depends on besides the pinned stack, relative to the
/// workspace root. The manifest records their hashes, so changing one without
/// regenerating fails `verify`.
pub const ACT4_INPUTS: [&str; 8] = [
    "tests/act4/build-act4.sh",
    "tests/act4/config/systemscope-rv32i/link.ld",
    "tests/act4/config/systemscope-rv32i/rvmodel_macros.h",
    "tests/act4/config/systemscope-rv32i/sail.json",
    "tests/act4/config/systemscope-rv32i/systemscope-act4-gcc",
    "tests/act4/config/systemscope-rv32i/systemscope-rv32i.yaml",
    "tests/act4/config/systemscope-rv32i/test_config.yaml",
    "tests/act4/fixtures/LICENSE.riscv-arch-test",
];

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// The identity lines that open a build's record, in order: what the build script
/// checked the stack against. A record with any other identity is not a generation of
/// this pin set.
pub fn identity() -> Vec<(&'static str, String)> {
    [
        ("schema", "1"),
        ("act4-repository", ACT4_REPO),
        ("act4-commit", ACT4_COMMIT),
        ("act4-checkout", "unchanged"),
        ("act4-testplan-sha256", ACT4_TESTPLAN_SHA256),
        ("act4-uv-lock-sha256", ACT4_UV_LOCK_SHA256),
        ("act4-gemfile-lock-sha256", ACT4_GEMFILE_LOCK_SHA256),
        ("extensions", EXTENSIONS),
        ("canonical-path", CANONICAL_PATH),
        ("sail-version", SAIL_VERSION),
        ("sail-tarball-sha256", SAIL_TARBALL_SHA256),
        ("sail-binary-sha256", SAIL_BINARY_SHA256),
        ("gcc-tarball-sha256", GCC_TARBALL_SHA256),
        ("gcc-binary-sha256", GCC_BINARY_SHA256),
        ("as-binary-sha256", AS_BINARY_SHA256),
        ("gcc-version", GCC_VERSION_LINE),
        ("as-version", AS_VERSION_LINE),
        ("mise-sha256", MISE_SHA256),
        ("ruby-version", RUBY_VERSION),
        ("uv-version", UV_VERSION),
        ("python-version", PYTHON_VERSION),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// The pins the build script downloads by, which the record does not repeat.
pub fn sources() -> Vec<(&'static str, String)> {
    [
        ("sail-commit", SAIL_COMMIT),
        ("sail-url", SAIL_URL),
        ("gcc-url", GCC_URL),
        ("mise-url", MISE_URL),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}

/// What UART output a passing test prints: ACT4's summary line for its source file.
pub fn pass_output(name: &str) -> String {
    format!("\nRVCP-SUMMARY: TEST PASSED - Test File \"{name}.S\"\n\n")
}

/// One test as the build script recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedTest {
    /// The test name, such as `I-add-00`.
    pub name: String,
    /// Its source, relative to the ACT4 root.
    pub source: String,
    /// SHA-256 of the signature Sail wrote for it.
    pub signature_sha256: [u8; 32],
    /// SHA-256 of the `.results` file generated from the signature.
    pub results_sha256: [u8; 32],
    /// The instruction audit: each mnemonic in the final ELF's executable sections and
    /// how often it occurs, sorted by mnemonic.
    pub instructions: Vec<(String, u64)>,
}

/// `record.txt`: what one generation checked, and what it built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The identity lines, in order.
    pub identity: Vec<(String, String)>,
    /// The tests ACT4 selected, sorted by name.
    pub tests: Vec<RecordedTest>,
}

impl Record {
    /// Reads the build script's record.
    pub fn parse(text: &str) -> Result<Record, String> {
        let mut identity = Vec::new();
        let mut tests = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let at = |e: String| format!("line {}: {e}", i + 1);
            if let Some(rest) = line.strip_prefix("test ") {
                let fields: Vec<&str> = rest.split(' ').collect();
                let [name, source, sig, results, instructions] = fields[..] else {
                    return Err(at(format!("malformed test line {line:?}")));
                };
                let sha = |v: &str| unhex32(v).ok_or_else(|| at(format!("{v:?} is no SHA-256")));
                let instructions = instructions
                    .split(',')
                    .map(|pair| {
                        let (m, n) = pair
                            .split_once('=')
                            .ok_or_else(|| at(format!("malformed audit entry {pair:?}")))?;
                        let n = n
                            .parse()
                            .map_err(|_| at(format!("malformed audit count {pair:?}")))?;
                        Ok((m.to_owned(), n))
                    })
                    .collect::<Result<_, String>>()?;
                tests.push(RecordedTest {
                    name: name.to_owned(),
                    source: source.to_owned(),
                    signature_sha256: sha(sig)?,
                    results_sha256: sha(results)?,
                    instructions,
                });
            } else if tests.is_empty() {
                let (k, v) = line
                    .split_once(' ')
                    .ok_or_else(|| at(format!("malformed identity line {line:?}")))?;
                identity.push((k.to_owned(), v.to_owned()));
            } else {
                return Err(at(format!("identity line {line:?} after the tests")));
            }
        }
        Ok(Record { identity, tests })
    }

    /// Fails unless the record is a generation of today's pins, and its tests are
    /// sensible: at least one, sorted and unique, each with its source under
    /// `tests/rv32i/`, and an audit that finds only [`ALLOWED_MNEMONICS`].
    pub fn check(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        let expected: Vec<(String, String)> = identity()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        if self.identity != expected {
            for (k, v) in &expected {
                match self.identity.iter().find(|(rk, _)| rk == k) {
                    None => errors.push(format!("the record has no {k}")),
                    Some((_, rv)) if rv != v => {
                        errors.push(format!("the record's {k} is {rv:?}, the pin {v:?}"))
                    }
                    Some(_) => {}
                }
            }
            if errors.is_empty() {
                errors.push(format!(
                    "the record's identity {:?} is not the pinned one {expected:?}",
                    self.identity
                ));
            }
        }
        if self.tests.is_empty() {
            errors.push("the record names no test".to_owned());
        }
        if !self.tests.windows(2).all(|w| w[0].name < w[1].name) {
            errors.push("the record's tests are not sorted and unique".to_owned());
        }
        for t in &self.tests {
            let good_name = !t.name.is_empty()
                && t.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !good_name {
                errors.push(format!("{:?} is not a test name", t.name));
            }
            if !t.source.starts_with("tests/rv32i/")
                || !t.source.ends_with(&format!("/{}.S", t.name))
            {
                errors.push(format!(
                    "{}: source {} is not an RV32I test",
                    t.name, t.source
                ));
            }
            errors.extend(audit(&t.name, &t.instructions));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Why `instructions`, an audit of test `name`, fails: an empty audit, one out of order,
/// or any mnemonic outside [`ALLOWED_MNEMONICS`].
pub fn audit(name: &str, instructions: &[(String, u64)]) -> Vec<String> {
    let mut errors = Vec::new();
    if instructions.is_empty() {
        errors.push(format!("{name}: the audit found no instruction"));
    }
    if !instructions.windows(2).all(|w| w[0].0 < w[1].0) {
        errors.push(format!("{name}: the audit is not sorted and unique"));
    }
    for (m, n) in instructions {
        if !ALLOWED_MNEMONICS.contains(&m.as_str()) {
            errors.push(format!(
                "{name}: {n} × {m}, outside RV32I and ECALL (the instruction audit)"
            ));
        }
        if *n == 0 {
            errors.push(format!("{name}: the audit counts 0 × {m}"));
        }
    }
    errors
}

/// A test in the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act4Test {
    /// What the build script recorded.
    pub recorded: RecordedTest,
    /// The ELF's file name in the fixture directory.
    pub elf: String,
    /// BLAKE3 of the ELF.
    pub blake3: [u8; 32],
    /// The ELF's size in bytes.
    pub size: u64,
    /// The loader's `image_hash`.
    pub image_hash: [u8; 32],
    /// The loader's entry point.
    pub entry: u32,
}

impl Act4Test {
    /// The entry for `recorded`, from the ELF's bytes. Fails unless the loader accepts
    /// the file for the `m1-reference` RAM with the entry at the RAM base.
    pub fn from_elf(recorded: &RecordedTest, bytes: &[u8]) -> Result<Act4Test, String> {
        let image = load(&recorded.name, bytes)?;
        Ok(Act4Test {
            recorded: recorded.clone(),
            elf: format!("{}.elf", recorded.name),
            blake3: *blake3::hash(bytes).as_bytes(),
            size: bytes.len() as u64,
            image_hash: image.image_hash,
            entry: image.entry,
        })
    }

    /// Loads the ELF's bytes, provided they are exactly the file this entry names.
    pub fn check(&self, bytes: &[u8]) -> Result<LoadImage, String> {
        let actual = *blake3::hash(bytes).as_bytes();
        if actual != self.blake3 || bytes.len() as u64 != self.size {
            return Err(format!(
                "{}: {} bytes with BLAKE3 {}, the manifest's {} bytes with {}",
                self.elf,
                bytes.len(),
                hex(&actual),
                self.size,
                hex(&self.blake3)
            ));
        }
        let image = load(&self.recorded.name, bytes)?;
        if image.image_hash != self.image_hash || image.entry != self.entry {
            return Err(format!(
                "{}: the loader gives image_hash {} and entry {:#x}, the manifest {} and {:#x}",
                self.elf,
                hex(&image.image_hash),
                image.entry,
                hex(&self.image_hash),
                self.entry
            ));
        }
        Ok(image)
    }

    /// Reads the ELF from the fixture directory under `root` and [checks](Self::check) it.
    pub fn read(&self, root: &Path) -> Result<LoadImage, String> {
        let path = root.join(ACT4_FIXTURE_DIR).join(&self.elf);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.check(&bytes)
    }
}

/// The contents of [`ACT4_MANIFEST`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act4Manifest {
    /// [`SCHEMA`].
    pub schema: u64,
    /// [`ARCHITECTURAL_CAPABILITY`].
    pub capability: String,
    /// [`ADAPTER_EXTENSIONS`].
    pub adapter_extensions: Vec<String>,
    /// [`SM_SHIM`].
    pub adapter_note: String,
    /// [`INCLUDE_PRIV_TESTS`].
    pub include_priv_tests: bool,
    /// The record's identity: the pinned stack the generation checked.
    pub identity: Vec<(String, String)>,
    /// [`sources`].
    pub sources: Vec<(String, String)>,
    /// The RAM base: every entry point.
    pub ram_base: u32,
    /// The RAM size the images are checked against.
    pub ram_size: u32,
    /// BLAKE3 of each file in [`ACT4_INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// [`ALLOWED_MNEMONICS`].
    pub allowed_mnemonics: Vec<String>,
    /// How many tests ACT4 selected: the length of `tests`, and what `run` must execute
    /// and pass.
    pub count: usize,
    /// The tests, sorted by name.
    pub tests: Vec<Act4Test>,
}

impl Act4Manifest {
    /// The manifest for `record`'s tests, with the ELFs in `elf_dir` and the inputs and
    /// pins under `root`.
    pub fn generate(root: &Path, record: &Record, elf_dir: &Path) -> Result<Act4Manifest, String> {
        let inputs = ACT4_INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let tests = record
            .tests
            .iter()
            .map(|t| {
                let path = elf_dir.join(format!("{}.elf", t.name));
                let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                Act4Test::from_elf(t, &bytes)
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Act4Manifest {
            schema: SCHEMA,
            capability: ARCHITECTURAL_CAPABILITY.to_owned(),
            adapter_extensions: ADAPTER_EXTENSIONS.iter().map(|&e| e.to_owned()).collect(),
            adapter_note: SM_SHIM.to_owned(),
            include_priv_tests: INCLUDE_PRIV_TESTS,
            identity: record.identity.clone(),
            sources: sources()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            ram_base: RAM_BASE,
            ram_size: RAM_SIZE,
            inputs,
            allowed_mnemonics: ALLOWED_MNEMONICS.iter().map(|&m| m.to_owned()).collect(),
            count: tests.len(),
            tests,
        })
    }

    /// The record this manifest was generated from.
    pub fn record(&self) -> Record {
        Record {
            identity: self.identity.clone(),
            tests: self.tests.iter().map(|t| t.recorded.clone()).collect(),
        }
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<Act4Manifest, String> {
        let text = fs::read_to_string(root.join(ACT4_MANIFEST))
            .map_err(|e| format!("{ACT4_MANIFEST}: {e}"))?;
        Act4Manifest::parse(&text).map_err(|e| format!("{ACT4_MANIFEST}: {e}"))
    }

    /// The file contents: stable, pretty-printed JSON with a trailing newline.
    pub fn render(&self) -> String {
        let pairs = |pairs: &[(String, String)]| {
            pairs
                .iter()
                .map(|(k, v)| format!("    [{}, {}]", q(k), q(v)))
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .map(|(path, hash)| {
                format!(
                    "    {{ \"path\": {}, \"blake3\": {} }}",
                    q(path),
                    q(&hex(hash))
                )
            })
            .collect();
        let list = |items: &[String]| items.iter().map(|i| q(i)).collect::<Vec<_>>().join(", ");
        let tests: Vec<String> = self
            .tests
            .iter()
            .map(|t| {
                let r = &t.recorded;
                let instructions: Vec<String> = r
                    .instructions
                    .iter()
                    .map(|(m, n)| format!("{}: {n}", q(m)))
                    .collect();
                format!(
                    "    {{\n      \"name\": {},\n      \"source\": {},\n      \"elf\": {},\n      \
                     \"blake3\": {},\n      \"size\": {},\n      \"entry\": {},\n      \
                     \"image_hash\": {},\n      \"signature_sha256\": {},\n      \
                     \"results_sha256\": {},\n      \"instructions\": {{ {} }}\n    }}",
                    q(&r.name),
                    q(&r.source),
                    q(&t.elf),
                    q(&hex(&t.blake3)),
                    t.size,
                    q(&format!("{:#010x}", t.entry)),
                    q(&hex(&t.image_hash)),
                    q(&hex(&r.signature_sha256)),
                    q(&hex(&r.results_sha256)),
                    instructions.join(", ")
                )
            })
            .collect();
        format!(
            "{{\n  \"schema\": {},\n  \"architectural_capability\": {},\n  \
             \"adapter_extensions\": [{}],\n  \"adapter_note\": {},\n  \
             \"include_priv_tests\": {},\n  \"identity\": [\n{}\n  ],\n  \
             \"sources\": [\n{}\n  ],\n  \"ram\": {{ \"base\": {}, \"size\": {} }},\n  \
             \"inputs\": [\n{}\n  ],\n  \"allowed_mnemonics\": [{}],\n  \"count\": {},\n  \
             \"tests\": [\n{}\n  ]\n}}\n",
            self.schema,
            q(&self.capability),
            list(&self.adapter_extensions),
            q(&self.adapter_note),
            self.include_priv_tests,
            pairs(&self.identity),
            pairs(&self.sources),
            q(&format!("{:#010x}", self.ram_base)),
            q(&format!("{:#010x}", self.ram_size)),
            inputs.join(",\n"),
            list(&self.allowed_mnemonics),
            self.count,
            tests.join(",\n"),
        )
    }

    /// Reads [`Act4Manifest::render`]'s output.
    pub fn parse(json: &str) -> Result<Act4Manifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        // Pairs, not objects: their order is part of the record.
        let pairs = |v: &Value| -> Result<Vec<(String, String)>, String> {
            array(v)?
                .iter()
                .map(|pair| match pair.as_array().map(Vec::as_slice) {
                    Some([k, v]) => Ok((s(k)?, s(v)?)),
                    _ => Err(format!("{pair} is not a [key, value] pair")),
                })
                .collect()
        };
        let strings =
            |v: &Value| -> Result<Vec<String>, String> { array(v)?.iter().map(s).collect() };
        let tests = array(&root["tests"])?
            .iter()
            .map(|t| {
                let instructions = t["instructions"]
                    .as_object()
                    .ok_or("no instructions")?
                    .iter()
                    .map(|(m, n)| {
                        Ok((
                            m.clone(),
                            n.as_u64().ok_or_else(|| format!("{n} is no count"))?,
                        ))
                    })
                    .collect::<Result<_, String>>()?;
                let sha = |v: &Value| digest(v);
                Ok(Act4Test {
                    recorded: RecordedTest {
                        name: s(&t["name"])?,
                        source: s(&t["source"])?,
                        signature_sha256: sha(&t["signature_sha256"])?,
                        results_sha256: sha(&t["results_sha256"])?,
                        instructions,
                    },
                    elf: s(&t["elf"])?,
                    blake3: digest(&t["blake3"])?,
                    size: t["size"].as_u64().ok_or("no size")?,
                    image_hash: digest(&t["image_hash"])?,
                    entry: addr(&t["entry"])?,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Act4Manifest {
            schema,
            capability: s(&root["architectural_capability"])?,
            adapter_extensions: strings(&root["adapter_extensions"])?,
            adapter_note: s(&root["adapter_note"])?,
            include_priv_tests: root["include_priv_tests"]
                .as_bool()
                .ok_or("no include_priv_tests")?,
            identity: pairs(&root["identity"])?,
            sources: pairs(&root["sources"])?,
            ram_base: addr(&root["ram"]["base"])?,
            ram_size: addr(&root["ram"]["size"])?,
            inputs: array(&root["inputs"])?
                .iter()
                .map(|i| Ok((s(&i["path"])?, digest(&i["blake3"])?)))
                .collect::<Result<_, String>>()?,
            allowed_mnemonics: strings(&root["allowed_mnemonics"])?,
            count: usize::try_from(root["count"].as_u64().ok_or("no count")?)
                .map_err(|e| e.to_string())?,
            tests,
        })
    }

    /// The test names, in order.
    pub fn names(&self) -> Vec<&str> {
        self.tests
            .iter()
            .map(|t| t.recorded.name.as_str())
            .collect()
    }
}

/// One line per difference between two manifests, field by field and test by test.
pub fn describe_differences(committed: &Act4Manifest, actual: &Act4Manifest) -> Vec<String> {
    let mut out = Vec::new();
    let head = |m: &Act4Manifest| {
        let mut m = m.clone();
        m.tests.clear();
        m.inputs.clear();
        m.count = 0;
        m
    };
    if head(committed) != head(actual) {
        out.push(format!(
            "pins: manifest {:?}, actual {:?}",
            head(committed),
            head(actual)
        ));
    }
    for (path, hash) in &actual.inputs {
        match committed.inputs.iter().find(|(p, _)| p == path) {
            None => out.push(format!("input {path}: not in the manifest")),
            Some((_, h)) if h != hash => out.push(format!(
                "input {path}: BLAKE3 {} differs from the manifest's {}; regenerate the corpus \
                 with `cargo xtask act4 build`",
                hex(hash),
                hex(h)
            )),
            Some(_) => {}
        }
    }
    for (path, _) in &committed.inputs {
        if !actual.inputs.iter().any(|(p, _)| p == path) {
            out.push(format!("input {path}: in the manifest, not an input"));
        }
    }
    if committed.count != actual.count {
        out.push(format!(
            "count: manifest {}, actual {}",
            committed.count, actual.count
        ));
    }
    for t in &actual.tests {
        match committed
            .tests
            .iter()
            .find(|c| c.recorded.name == t.recorded.name)
        {
            None => out.push(format!("{}: not in the manifest", t.recorded.name)),
            Some(c) if c != t => {
                out.push(format!("{}: manifest {c:?}, actual {t:?}", t.recorded.name))
            }
            Some(_) => {}
        }
    }
    for c in &committed.tests {
        if !actual
            .tests
            .iter()
            .any(|t| t.recorded.name == c.recorded.name)
        {
            out.push(format!(
                "{}: in the manifest, not generated",
                c.recorded.name
            ));
        }
    }
    out
}

/// `cargo xtask act4 verify`: checks the committed corpus without a network, ACT4, Sail,
/// or a compiler.
///
/// - The manifest parses, declares capability RV32I, adapter extensions I + Sm with
///   privileged tests off, and records a generation of today's pins whose audit found
///   RV32I and ECALL only.
/// - `count` equals the number of tests.
/// - It equals the manifest generated from the files on disk: every ELF has the recorded
///   BLAKE3 and size, the loader accepts it with the recorded `image_hash` and an entry
///   at the RAM base, and every input has the recorded hash. The file is byte-identical
///   to the rendering, so formatting drift fails too.
/// - The fixture directory holds exactly the ELFs and ACT4's license.
///
/// Returns the verified manifest.
pub fn verify(root: &Path) -> Result<Act4Manifest, Vec<String>> {
    let committed = Act4Manifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    if committed.capability != ARCHITECTURAL_CAPABILITY
        || committed.adapter_extensions != ADAPTER_EXTENSIONS
        || committed.adapter_note != SM_SHIM
        || committed.include_priv_tests != INCLUDE_PRIV_TESTS
        || committed.allowed_mnemonics != ALLOWED_MNEMONICS
    {
        errors.push(format!(
            "{ACT4_MANIFEST}: capability {:?}, adapter extensions {:?}, include_priv_tests \
             {}, or the audit allowlist is not what this crate pins",
            committed.capability, committed.adapter_extensions, committed.include_priv_tests
        ));
    }
    let record = committed.record();
    if let Err(e) = record.check() {
        errors.extend(e);
    }
    if committed.count != committed.tests.len() {
        errors.push(format!(
            "{ACT4_MANIFEST}: count {} but {} tests",
            committed.count,
            committed.tests.len()
        ));
    }
    match Act4Manifest::generate(root, &record, &root.join(ACT4_FIXTURE_DIR)) {
        Err(e) => errors.push(e),
        Ok(actual) => {
            errors.extend(describe_differences(&committed, &actual));
            let text = fs::read_to_string(root.join(ACT4_MANIFEST)).unwrap_or_default();
            if errors.is_empty() && text != actual.render() {
                errors.push(format!(
                    "{ACT4_MANIFEST} is not formatted as `cargo xtask act4` writes it"
                ));
            }
        }
    }
    errors.extend(check_entries(
        &root.join(ACT4_FIXTURE_DIR),
        &committed.record(),
        &[ACT4_LICENSE],
    ));
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// Why `dir` does not hold exactly `record`'s ELFs and `extra`.
fn check_entries(dir: &Path, record: &Record, extra: &[&str]) -> Vec<String> {
    let entries = match dir_entries(dir) {
        Ok(entries) => entries,
        Err(e) => return vec![e],
    };
    let mut expected: Vec<String> = record
        .tests
        .iter()
        .map(|t| format!("{}.elf", t.name))
        .collect();
    expected.extend(extra.iter().map(|&e| e.to_owned()));
    expected.sort();
    let mut errors = Vec::new();
    for extra in entries.iter().filter(|e| !expected.contains(e)) {
        errors.push(format!("{}/{extra}: not in the manifest", dir.display()));
    }
    for missing in expected.iter().filter(|e| !entries.contains(e)) {
        errors.push(format!("{}/{missing}: missing", dir.display()));
    }
    errors
}

/// Reads and [checks](Record::check) the record of the generation in `out`.
pub fn read_record(out: &Path) -> Result<Record, Vec<String>> {
    let path = out.join(RECORD);
    let text = fs::read_to_string(&path).map_err(|e| vec![format!("{}: {e}", path.display())])?;
    let record = Record::parse(&text).map_err(|e| vec![format!("{}: {e}", path.display())])?;
    record.check()?;
    let errors = check_entries(&out.join(OUT_ELFS), &record, &[]);
    if errors.is_empty() {
        Ok(record)
    } else {
        Err(errors)
    }
}

/// `cargo xtask act4 check <out>`: the generation in `out` reproduces the committed
/// corpus exactly: the same tests, byte-identical ELFs, and the same record, so the
/// manifest generated from it is the committed manifest byte for byte.
pub fn check_regenerated(root: &Path, out: &Path) -> Result<Act4Manifest, Vec<String>> {
    let committed = verify(root)?;
    let record = read_record(out)?;
    let mut errors = Vec::new();
    if record.tests.len() != committed.count {
        errors.push(format!(
            "the generation selected {} tests, the committed corpus {}",
            record.tests.len(),
            committed.count
        ));
    }
    for t in &record.tests {
        let generated = fs::read(out.join(OUT_ELFS).join(format!("{}.elf", t.name)));
        let committed_elf = fs::read(root.join(ACT4_FIXTURE_DIR).join(format!("{}.elf", t.name)));
        match (generated, committed_elf) {
            (Ok(a), Ok(b)) if a == b => {}
            (Ok(_), Ok(_)) => errors.push(format!("{}: the generated ELF differs", t.name)),
            (Err(e), _) => errors.push(format!("{}: generated: {e}", t.name)),
            (_, Err(e)) => errors.push(format!("{}: committed: {e}", t.name)),
        }
    }
    match Act4Manifest::generate(root, &record, &out.join(OUT_ELFS)) {
        Err(e) => errors.push(e),
        Ok(generated) => {
            errors.extend(describe_differences(&committed, &generated));
            let text = fs::read_to_string(root.join(ACT4_MANIFEST)).unwrap_or_default();
            if errors.is_empty() && generated.render() != text {
                errors.push(format!(
                    "the manifest generated from {} is not {ACT4_MANIFEST} byte for byte",
                    out.display()
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// `cargo xtask act4 build`'s last step: replaces the committed ELFs with the generation
/// in `out` and rewrites the manifest. Returns the new manifest and the old one, if any.
pub fn install(
    root: &Path,
    out: &Path,
) -> Result<(Act4Manifest, Option<Act4Manifest>), Vec<String>> {
    let record = read_record(out)?;
    let generated =
        Act4Manifest::generate(root, &record, &out.join(OUT_ELFS)).map_err(|e| vec![e])?;
    let old = Act4Manifest::read(root).ok();
    let dir = root.join(ACT4_FIXTURE_DIR);
    let io = |e: std::io::Error, what: &Path| vec![format!("{}: {e}", what.display())];
    for name in dir_entries(&dir).map_err(|e| vec![e])? {
        if name.ends_with(".elf") {
            let path = dir.join(&name);
            fs::remove_file(&path).map_err(|e| io(e, &path))?;
        }
    }
    for t in &record.tests {
        let from = out.join(OUT_ELFS).join(format!("{}.elf", t.name));
        let to = dir.join(format!("{}.elf", t.name));
        fs::copy(&from, &to).map_err(|e| io(e, &from))?;
    }
    let path = root.join(ACT4_MANIFEST);
    fs::write(&path, generated.render()).map_err(|e| io(e, &path))?;
    Ok((generated, old))
}

/// A run of one ACT4 ELF: its outcome and everything the UART printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act4Run {
    /// How it ended.
    pub outcome: Outcome,
    /// The UART's output, from the writes it received; an error if it received anything
    /// but one-byte writes to its TX register, or if its own view disagrees.
    pub output: Result<Vec<u8>, String>,
}

/// Runs `image` untraced on `m1-reference` with the UART.
pub fn run(image: &LoadImage) -> Act4Run {
    run_with(image, Rv32iProfile::M1)
}

/// [`run`] with the CPU in `profile`.
pub fn run_with(image: &LoadImage, profile: Rv32iProfile) -> Act4Run {
    let finished = runner::execute(
        runner::platform_with_profile(image, true, runner::SEED, profile),
        Start::Init { traced: false },
        Vec::new(),
    );
    let output = uart_bytes(&finished);
    Act4Run {
        outcome: finished.outcome,
        output,
    }
}

/// The bytes the UART received, cross-checked with its view: `tx_len` must count them
/// all and `tx_tail` must be their end.
fn uart_bytes(finished: &runner::Finished) -> Result<Vec<u8>, String> {
    let (requests, _) = uart_traffic(&finished.dispatched);
    let mut out = Vec::new();
    for m in requests {
        match m {
            MemMsg::WriteReq { addr, data, .. } if *addr == uart::TX && data.len() == 1 => {
                out.push(data[0]);
            }
            other => {
                return Err(format!(
                    "the UART received {other:?}, not a one-byte TX write"
                ));
            }
        }
    }
    if out.is_empty() {
        return Ok(out);
    }
    let view = finished
        .views
        .get(UART.0 as usize)
        .ok_or("no view of the UART")?;
    let (Some(TraceValue::U64(len)), Some(TraceValue::Bytes(tail))) =
        (view.get("tx_len"), view.get("tx_tail"))
    else {
        return Err(format!(
            "the UART's view has no tx_len and tx_tail: {view:?}"
        ));
    };
    if *len != out.len() as u64 || !out.ends_with(tail) {
        return Err(format!(
            "the UART's view counts {len} bytes ending {tail:?}; it received {} bytes",
            out.len()
        ));
    }
    Ok(out)
}

/// The M1-A4 rule for test `name`: `Ok` if it passed, otherwise why it failed.
///
/// The run must end as an `rv32ui` test passes ([`runner::judge`]:
/// `Trap(EnvironmentCall)`, `gp == 1`, `a0 == 0`; an instruction limit, a fault, or any
/// other trap fails), and the UART must have printed exactly [`pass_output`]`(name)`:
/// nothing before or after, no failure diagnostic, and no other test's summary.
pub fn judge(name: &str, run: &Act4Run) -> Result<(), String> {
    judge_with(name, run, Rv32iProfile::M1)
}

/// [`judge`] for a run with the CPU in `profile`, whose pass cause is
/// [`runner::pass_cause`]`(profile)`.
pub fn judge_with(name: &str, run: &Act4Run, profile: Rv32iProfile) -> Result<(), String> {
    let diagnostic = || match &run.output {
        Ok(bytes) => format!("the UART printed {:?}", String::from_utf8_lossy(bytes)),
        Err(e) => e.clone(),
    };
    runner::judge_with(&run.outcome, profile).map_err(|e| format!("{e}; {}", diagnostic()))?;
    let output = run.output.as_ref().map_err(Clone::clone)?;
    let expected = pass_output(name);
    if output.as_slice() != expected.as_bytes() {
        return Err(format!("{}, not {expected:?}", diagnostic()));
    }
    Ok(())
}

/// Reads `test` under `root`, checks it against its manifest entry, and runs it.
pub fn run_test(root: &Path, test: &Act4Test) -> Result<FixtureResult, String> {
    run_test_with(root, test, Rv32iProfile::M1)
}

/// [`run_test`] with the CPU in `profile`.
pub fn run_test_with(
    root: &Path,
    test: &Act4Test,
    profile: Rv32iProfile,
) -> Result<FixtureResult, String> {
    let image = test.read(root)?;
    let run = run_with(&image, profile);
    Ok(FixtureResult {
        name: test.recorded.name.clone(),
        blake3: test.blake3,
        image_hash: image.image_hash,
        verdict: judge_with(&test.recorded.name, &run, profile),
        outcome: run.outcome,
    })
}

/// `cargo xtask act4 run`: verifies the committed corpus, then runs every test in it.
/// [`Report::accept`] with the manifest's `count` is M1-A4.
pub fn run_corpus(root: &Path) -> Result<(Act4Manifest, Report), String> {
    run_corpus_with(root, Rv32iProfile::M1)
}

/// [`run_corpus`] with the CPU in `profile`: the M2 and M3 profiles must pass the same
/// corpus (`docs/m2-design.md` §15.4, `docs/m3-design.md` §15.4).
pub fn run_corpus_with(
    root: &Path,
    profile: Rv32iProfile,
) -> Result<(Act4Manifest, Report), String> {
    let manifest = verify(root).map_err(|errors| {
        format!(
            "the ACT4 corpus does not match {ACT4_MANIFEST}:\n  {}",
            errors.join("\n  ")
        )
    })?;
    let mut report = Report {
        selected: manifest.count,
        results: Vec::new(),
        errors: Vec::new(),
    };
    for test in &manifest.tests {
        match run_test_with(root, test, profile) {
            Ok(result) => report.results.push(result),
            Err(e) => report.errors.push(e),
        }
    }
    Ok((manifest, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::End;
    use crate::workspace_root;

    fn outcome(end: End, gp: u32, a0: u32) -> Outcome {
        Outcome {
            end,
            gp,
            a0,
            pc: RAM_BASE,
            instret: 1,
            events: 1,
            state: None,
            execution: [0; 32],
            trace: None,
        }
    }

    fn ecall() -> End {
        End::Trap {
            cause: "EnvironmentCall".to_owned(),
            pc: RAM_BASE,
            tval: 0,
        }
    }

    fn passing(name: &str) -> Act4Run {
        Act4Run {
            outcome: outcome(ecall(), 1, 0),
            output: Ok(pass_output(name).into_bytes()),
        }
    }

    #[test]
    fn a_passing_run_is_accepted() {
        assert_eq!(judge("I-add-00", &passing("I-add-00")), Ok(()));
    }

    #[test]
    fn gp_other_than_one_fails() {
        let mut run = passing("I-add-00");
        run.outcome.gp = 0;
        assert!(judge("I-add-00", &run).is_err());
    }

    #[test]
    fn a0_other_than_zero_fails_even_with_the_pass_summary() {
        let mut run = passing("I-add-00");
        run.outcome.a0 = 1;
        assert!(judge("I-add-00", &run).is_err());
    }

    #[test]
    fn an_instruction_limit_fails_even_with_the_pass_summary() {
        let mut run = passing("I-add-00");
        run.outcome.end = End::InstructionLimit;
        assert!(judge("I-add-00", &run).is_err());
    }

    #[test]
    fn any_other_end_fails() {
        for end in [
            End::Trap {
                cause: "IllegalInstruction".to_owned(),
                pc: RAM_BASE,
                tval: 0,
            },
            End::Trap {
                cause: "LoadAccessFault".to_owned(),
                pc: RAM_BASE,
                tval: 0,
            },
            End::Trap {
                cause: "LoadAddressMisaligned".to_owned(),
                pc: RAM_BASE,
                tval: 1,
            },
            End::Trap {
                cause: "Breakpoint".to_owned(),
                pc: RAM_BASE,
                tval: 0,
            },
            End::Fault("boom".to_owned()),
            End::NotHalted,
        ] {
            let mut run = passing("I-add-00");
            run.outcome.end = end.clone();
            assert!(judge("I-add-00", &run).is_err(), "{end}");
        }
    }

    #[test]
    fn the_output_must_be_exactly_this_tests_summary() {
        let wrong = [
            String::new(),
            pass_output("I-sub-00"),
            format!("{}x", pass_output("I-add-00")),
            format!("x{}", pass_output("I-add-00")),
            pass_output("I-add-00").trim_end().to_owned(),
            "\nRVCP-SUMMARY: TEST PASSED".to_owned(),
            "\nRVCP-SUMMARY: TEST FAILED - Test File \"I-add-00.S\"\n\n".to_owned(),
            format!(
                "\nRVCP-SUMMARY: TEST FAILED - Test File \"I-add-00.S\"\n\n{}",
                pass_output("I-add-00")
            ),
        ];
        for output in wrong {
            let mut run = passing("I-add-00");
            run.output = Ok(output.clone().into_bytes());
            assert!(judge("I-add-00", &run).is_err(), "{output:?}");
        }
        let mut run = passing("I-add-00");
        run.output = Err("the UART received a read".to_owned());
        assert!(judge("I-add-00", &run).is_err());
    }

    #[test]
    fn a_report_that_skips_or_fails_a_test_is_rejected() {
        let pass = |name: &str| FixtureResult {
            name: name.to_owned(),
            blake3: [0; 32],
            image_hash: [0; 32],
            outcome: outcome(ecall(), 1, 0),
            verdict: Ok(()),
        };
        let full = Report {
            selected: 2,
            results: vec![pass("a"), pass("b")],
            errors: Vec::new(),
        };
        assert_eq!(full.accept(2), Ok(()));
        let skipped = Report {
            selected: 2,
            results: vec![pass("a")],
            errors: Vec::new(),
        };
        assert!(skipped.accept(2).is_err());
        let over_selected = Report {
            selected: 3,
            results: vec![pass("a"), pass("b")],
            errors: Vec::new(),
        };
        assert!(over_selected.accept(2).is_err());
        let missing = Report {
            selected: 2,
            results: vec![pass("a"), pass("b")],
            errors: vec!["c.elf: missing".to_owned()],
        };
        assert!(missing.accept(2).is_err());
        let mut failed = full.clone();
        failed.results[1].verdict = Err("a0 = 1".to_owned());
        assert!(failed.accept(2).is_err());
    }

    fn record() -> Record {
        let text = fs::read_to_string(workspace_root().join(ACT4_MANIFEST)).unwrap();
        Act4Manifest::parse(&text).unwrap().record()
    }

    #[test]
    fn the_record_format_round_trips() {
        let record = record();
        let mut text = String::new();
        for (k, v) in &record.identity {
            text.push_str(&format!("{k} {v}\n"));
        }
        for t in &record.tests {
            let audit: Vec<String> = t
                .instructions
                .iter()
                .map(|(m, n)| format!("{m}={n}"))
                .collect();
            text.push_str(&format!(
                "test {} {} {} {} {}\n",
                t.name,
                t.source,
                hex(&t.signature_sha256),
                hex(&t.results_sha256),
                audit.join(",")
            ));
        }
        assert_eq!(Record::parse(&text), Ok(record.clone()));
        assert_eq!(record.check(), Ok(()));
    }

    #[test]
    fn a_record_of_other_pins_or_instructions_is_rejected() {
        let mut other_pin = record();
        other_pin.identity[2].1 = "0".repeat(40);
        assert!(other_pin.check().is_err());
        let mut csr = record();
        csr.tests[0].instructions.push(("csrrw".to_owned(), 1));
        assert!(csr.check().is_err());
        let mut mul = record();
        mul.tests[0].instructions.insert(0, ("#mul".to_owned(), 1));
        assert!(mul.check().is_err());
        let mut undecoded = record();
        undecoded.tests[0]
            .instructions
            .insert(0, ("!c.addi@80000000:".to_owned(), 1));
        assert!(undecoded.check().is_err());
        let mut empty = record();
        empty.tests.clear();
        assert!(empty.check().is_err());
    }

    #[test]
    fn a_manifest_mismatch_or_a_missing_fixture_is_rejected() {
        let root = workspace_root();
        let manifest = Act4Manifest::read(&root).unwrap();
        let test = &manifest.tests[0];
        let mut bytes = fs::read(root.join(ACT4_FIXTURE_DIR).join(&test.elf)).unwrap();
        assert!(test.check(&bytes).is_ok());
        let last = bytes.len() - 1;
        // The manifest's BLAKE3 and size reject the file before the loader sees it.
        let rejected_by_hash = |bytes: &[u8]| {
            test.check(bytes)
                .is_err_and(|e| e.contains(&format!("the manifest's {} bytes", test.size)))
        };
        bytes[last] ^= 1;
        assert!(rejected_by_hash(&bytes), "a changed byte");
        bytes[last] ^= 1;
        bytes.push(0);
        assert!(rejected_by_hash(&bytes), "an appended byte");
        // A manifest whose loader fields do not match the file is rejected too.
        let mut image_hash = test.clone();
        image_hash.image_hash[0] ^= 1;
        assert!(image_hash.check(&bytes[..last + 1]).is_err(), "image_hash");
        let mut entry = test.clone();
        entry.entry += 4;
        assert!(entry.check(&bytes[..last + 1]).is_err(), "entry");
        let mut other = test.clone();
        other.elf = "I-none-00.elf".to_owned();
        assert!(other.read(&root).is_err(), "a missing fixture");
        let wrong = &manifest.tests[1];
        let wrong_bytes = fs::read(root.join(ACT4_FIXTURE_DIR).join(&wrong.elf)).unwrap();
        assert!(rejected_by_hash(&wrong_bytes), "another test's ELF");
    }

    #[test]
    fn describe_differences_reports_every_change() {
        let manifest = Act4Manifest::read(&workspace_root()).unwrap();
        assert!(describe_differences(&manifest, &manifest).is_empty());
        let mut fewer = manifest.clone();
        fewer.tests.pop();
        fewer.count -= 1;
        assert_eq!(describe_differences(&manifest, &fewer).len(), 2);
        let mut input = manifest.clone();
        input.inputs[0].1[0] ^= 1;
        assert_eq!(describe_differences(&manifest, &input).len(), 1);
        let mut shim = manifest.clone();
        shim.adapter_extensions.push("Zicsr".to_owned());
        assert_eq!(describe_differences(&manifest, &shim).len(), 1);
    }

    #[test]
    fn the_manifest_round_trips() {
        let text = fs::read_to_string(workspace_root().join(ACT4_MANIFEST)).unwrap();
        let manifest = Act4Manifest::parse(&text).unwrap();
        assert_eq!(manifest.render(), text);
    }
}
