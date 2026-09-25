//! `block_irq.elf`, the M2 end-to-end program, on `m2-reference`
//! (`docs/m2-design.md` §11, §12, §18).
//!
//! ```text
//! block_irq/block_irq.S ──build-block-irq.sh (Linux)──▶ block_irq/block_irq.elf
//! disk_image() ──cargo xtask rv32-fixtures──▶ block_irq/disk.img
//! both ──▶ block_irq/manifest.json
//! block_irq.elf ──systemscope-elf──▶ m2-reference (m2ref::build) ──▶ M2 PASS
//! ```
//!
//! The program is RV32I + Zicsr assembly, built with the `rv32ui` toolchain and linker
//! script and the `rv32ui` flags with `-march=rv32i_zicsr` ([`FLAGS`]). It runs three
//! one-block DMA transfers, each ended by the controller's completion interrupt:
//!
//! 1. READ LBA 0 → buffer A, checked against [`pattern`];
//! 2. every word of A inverted ([`transformed`]), then WRITE A → LBA 1;
//! 3. READ LBA 1 → buffer B, compared with A.
//!
//! The disk fixture, `disk.img`, is [`disk_image`]: [`crate::m2ref::DISK_BLOCKS`] blocks,
//! [`pattern`] at LBA 0 and zeros elsewhere. The manifest pins both files by BLAKE3.
//!
//! A run passes ([`judge`], §12.4) only if it ends with `Trap(EnvironmentCall)`, `gp == 1`,
//! `a0 == 0`; the UART printed exactly [`EXPECTED_OUTPUT`]; the handler counted exactly
//! [`EXPECTED_ENTRIES`] completions; the disk's LBA 1 holds [`transformed`] and its LBA 0
//! still holds [`pattern`]; and the controller's `STATUS.REJECTED` was never set.

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use serde_json::Value;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::trace::Value as TraceValue;
use systemscope_elf::LoadImage;

use crate::hello::uart_output;
use crate::m2ref::{self, BLK, Config, DISK, DISK_BLOCKS, M2Finished, RAM};
use crate::manifest::{Tool, addr, array, digest, dir_entries, load, q, s};
use crate::runner::{self, Start};
use crate::{RAM_BASE, RAM_SIZE, TOOLCHAIN, TOOLCHAIN_DISTRO, hex};

/// What the UART must print on success.
pub const EXPECTED_OUTPUT: &[u8; 8] = b"M2 PASS\n";
/// What the program prints on failure.
pub const FAIL_OUTPUT: &[u8; 8] = b"M2 FAIL\n";
/// Completion interrupts the handler must count: one per transfer (§12.3).
pub const EXPECTED_ENTRIES: u32 = 3;

/// The first instruction of the wait loop in `transfer`: a load of `flag`.
pub const POLL: u32 = 0x8000_014c;
/// The machine external interrupt handler, `mtvec` in Direct mode.
pub const HANDLER: u32 = 0x8000_0164;
/// The handler's words: `flag`, the saved `STATUS`, and the entry counter.
pub const VARS: u32 = 0x8000_2000;
/// `flag`, set to 1 by the handler.
pub const FLAG: u32 = VARS;
/// `STATUS` as the handler last read it.
pub const SAVED_STATUS: u32 = VARS + 4;
/// The number of completions the handler counted.
pub const ENTRIES: u32 = VARS + 8;
/// Buffer A, the target of transfer 1 and the source of transfer 2.
pub const BUFFER_A: u32 = 0x8000_2200;
/// Buffer B, the target of transfer 3.
pub const BUFFER_B: u32 = 0x8000_2400;

/// The directory holding the source, the build script, the ELF, the disk fixture, and
/// the manifest, relative to the workspace root.
pub const BLOCK_IRQ_DIR: &str = "tests/rv32/block_irq";
/// The ELF's file name in [`BLOCK_IRQ_DIR`].
pub const BLOCK_IRQ_ELF: &str = "block_irq.elf";
/// The disk fixture's file name in [`BLOCK_IRQ_DIR`].
pub const DISK_IMG: &str = "disk.img";
/// The manifest, relative to the workspace root.
pub const BLOCK_IRQ_MANIFEST: &str = "tests/rv32/block_irq/manifest.json";
/// The build script, relative to the workspace root.
pub const BLOCK_IRQ_SCRIPT: &str = "tests/rv32/block_irq/build-block-irq.sh";
/// The files the ELF is built from besides the toolchain, relative to the workspace root.
/// The manifest records their hashes, so changing one without rebuilding fails `verify`.
pub const BLOCK_IRQ_INPUTS: [&str; 3] = [
    "tests/rv32/block_irq/block_irq.S",
    "tests/rv32/block_irq/build-block-irq.sh",
    "tests/rv32/env/linker.ld",
];
/// Everything [`BLOCK_IRQ_DIR`] holds, in name order.
pub const BLOCK_IRQ_FILES: [&str; 5] = [
    "block_irq.S",
    "block_irq.elf",
    "build-block-irq.sh",
    "disk.img",
    "manifest.json",
];

/// The compiler flags: [`crate::FLAGS`] with Zicsr.
pub const FLAGS: [&str; 8] = [
    "-march=rv32i_zicsr",
    "-mabi=ilp32",
    "-static",
    "-mcmodel=medany",
    "-fvisibility=hidden",
    "-nostdlib",
    "-nostartfiles",
    "-Wl,--build-id=none",
];

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// The LBA 0 pattern: word `k` (little-endian) is `(k * 0x01010101) ^ 0x5aa5c33c`.
pub fn pattern() -> Vec<u8> {
    (0..128u32)
        .flat_map(|k| (k.wrapping_mul(0x0101_0101) ^ 0x5aa5_c33c).to_le_bytes())
        .collect()
}

/// What transfer 2 writes to LBA 1: [`pattern`] with every word inverted.
pub fn transformed() -> Vec<u8> {
    pattern().into_iter().map(|b| !b).collect()
}

/// The disk fixture: [`DISK_BLOCKS`] blocks, [`pattern`] at LBA 0, zeros elsewhere.
pub fn disk_image() -> Vec<u8> {
    let mut bytes = pattern();
    bytes.resize(DISK_BLOCKS as usize * 512, 0);
    bytes
}

/// Writes [`disk_image`] to [`DISK_IMG`] under `root`, as `cargo xtask rv32-fixtures`
/// does before it writes the manifest.
pub fn write_disk(root: &Path) -> Result<(), String> {
    let path = root.join(BLOCK_IRQ_DIR).join(DISK_IMG);
    fs::write(&path, disk_image()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The contents of [`BLOCK_IRQ_MANIFEST`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockIrqManifest {
    /// [`SCHEMA`].
    pub schema: u64,
    /// Where the toolchain packages come from.
    pub distro: String,
    /// The toolchain packages.
    pub toolchain: Vec<Tool>,
    /// The compiler flags.
    pub flags: Vec<String>,
    /// The RAM base: the entry point.
    pub ram_base: u32,
    /// The RAM size the image is checked against.
    pub ram_size: u32,
    /// BLAKE3 of each build input in [`BLOCK_IRQ_INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// The ELF's file name in [`BLOCK_IRQ_DIR`].
    pub elf: String,
    /// BLAKE3 of the ELF file.
    pub blake3: [u8; 32],
    /// The loader's `image_hash`.
    pub image_hash: [u8; 32],
    /// The loader's entry point.
    pub entry: u32,
    /// The disk fixture's file name in [`BLOCK_IRQ_DIR`].
    pub disk: String,
    /// Its size in blocks.
    pub disk_blocks: u64,
    /// BLAKE3 of the disk fixture: the media's `image_hash`.
    pub disk_blake3: [u8; 32],
}

impl BlockIrqManifest {
    /// The manifest for the files under `root`, with today's pins.
    pub fn generate(root: &Path) -> Result<BlockIrqManifest, String> {
        let inputs = BLOCK_IRQ_INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let path = root.join(BLOCK_IRQ_DIR).join(BLOCK_IRQ_ELF);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let image = load("block_irq", &bytes)?;
        let disk_path = root.join(BLOCK_IRQ_DIR).join(DISK_IMG);
        let disk = fs::read(&disk_path).map_err(|e| format!("{}: {e}", disk_path.display()))?;
        if !disk.len().is_multiple_of(512) {
            return Err(format!(
                "{DISK_IMG} is {} bytes, not a whole number of blocks",
                disk.len()
            ));
        }
        Ok(BlockIrqManifest {
            schema: SCHEMA,
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
            elf: BLOCK_IRQ_ELF.to_owned(),
            blake3: *blake3::hash(&bytes).as_bytes(),
            image_hash: image.image_hash,
            entry: image.entry,
            disk: DISK_IMG.to_owned(),
            disk_blocks: (disk.len() / 512) as u64,
            disk_blake3: *blake3::hash(&disk).as_bytes(),
        })
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<BlockIrqManifest, String> {
        let text = fs::read_to_string(root.join(BLOCK_IRQ_MANIFEST))
            .map_err(|e| format!("{BLOCK_IRQ_MANIFEST}: {e}"))?;
        BlockIrqManifest::parse(&text).map_err(|e| format!("{BLOCK_IRQ_MANIFEST}: {e}"))
    }

    /// Loads the ELF's bytes, provided they are exactly the file the manifest names.
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
        let image = load("block_irq", bytes)?;
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

    /// Reads the ELF under `root` and [checks](Self::check) it.
    pub fn read_image(&self, root: &Path) -> Result<LoadImage, String> {
        let path = root.join(BLOCK_IRQ_DIR).join(&self.elf);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.check(&bytes)
    }

    /// Reads the disk fixture under `root`, provided it is exactly the file the manifest
    /// names.
    pub fn read_disk(&self, root: &Path) -> Result<Vec<u8>, String> {
        let path = root.join(BLOCK_IRQ_DIR).join(&self.disk);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let actual = *blake3::hash(&bytes).as_bytes();
        if actual != self.disk_blake3 || bytes.len() as u64 != self.disk_blocks * 512 {
            return Err(format!(
                "{}: {} bytes with BLAKE3 {}, the manifest says {} blocks with {}",
                self.disk,
                bytes.len(),
                hex(&actual),
                self.disk_blocks,
                hex(&self.disk_blake3)
            ));
        }
        Ok(bytes)
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
        format!(
            "{{\n  \"schema\": {},\n  \"toolchain\": {{\n    \"distro\": {},\n    \
             \"packages\": [\n{}\n    ]\n  }},\n  \"flags\": [\n{}\n  ],\n  \
             \"ram\": {{ \"base\": {}, \"size\": {} }},\n  \"inputs\": [\n{}\n  ],\n  \
             \"elf\": {},\n  \"blake3\": {},\n  \"image_hash\": {},\n  \"entry\": {},\n  \
             \"disk\": {{ \"file\": {}, \"blocks\": {}, \"blake3\": {} }}\n}}\n",
            self.schema,
            q(&self.distro),
            tools.join(",\n"),
            flags.join(",\n"),
            q(&format!("{:#010x}", self.ram_base)),
            q(&format!("{:#010x}", self.ram_size)),
            inputs.join(",\n"),
            q(&self.elf),
            q(&hex(&self.blake3)),
            q(&hex(&self.image_hash)),
            q(&format!("{:#010x}", self.entry)),
            q(&self.disk),
            self.disk_blocks,
            q(&hex(&self.disk_blake3)),
        )
    }

    /// Reads [`BlockIrqManifest::render`]'s output.
    pub fn parse(json: &str) -> Result<BlockIrqManifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        let toolchain = &root["toolchain"];
        let disk = &root["disk"];
        Ok(BlockIrqManifest {
            schema,
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
            elf: s(&root["elf"])?,
            blake3: digest(&root["blake3"])?,
            image_hash: digest(&root["image_hash"])?,
            entry: addr(&root["entry"])?,
            disk: s(&disk["file"])?,
            disk_blocks: disk["blocks"].as_u64().ok_or("no disk block count")?,
            disk_blake3: digest(&disk["blake3"])?,
        })
    }
}

/// Checks the committed `block_irq.elf` and disk fixture without a network or a
/// compiler: the manifest equals the one generated from the files on disk with today's
/// pins, byte for byte; the disk fixture is exactly [`disk_image`]; and
/// [`BLOCK_IRQ_DIR`] holds exactly [`BLOCK_IRQ_FILES`]. Returns the verified manifest.
pub fn verify(root: &Path) -> Result<BlockIrqManifest, Vec<String>> {
    let committed = BlockIrqManifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    match BlockIrqManifest::generate(root) {
        Err(e) => errors.push(e),
        Ok(actual) if actual != committed => errors.push(format!(
            "{BLOCK_IRQ_MANIFEST} differs from the files on disk; rebuild block_irq.elf:\n  \
             manifest {committed:?}\n  actual   {actual:?}"
        )),
        Ok(actual) => {
            let text = fs::read_to_string(root.join(BLOCK_IRQ_MANIFEST)).unwrap_or_default();
            if text != actual.render() {
                errors.push(format!(
                    "{BLOCK_IRQ_MANIFEST} is not formatted as `cargo xtask rv32-fixtures` \
                     writes it"
                ));
            }
        }
    }
    match fs::read(root.join(BLOCK_IRQ_DIR).join(DISK_IMG)) {
        Err(e) => errors.push(format!("{DISK_IMG}: {e}")),
        Ok(bytes) if bytes != disk_image() => errors.push(format!(
            "{DISK_IMG} is not the disk fixture block_irq::disk_image() defines"
        )),
        Ok(_) => {}
    }
    match dir_entries(&root.join(BLOCK_IRQ_DIR)) {
        Err(e) => errors.push(e),
        Ok(entries) if entries != BLOCK_IRQ_FILES => errors.push(format!(
            "{BLOCK_IRQ_DIR} holds {entries:?}, not exactly {BLOCK_IRQ_FILES:?}"
        )),
        Ok(_) => {}
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// Reads the committed ELF and disk fixture under `root`, both checked against the
/// manifest.
pub fn read_fixture(root: &Path) -> Result<(LoadImage, Vec<u8>), String> {
    let manifest = BlockIrqManifest::read(root)?;
    Ok((manifest.read_image(root)?, manifest.read_disk(root)?))
}

/// Remembers whether the block controller's view ever showed `rejected`, checked after
/// every event.
struct RejectedWatch(Rc<Cell<bool>>);

impl Observer for RejectedWatch {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let view = world.inspect(BLK).unwrap_or_default();
        if view.get("rejected") != Some(&TraceValue::Bool(false)) {
            self.0.set(true);
        }
        Control::Continue
    }
}

/// A run of `block_irq.elf` and what it left behind.
#[derive(Debug)]
pub struct BlockIrqRun {
    /// The session, run to its end, and its final snapshot.
    pub finished: M2Finished,
    /// The UART's output, from its view after the last event.
    pub output: Result<Vec<u8>, String>,
    /// Whether the controller's `rejected` flag was ever set after an event.
    pub rejected: bool,
    /// What the final snapshot holds; `Err` if there is none.
    pub state: Result<FinalState, String>,
}

/// The parts of the final state §12.4 checks, read from the final snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalState {
    /// The handler's entry counter.
    pub entries: u32,
    /// `flag`.
    pub flag: u32,
    /// The saved `STATUS`.
    pub saved_status: u32,
    /// Buffer A.
    pub buffer_a: Vec<u8>,
    /// Buffer B.
    pub buffer_b: Vec<u8>,
    /// Disk LBA 0.
    pub lba0: Vec<u8>,
    /// Disk LBA 1.
    pub lba1: Vec<u8>,
    /// Every non-zero disk block's LBA.
    pub stored: Vec<u64>,
}

impl FinalState {
    /// Reads the final state from a runtime snapshot of `m2-reference`.
    pub fn read(snapshot: &[u8]) -> Result<FinalState, String> {
        let components = m2ref::components(snapshot).map_err(|e| format!("{e:?}"))?;
        let ram = &components[RAM.0 as usize].bytes;
        let disk = &components[DISK.0 as usize].bytes;
        let err = |e| format!("{e:?}");
        Ok(FinalState {
            entries: m2ref::ram_word(ram, ENTRIES).map_err(err)?,
            flag: m2ref::ram_word(ram, FLAG).map_err(err)?,
            saved_status: m2ref::ram_word(ram, SAVED_STATUS).map_err(err)?,
            buffer_a: m2ref::ram_read(ram, BUFFER_A, 512).map_err(err)?,
            buffer_b: m2ref::ram_read(ram, BUFFER_B, 512).map_err(err)?,
            lba0: m2ref::disk_block(disk, 0).map_err(err)?,
            lba1: m2ref::disk_block(disk, 1).map_err(err)?,
            stored: m2ref::disk_blocks(disk)
                .map_err(err)?
                .1
                .into_iter()
                .map(|(lba, _)| lba)
                .collect(),
        })
    }
}

/// Runs `image` with the raw disk image `disk` on `m2-reference` configured as `config`,
/// started as `start` says. `observers` are added after the run's own, which only read.
pub fn run_with(
    image: &LoadImage,
    disk: &[u8],
    config: &Config,
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> BlockIrqRun {
    let rt = m2ref::build(image, disk, config).expect("the platform builds");
    let rejected = Rc::new(Cell::new(false));
    let mut all: Vec<Box<dyn Observer>> = vec![Box::new(RejectedWatch(Rc::clone(&rejected)))];
    all.extend(observers);
    let finished = m2ref::execute(rt, start, all);
    BlockIrqRun {
        output: uart_output(&finished.finished.views),
        state: finished
            .snapshot
            .as_deref()
            .ok_or_else(|| "no final snapshot: the session faulted".to_owned())
            .and_then(FinalState::read),
        rejected: rejected.get(),
        finished,
    }
}

/// Runs `image` with `disk` on the frozen `m2-reference`.
pub fn run(
    image: &LoadImage,
    disk: &[u8],
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> BlockIrqRun {
    run_with(image, disk, &Config::frozen(), start, observers)
}

/// The §12.4 rule: `Ok` if the run passed, otherwise why it failed.
pub fn judge(run: &BlockIrqRun) -> Result<(), String> {
    runner::judge(&run.finished.finished.outcome)?;
    let output = run.output.as_ref().map_err(Clone::clone)?;
    if output.as_slice() != EXPECTED_OUTPUT {
        return Err(format!(
            "the UART printed {:?}, not {:?}",
            String::from_utf8_lossy(output),
            String::from_utf8_lossy(EXPECTED_OUTPUT)
        ));
    }
    let state = run.state.as_ref().map_err(Clone::clone)?;
    if state.entries != EXPECTED_ENTRIES {
        return Err(format!(
            "the handler counted {} completions, not {EXPECTED_ENTRIES}",
            state.entries
        ));
    }
    if state.lba1 != transformed() {
        return Err("disk LBA 1 does not hold the transformed pattern".to_owned());
    }
    if state.lba0 != pattern() {
        return Err("disk LBA 0 no longer holds the pattern".to_owned());
    }
    if run.rejected {
        return Err("the block controller set STATUS.REJECTED".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pattern_and_its_transformation_are_as_documented() {
        let p = pattern();
        assert_eq!(p.len(), 512);
        assert_eq!(&p[..4], &0x5aa5_c33cu32.to_le_bytes());
        assert_eq!(&p[4..8], &(0x0101_0101u32 ^ 0x5aa5_c33c).to_le_bytes());
        assert_eq!(
            &p[508..],
            &(127u32.wrapping_mul(0x0101_0101) ^ 0x5aa5_c33c).to_le_bytes()
        );
        let t = transformed();
        assert!(p.iter().zip(&t).all(|(a, b)| a ^ b == 0xff));
    }

    #[test]
    fn the_disk_holds_the_pattern_at_lba_0_and_zeros_elsewhere() {
        let disk = disk_image();
        assert_eq!(disk.len(), DISK_BLOCKS as usize * 512);
        assert_eq!(&disk[..512], pattern().as_slice());
        assert!(disk[512..].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_program_flags_are_the_rv32ui_flags_with_zicsr() {
        assert_eq!(FLAGS[0], "-march=rv32i_zicsr");
        assert_eq!(crate::FLAGS[0], "-march=rv32i");
        assert_eq!(FLAGS[1..], crate::FLAGS[1..]);
    }
}
