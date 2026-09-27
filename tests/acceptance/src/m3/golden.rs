//! The M3 golden files (`docs/m3-design.md` §9.2, §17.4), in the M1 and M2 format
//! ([`crate::m2::golden`]).
//!
//! - `tests/golden/m3-reference.json`: the firmware's BLAKE3; for each scenario, its
//!   disk's BLAKE3, the halt, the shutdown reason, the exact UART output and its BLAKE3,
//!   the §12.3 metrics, `StateDigest`, `ExecutionDigest`, `TraceDigest`, and the final
//!   snapshot's size and BLAKE3; then the portable snapshot's provenance.
//! - `tests/golden/m3-reference.mid.snap`: the reference scenario snapshotted at stress
//!   point 3 during a syscall, with part of the `write` at the UART
//!   ([`checkpoint::is_mid`]).
//!
//! Only `cargo xtask m3-golden bless` writes them, through [`Golden::generate`] and
//! [`Golden::render`]. Tests and `cargo xtask m3-golden verify` read the committed copies
//! and never write. The event watchdog is not part of either file.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use systemscope_contracts::COMPATIBILITY_ID;
use systemscope_rv32::m3::{M3Run, Metrics, Scenario};
use systemscope_rv32::m3ref;
use systemscope_rv32::runner::{End, Start};

use super::checkpoint::{self, Tracker};
use super::portable::{self, CpuState, SCHEMAS};
use super::{Fixture, SCENARIO};
use crate::{hex, unhex32};

/// The golden file, relative to the workspace root.
pub const GOLDEN_PATH: &str = "tests/golden/m3-reference.json";
/// The portable snapshot, relative to the workspace root.
pub const MID_SNAPSHOT_PATH: &str = "tests/golden/m3-reference.mid.snap";
/// The portable snapshot's file name, as the golden file records it.
pub const MID_FILE: &str = "m3-reference.mid.snap";
/// Where the portable snapshot is taken.
pub const MID_CHECKPOINT: &str = "first boundary with ENTER held and the kernel's access in \
                                  flight during a write syscall, some bytes at the UART";

/// What one scenario's run ends with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// [`Scenario::name`].
    pub scenario: String,
    /// BLAKE3 of the disk: the media's `image_hash`.
    pub disk_blake3: [u8; 32],
    /// The trap the CPU halts on: the firmware's SBI shutdown `ecall` from S.
    pub halt: String,
    /// The halting `ecall`'s `pc`.
    pub halt_pc: u32,
    /// The shutdown reason in `a1` (§6.8): 0 if every process exited, 1 if one faulted.
    pub reason: u32,
    /// The exact UART output.
    pub uart: Vec<u8>,
    /// Events dispatched.
    pub events: u64,
    /// Instructions retired.
    pub instret: u64,
    /// `os.*` records.
    pub os_records: u64,
    /// `os.syscall.enter` records.
    pub syscalls: u64,
    /// `os.process.switch` records.
    pub switches: u64,
    /// Delegated exceptions.
    pub exceptions: u64,
    /// Block controller commands.
    pub dma_commands: u64,
    /// UART TX records.
    pub uart_bytes: u64,
    /// `StateDigest` at the end.
    pub state: [u8; 32],
    /// `ExecutionDigest` at the end.
    pub execution: [u8; 32],
    /// `TraceDigest` of the whole run.
    pub trace: [u8; 32],
    /// The final snapshot's size.
    pub final_size: u64,
    /// BLAKE3 of the final snapshot, which is `StateDigest` by its definition (M0 §7);
    /// the file records both so each can be checked on its own.
    pub final_blake3: [u8; 32],
}

impl Record {
    /// The record of `run`, a traced run of `fixture` from `init` that passed §12.3. The
    /// halt and the reason come from the final snapshot, read by [`portable::read`].
    pub fn of(fixture: &Fixture, run: &M3Run) -> Result<Record, String> {
        let metrics = Metrics::of(run);
        let End::Trap { cause, pc, .. } = &run.finished.outcome.end else {
            return Err(format!("ended with {}", run.finished.outcome.end));
        };
        let snapshot = run.snapshot.as_deref().ok_or("no final snapshot")?;
        let (p, _) = portable::read(snapshot).map_err(|e| format!("the final snapshot: {e}"))?;
        if !matches!(p.cpu.state, CpuState::Halted(Some(_))) {
            return Err("the final snapshot's CPU has not halted".to_owned());
        }
        let count = |n: usize| n as u64;
        Ok(Record {
            scenario: fixture.scenario.name().to_owned(),
            disk_blake3: fixture.disk_blake3,
            halt: cause.clone(),
            halt_pc: *pc,
            reason: p.cpu.regs[10],
            uart: run.output.clone()?,
            events: metrics.events,
            instret: metrics.instret,
            os_records: count(metrics.os_records),
            syscalls: count(metrics.syscalls),
            switches: count(metrics.switches),
            exceptions: count(metrics.exceptions),
            dma_commands: count(metrics.dma_commands),
            uart_bytes: count(metrics.uart_bytes),
            state: metrics.state.ok_or("no StateDigest")?,
            execution: metrics.execution,
            trace: metrics.trace.ok_or("no TraceDigest")?,
            final_size: snapshot.len() as u64,
            final_blake3: *blake3::hash(snapshot).as_bytes(),
        })
    }
}

/// The portable snapshot's provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mid {
    /// The scenario it comes from: `reference`.
    pub scenario: String,
    /// [`MID_CHECKPOINT`].
    pub checkpoint: String,
    /// Events dispatched before the snapshot.
    pub after_events: u64,
    /// The component schemas in it, by `ComponentId`: [`SCHEMAS`].
    pub schemas: Vec<u32>,
    /// The session seed.
    pub seed: u64,
    /// The contracts compatibility id in its session information.
    pub compatibility_id: String,
    /// BLAKE3 of the firmware.
    pub firmware_blake3: [u8; 32],
    /// BLAKE3 of the disk.
    pub disk_blake3: [u8; 32],
    /// The file's size in bytes.
    pub size: u64,
    /// BLAKE3 of the file. Guards against line-ending conversion.
    pub blake3: [u8; 32],
}

/// The contents of [`GOLDEN_PATH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    /// The contracts compatibility id the runs' sessions carry.
    pub compatibility_id: String,
    /// The session seed.
    pub seed: u64,
    /// BLAKE3 of the firmware.
    pub firmware_blake3: [u8; 32],
    /// One record per scenario, in [`Scenario::ALL`] order.
    pub records: Vec<Record>,
    /// The portable snapshot.
    pub mid: Mid,
}

impl Golden {
    /// Runs both committed scenarios under `root` and produces the golden file and the
    /// portable snapshot. Fails if a run fails §12.3: a failing run is never blessed.
    pub fn generate(root: &Path) -> Result<(Golden, Vec<u8>), String> {
        let mut records = Vec::new();
        let mut firmware_blake3 = [0; 32];
        let mut mid = None;
        for scenario in Scenario::ALL {
            let fixture = Fixture::read(root, scenario)?;
            let run = fixture
                .judged_run(Vec::new())
                .map_err(|e| format!("{}: {e}", scenario.name()))?;
            records
                .push(Record::of(&fixture, &run).map_err(|e| format!("{}: {e}", scenario.name()))?);
            firmware_blake3 = fixture.firmware_blake3;
            if scenario == Scenario::Reference {
                let (k, snapshot) = checkpoint::mid_point(&fixture)?;
                let (p, _) = portable::read(&snapshot)?;
                mid = Some((
                    Mid {
                        scenario: scenario.name().to_owned(),
                        checkpoint: MID_CHECKPOINT.to_owned(),
                        after_events: k as u64,
                        schemas: p.entries.iter().map(|e| e.schema).collect(),
                        seed: p.seed,
                        compatibility_id: COMPATIBILITY_ID.to_owned(),
                        firmware_blake3: fixture.firmware_blake3,
                        disk_blake3: fixture.disk_blake3,
                        size: snapshot.len() as u64,
                        blake3: *blake3::hash(&snapshot).as_bytes(),
                    },
                    snapshot,
                ));
            }
        }
        let (mid, snapshot) = mid.ok_or("no reference scenario")?;
        let golden = Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: m3ref::SEED,
            firmware_blake3,
            records,
            mid,
        };
        Ok((golden, snapshot))
    }

    /// The file contents: stable JSON with a fixed field order, lowercase hex, LF line
    /// ends, and a trailing newline. Nothing host-specific: no path, time, or run id.
    pub fn render(&self) -> String {
        let records: Vec<String> = self.records.iter().map(render_record).collect();
        let m = &self.mid;
        let schemas: Vec<String> = m.schemas.iter().map(u32::to_string).collect();
        format!(
            "{{\n  \"scenario\": {},\n  \"compatibility_id\": {},\n  \"seed\": {},\n  \
             \"firmware_blake3\": {},\n  \"runs\": [\n{}\n  ],\n  \"mid_snapshot\": {{\n    \
             \"file\": {},\n    \"scenario\": {},\n    \"checkpoint\": {},\n    \
             \"after_events\": {},\n    \"schemas\": [{}],\n    \"seed\": {},\n    \
             \"compatibility_id\": {},\n    \"firmware_blake3\": {},\n    \
             \"disk_blake3\": {},\n    \"size\": {},\n    \"blake3\": {}\n  }}\n}}\n",
            q(SCENARIO),
            q(&self.compatibility_id),
            self.seed,
            q(&hex(&self.firmware_blake3)),
            records.join(",\n"),
            q(MID_FILE),
            q(&m.scenario),
            q(&m.checkpoint),
            m.after_events,
            schemas.join(", "),
            m.seed,
            q(&m.compatibility_id),
            q(&hex(&m.firmware_blake3)),
            q(&hex(&m.disk_blake3)),
            m.size,
            q(&hex(&m.blake3)),
        )
    }

    /// Reads [`Golden::render`]'s output.
    pub fn parse(json: &str) -> Result<Golden, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if root["scenario"] != SCENARIO {
            return Err(format!("not the {SCENARIO} golden file"));
        }
        let m = &root["mid_snapshot"];
        if m["file"] != MID_FILE {
            return Err(format!("the portable snapshot is not {MID_FILE}"));
        }
        let records = root["runs"]
            .as_array()
            .ok_or("no runs")?
            .iter()
            .map(parse_record)
            .collect::<Result<_, _>>()?;
        let schemas = m["schemas"]
            .as_array()
            .ok_or("no schemas")?
            .iter()
            .map(|v| u(v).and_then(|x| u32::try_from(x).map_err(|e| e.to_string())))
            .collect::<Result<_, _>>()?;
        Ok(Golden {
            compatibility_id: s(&root["compatibility_id"])?,
            seed: u(&root["seed"])?,
            firmware_blake3: digest(&root["firmware_blake3"])?,
            records,
            mid: Mid {
                scenario: s(&m["scenario"])?,
                checkpoint: s(&m["checkpoint"])?,
                after_events: u(&m["after_events"])?,
                schemas,
                seed: u(&m["seed"])?,
                compatibility_id: s(&m["compatibility_id"])?,
                firmware_blake3: digest(&m["firmware_blake3"])?,
                disk_blake3: digest(&m["disk_blake3"])?,
                size: u(&m["size"])?,
                blake3: digest(&m["blake3"])?,
            },
        })
    }

    /// Fails unless the file describes today's scenarios: the compatibility id, the seed,
    /// the scenarios in order, the schemas, and the portable snapshot's scenario and point.
    pub fn ensure_current(&self) -> Result<(), String> {
        for (what, id) in [
            ("the golden file", &self.compatibility_id),
            ("the portable snapshot", &self.mid.compatibility_id),
        ] {
            if id != COMPATIBILITY_ID {
                return Err(format!(
                    "{what} is for compatibility id {id:?}, the contracts are {COMPATIBILITY_ID:?}"
                ));
            }
        }
        if self.seed != m3ref::SEED || self.mid.seed != m3ref::SEED {
            return Err(format!(
                "golden seeds {:#x} and {:#x} are not the m3-reference seed {:#x}",
                self.seed,
                self.mid.seed,
                m3ref::SEED
            ));
        }
        let names: Vec<&str> = self.records.iter().map(|r| r.scenario.as_str()).collect();
        let want: Vec<&str> = Scenario::ALL.iter().map(|s| s.name()).collect();
        if names != want {
            return Err(format!("the golden scenarios are {names:?}, not {want:?}"));
        }
        if self.mid.scenario != Scenario::Reference.name() || self.mid.checkpoint != MID_CHECKPOINT
        {
            return Err(format!(
                "the portable snapshot is {:?} of {}, not {MID_CHECKPOINT:?} of reference",
                self.mid.checkpoint, self.mid.scenario
            ));
        }
        if let Some(r) = self.records.iter().find(|r| r.final_blake3 != r.state) {
            return Err(format!(
                "{}: the final snapshot's BLAKE3 is not its StateDigest",
                r.scenario
            ));
        }
        if self.mid.schemas != SCHEMAS {
            return Err(format!(
                "the portable snapshot's schemas are {:?}, not {SCHEMAS:?}",
                self.mid.schemas
            ));
        }
        if self.mid.firmware_blake3 != self.firmware_blake3
            || self.records.first().map(|r| r.disk_blake3) != Some(self.mid.disk_blake3)
        {
            return Err("the portable snapshot's fixture is not the golden one".to_owned());
        }
        Ok(())
    }

    /// `actual` equals the golden record of its scenario, field by field.
    pub fn check(&self, actual: &Record) -> Result<(), String> {
        self.ensure_current()?;
        let expected = self
            .records
            .iter()
            .find(|r| r.scenario == actual.scenario)
            .ok_or_else(|| format!("no golden record for {}", actual.scenario))?;
        let differ = differing_fields(expected, actual);
        if differ.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{}: {} differ from the golden",
                actual.scenario,
                differ.join(", ")
            ))
        }
    }

    /// Portability, as in M1-A6 and M2: `snapshot` has the golden size and BLAKE3 and
    /// schemas, passes [`portable::read`], is at [`checkpoint::is_mid`], restores on a
    /// freshly built frozen platform, re-encodes to the same bytes, and runs to the golden
    /// end of the reference scenario under the watchdog without reissuing anything
    /// ([`checkpoint::no_reissue`]): event count, halt, shutdown reason, exact UART
    /// output, `StateDigest`, `ExecutionDigest`, and the final snapshot. The file has no
    /// trace prefix, so `TraceDigest` is checked by the checkpoint runs instead.
    pub fn check_portable(&self, fixture: &Fixture, snapshot: &[u8]) -> Result<(), String> {
        self.ensure_current()?;
        if fixture.scenario != Scenario::Reference
            || fixture.firmware_blake3 != self.mid.firmware_blake3
            || fixture.disk_blake3 != self.mid.disk_blake3
        {
            return Err("the portable snapshot belongs to another fixture".to_owned());
        }
        if snapshot.len() as u64 != self.mid.size
            || *blake3::hash(snapshot).as_bytes() != self.mid.blake3
        {
            return Err("the portable snapshot's bytes differ from the golden".to_owned());
        }
        let (p, acc) =
            portable::read(snapshot).map_err(|e| format!("the portable snapshot: {e}"))?;
        let schemas: Vec<u32> = p.entries.iter().map(|e| e.schema).collect();
        if schemas != self.mid.schemas || p.seed != self.mid.seed {
            return Err(
                "the portable snapshot's schemas or seed differ from the golden".to_owned(),
            );
        }
        let points: BTreeSet<_> = Tracker::default().classify(&p, &acc);
        if !checkpoint::is_mid(&points) {
            return Err(format!(
                "the portable snapshot is not at {MID_CHECKPOINT:?}: {points:?}"
            ));
        }
        let mut rt = fixture.platform();
        rt.restore(snapshot)
            .map_err(|e| format!("the portable snapshot does not restore: {e}"))?;
        if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
            return Err("restoring the portable snapshot is not the identity".to_owned());
        }
        let resumed = fixture.run(
            Start::Restore {
                snapshot: snapshot.to_vec(),
                prefix: None,
            },
            Vec::new(),
        );
        let expected = &self.records[0];
        let o = &resumed.finished.outcome;
        let mut differ = Vec::new();
        if let Err(e) = checkpoint::no_reissue(&p, &acc, &resumed.finished.dispatched) {
            differ.push(e);
        }
        if self.mid.after_events + o.events != expected.events {
            differ.push("event count".to_owned());
        }
        match &o.end {
            End::Trap { cause, pc, .. } if *cause == expected.halt && *pc == expected.halt_pc => {}
            other => differ.push(format!("halt ({other})")),
        }
        if o.state != Some(expected.state) {
            differ.push("StateDigest".to_owned());
        }
        if o.execution != expected.execution {
            differ.push("ExecutionDigest".to_owned());
        }
        if resumed.output.as_ref().ok() != Some(&expected.uart) {
            differ.push("UART output".to_owned());
        }
        match resumed.snapshot.as_deref() {
            Some(end)
                if end.len() as u64 == expected.final_size
                    && *blake3::hash(end).as_bytes() == expected.final_blake3 =>
            {
                match portable::read(end) {
                    Ok((q, _)) if q.cpu.regs[10] == expected.reason => {}
                    _ => differ.push("shutdown reason".to_owned()),
                }
            }
            _ => differ.push("final snapshot".to_owned()),
        }
        if differ.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the resumed portable snapshot differs in {} from the golden",
                differ.join(", ")
            ))
        }
    }

    /// `cargo xtask m3-golden verify`: regenerates everything under `root` on this
    /// machine and requires the committed `json` and `snapshot` back, byte for byte, then
    /// checks the committed snapshot's portability. Writes nothing.
    pub fn verify(root: &Path, json: &str, snapshot: &[u8]) -> Result<(), Vec<String>> {
        let committed = Golden::parse(json).map_err(|e| vec![format!("{GOLDEN_PATH}: {e}")])?;
        committed.ensure_current().map_err(|e| vec![e])?;
        let (fresh, fresh_snapshot) = Golden::generate(root).map_err(|e| vec![e])?;
        let mut errors = compare_files(
            "this machine's run",
            (&committed, json, snapshot),
            (&fresh, &fresh.render(), &fresh_snapshot),
        );
        let fixture = Fixture::read(root, Scenario::Reference).map_err(|e| vec![e])?;
        if let Err(e) = committed.check_portable(&fixture, snapshot) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `cargo xtask m3-golden check`: the result another machine emitted, its `json` and
    /// its own `snapshot`, must equal the committed files and this machine's run byte
    /// for byte, and the foreign snapshot must restore here and run to the golden end.
    pub fn check_foreign(
        root: &Path,
        committed: (&str, &[u8]),
        foreign: (&str, &[u8]),
    ) -> Result<(), Vec<String>> {
        let golden = Golden::parse(committed.0).map_err(|e| vec![format!("{GOLDEN_PATH}: {e}")])?;
        golden.ensure_current().map_err(|e| vec![e])?;
        let other =
            Golden::parse(foreign.0).map_err(|e| vec![format!("the foreign result: {e}")])?;
        let mut errors = compare_files(
            "the foreign result",
            (&golden, committed.0, committed.1),
            (&other, foreign.0, foreign.1),
        );
        let (fresh, fresh_snapshot) = Golden::generate(root).map_err(|e| vec![e])?;
        errors.extend(compare_files(
            "the foreign result, against this machine's run,",
            (&fresh, &fresh.render(), &fresh_snapshot),
            (&other, foreign.0, foreign.1),
        ));
        let fixture = Fixture::read(root, Scenario::Reference).map_err(|e| vec![e])?;
        if let Err(e) = golden.check_portable(&fixture, foreign.1) {
            errors.push(format!("the foreign snapshot on this machine: {e}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// The differences between the `expected` files and the `actual` ones, as parsed
/// golden files, their text, and the snapshot bytes.
fn compare_files(
    what: &str,
    expected: (&Golden, &str, &[u8]),
    actual: (&Golden, &str, &[u8]),
) -> Vec<String> {
    let mut errors = Vec::new();
    let changes = describe_changes(Some(expected.0), actual.0);
    if !changes.is_empty() {
        errors.push(format!("{what} differs from the golden:"));
        errors.extend(changes.into_iter().map(|c| format!("  {c}")));
    } else if expected.1 != actual.1 {
        errors.push(format!("{what} renders the golden file differently"));
    }
    if expected.2 != actual.2 {
        errors.push(format!("{what} has different portable snapshot bytes"));
    }
    errors
}

fn render_record(r: &Record) -> String {
    format!(
        "    {{\n      \"scenario\": {},\n      \"disk_blake3\": {},\n      \"halt\": {},\n      \
         \"halt_pc\": {},\n      \"shutdown_reason\": {},\n      \"uart\": {},\n      \
         \"uart_blake3\": {},\n      \"events\": {},\n      \"instret\": {},\n      \
         \"os_records\": {},\n      \"syscalls\": {},\n      \"switches\": {},\n      \
         \"exceptions\": {},\n      \"dma_commands\": {},\n      \"uart_bytes\": {},\n      \
         \"state\": {},\n      \"execution\": {},\n      \"trace\": {},\n      \
         \"final_snapshot_size\": {},\n      \"final_snapshot_blake3\": {}\n    }}",
        q(&r.scenario),
        q(&hex(&r.disk_blake3)),
        q(&r.halt),
        q(&word(r.halt_pc)),
        r.reason,
        q(&hex(&r.uart)),
        q(&hex(blake3::hash(&r.uart).as_bytes())),
        r.events,
        r.instret,
        r.os_records,
        r.syscalls,
        r.switches,
        r.exceptions,
        r.dma_commands,
        r.uart_bytes,
        q(&hex(&r.state)),
        q(&hex(&r.execution)),
        q(&hex(&r.trace)),
        r.final_size,
        q(&hex(&r.final_blake3)),
    )
}

/// Every field of a record, rendered, in file order.
fn fields(r: &Record) -> Vec<(&'static str, String)> {
    vec![
        ("disk_blake3", hex(&r.disk_blake3)),
        ("halt", r.halt.clone()),
        ("halt_pc", word(r.halt_pc)),
        ("shutdown_reason", r.reason.to_string()),
        ("uart", hex(&r.uart)),
        ("events", r.events.to_string()),
        ("instret", r.instret.to_string()),
        ("os_records", r.os_records.to_string()),
        ("syscalls", r.syscalls.to_string()),
        ("switches", r.switches.to_string()),
        ("exceptions", r.exceptions.to_string()),
        ("dma_commands", r.dma_commands.to_string()),
        ("uart_bytes", r.uart_bytes.to_string()),
        ("StateDigest", hex(&r.state)),
        ("ExecutionDigest", hex(&r.execution)),
        ("TraceDigest", hex(&r.trace)),
        ("final_snapshot_size", r.final_size.to_string()),
        ("final_snapshot_blake3", hex(&r.final_blake3)),
    ]
}

fn differing_fields(expected: &Record, actual: &Record) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = fields(expected)
        .into_iter()
        .zip(fields(actual))
        .filter(|(a, b)| a.1 != b.1)
        .map(|(a, _)| a.0)
        .collect();
    if expected.scenario != actual.scenario {
        out.insert(0, "scenario");
    }
    out
}

/// One line per difference between two golden files, for `cargo xtask m3-golden`.
pub fn describe_changes(old: Option<&Golden>, new: &Golden) -> Vec<String> {
    let Some(old) = old else {
        return vec!["+ new golden file".to_owned()];
    };
    let mut out = Vec::new();
    if old.compatibility_id != new.compatibility_id {
        out.push(format!(
            "~ compatibility id {:?} -> {:?}",
            old.compatibility_id, new.compatibility_id
        ));
    }
    if old.seed != new.seed {
        out.push(format!("~ seed {:#x} -> {:#x}", old.seed, new.seed));
    }
    if old.firmware_blake3 != new.firmware_blake3 {
        out.push("~ firmware_blake3".to_owned());
    }
    let short = |s: &str| {
        if s.len() > 19 {
            format!("{}..", &s[..16])
        } else {
            s.to_owned()
        }
    };
    let names = |g: &Golden| {
        g.records
            .iter()
            .map(|r| r.scenario.clone())
            .collect::<Vec<_>>()
    };
    if names(old) != names(new) {
        out.push(format!("~ scenarios {:?} -> {:?}", names(old), names(new)));
    }
    for (a, b) in old.records.iter().zip(&new.records) {
        for ((name, before), (_, after)) in fields(a).into_iter().zip(fields(b)) {
            if before != after {
                out.push(format!(
                    "~ {}: {name} {} -> {}",
                    b.scenario,
                    short(&before),
                    short(&after)
                ));
            }
        }
    }
    if old.mid != new.mid {
        out.push(format!(
            "~ mid snapshot: {} after {} events, {} bytes ({}..) -> {} after {} events, {} bytes ({}..)",
            old.mid.scenario,
            old.mid.after_events,
            old.mid.size,
            &hex(&old.mid.blake3)[..16],
            new.mid.scenario,
            new.mid.after_events,
            new.mid.size,
            &hex(&new.mid.blake3)[..16],
        ));
    }
    out
}

fn parse_record(v: &Value) -> Result<Record, String> {
    let scenario = s(&v["scenario"])?;
    let uart = bytes(&v["uart"])?;
    if *blake3::hash(&uart).as_bytes() != digest(&v["uart_blake3"])? {
        return Err(format!("{scenario}: uart_blake3 is not the BLAKE3 of uart"));
    }
    Ok(Record {
        disk_blake3: digest(&v["disk_blake3"])?,
        halt: s(&v["halt"])?,
        halt_pc: parse_word(&v["halt_pc"])?,
        reason: u32::try_from(u(&v["shutdown_reason"])?).map_err(|e| e.to_string())?,
        uart,
        events: u(&v["events"])?,
        instret: u(&v["instret"])?,
        os_records: u(&v["os_records"])?,
        syscalls: u(&v["syscalls"])?,
        switches: u(&v["switches"])?,
        exceptions: u(&v["exceptions"])?,
        dma_commands: u(&v["dma_commands"])?,
        uart_bytes: u(&v["uart_bytes"])?,
        state: digest(&v["state"])?,
        execution: digest(&v["execution"])?,
        trace: digest(&v["trace"])?,
        final_size: u(&v["final_snapshot_size"])?,
        final_blake3: digest(&v["final_snapshot_blake3"])?,
        scenario,
    })
}

/// A 32-bit word as it appears in the file: `0x` and eight lowercase digits.
fn word(x: u32) -> String {
    format!("{x:#010x}")
}

fn parse_word(v: &Value) -> Result<u32, String> {
    v.as_str()
        .filter(|t| t.len() == 10)
        .and_then(|t| t.strip_prefix("0x"))
        .filter(|t| {
            t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .and_then(|t| u32::from_str_radix(t, 16).ok())
        .ok_or_else(|| format!("{v} is not a word"))
}

fn q(text: &str) -> String {
    format!("\"{text}\"")
}

fn s(v: &Value) -> Result<String, String> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{v} is not a string"))
}

fn u(v: &Value) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("{v} is not an unsigned integer"))
}

fn digest(v: &Value) -> Result<[u8; 32], String> {
    v.as_str()
        .and_then(unhex32)
        .ok_or_else(|| format!("{v} is not a digest"))
}

fn bytes(v: &Value) -> Result<Vec<u8>, String> {
    let text = v.as_str().ok_or_else(|| format!("{v} is not hex"))?;
    if text.len() % 2 != 0
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("{v} is not lowercase hex"));
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(scenario: Scenario, reason: u32) -> Record {
        Record {
            scenario: scenario.name().to_owned(),
            disk_blake3: [3 + reason as u8; 32],
            halt: "EnvironmentCallFromS".to_owned(),
            halt_pc: 0x8000_0040,
            reason,
            uart: scenario.expected_output().to_vec(),
            events: 160_000,
            instret: 2000,
            os_records: 70,
            syscalls: 21,
            switches: 10,
            exceptions: 22,
            dma_commands: 6,
            uart_bytes: scenario.expected_output().len() as u64,
            state: [6; 32],
            execution: [7; 32],
            trace: [8; 32],
            final_size: 140_000,
            final_blake3: [6; 32],
        }
    }

    fn sample() -> Golden {
        Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: m3ref::SEED,
            firmware_blake3: [1; 32],
            records: vec![record(Scenario::Reference, 1), record(Scenario::NoFault, 0)],
            mid: Mid {
                scenario: "reference".to_owned(),
                checkpoint: MID_CHECKPOINT.to_owned(),
                after_events: 120_000,
                schemas: SCHEMAS.to_vec(),
                seed: m3ref::SEED,
                compatibility_id: COMPATIBILITY_ID.to_owned(),
                firmware_blake3: [1; 32],
                disk_blake3: [4; 32],
                size: 140_000,
                blake3: [10; 32],
            },
        }
    }

    #[test]
    fn rendering_parses_back_and_is_canonical() {
        let golden = sample();
        let text = golden.render();
        assert_eq!(Golden::parse(&text), Ok(golden.clone()));
        assert_eq!(Golden::parse(&text).unwrap().render(), text);
        assert_eq!(golden.ensure_current(), Ok(()));
        assert!(text.ends_with("}\n") && !text.contains('\r'));
        assert!(text.contains("\"schemas\": [3, 1, 1, 1, 1, 1, 1, 1]"));
        assert!(!text.contains("budget"), "the watchdog is not recorded");
    }

    #[test]
    fn records_are_checked_field_by_field() {
        let golden = sample();
        for r in &golden.records {
            assert_eq!(golden.check(r), Ok(()));
        }
        let base = golden.records[0].clone();
        let mut uart = base.clone();
        // The last byte, so a comparison of a prefix cannot pass.
        *uart.uart.last_mut().unwrap() ^= 1;
        let mut reason = base.clone();
        reason.reason = 0;
        let mut events = base.clone();
        events.events += 1;
        let mut trace = base.clone();
        trace.trace[0] ^= 1;
        let mut end = base.clone();
        end.final_blake3[31] ^= 1;
        let mut size = base.clone();
        size.final_size += 1;
        for (name, r) in [
            ("uart", uart),
            ("shutdown_reason", reason),
            ("events", events),
            ("TraceDigest", trace),
            ("final_snapshot_blake3", end),
            ("final_snapshot_size", size),
        ] {
            let err = golden.check(&r).unwrap_err();
            assert!(err.contains(name), "{name}: {err}");
        }
    }

    #[test]
    fn a_stale_or_doctored_file_is_refused() {
        let mut g = sample();
        g.compatibility_id = "9.9.9".to_owned();
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.mid.seed = 1;
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.mid.schemas[0] = 2;
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.records[1].final_blake3[0] ^= 1;
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.records.swap(0, 1);
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.mid.checkpoint = "elsewhere".to_owned();
        assert!(g.ensure_current().is_err());
        let text = sample().render();
        let tampered = text.replacen("\"uart\": \"", "\"uart\": \"00", 1);
        assert!(
            Golden::parse(&tampered)
                .unwrap_err()
                .contains("uart_blake3")
        );
        assert!(Golden::parse(&text.replacen("m3-reference\"", "m2-reference\"", 1)).is_err());
        assert!(Golden::parse(&text.replacen("\"0x80000040\"", "\"0x80000040 \"", 1)).is_err());
    }

    #[test]
    fn changes_are_described_field_by_field() {
        let old = sample();
        assert_eq!(describe_changes(Some(&old), &old), Vec::<String>::new());
        assert_eq!(describe_changes(None, &old), ["+ new golden file"]);
        let mut new = old.clone();
        new.records[1].trace[0] ^= 1;
        new.mid.after_events += 1;
        let changes = describe_changes(Some(&old), &new);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(
            changes[0].starts_with("~ nofault: TraceDigest "),
            "{changes:?}"
        );
        assert!(changes[1].starts_with("~ mid snapshot:"));
    }
}
