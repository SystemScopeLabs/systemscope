//! The M3 scenario on `m3-reference` (`docs/m3-design.md` §7, §12, §15.2, §17.3).
//!
//! ```text
//! m3/firmware.S ─────────build-m3.sh (Linux)──▶ m3/m3-firmware.elf
//! m3/{hello,ping,pong,fault,badptr}.S ─────────▶ m3/<name>.elf
//! the user ELFs ──m3ref::storage_image──▶ m3/disk.img, m3/disk-nofault.img
//! a run of each disk ──os_trace──▶ m3/os-trace.txt, m3/os-trace-nofault.txt
//! all of it ──▶ m3/manifest.json
//! m3-firmware.elf ──host loader (boot ROM)──▶ m3-reference RAM
//! disk.img ──▶ soc.disk ──DMA, polled by soc.kernel──▶ staging ──▶ processes
//! ```
//!
//! - The firmware is RV32I + Zicsr assembly ([`FIRMWARE_FLAGS`], `firmware.ld`): the
//!   M-mode boot stub, `s_boot`, the S-mode trampoline, and the SBI shutdown (§7). It is
//!   the only thing the host places in RAM, as a boot ROM would be.
//! - The five user programs of §12.2 ([`PROGRAMS`]) are RV32I assembly linked at
//!   `USER_BASE` without relaxation ([`USER_FLAGS`]), so no instruction depends on `gp`
//!   (§17.1). They reach the kernel only by `ecall` with the §6.5 ABI. They are never
//!   loaded by the host: they are files on the disk, which the kernel reads through the
//!   block controller.
//! - A [`Scenario`] is a disk and its expected results: [`Scenario::Reference`], the five
//!   programs in table order, and [`Scenario::NoFault`], the same without `fault`, which
//!   must shut down with reason 0 (§12.3).
//! - The expected `os.*` sequence of each disk ([`os_trace`]) is a committed file whose
//!   header names the disk's `image_hash` (§15.2). It was recorded from a run that passed
//!   every other §12.3 check, and it is compared exactly.
//! - [`judge`] is §12.3: the SRST halt with its registers; the UART bytes; the `os.*`
//!   records; exactly one `StorePageFault` from U, at `fault`'s store, plus one
//!   `EnvironmentCallFromU` per syscall and no other exception; no rejected command or bus
//!   fault; and, after shutdown, every frame free and every process `Exited` or `Faulted`.
//!
//! Tests only read the committed fixtures. `cargo xtask rv32-fixtures build` rebuilds
//! them on Linux, `cargo xtask rv32-fixtures verify` checks the manifest without a network
//! or a compiler, and `cargo xtask m3-reference verify` also runs both disks.

use std::fs;
use std::path::Path;

use serde_json::Value;
use systemscope_contracts::observe::Observer;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value as TraceValue};
use systemscope_elf::LoadImage;
use systemscope_os::kernel::{ENTER_KIND, RELEASE_KIND};
use systemscope_os::procop::{FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND};
use systemscope_platform::bus::FAULT_KIND as BUS_FAULT_KIND;
use systemscope_platform::dma::{COMMAND_KIND, REJECTED_KIND};
use systemscope_platform::uart::TX_KIND;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::EXCEPTION_KIND;

use crate::hello::uart_output;
use crate::m3ref::{self, CPU, Config, DISK_BLOCKS, KERNEL, POOL_FRAMES, SRST, STAGING_SIZE};
use crate::manifest::{Tool, addr, array, digest, dir_entries, load, q, s};
use crate::runner::{self, End, Finished, Start};
use crate::{RAM_BASE, RAM_SIZE, TOOLCHAIN, TOOLCHAIN_DISTRO, hex, unhex32};

/// The directory holding the sources, the build script, the ELFs, the disks, the
/// expected traces, and the manifest, relative to the workspace root.
pub const M3_DIR: &str = "tests/rv32/m3";
/// The firmware's file name in [`M3_DIR`].
pub const FIRMWARE_ELF: &str = "m3-firmware.elf";
/// The user programs of §12.2, in table order; each is `<name>.elf` in [`M3_DIR`].
pub const PROGRAMS: [&str; 5] = ["hello", "ping", "pong", "fault", "badptr"];
/// The manifest, relative to the workspace root.
pub const M3_MANIFEST: &str = "tests/rv32/m3/manifest.json";
/// The build script, relative to the workspace root.
pub const M3_SCRIPT: &str = "tests/rv32/m3/build-m3.sh";
/// The files the ELFs are built from besides the toolchain, relative to the workspace
/// root. The manifest records their hashes, so changing one without rebuilding fails
/// `verify`.
pub const M3_INPUTS: [&str; 10] = [
    "tests/rv32/m3/badptr.S",
    "tests/rv32/m3/build-m3.sh",
    "tests/rv32/m3/fault.S",
    "tests/rv32/m3/firmware.S",
    "tests/rv32/m3/firmware.ld",
    "tests/rv32/m3/hello.S",
    "tests/rv32/m3/ping.S",
    "tests/rv32/m3/pong.S",
    "tests/rv32/m3/user-data.ld",
    "tests/rv32/m3/user.ld",
];
/// Everything [`M3_DIR`] holds, in name order.
pub const M3_FILES: [&str; 21] = [
    "badptr.S",
    "badptr.elf",
    "build-m3.sh",
    "disk-nofault.img",
    "disk.img",
    "fault.S",
    "fault.elf",
    "firmware.S",
    "firmware.ld",
    "hello.S",
    "hello.elf",
    "m3-firmware.elf",
    "manifest.json",
    "os-trace-nofault.txt",
    "os-trace.txt",
    "ping.S",
    "ping.elf",
    "pong.S",
    "pong.elf",
    "user-data.ld",
    "user.ld",
];

/// The firmware's compiler flags: [`crate::FLAGS`] with Zicsr, linked without relaxation.
pub const FIRMWARE_FLAGS: [&str; 9] = [
    "-march=rv32i_zicsr",
    "-mabi=ilp32",
    "-static",
    "-mcmodel=medany",
    "-fvisibility=hidden",
    "-nostdlib",
    "-nostartfiles",
    "-Wl,--build-id=none",
    "-Wl,--no-relax",
];
/// The user programs' compiler flags: [`crate::FLAGS`], linked without relaxation.
pub const USER_FLAGS: [&str; 9] = [
    "-march=rv32i",
    "-mabi=ilp32",
    "-static",
    "-mcmodel=medany",
    "-fvisibility=hidden",
    "-nostdlib",
    "-nostartfiles",
    "-Wl,--build-id=none",
    "-Wl,--no-relax",
];

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// The most events a run of the scenario may take. The reference disk needs about
/// 167,000 events; a run that never shuts down stops here.
pub const EVENT_BUDGET: u64 = 1_000_000;

/// The halting trap of the SBI shutdown (§7.4).
pub const HALT_CAUSE: &str = "EnvironmentCallFromS";

/// The first line of an expected trace file.
pub const TRACE_HEADER: &str = "# m3-reference os.* trace (docs/m3-design.md §12.3, §15.2)";

/// An M3 disk and its expected results (§12.1, §12.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// The five programs in table order: `fault` is killed, so shutdown reason 1.
    Reference,
    /// The variant disk without `fault`: every process exits 0, so shutdown reason 0.
    NoFault,
}

impl Scenario {
    /// Both scenarios.
    pub const ALL: [Scenario; 2] = [Scenario::Reference, Scenario::NoFault];

    /// A short name.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Reference => "reference",
            Scenario::NoFault => "nofault",
        }
    }

    /// The programs on the disk, in table order.
    pub fn programs(self) -> &'static [&'static str] {
        match self {
            Scenario::Reference => &PROGRAMS,
            Scenario::NoFault => &["hello", "ping", "pong", "badptr"],
        }
    }

    /// The disk's file name in [`M3_DIR`].
    pub fn disk_file(self) -> &'static str {
        match self {
            Scenario::Reference => "disk.img",
            Scenario::NoFault => "disk-nofault.img",
        }
    }

    /// The expected trace's file name in [`M3_DIR`].
    pub fn trace_file(self) -> &'static str {
        match self {
            Scenario::Reference => "os-trace.txt",
            Scenario::NoFault => "os-trace-nofault.txt",
        }
    }

    /// What the UART must print.
    pub fn expected_output(self) -> &'static [u8] {
        match self {
            Scenario::Reference => b"hello from pid 1\nping\npong\nfault\nping\npong\nping\npong\n",
            Scenario::NoFault => b"hello from pid 1\nping\npong\nping\npong\nping\npong\n",
        }
    }

    /// The shutdown reason in `a1` (§7.4): 1 if a process was killed.
    pub fn expected_reason(self) -> u32 {
        match self {
            Scenario::Reference => 1,
            Scenario::NoFault => 0,
        }
    }

    /// How many processes the kernel kills.
    pub fn faults(self) -> usize {
        match self {
            Scenario::Reference => 1,
            Scenario::NoFault => 0,
        }
    }
}

/// Reads the firmware under `root` as the host loader places it: RAM at `RAM_BASE`, the
/// entry at the RAM base.
pub fn read_firmware(root: &Path) -> Result<LoadImage, String> {
    let path = root.join(M3_DIR).join(FIRMWARE_ELF);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    load("m3-firmware", &bytes)
}

/// Reads the user ELFs of `scenario` under `root`, in table order.
pub fn read_programs(root: &Path, scenario: Scenario) -> Result<Vec<Vec<u8>>, String> {
    scenario
        .programs()
        .iter()
        .map(|name| {
            let path = root.join(M3_DIR).join(format!("{name}.elf"));
            fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect()
}

/// The disk of `programs` in table order: [`m3ref::storage_image`] for the
/// `m3-reference` disk and staging.
pub fn disk_image(programs: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let files: Vec<&[u8]> = programs.iter().map(Vec::as_slice).collect();
    m3ref::storage_image(&files, DISK_BLOCKS as u32, STAGING_SIZE as u32)
}

/// The disk of `scenario` from the user ELFs under `root`.
pub fn scenario_disk(root: &Path, scenario: Scenario) -> Result<Vec<u8>, String> {
    disk_image(&read_programs(root, scenario)?)
}

/// Writes both disks under `root`, as `cargo xtask rv32-fixtures build` does after the
/// ELFs.
pub fn write_disks(root: &Path) -> Result<(), String> {
    for scenario in Scenario::ALL {
        let path = root.join(M3_DIR).join(scenario.disk_file());
        let disk = scenario_disk(root, scenario)?;
        fs::write(&path, disk).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether `record` is one of the `os.*` records of §12.3: every kernel record but the
/// gate's `ENTER` and release, which only frame each operation.
pub fn is_os_record(record: &TraceRecord) -> bool {
    record.origin == TraceOrigin::Component
        && record.kind.starts_with("os.")
        && record.kind != ENTER_KIND
        && record.kind != RELEASE_KIND
}

fn render_value(value: &TraceValue) -> String {
    match value {
        TraceValue::U64(v) => v.to_string(),
        TraceValue::I64(v) => v.to_string(),
        TraceValue::Bool(v) => v.to_string(),
        TraceValue::Str(v) => format!("{v:?}"),
        TraceValue::Bytes(v) => format!("0x{}", hex(v)),
    }
}

/// The `os.*` records of `trace`, one per line: the kind, then every field as
/// `name=value` in the order traced. Numbers are decimal, `I64` signed; strings are
/// quoted.
pub fn os_trace(trace: &Trace) -> String {
    let mut out = String::new();
    for r in trace.records.iter().filter(|r| is_os_record(r)) {
        out.push_str(r.kind);
        for (name, value) in &r.fields {
            out.push(' ');
            out.push_str(name);
            out.push('=');
            out.push_str(&render_value(value));
        }
        out.push('\n');
    }
    out
}

/// An expected trace file: [`TRACE_HEADER`], the disk's name and `image_hash`, then the
/// records.
pub fn render_trace_file(scenario: Scenario, disk_hash: &[u8; 32], records: &str) -> String {
    format!(
        "{TRACE_HEADER}\n# disk {} image_hash {}\n{records}",
        scenario.disk_file(),
        hex(disk_hash)
    )
}

/// Reads [`render_trace_file`]'s output for `scenario`: the disk's `image_hash` and the
/// records.
pub fn parse_trace_file(scenario: Scenario, text: &str) -> Result<([u8; 32], String), String> {
    let mut lines = text.splitn(3, '\n');
    if lines.next() != Some(TRACE_HEADER) {
        return Err(format!("{}: no header", scenario.trace_file()));
    }
    let key = lines.next().unwrap_or_default();
    let prefix = format!("# disk {} image_hash ", scenario.disk_file());
    let hash = key
        .strip_prefix(&prefix)
        .and_then(unhex32)
        .ok_or_else(|| format!("{}: the second line is {key:?}", scenario.trace_file()))?;
    Ok((hash, lines.next().unwrap_or_default().to_owned()))
}

/// Reads the expected trace of `scenario` under `root`, provided its key is `disk`'s
/// `image_hash`.
pub fn read_expected_trace(root: &Path, scenario: Scenario, disk: &[u8]) -> Result<String, String> {
    let path = root.join(M3_DIR).join(scenario.trace_file());
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (hash, records) = parse_trace_file(scenario, &text)?;
    let actual = *blake3::hash(disk).as_bytes();
    if hash != actual {
        return Err(format!(
            "{} is keyed by image_hash {}, the disk's is {}",
            scenario.trace_file(),
            hex(&hash),
            hex(&actual)
        ));
    }
    Ok(records)
}

/// Runs both disks under `root` and writes each one's expected trace, as `cargo xtask
/// rv32-fixtures build` does after the disks. A run must pass every other §12.3 check
/// before its trace is written.
pub fn write_traces(root: &Path) -> Result<(), String> {
    let firmware = read_firmware(root)?;
    for scenario in Scenario::ALL {
        let disk = scenario_disk(root, scenario)?;
        let run = run(&firmware, &disk, Start::Init { traced: true }, Vec::new());
        judge_without_trace(&run, scenario).map_err(|e| format!("{}: {e}", scenario.name()))?;
        let records = os_trace(run.finished.trace.as_ref().expect("traced"));
        let text = render_trace_file(scenario, blake3::hash(&disk).as_bytes(), &records);
        let path = root.join(M3_DIR).join(scenario.trace_file());
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// A file and its BLAKE3 as the manifest records them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinned {
    /// The file name in [`M3_DIR`].
    pub file: String,
    /// BLAKE3 of the file.
    pub blake3: [u8; 32],
}

/// A disk as the manifest records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedDisk {
    /// The file name in [`M3_DIR`].
    pub file: String,
    /// Its size in blocks.
    pub blocks: u64,
    /// The programs on it, in table order.
    pub programs: Vec<String>,
    /// BLAKE3 of the disk: the media's `image_hash`.
    pub blake3: [u8; 32],
}

/// The contents of [`M3_MANIFEST`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M3Manifest {
    /// [`SCHEMA`].
    pub schema: u64,
    /// Where the toolchain packages come from.
    pub distro: String,
    /// The toolchain packages.
    pub toolchain: Vec<Tool>,
    /// The firmware's compiler flags.
    pub firmware_flags: Vec<String>,
    /// The user programs' compiler flags.
    pub user_flags: Vec<String>,
    /// The RAM base: the firmware's entry point.
    pub ram_base: u32,
    /// The RAM size the firmware is checked against.
    pub ram_size: u32,
    /// BLAKE3 of each build input in [`M3_INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// The firmware ELF.
    pub firmware: Pinned,
    /// The loader's `image_hash` of the firmware.
    pub firmware_image_hash: [u8; 32],
    /// The firmware's entry point.
    pub firmware_entry: u32,
    /// The user ELFs, in [`PROGRAMS`] order.
    pub programs: Vec<Pinned>,
    /// The disks, in [`Scenario::ALL`] order.
    pub disks: Vec<PinnedDisk>,
    /// The expected traces, in [`Scenario::ALL`] order.
    pub traces: Vec<Pinned>,
}

fn pin(root: &Path, file: &str) -> Result<(Pinned, Vec<u8>), String> {
    let path = root.join(M3_DIR).join(file);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((
        Pinned {
            file: file.to_owned(),
            blake3: *blake3::hash(&bytes).as_bytes(),
        },
        bytes,
    ))
}

impl M3Manifest {
    /// The manifest for the files under `root`, with today's pins.
    pub fn generate(root: &Path) -> Result<M3Manifest, String> {
        let inputs = M3_INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let (firmware, bytes) = pin(root, FIRMWARE_ELF)?;
        let image = load("m3-firmware", &bytes)?;
        let programs = PROGRAMS
            .iter()
            .map(|name| Ok(pin(root, &format!("{name}.elf"))?.0))
            .collect::<Result<Vec<_>, String>>()?;
        let mut disks = Vec::new();
        let mut traces = Vec::new();
        for scenario in Scenario::ALL {
            let (disk, bytes) = pin(root, scenario.disk_file())?;
            if !bytes.len().is_multiple_of(512) {
                return Err(format!(
                    "{} is {} bytes, not a whole number of blocks",
                    disk.file,
                    bytes.len()
                ));
            }
            disks.push(PinnedDisk {
                file: disk.file,
                blocks: (bytes.len() / 512) as u64,
                programs: scenario.programs().iter().map(|&p| p.to_owned()).collect(),
                blake3: disk.blake3,
            });
            traces.push(pin(root, scenario.trace_file())?.0);
        }
        Ok(M3Manifest {
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
            firmware_flags: FIRMWARE_FLAGS.iter().map(|&f| f.to_owned()).collect(),
            user_flags: USER_FLAGS.iter().map(|&f| f.to_owned()).collect(),
            ram_base: RAM_BASE,
            ram_size: RAM_SIZE,
            inputs,
            firmware,
            firmware_image_hash: image.image_hash,
            firmware_entry: image.entry,
            programs,
            disks,
            traces,
        })
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<M3Manifest, String> {
        let text = fs::read_to_string(root.join(M3_MANIFEST))
            .map_err(|e| format!("{M3_MANIFEST}: {e}"))?;
        M3Manifest::parse(&text).map_err(|e| format!("{M3_MANIFEST}: {e}"))
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
        let flags = |fs: &[String]| -> String {
            fs.iter()
                .map(|f| format!("    {}", q(f)))
                .collect::<Vec<_>>()
                .join(",\n")
        };
        let pinned = |ps: &[Pinned]| -> String {
            ps.iter()
                .map(|p| {
                    format!(
                        "    {{ \"file\": {}, \"blake3\": {} }}",
                        q(&p.file),
                        q(&hex(&p.blake3))
                    )
                })
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
        let disks: Vec<String> = self
            .disks
            .iter()
            .map(|d| {
                let programs: Vec<String> = d.programs.iter().map(|p| q(p)).collect();
                format!(
                    "    {{ \"file\": {}, \"blocks\": {}, \"programs\": [{}], \"blake3\": {} }}",
                    q(&d.file),
                    d.blocks,
                    programs.join(", "),
                    q(&hex(&d.blake3))
                )
            })
            .collect();
        format!(
            "{{\n  \"schema\": {},\n  \"toolchain\": {{\n    \"distro\": {},\n    \
             \"packages\": [\n{}\n    ]\n  }},\n  \"firmware_flags\": [\n{}\n  ],\n  \
             \"user_flags\": [\n{}\n  ],\n  \
             \"ram\": {{ \"base\": {}, \"size\": {} }},\n  \"inputs\": [\n{}\n  ],\n  \
             \"firmware\": {{ \"file\": {}, \"blake3\": {}, \"image_hash\": {}, \"entry\": {} }},\n  \
             \"programs\": [\n{}\n  ],\n  \"disks\": [\n{}\n  ],\n  \"traces\": [\n{}\n  ]\n}}\n",
            self.schema,
            q(&self.distro),
            tools.join(",\n"),
            flags(&self.firmware_flags),
            flags(&self.user_flags),
            q(&format!("{:#010x}", self.ram_base)),
            q(&format!("{:#010x}", self.ram_size)),
            inputs.join(",\n"),
            q(&self.firmware.file),
            q(&hex(&self.firmware.blake3)),
            q(&hex(&self.firmware_image_hash)),
            q(&format!("{:#010x}", self.firmware_entry)),
            pinned(&self.programs),
            disks.join(",\n"),
            pinned(&self.traces),
        )
    }

    /// Reads [`M3Manifest::render`]'s output.
    pub fn parse(json: &str) -> Result<M3Manifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        let toolchain = &root["toolchain"];
        let strings =
            |v: &Value| -> Result<Vec<String>, String> { array(v)?.iter().map(s).collect() };
        let pinned = |v: &Value| -> Result<Pinned, String> {
            Ok(Pinned {
                file: s(&v["file"])?,
                blake3: digest(&v["blake3"])?,
            })
        };
        let firmware = &root["firmware"];
        Ok(M3Manifest {
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
            firmware_flags: strings(&root["firmware_flags"])?,
            user_flags: strings(&root["user_flags"])?,
            ram_base: addr(&root["ram"]["base"])?,
            ram_size: addr(&root["ram"]["size"])?,
            inputs: array(&root["inputs"])?
                .iter()
                .map(|i| Ok((s(&i["path"])?, digest(&i["blake3"])?)))
                .collect::<Result<_, String>>()?,
            firmware: pinned(firmware)?,
            firmware_image_hash: digest(&firmware["image_hash"])?,
            firmware_entry: addr(&firmware["entry"])?,
            programs: array(&root["programs"])?
                .iter()
                .map(pinned)
                .collect::<Result<_, String>>()?,
            disks: array(&root["disks"])?
                .iter()
                .map(|d| {
                    Ok(PinnedDisk {
                        file: s(&d["file"])?,
                        blocks: d["blocks"].as_u64().ok_or("no disk block count")?,
                        programs: strings(&d["programs"])?,
                        blake3: digest(&d["blake3"])?,
                    })
                })
                .collect::<Result<_, String>>()?,
            traces: array(&root["traces"])?
                .iter()
                .map(pinned)
                .collect::<Result<_, String>>()?,
        })
    }
}

/// Checks the committed M3 fixtures without a network, a compiler, or a run: the
/// manifest equals the one generated from the files on disk with today's pins, byte for
/// byte; each disk is exactly the storage image of its programs; each expected trace is
/// keyed by its disk's `image_hash`; and [`M3_DIR`] holds exactly [`M3_FILES`]. Returns
/// the verified manifest.
pub fn verify(root: &Path) -> Result<M3Manifest, Vec<String>> {
    let committed = M3Manifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    match M3Manifest::generate(root) {
        Err(e) => errors.push(e),
        Ok(actual) if actual != committed => errors.push(format!(
            "{M3_MANIFEST} differs from the files on disk; rebuild the M3 fixtures:\n  \
             manifest {committed:?}\n  actual   {actual:?}"
        )),
        Ok(actual) => {
            let text = fs::read_to_string(root.join(M3_MANIFEST)).unwrap_or_default();
            if text != actual.render() {
                errors.push(format!(
                    "{M3_MANIFEST} is not formatted as `cargo xtask rv32-fixtures` writes it"
                ));
            }
        }
    }
    for scenario in Scenario::ALL {
        let path = root.join(M3_DIR).join(scenario.disk_file());
        let read = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()));
        match (read, scenario_disk(root, scenario)) {
            (Err(e), _) | (_, Err(e)) => errors.push(format!("{}: {e}", scenario.name())),
            (Ok(bytes), Ok(expected)) => {
                if bytes != expected {
                    errors.push(format!(
                        "{} is not the storage image of {:?}",
                        scenario.disk_file(),
                        scenario.programs()
                    ));
                }
                if let Err(e) = read_expected_trace(root, scenario, &bytes) {
                    errors.push(e);
                }
            }
        }
    }
    match dir_entries(&root.join(M3_DIR)) {
        Err(e) => errors.push(e),
        Ok(entries) if entries != M3_FILES => errors.push(format!(
            "{M3_DIR} holds {entries:?}, not exactly {M3_FILES:?}"
        )),
        Ok(_) => {}
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// The verified fixture of `scenario` under `root`: the firmware, the disk, and the
/// expected `os.*` records, each checked against the manifest.
pub fn read_fixture(
    root: &Path,
    scenario: Scenario,
) -> Result<(LoadImage, Vec<u8>, String), String> {
    let manifest = M3Manifest::read(root)?;
    let (firmware, bytes) = pin(root, FIRMWARE_ELF)?;
    if firmware != manifest.firmware {
        return Err(format!("{FIRMWARE_ELF} differs from the manifest"));
    }
    let image = load("m3-firmware", &bytes)?;
    if image.image_hash != manifest.firmware_image_hash || image.entry != manifest.firmware_entry {
        return Err(format!(
            "{FIRMWARE_ELF}: the loader's image differs from the manifest"
        ));
    }
    let (disk, bytes) = pin(root, scenario.disk_file())?;
    let pinned = manifest
        .disks
        .iter()
        .find(|d| d.file == disk.file)
        .ok_or_else(|| format!("the manifest has no {}", disk.file))?;
    if pinned.blake3 != disk.blake3 || pinned.blocks * 512 != bytes.len() as u64 {
        return Err(format!("{} differs from the manifest", disk.file));
    }
    let (trace, _) = pin(root, scenario.trace_file())?;
    if !manifest.traces.contains(&trace) {
        return Err(format!("{} differs from the manifest", trace.file));
    }
    let records = read_expected_trace(root, scenario, &bytes)?;
    Ok((image, bytes, records))
}

/// A run of the scenario and what it left behind.
#[derive(Debug)]
pub struct M3Run {
    /// The session, run to its end.
    pub finished: Finished,
    /// The runtime snapshot after the last event; `None` if the session faulted.
    pub snapshot: Option<Vec<u8>>,
    /// The UART's output, from its view after the last event.
    pub output: Result<Vec<u8>, String>,
}

/// Runs `firmware` with the raw disk image `disk` on `m3-reference` configured as
/// `config`, started as `start` says, for at most [`EVENT_BUDGET`] events. `observers`
/// are added after the runner's own, which only reads.
pub fn run_with(
    firmware: &LoadImage,
    disk: &[u8],
    config: &Config,
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> M3Run {
    let rt = m3ref::build(firmware, disk, config).expect("the platform builds");
    let (finished, rt) = runner::execute_bounded(rt, start, observers, Some(EVENT_BUDGET));
    M3Run {
        snapshot: rt.snapshot().ok(),
        output: uart_output(&finished.views),
        finished,
    }
}

/// Runs `firmware` with `disk` on the frozen `m3-reference`.
pub fn run(
    firmware: &LoadImage,
    disk: &[u8],
    start: Start,
    observers: Vec<Box<dyn Observer>>,
) -> M3Run {
    run_with(firmware, disk, &Config::frozen(), start, observers)
}

fn register(run: &M3Run, name: &str) -> Option<u64> {
    match run.finished.views.get(CPU.0 as usize)?.get(name) {
        Some(TraceValue::U64(v)) => Some(*v),
        _ => None,
    }
}

fn kernel_field<'a>(run: &'a M3Run, name: &str) -> Option<&'a TraceValue> {
    run.finished.views.get(KERNEL.0 as usize)?.get(name)
}

fn field<'a>(r: &'a TraceRecord, name: &str) -> Option<&'a TraceValue> {
    r.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
}

fn text(r: &TraceRecord, name: &str) -> String {
    match field(r, name) {
        Some(TraceValue::Str(s)) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn number(r: &TraceRecord, name: &str) -> Option<u64> {
    match field(r, name) {
        Some(TraceValue::U64(v)) => Some(*v),
        _ => None,
    }
}

/// §12.3 without the trace comparison: every check [`judge`] makes but the equality of
/// the `os.*` records with the committed ones.
pub fn judge_without_trace(run: &M3Run, scenario: Scenario) -> Result<(), String> {
    match &run.finished.outcome.end {
        End::Trap { cause, .. } if cause == HALT_CAUSE => {}
        other => return Err(format!("ended with {other}, not Trap({HALT_CAUSE})")),
    }
    let regs = [
        ("a7", "x17", u64::from(SRST)),
        ("a6", "x16", 0),
        ("a0", "x10", 0),
        ("a1", "x11", u64::from(scenario.expected_reason())),
    ];
    for (abi, reg, want) in regs {
        let got = register(run, reg);
        if got != Some(want) {
            return Err(format!("{abi} = {got:?} at the halt, not {want:#x}"));
        }
    }
    let output = run.output.as_ref().map_err(Clone::clone)?;
    if output.as_slice() != scenario.expected_output() {
        return Err(format!(
            "the UART printed {:?}, not {:?}",
            String::from_utf8_lossy(output),
            String::from_utf8_lossy(scenario.expected_output())
        ));
    }
    let trace = run
        .finished
        .trace
        .as_ref()
        .ok_or("the run was not traced")?;
    let records: Vec<&TraceRecord> = trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component)
        .collect();
    let of = |kind: &str| -> Vec<&TraceRecord> {
        records.iter().copied().filter(|r| r.kind == kind).collect()
    };
    // Exactly one StorePageFault from U, at the killed process's store, and one
    // EnvironmentCallFromU per syscall; nothing else, and nothing from S.
    let exceptions = of(EXCEPTION_KIND);
    let syscalls = of(SYSCALL_ENTER_KIND).len();
    let faults = of(FAULT_KIND);
    let calls = exceptions
        .iter()
        .filter(|e| text(e, "cause") == "EnvironmentCallFromU" && text(e, "from") == "U")
        .count();
    let stores: Vec<&&TraceRecord> = exceptions
        .iter()
        .filter(|e| text(e, "cause") == "StorePageFault" && text(e, "from") == "U")
        .collect();
    if calls != syscalls {
        return Err(format!(
            "{calls} EnvironmentCallFromU exceptions for {syscalls} syscalls"
        ));
    }
    if stores.len() != scenario.faults() || faults.len() != scenario.faults() {
        return Err(format!(
            "{} StorePageFault exceptions and {} killed processes, not {}",
            stores.len(),
            faults.len(),
            scenario.faults()
        ));
    }
    for (e, f) in stores.iter().zip(&faults) {
        let at = (number(e, "pc"), number(e, "tval"));
        if at != (number(f, "epc"), number(f, "tval")) || text(f, "cause") != "StorePageFault" {
            return Err(format!("the kill {f:?} is not the exception {e:?}"));
        }
    }
    if exceptions.len() != calls + stores.len() {
        return Err(format!("unexpected exceptions: {exceptions:?}"));
    }
    for kind in [REJECTED_KIND, BUS_FAULT_KIND] {
        if let Some(r) = of(kind).first() {
            return Err(format!("the run traced {r:?}"));
        }
    }
    let tx = of(TX_KIND).len();
    if tx != output.len() {
        return Err(format!("{tx} UART TX records for {} bytes", output.len()));
    }
    // After shutdown: every frame free, every process Exited or Faulted, nothing queued.
    let free = kernel_field(run, "free_frames");
    if free != Some(&TraceValue::U64(POOL_FRAMES)) {
        return Err(format!(
            "{free:?} free frames after shutdown, not {POOL_FRAMES}"
        ));
    }
    let state = |name: &str| match kernel_field(run, name) {
        Some(TraceValue::Str(s)) => s.clone(),
        other => format!("{other:?}"),
    };
    if (
        state("life").as_str(),
        state("running").as_str(),
        state("queue").as_str(),
    ) != ("down", "none", "")
    {
        return Err(format!(
            "the kernel is {}, running {}, queue {:?} after shutdown",
            state("life"),
            state("running"),
            state("queue")
        ));
    }
    let processes = state("processes");
    let table: Vec<&str> = processes.split(';').collect();
    if table.len() != scenario.programs().len()
        || !table
            .iter()
            .all(|p| p.contains(":exited(") || p.contains(":faulted("))
    {
        return Err(format!("the processes after shutdown are {processes:?}"));
    }
    Ok(())
}

/// The §12.3 rule for `scenario`: `Ok` if the run passed, otherwise why it failed.
/// `expected` is the committed `os.*` records.
pub fn judge(run: &M3Run, scenario: Scenario, expected: &str) -> Result<(), String> {
    judge_without_trace(run, scenario)?;
    let actual = os_trace(run.finished.trace.as_ref().expect("checked"));
    if actual != expected {
        let line = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
        return Err(format!(
            "the os.* records differ from the committed ones at record {}: {:?} vs {:?}",
            line + 1,
            actual.lines().nth(line),
            expected.lines().nth(line)
        ));
    }
    Ok(())
}

/// What a run did, counted from its outcome and trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// Events dispatched.
    pub events: u64,
    /// Instructions retired.
    pub instret: u64,
    /// `os.*` records (§12.3).
    pub os_records: usize,
    /// `os.syscall.enter` records.
    pub syscalls: usize,
    /// `os.process.switch` records, the first dispatch included.
    pub switches: usize,
    /// Delegated exceptions.
    pub exceptions: usize,
    /// Block controller commands: the table and every executable.
    pub dma_commands: usize,
    /// UART bytes.
    pub uart_bytes: usize,
    /// `ExecutionDigest`.
    pub execution: [u8; 32],
    /// `StateDigest`, if the session did not fault.
    pub state: Option<[u8; 32]>,
    /// `TraceDigest`, if the run was traced.
    pub trace: Option<[u8; 32]>,
}

impl Metrics {
    /// The metrics of `run`.
    pub fn of(run: &M3Run) -> Metrics {
        let outcome = &run.finished.outcome;
        let count = |pred: &dyn Fn(&TraceRecord) -> bool| {
            run.finished
                .trace
                .as_ref()
                .map_or(0, |t| t.records.iter().filter(|r| pred(r)).count())
        };
        let kind = |k: &'static str| {
            move |r: &TraceRecord| r.origin == TraceOrigin::Component && r.kind == k
        };
        Metrics {
            events: outcome.events,
            instret: outcome.instret,
            os_records: count(&is_os_record),
            syscalls: count(&kind(SYSCALL_ENTER_KIND)),
            switches: count(&kind(SWITCH_KIND)),
            exceptions: count(&kind(EXCEPTION_KIND)),
            dma_commands: count(&kind(COMMAND_KIND)),
            uart_bytes: count(&kind(TX_KIND)),
            execution: outcome.execution,
            state: outcome.state,
            trace: outcome.trace,
        }
    }

    /// One line for a report.
    pub fn line(&self) -> String {
        format!(
            "events {} instret {} os-records {} syscalls {} switches {} exceptions {} \
             dma-commands {} uart-bytes {} execution {} state {} trace {}",
            self.events,
            self.instret,
            self.os_records,
            self.syscalls,
            self.switches,
            self.exceptions,
            self.dma_commands,
            self.uart_bytes,
            hex(&self.execution),
            self.state.map_or("none".to_owned(), |s| hex(&s)),
            self.trace.map_or("none".to_owned(), |t| hex(&t)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flags_are_the_rv32ui_flags_without_relaxation() {
        assert_eq!(FIRMWARE_FLAGS[0], "-march=rv32i_zicsr");
        assert_eq!(USER_FLAGS[..8], crate::FLAGS);
        assert_eq!(FIRMWARE_FLAGS[1..8], crate::FLAGS[1..]);
        assert_eq!(FIRMWARE_FLAGS[8], "-Wl,--no-relax");
        assert_eq!(USER_FLAGS[8], "-Wl,--no-relax");
    }

    #[test]
    fn the_files_are_in_name_order_and_hold_every_input_and_output() {
        let mut sorted = M3_FILES;
        sorted.sort();
        assert_eq!(sorted, M3_FILES);
        for input in M3_INPUTS {
            let name = input.strip_prefix("tests/rv32/m3/").unwrap();
            assert!(M3_FILES.contains(&name), "{name}");
        }
        for scenario in Scenario::ALL {
            assert!(M3_FILES.contains(&scenario.disk_file()));
            assert!(M3_FILES.contains(&scenario.trace_file()));
        }
        for name in PROGRAMS {
            assert!(M3_FILES.contains(&format!("{name}.elf").as_str()));
        }
    }

    #[test]
    fn the_variant_disk_is_the_reference_without_fault() {
        let reference: Vec<&str> = Scenario::Reference
            .programs()
            .iter()
            .copied()
            .filter(|&p| p != "fault")
            .collect();
        assert_eq!(Scenario::NoFault.programs(), reference.as_slice());
        let out = String::from_utf8_lossy(Scenario::Reference.expected_output());
        assert_eq!(
            out.replace("fault\n", "").as_bytes(),
            Scenario::NoFault.expected_output()
        );
    }

    #[test]
    fn a_trace_file_round_trips_and_names_its_disk() {
        let hash = [7u8; 32];
        let text = render_trace_file(Scenario::NoFault, &hash, "os.boot entries=4\n");
        assert_eq!(
            parse_trace_file(Scenario::NoFault, &text),
            Ok((hash, "os.boot entries=4\n".to_owned()))
        );
        assert!(parse_trace_file(Scenario::Reference, &text).is_err());
        assert!(parse_trace_file(Scenario::NoFault, "os.boot\n").is_err());
    }
}
