//! M1-A5: `hello.elf` prints `Hello, SystemScope!\n` through the UART
//! (`docs/m1-design.md` §7.3, §9, §10.6).
//!
//! ```text
//! hello/hello.S ──build-hello.sh (Linux)──▶ hello/hello.elf + hello/manifest.json
//! hello.elf ──systemscope-elf──▶ Ram ──▶ Rv32iCpu ──SB──▶ AddressBus ──▶ SimpleUart
//! ```
//!
//! The program is pure RV32I assembly, built with the `rv32ui` toolchain, flags, and
//! linker script. It has its own small manifest, so the `rv32ui` manifest keeps meaning
//! exactly the 40 selected tests. It runs on `m1-reference` with the UART
//! ([`runner::platform`]), and passes when:
//!
//! - it ends as an `rv32ui` test passes: `Trap(EnvironmentCall)`, `gp == 1`, `a0 == 0`;
//! - the UART's output is exactly [`EXPECTED_OUTPUT`], read from its `inspect()` view;
//! - the `platform.uart.tx` trace records, when traced, carry the same bytes.

use std::fs;
use std::path::Path;

use serde_json::Value;
use systemscope_contracts::component::Delivered;
use systemscope_contracts::observe::{Observer, StateView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemMsg, WriteOutcome};
use systemscope_contracts::trace::{TraceOrigin, Value as TraceValue};
use systemscope_elf::LoadImage;
use systemscope_platform::uart;
use systemscope_runtime::runtime::Dispatched;
use systemscope_runtime::trace::Trace;

use crate::manifest::{Tool, addr, array, digest, dir_entries, load, q, s};
use crate::runner::{self, Finished, Start, UART};
use crate::{FLAGS, RAM_BASE, RAM_SIZE, TOOLCHAIN, TOOLCHAIN_DISTRO, hex};

/// What the UART must print: 20 bytes, with no terminator.
pub const EXPECTED_OUTPUT: &[u8; 20] = b"Hello, SystemScope!\n";

/// The directory holding the source, the build script, the ELF, and the manifest,
/// relative to the workspace root.
pub const HELLO_DIR: &str = "tests/rv32/hello";
/// The ELF's file name in [`HELLO_DIR`].
pub const HELLO_ELF: &str = "hello.elf";
/// The manifest, relative to the workspace root.
pub const HELLO_MANIFEST: &str = "tests/rv32/hello/manifest.json";
/// The build script, relative to the workspace root.
pub const HELLO_SCRIPT: &str = "tests/rv32/hello/build-hello.sh";
/// The files the ELF is built from besides the toolchain, relative to the workspace root.
/// The manifest records their hashes, so changing one without rebuilding fails `verify`.
pub const HELLO_INPUTS: [&str; 3] = [
    "tests/rv32/env/linker.ld",
    "tests/rv32/hello/build-hello.sh",
    "tests/rv32/hello/hello.S",
];
/// Everything [`HELLO_DIR`] holds, in name order.
pub const HELLO_FILES: [&str; 4] = ["build-hello.sh", "hello.S", "hello.elf", "manifest.json"];

/// The manifest format version.
pub const SCHEMA: u64 = 1;

/// The contents of [`HELLO_MANIFEST`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloManifest {
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
    /// BLAKE3 of each build input in [`HELLO_INPUTS`].
    pub inputs: Vec<(String, [u8; 32])>,
    /// The ELF's file name in [`HELLO_DIR`].
    pub elf: String,
    /// BLAKE3 of the ELF file.
    pub blake3: [u8; 32],
    /// The loader's `image_hash`.
    pub image_hash: [u8; 32],
    /// The loader's entry point.
    pub entry: u32,
}

impl HelloManifest {
    /// The manifest for the ELF and inputs under `root`, with today's pins.
    pub fn generate(root: &Path) -> Result<HelloManifest, String> {
        let inputs = HELLO_INPUTS
            .iter()
            .map(|&path| {
                let bytes = fs::read(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
                Ok((path.to_owned(), *blake3::hash(&bytes).as_bytes()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let path = root.join(HELLO_DIR).join(HELLO_ELF);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let image = load("hello", &bytes)?;
        Ok(HelloManifest {
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
            elf: HELLO_ELF.to_owned(),
            blake3: *blake3::hash(&bytes).as_bytes(),
            image_hash: image.image_hash,
            entry: image.entry,
        })
    }

    /// Reads the committed manifest under `root`.
    pub fn read(root: &Path) -> Result<HelloManifest, String> {
        let text = fs::read_to_string(root.join(HELLO_MANIFEST))
            .map_err(|e| format!("{HELLO_MANIFEST}: {e}"))?;
        HelloManifest::parse(&text).map_err(|e| format!("{HELLO_MANIFEST}: {e}"))
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
        let image = load("hello", bytes)?;
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
        let path = root.join(HELLO_DIR).join(&self.elf);
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.check(&bytes)
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
             \"elf\": {},\n  \"blake3\": {},\n  \"image_hash\": {},\n  \"entry\": {}\n}}\n",
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
        )
    }

    /// Reads [`HelloManifest::render`]'s output.
    pub fn parse(json: &str) -> Result<HelloManifest, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let schema = root["schema"].as_u64().ok_or("no schema")?;
        if schema != SCHEMA {
            return Err(format!("schema {schema}, expected {SCHEMA}"));
        }
        let toolchain = &root["toolchain"];
        Ok(HelloManifest {
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
        })
    }
}

/// Checks the committed `hello.elf` without a network or a compiler: the manifest equals
/// the one generated from the files on disk with today's pins, byte for byte, and
/// [`HELLO_DIR`] holds exactly [`HELLO_FILES`]. Returns the verified manifest.
pub fn verify(root: &Path) -> Result<HelloManifest, Vec<String>> {
    let committed = HelloManifest::read(root).map_err(|e| vec![e])?;
    let mut errors = Vec::new();
    match HelloManifest::generate(root) {
        Err(e) => errors.push(e),
        Ok(actual) if actual != committed => errors.push(format!(
            "{HELLO_MANIFEST} differs from the files on disk; rebuild hello.elf:\n  \
             manifest {committed:?}\n  actual   {actual:?}"
        )),
        Ok(actual) => {
            let text = fs::read_to_string(root.join(HELLO_MANIFEST)).unwrap_or_default();
            if text != actual.render() {
                errors.push(format!(
                    "{HELLO_MANIFEST} is not formatted as `cargo xtask rv32-fixtures` writes it"
                ));
            }
        }
    }
    match dir_entries(&root.join(HELLO_DIR)) {
        Err(e) => errors.push(e),
        Ok(entries) if entries != HELLO_FILES => errors.push(format!(
            "{HELLO_DIR} holds {entries:?}, not exactly {HELLO_FILES:?}"
        )),
        Ok(_) => {}
    }
    if errors.is_empty() {
        Ok(committed)
    } else {
        Err(errors)
    }
}

/// A run of `hello.elf` and what the UART printed.
#[derive(Debug)]
pub struct HelloRun {
    /// The session, run to its end.
    pub finished: Finished,
    /// The UART's output, from its view after the last event.
    pub output: Result<Vec<u8>, String>,
    /// The bytes of the `platform.uart.tx` records, if the run was traced.
    pub trace_tx: Option<Vec<u8>>,
}

/// Runs `image` on `m1-reference` with the UART, started as `start` says.
pub fn run(image: &LoadImage, start: Start, observers: Vec<Box<dyn Observer>>) -> HelloRun {
    let finished = runner::execute(runner::platform(image, true), start, observers);
    HelloRun {
        output: uart_output(&finished.views),
        trace_tx: finished.trace.as_ref().map(trace_tx),
        finished,
    }
}

/// The UART's whole output, from its view: `tx_len` bytes, which must all be in `tx_tail`.
pub fn uart_output(views: &[StateView]) -> Result<Vec<u8>, String> {
    let view = views
        .get(UART.0 as usize)
        .ok_or("no view of the UART: no event ran")?;
    let (Some(TraceValue::U64(len)), Some(TraceValue::Bytes(tail))) =
        (view.get("tx_len"), view.get("tx_tail"))
    else {
        return Err(format!(
            "the UART's view has no tx_len and tx_tail: {view:?}"
        ));
    };
    if *len != tail.len() as u64 {
        return Err(format!(
            "the UART printed {len} bytes, more than the {} its view shows",
            tail.len()
        ));
    }
    Ok(tail.clone())
}

/// The bytes of the UART's `platform.uart.tx` records in `trace`, in order.
pub fn trace_tx(trace: &Trace) -> Vec<u8> {
    trace
        .records
        .iter()
        .filter(|r| r.origin == TraceOrigin::Component && r.component == UART)
        .filter(|r| r.kind == uart::TX_KIND)
        .map(|r| match r.fields.as_slice() {
            [("byte", TraceValue::U64(b))] => u8::try_from(*b).expect("a byte"),
            other => panic!("malformed {} record: {other:?}", uart::TX_KIND),
        })
        .collect()
}

/// The `mem.v1` requests the UART received and the responses it sent, in dispatch order.
pub fn uart_traffic(dispatched: &[Dispatched]) -> (Vec<&MemMsg>, Vec<&MemMsg>) {
    fn mem(ev: &Dispatched) -> Option<&MemMsg> {
        match &ev.delivery {
            Delivered::Message {
                msg: Message::MemV1(msg),
                ..
            } => Some(msg),
            _ => None,
        }
    }
    let requests = dispatched
        .iter()
        .filter(|ev| ev.target == UART)
        .filter_map(mem)
        .collect();
    let responses = dispatched
        .iter()
        .filter(|ev| ev.source == UART)
        .filter_map(mem)
        .collect();
    (requests, responses)
}

/// The M1-A5 rule: `Ok` if the run passed, otherwise why it failed.
///
/// The run must end as an `rv32ui` test passes, the UART's output must be exactly
/// [`EXPECTED_OUTPUT`], and a traced run's `platform.uart.tx` bytes must equal the
/// output. For a run that started fresh, every request the UART received must also be a
/// one-byte write at offset 0 answered `Done`, one per output byte.
pub fn judge(run: &HelloRun, fresh: bool) -> Result<(), String> {
    runner::judge(&run.finished.outcome)?;
    let output = run.output.as_ref().map_err(Clone::clone)?;
    if output.as_slice() != EXPECTED_OUTPUT {
        return Err(format!(
            "the UART printed {:?}, not {:?}",
            String::from_utf8_lossy(output),
            String::from_utf8_lossy(EXPECTED_OUTPUT)
        ));
    }
    if let Some(traced) = &run.trace_tx
        && traced != output
    {
        return Err(format!(
            "the {} records carry {:?}, the UART's output is {:?}",
            uart::TX_KIND,
            String::from_utf8_lossy(traced),
            String::from_utf8_lossy(output)
        ));
    }
    if fresh {
        let (requests, responses) = uart_traffic(&run.finished.dispatched);
        let one_byte_tx = |m: &&MemMsg| matches!(m, MemMsg::WriteReq { addr, data, .. } if *addr == uart::TX && data.len() == 1);
        let done = |m: &&MemMsg| {
            matches!(
                m,
                MemMsg::WriteResp {
                    outcome: WriteOutcome::Done,
                    ..
                }
            )
        };
        if requests.len() != output.len() || !requests.iter().all(one_byte_tx) {
            return Err(format!(
                "the UART received {requests:?}, not {} one-byte TX writes",
                output.len()
            ));
        }
        if responses.len() != output.len() || !responses.iter().all(done) {
            return Err(format!(
                "the UART answered {responses:?}, not {} Done",
                output.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{End, Outcome, PASS_CAUSE};

    fn passing(output: &[u8], trace_tx: Option<&[u8]>) -> HelloRun {
        HelloRun {
            finished: Finished {
                outcome: Outcome {
                    end: End::Trap {
                        cause: PASS_CAUSE.to_owned(),
                        pc: RAM_BASE + 0x2c,
                        tval: 0,
                    },
                    gp: 1,
                    a0: 0,
                    pc: RAM_BASE + 0x2c,
                    instret: 87,
                    events: 1,
                    state: Some([1; 32]),
                    execution: [2; 32],
                    trace: None,
                },
                views: Vec::new(),
                dispatched: Vec::new(),
                trace: None,
            },
            output: Ok(output.to_vec()),
            trace_tx: trace_tx.map(<[u8]>::to_vec),
        }
    }

    #[test]
    fn only_the_exact_output_passes() {
        assert_eq!(judge(&passing(EXPECTED_OUTPUT, None), false), Ok(()));
        let prefix = &EXPECTED_OUTPUT[..19];
        let mut longer = EXPECTED_OUTPUT.to_vec();
        longer.push(0);
        for output in [&b""[..], prefix, &longer, b"Hello, SystemScope!!"] {
            assert!(judge(&passing(output, None), false).is_err(), "{output:?}");
        }
    }

    #[test]
    fn the_trace_bytes_must_equal_the_output() {
        let ok = passing(EXPECTED_OUTPUT, Some(EXPECTED_OUTPUT));
        assert_eq!(judge(&ok, false), Ok(()));
        for traced in [&b""[..], &EXPECTED_OUTPUT[..19], b"Jello, SystemScope!\n"] {
            let run = passing(EXPECTED_OUTPUT, Some(traced));
            assert!(judge(&run, false).is_err(), "{traced:?}");
        }
    }

    #[test]
    fn a_fresh_run_needs_one_uart_write_per_byte() {
        // No UART traffic at all behind the output.
        let run = passing(EXPECTED_OUTPUT, None);
        assert!(
            judge(&run, true)
                .unwrap_err()
                .contains("one-byte TX writes")
        );
    }

    #[test]
    fn the_run_must_end_as_a_passing_test() {
        let mut run = passing(EXPECTED_OUTPUT, None);
        run.finished.outcome.end = End::InstructionLimit;
        assert!(judge(&run, false).is_err());
        let mut run = passing(EXPECTED_OUTPUT, None);
        run.finished.outcome.a0 = 3;
        assert!(judge(&run, false).is_err());
    }
}
