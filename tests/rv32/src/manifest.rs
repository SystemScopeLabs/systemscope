//! `tests/rv32/fixtures/manifest.json`, the acceptance contract for the fixtures
//! (`docs/m1-design.md` §10.6).
//!
//! It names every selected test with its sources, its ELF, the ELF's BLAKE3, and what the
//! loader makes of it; every excluded test with the reason; and the pins the fixtures
//! were built with. Only `cargo xtask rv32-fixtures build` (or its `manifest` step) writes
//! it, through [`Manifest::generate`] and [`Manifest::render`]. [`verify`] checks the
//! committed fixtures against it, and tests and the runner only read it.

use std::fs;
use std::path::Path;

use serde_json::Value;
use systemscope_elf::{LoadImage, load_elf32};

use crate::{
    EXCLUDED, FIXTURE_DIR, FIXTURE_LICENSE, FLAGS, INPUTS, MANIFEST_PATH, RAM_BASE, RAM_SIZE,
    RISCV_TEST_ENV_COMMIT, RISCV_TESTS_COMMIT, RISCV_TESTS_REPO, SELECTED, TOOLCHAIN,
    TOOLCHAIN_DISTRO, elf_name, hex, unhex32,
};

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// A pinned toolchain package, as the manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    /// The package name.
    pub name: String,
    /// The exact package version.
    pub version: String,
    /// SHA-256 of the `.deb` file.
    pub deb_sha256: String,
    /// The first line of the tool's `--version` output.
    pub version_line: String,
}

/// A selected test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    /// The upstream test name, such as `add`.
    pub name: String,
    /// The upstream wrapper the ELF is built from, relative to the `riscv-tests` root.
    pub source: String,
    /// The upstream test body the wrapper includes.
    pub body: String,
    /// The ELF's file name in the fixture directory.
    pub elf: String,
    /// BLAKE3 of the ELF file.
    pub blake3: [u8; 32],
    /// The loader's `image_hash`: the RAM's name for the program.
    pub image_hash: [u8; 32],
    /// The loader's entry point.
    pub entry: u32,
}

impl Fixture {
    /// The entry for test `name`, from the ELF's bytes. Fails unless the loader accepts
    /// the file for the `m1-reference` RAM with the entry at the RAM base.
    pub fn from_elf(name: &str, bytes: &[u8]) -> Result<Fixture, String> {
        let image = load(name, bytes)?;
        Ok(Fixture {
            name: name.to_owned(),
            source: format!("isa/rv32ui/{name}.S"),
            body: format!("isa/rv64ui/{name}.S"),
            elf: elf_name(name),
            blake3: *blake3::hash(bytes).as_bytes(),
            image_hash: image.image_hash,
            entry: image.entry,
        })
    }

    /// Loads the ELF's bytes, provided they are exactly the file this entry names.
    pub fn check(&self, bytes: &[u8]) -> Result<LoadImage, String> {
        let actual = *blake3::hash(bytes).as_bytes();
        if actual != self.blake3 {
            return Err(format!(
                "{}: BLAKE3 {} differs from the manifest's {}",
                self.elf,
                hex(&actual),
                hex(&self.blake3)
            ));
        }
        let image = load(&self.name, bytes)?;
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
        let path = root.join(FIXTURE_DIR).join(&self.elf);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.check(&bytes)
    }
}

/// Loads a fixture for the `m1-reference` RAM and requires the entry at its base.
fn load(name: &str, bytes: &[u8]) -> Result<LoadImage, String> {
    let image = load_elf32(bytes, RAM_BASE, RAM_SIZE)
        .map_err(|e| format!("{name}: the loader rejects the ELF: {e}"))?;
    if image.entry != RAM_BASE {
        return Err(format!(
            "{name}: entry {:#x} is not the RAM base {RAM_BASE:#x}",
            image.entry
        ));
    }
    Ok(image)
}

/// The contents of [`MANIFEST_PATH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// [`SCHEMA`].
    pub schema: u64,
    /// The upstream repository.
    pub repository: String,
    /// The pinned `riscv-tests` commit.
    pub commit: String,
    /// Its `env` submodule commit.
    pub env_commit: String,
    /// Where the toolchain packages come from.
    pub distro: String,
    /// The toolchain packages.
    pub toolchain: Vec<Tool>,
    /// The compiler flags.
    pub flags: Vec<String>,
    /// The RAM base: every entry point.
    pub ram_base: u32,
    /// The RAM size the images are checked against.
    pub ram_size: u32,
    /// BLAKE3 of each build input in [`INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// The selected tests, in [`SELECTED`] order.
    pub selected: Vec<Fixture>,
    /// The excluded tests and why, in [`EXCLUDED`] order.
    pub excluded: Vec<(String, String)>,
}

impl Manifest {
    /// The manifest for the fixtures and inputs under `root`, with today's pins.
    pub fn generate(root: &Path) -> Result<Manifest, String> {
        let inputs = INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let selected = SELECTED
            .iter()
            .map(|&name| {
                let path = root.join(FIXTURE_DIR).join(elf_name(name));
                let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                Fixture::from_elf(name, &bytes)
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Manifest {
            schema: SCHEMA,
            repository: RISCV_TESTS_REPO.to_owned(),
            commit: RISCV_TESTS_COMMIT.to_owned(),
            env_commit: RISCV_TEST_ENV_COMMIT.to_owned(),
            distro: TOOLCHAIN_DISTRO.to_owned(),
            toolchain: TOOLCHAIN
                .iter()
                .map(|p| Tool {
                    name: p.name.to_owned(),
                    version: p.version.to_owned(),
                    deb_sha256: p.deb_sha256.to_owned(),
                    version_line: p.version_line.to_owned(),
                })
                .collect(),
            flags: FLAGS.iter().map(|&f| f.to_owned()).collect(),
            ram_base: RAM_BASE,
            ram_size: RAM_SIZE,
            inputs,
            selected,
            excluded: EXCLUDED
                .iter()
                .map(|&(name, reason)| (name.to_owned(), reason.to_owned()))
                .collect(),
        })
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<Manifest, String> {
        let text = fs::read_to_string(root.join(MANIFEST_PATH))
            .map_err(|e| format!("{MANIFEST_PATH}: {e}"))?;
        Manifest::parse(&text).map_err(|e| format!("{MANIFEST_PATH}: {e}"))
    }

    /// The file contents: stable, pretty-printed JSON with a trailing newline.
    pub fn render(&self) -> String {
        let tools: Vec<String> = self
            .toolchain
            .iter()
            .map(|t| {
                format!(
                    "      {{\n        \"name\": {},\n        \"version\": {},\n        \
                     \"deb_sha256\": {},\n        \"version_line\": {}\n      }}",
                    q(&t.name),
                    q(&t.version),
                    q(&t.deb_sha256),
                    q(&t.version_line)
                )
            })
            .collect();
        let flags: Vec<String> = self.flags.iter().map(|f| format!("    {}", q(f))).collect();
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
        let selected: Vec<String> = self
            .selected
            .iter()
            .map(|f| {
                format!(
                    "    {{\n      \"name\": {},\n      \"source\": {},\n      \"body\": {},\n      \
                     \"elf\": {},\n      \"blake3\": {},\n      \"image_hash\": {},\n      \
                     \"entry\": {}\n    }}",
                    q(&f.name),
                    q(&f.source),
                    q(&f.body),
                    q(&f.elf),
                    q(&hex(&f.blake3)),
                    q(&hex(&f.image_hash)),
                    q(&format!("{:#010x}", f.entry))
                )
            })
            .collect();
        let excluded: Vec<String> = self
            .excluded
            .iter()
            .map(|(name, reason)| {
                format!("    {{ \"name\": {}, \"reason\": {} }}", q(name), q(reason))
            })
            .collect();
        format!(
            "{{\n  \"schema\": {},\n  \"riscv_tests\": {{\n    \"repository\": {},\n    \
             \"commit\": {},\n    \"env_commit\": {}\n  }},\n  \"toolchain\": {{\n    \
             \"distro\": {},\n    \"packages\": [\n{}\n    ]\n  }},\n  \"flags\": [\n{}\n  ],\n  \
             \"ram\": {{ \"base\": {}, \"size\": {} }},\n  \"inputs\": [\n{}\n  ],\n  \
             \"selected\": [\n{}\n  ],\n  \"excluded\": [\n{}\n  ]\n}}\n",
            self.schema,
            q(&self.repository),
            q(&self.commit),
            q(&self.env_commit),
            q(&self.distro),
            tools.join(",\n"),
            flags.join(",\n"),
            q(&format!("{:#010x}", self.ram_base)),
            q(&format!("{:#010x}", self.ram_size)),
            inputs.join(",\n"),
            selected.join(",\n"),
            excluded.join(",\n"),
        )
    }

    /// Reads [`Manifest::render`]'s output.
    pub fn parse(json: &str) -> Result<Manifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        let upstream = &root["riscv_tests"];
        let toolchain = &root["toolchain"];
        Ok(Manifest {
            schema,
            repository: s(&upstream["repository"])?,
            commit: s(&upstream["commit"])?,
            env_commit: s(&upstream["env_commit"])?,
            distro: s(&toolchain["distro"])?,
            toolchain: array(&toolchain["packages"])?
                .iter()
                .map(|t| {
                    Ok(Tool {
                        name: s(&t["name"])?,
                        version: s(&t["version"])?,
                        deb_sha256: s(&t["deb_sha256"])?,
                        version_line: s(&t["version_line"])?,
                    })
                })
                .collect::<Result<_, String>>()?,
            flags: array(&root["flags"])?
                .iter()
                .map(s)
                .collect::<Result<_, String>>()?,
            ram_base: addr(&root["ram"]["base"])?,
            ram_size: addr(&root["ram"]["size"])?,
            inputs: array(&root["inputs"])?
                .iter()
                .map(|i| Ok((s(&i["path"])?, digest(&i["blake3"])?)))
                .collect::<Result<_, String>>()?,
            selected: array(&root["selected"])?
                .iter()
                .map(|f| {
                    Ok(Fixture {
                        name: s(&f["name"])?,
                        source: s(&f["source"])?,
                        body: s(&f["body"])?,
                        elf: s(&f["elf"])?,
                        blake3: digest(&f["blake3"])?,
                        image_hash: digest(&f["image_hash"])?,
                        entry: addr(&f["entry"])?,
                    })
                })
                .collect::<Result<_, String>>()?,
            excluded: array(&root["excluded"])?
                .iter()
                .map(|e| Ok((s(&e["name"])?, s(&e["reason"])?)))
                .collect::<Result<_, String>>()?,
        })
    }

    /// Fails unless the manifest selects exactly [`SELECTED`] and excludes exactly
    /// [`EXCLUDED`], in that order. A missing or extra test is a contract change, never a
    /// smaller or larger green run.
    pub fn ensure_selection(&self) -> Result<(), String> {
        let selected: Vec<&str> = self.selected.iter().map(|f| f.name.as_str()).collect();
        if selected != SELECTED {
            return Err(format!(
                "the manifest selects {selected:?}, not the {} tests {SELECTED:?}",
                SELECTED.len()
            ));
        }
        let excluded: Vec<(&str, &str)> = self
            .excluded
            .iter()
            .map(|(n, r)| (n.as_str(), r.as_str()))
            .collect();
        if excluded != EXCLUDED {
            return Err(format!(
                "the manifest excludes {excluded:?}, not {EXCLUDED:?}"
            ));
        }
        Ok(())
    }
}

/// One line per difference between the committed manifest and the one generated from the
/// files on disk with today's pins.
pub fn describe_differences(committed: &Manifest, actual: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    let fields = [
        (
            "schema",
            committed.schema.to_string(),
            actual.schema.to_string(),
        ),
        (
            "repository",
            committed.repository.clone(),
            actual.repository.clone(),
        ),
        ("commit", committed.commit.clone(), actual.commit.clone()),
        (
            "env_commit",
            committed.env_commit.clone(),
            actual.env_commit.clone(),
        ),
        ("distro", committed.distro.clone(), actual.distro.clone()),
        (
            "toolchain",
            format!("{:?}", committed.toolchain),
            format!("{:?}", actual.toolchain),
        ),
        ("flags", committed.flags.join(" "), actual.flags.join(" ")),
        (
            "ram",
            format!("{:#x}+{:#x}", committed.ram_base, committed.ram_size),
            format!("{:#x}+{:#x}", actual.ram_base, actual.ram_size),
        ),
        (
            "excluded",
            format!("{:?}", committed.excluded),
            format!("{:?}", actual.excluded),
        ),
    ];
    for (name, before, after) in fields {
        if before != after {
            out.push(format!("{name}: manifest {before}, actual {after}"));
        }
    }
    for (path, hash) in &actual.inputs {
        match committed.inputs.iter().find(|(p, _)| p == path) {
            None => out.push(format!("input {path}: not in the manifest")),
            Some((_, h)) if h != hash => out.push(format!(
                "input {path}: BLAKE3 {} differs from the manifest's {}; rebuild the fixtures",
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
    for f in &actual.selected {
        match committed.selected.iter().find(|c| c.name == f.name) {
            None => out.push(format!("{}: not in the manifest", f.name)),
            Some(c) if c != f => out.push(format!("{}: manifest {c:?}, actual {f:?}", f.name)),
            Some(_) => {}
        }
    }
    for c in &committed.selected {
        if !actual.selected.iter().any(|f| f.name == c.name) {
            out.push(format!("{}: in the manifest, not selected", c.name));
        }
    }
    let order = |m: &Manifest| {
        m.selected
            .iter()
            .map(|f| f.name.clone())
            .collect::<Vec<_>>()
    };
    if out.is_empty() && order(committed) != order(actual) {
        out.push("selected: the order differs".to_owned());
    }
    out
}

/// `cargo xtask rv32-fixtures verify`: checks the committed fixtures without a network or
/// a compiler.
///
/// - The manifest parses, selects exactly the 40 tests, and excludes exactly the two.
/// - It equals the manifest generated from the files on disk with today's pins: every
///   ELF exists, has the recorded BLAKE3, is accepted by the loader with the recorded
///   `image_hash` and an entry at the RAM base, and every build input has the recorded
///   hash. The file is byte-identical to the rendering, so formatting drift fails too.
/// - The fixture directory holds nothing else: the 40 ELFs, the manifest, and the
///   upstream license.
///
/// Returns the verified manifest.
pub fn verify(root: &Path) -> Result<Manifest, Vec<String>> {
    let committed = Manifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    if let Err(e) = committed.ensure_selection() {
        errors.push(e);
    }
    match Manifest::generate(root) {
        Err(e) => errors.push(e),
        Ok(actual) => {
            errors.extend(describe_differences(&committed, &actual));
            let text = fs::read_to_string(root.join(MANIFEST_PATH)).unwrap_or_default();
            if errors.is_empty() && text != actual.render() {
                errors.push(format!(
                    "{MANIFEST_PATH} is not formatted as `cargo xtask rv32-fixtures` writes it"
                ));
            }
        }
    }
    match fixture_dir_entries(root) {
        Err(e) => errors.push(e),
        Ok(entries) => {
            let mut expected: Vec<String> = SELECTED.iter().map(|n| elf_name(n)).collect();
            expected.push("manifest.json".to_owned());
            expected.push(FIXTURE_LICENSE.to_owned());
            expected.sort();
            for extra in entries.iter().filter(|e| !expected.contains(e)) {
                errors.push(format!("{FIXTURE_DIR}/{extra}: not a fixture"));
            }
            for missing in expected.iter().filter(|e| !entries.contains(e)) {
                errors.push(format!("{FIXTURE_DIR}/{missing}: missing"));
            }
        }
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

fn fixture_dir_entries(root: &Path) -> Result<Vec<String>, String> {
    let dir = root.join(FIXTURE_DIR);
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn q(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

fn s(v: &Value) -> Result<String, String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{v} is not a string"))
}

fn array(v: &Value) -> Result<&Vec<Value>, String> {
    v.as_array().ok_or_else(|| format!("{v} is not an array"))
}

fn digest(v: &Value) -> Result<[u8; 32], String> {
    v.as_str()
        .and_then(unhex32)
        .ok_or_else(|| format!("{v} is not a digest"))
}

fn addr(v: &Value) -> Result<u32, String> {
    v.as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .filter(|s| s.len() == 8)
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| format!("{v} is not a 32-bit address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, n: u8) -> Fixture {
        Fixture {
            name: name.to_owned(),
            source: format!("isa/rv32ui/{name}.S"),
            body: format!("isa/rv64ui/{name}.S"),
            elf: elf_name(name),
            blake3: [n; 32],
            image_hash: [n; 32],
            entry: RAM_BASE,
        }
    }

    fn sample() -> Manifest {
        Manifest {
            schema: SCHEMA,
            repository: RISCV_TESTS_REPO.to_owned(),
            commit: RISCV_TESTS_COMMIT.to_owned(),
            env_commit: RISCV_TEST_ENV_COMMIT.to_owned(),
            distro: TOOLCHAIN_DISTRO.to_owned(),
            toolchain: vec![Tool {
                name: "gcc".to_owned(),
                version: "1".to_owned(),
                deb_sha256: "ab".repeat(32),
                version_line: "gcc (1) 1 \"quoted\"".to_owned(),
            }],
            flags: FLAGS.iter().map(|&f| f.to_owned()).collect(),
            ram_base: RAM_BASE,
            ram_size: RAM_SIZE,
            inputs: vec![("tests/rv32/env/linker.ld".to_owned(), [9; 32])],
            selected: SELECTED
                .iter()
                .zip(1..)
                .map(|(name, n)| fixture(name, n))
                .collect(),
            excluded: EXCLUDED
                .iter()
                .map(|&(n, r)| (n.to_owned(), r.to_owned()))
                .collect(),
        }
    }

    #[test]
    fn rendering_parses_back() {
        let manifest = sample();
        let text = manifest.render();
        assert_eq!(Manifest::parse(&text), Ok(manifest.clone()));
        assert!(text.ends_with("}\n"));
        assert_eq!(manifest.ensure_selection(), Ok(()));
        assert_eq!(
            describe_differences(&manifest, &manifest),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_missing_extra_or_reordered_test_is_refused() {
        let mut m = sample();
        m.selected.pop();
        assert!(m.ensure_selection().is_err());
        let mut m = sample();
        m.selected.push(fixture("fence_i", 99));
        assert!(m.ensure_selection().is_err());
        let mut m = sample();
        m.selected.swap(0, 1);
        assert!(m.ensure_selection().is_err());
        let mut m = sample();
        m.excluded.pop();
        assert!(m.ensure_selection().is_err());
    }

    #[test]
    fn differences_are_described_per_field_and_fixture() {
        let committed = sample();
        let mut actual = sample();
        actual.selected[3].blake3 = [0xEE; 32];
        actual.inputs[0].1 = [0; 32];
        actual.commit = "0".repeat(40);
        let diff = describe_differences(&committed, &actual);
        assert_eq!(diff.len(), 3, "{diff:?}");
        assert!(diff[0].starts_with("commit: "));
        assert!(diff[1].starts_with("input tests/rv32/env/linker.ld: "));
        assert!(diff[2].starts_with("andi: "));
        let mut actual = sample();
        actual.selected.pop();
        let diff = describe_differences(&committed, &actual);
        assert_eq!(diff, ["xori: in the manifest, not selected"]);
    }

    #[test]
    fn a_file_is_checked_against_its_own_entry() {
        let bytes = b"not an elf";
        let mut f = fixture("add", 0);
        // Wrong bytes fail on the hash before the loader sees them.
        let err = f.check(bytes).unwrap_err();
        assert!(err.contains("BLAKE3"), "{err}");
        // With the right hash, the loader still has to accept the file.
        f.blake3 = *blake3::hash(bytes).as_bytes();
        let err = f.check(bytes).unwrap_err();
        assert!(err.contains("loader rejects"), "{err}");
    }

    #[test]
    fn malformed_values_are_refused() {
        let text = sample().render();
        let bad = text.replacen("\"entry\": \"0x80000000\"", "\"entry\": \"0x8000\"", 1);
        assert!(Manifest::parse(&bad).is_err());
        let bad = text.replacen("\"schema\": 1", "\"schema\": 2", 1);
        assert!(Manifest::parse(&bad).is_err());
    }
}
