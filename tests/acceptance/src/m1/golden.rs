//! The M1 golden files (`docs/m1-design.md` §10.1, M1-A6 and M1-A8).
//!
//! - `tests/golden/m1-reference.json`: per committed program, its ELF and `image_hash`,
//!   halt, `instret`, event count, final `pc` and registers, UART output and its BLAKE3,
//!   and `StateDigest`, `ExecutionDigest`, and `TraceDigest`; then the portable snapshot's
//!   provenance.
//! - `tests/golden/m1-reference.mid.snap`: `hello.elf` snapshotted with its tenth UART
//!   byte's `WriteResp` pending.
//!
//! Only `cargo xtask m1-golden bless` writes them, through [`Golden::generate`] and
//! [`Golden::render`]. Tests and `cargo xtask m1-golden verify` read the committed copies
//! and never write.

use std::path::Path;

use serde_json::Value;
use systemscope_contracts::COMPATIBILITY_ID;
use systemscope_rv32::SELECTED;
use systemscope_rv32::hello::{self, EXPECTED_OUTPUT};
use systemscope_rv32::runner::{self, Start};

use super::{Program, REFERENCE, Record, SCENARIO, programs, registers, uart_writes};
use crate::{hex, unhex32};

/// The golden file, relative to the workspace root.
pub const GOLDEN_PATH: &str = "tests/golden/m1-reference.json";
/// The portable snapshot, relative to the workspace root.
pub const MID_SNAPSHOT_PATH: &str = "tests/golden/m1-reference.mid.snap";
/// The portable snapshot's file name, as the golden file records it.
pub const MID_FILE: &str = "m1-reference.mid.snap";
/// UART bytes already printed when the portable snapshot is taken.
pub const MID_BYTES: usize = 10;
/// Where the portable snapshot is taken.
pub const MID_CHECKPOINT: &str = "tenth UART byte accepted, its WriteResp pending";

/// The portable snapshot's provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mid {
    /// The program it comes from: [`REFERENCE`].
    pub program: String,
    /// [`MID_CHECKPOINT`].
    pub checkpoint: String,
    /// Events dispatched before the snapshot.
    pub after_events: u64,
    /// The session seed.
    pub seed: u64,
    /// The contracts compatibility id in its session information.
    pub compatibility_id: String,
    /// The program's `image_hash`.
    pub image_hash: [u8; 32],
    /// The file's size in bytes.
    pub size: u64,
    /// BLAKE3 of the file. Guards against line-ending conversion.
    pub blake3: [u8; 32],
}

/// The contents of [`GOLDEN_PATH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    /// The contracts compatibility id every run's session carries.
    pub compatibility_id: String,
    /// The session seed of every run.
    pub seed: u64,
    /// One record per committed program: `hello`, then the `rv32ui` selection.
    pub programs: Vec<Record>,
    /// The portable snapshot.
    pub mid: Mid,
}

/// Where the portable snapshot is taken in a run whose events are `events`: right after
/// the UART accepted its [`MID_BYTES`]th byte.
pub fn mid_after(events: &[systemscope_runtime::runtime::Dispatched]) -> Option<usize> {
    uart_writes(events).get(MID_BYTES - 1).map(|i| i + 1)
}

impl Golden {
    /// Runs every committed program under `root` and produces the golden file and the
    /// portable snapshot. Fails if any run fails its pass rule: a failing run is never
    /// blessed.
    pub fn generate(root: &Path) -> Result<(Golden, Vec<u8>), String> {
        let programs = programs(root)?;
        let mut records = Vec::new();
        let mut mid = None;
        for program in &programs {
            if program.name == REFERENCE {
                let run = hello::run(&program.image, Start::Init { traced: true }, Vec::new());
                hello::judge(&run, true).map_err(|e| format!("{REFERENCE}: {e}"))?;
                records.push(Record::of(program, &run.finished)?);
                let k = mid_after(&run.finished.dispatched)
                    .ok_or("hello printed fewer bytes than the portable snapshot needs")?;
                mid = Some((program, k));
            } else {
                records.push(Record::run(program)?);
            }
        }
        let (program, k) = mid.ok_or("no reference program")?;
        let snapshot = snapshot_after(program, k)?;
        let golden = Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: runner::SEED,
            programs: records,
            mid: Mid {
                program: program.name.clone(),
                checkpoint: MID_CHECKPOINT.to_owned(),
                after_events: k as u64,
                seed: runner::SEED,
                compatibility_id: COMPATIBILITY_ID.to_owned(),
                image_hash: program.image.image_hash,
                size: snapshot.len() as u64,
                blake3: *blake3::hash(&snapshot).as_bytes(),
            },
        };
        Ok((golden, snapshot))
    }

    /// The record for `program`, if it has one.
    pub fn record(&self, program: &str) -> Option<&Record> {
        self.programs.iter().find(|r| r.program == program)
    }

    /// The file contents: stable JSON with a fixed field order, lowercase hex, LF line
    /// ends, and a trailing newline. Nothing host-specific: no path, time, or run id.
    pub fn render(&self) -> String {
        let programs: Vec<String> = self.programs.iter().map(render_record).collect();
        let m = &self.mid;
        format!(
            "{{\n  \"scenario\": {},\n  \"compatibility_id\": {},\n  \"seed\": {},\n  \
             \"programs\": [\n{}\n  ],\n  \"mid_snapshot\": {{\n    \"file\": {},\n    \
             \"program\": {},\n    \"checkpoint\": {},\n    \"after_events\": {},\n    \
             \"seed\": {},\n    \"compatibility_id\": {},\n    \"image_hash\": {},\n    \
             \"size\": {},\n    \"blake3\": {}\n  }}\n}}\n",
            q(SCENARIO),
            q(&self.compatibility_id),
            self.seed,
            programs.join(",\n"),
            q(MID_FILE),
            q(&m.program),
            q(&m.checkpoint),
            m.after_events,
            m.seed,
            q(&m.compatibility_id),
            q(&hex(&m.image_hash)),
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
        let programs = root["programs"]
            .as_array()
            .ok_or("no programs")?
            .iter()
            .map(parse_record)
            .collect::<Result<Vec<_>, String>>()?;
        let m = &root["mid_snapshot"];
        if m["file"] != MID_FILE {
            return Err(format!("the portable snapshot is not {MID_FILE}"));
        }
        Ok(Golden {
            compatibility_id: s(&root["compatibility_id"])?,
            seed: u(&root["seed"])?,
            programs,
            mid: Mid {
                program: s(&m["program"])?,
                checkpoint: s(&m["checkpoint"])?,
                after_events: u(&m["after_events"])?,
                seed: u(&m["seed"])?,
                compatibility_id: s(&m["compatibility_id"])?,
                image_hash: digest(&m["image_hash"])?,
                size: u(&m["size"])?,
                blake3: digest(&m["blake3"])?,
            },
        })
    }

    /// Fails unless the file describes today's scenario: the compatibility id, the seed,
    /// the committed programs in order, and the portable snapshot's program and point.
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
        if self.seed != runner::SEED || self.mid.seed != runner::SEED {
            return Err(format!(
                "golden seeds {:#x} and {:#x} are not the m1-reference seed {:#x}",
                self.seed,
                self.mid.seed,
                runner::SEED
            ));
        }
        let names: Vec<&str> = self.programs.iter().map(|r| r.program.as_str()).collect();
        let expected: Vec<&str> = [REFERENCE].into_iter().chain(SELECTED).collect();
        if names != expected {
            return Err(format!(
                "golden programs {names:?} are not the committed programs {expected:?}"
            ));
        }
        if self.mid.program != REFERENCE || self.mid.checkpoint != MID_CHECKPOINT {
            return Err(format!(
                "the portable snapshot is {:?} of {}, not {MID_CHECKPOINT:?} of {REFERENCE}",
                self.mid.checkpoint, self.mid.program
            ));
        }
        Ok(())
    }

    /// M1-A8: `actual` equals the golden record for its program, field by field.
    pub fn check(&self, actual: &Record) -> Result<(), String> {
        self.ensure_current()?;
        let expected = self
            .record(&actual.program)
            .ok_or_else(|| format!("{} has no golden record", actual.program))?;
        let differ = differing_fields(expected, actual);
        if differ.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{}: {} differ from the golden",
                actual.program,
                differ.join(", ")
            ))
        }
    }

    /// M1-A6 portability, as M0 AT-2 step 4: `snapshot` has the golden size and BLAKE3,
    /// restores on a fresh `program` platform, re-encodes to the same bytes, and runs to
    /// the golden end: event count, `StateDigest`, `ExecutionDigest`, registers, and the
    /// exact UART output, with each remaining byte written once. The file has no trace
    /// prefix, so the full-run `TraceDigest` is checked by the checkpoint runs instead.
    pub fn check_portable(&self, program: &Program, snapshot: &[u8]) -> Result<(), String> {
        self.ensure_current()?;
        if program.name != self.mid.program || program.image.image_hash != self.mid.image_hash {
            return Err(format!(
                "the portable snapshot belongs to {}, not {}",
                self.mid.program, program.name
            ));
        }
        if snapshot.len() as u64 != self.mid.size
            || *blake3::hash(snapshot).as_bytes() != self.mid.blake3
        {
            return Err("the portable snapshot's bytes differ from the golden".to_owned());
        }
        let mut rt = program.platform();
        rt.restore(snapshot)
            .map_err(|e| format!("the portable snapshot does not restore: {e}"))?;
        if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
            return Err("restoring the portable snapshot is not the identity".to_owned());
        }
        let resumed = program.execute(Start::Restore {
            snapshot: snapshot.to_vec(),
            prefix: None,
        });
        let expected = self
            .record(&program.name)
            .ok_or("the portable snapshot's program has no golden record")?;
        let o = &resumed.outcome;
        runner::judge(o).map_err(|e| format!("the resumed portable snapshot: {e}"))?;
        let mut differ = Vec::new();
        if self.mid.after_events + o.events != expected.events {
            differ.push("event count");
        }
        if o.state != Some(expected.state) {
            differ.push("StateDigest");
        }
        if o.execution != expected.execution {
            differ.push("ExecutionDigest");
        }
        if registers(&resumed).ok() != Some(expected.registers) {
            differ.push("registers");
        }
        if hello::uart_output(&resumed.views).ok() != expected.uart {
            differ.push("UART output");
        }
        let rest = uart_writes(&resumed.dispatched).len();
        if rest != EXPECTED_OUTPUT.len() - MID_BYTES {
            differ.push("UART writes after the snapshot");
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

    /// `cargo xtask m1-golden verify`: regenerates everything under `root` on this
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
        let reference = super::program(root, REFERENCE).map_err(|e| vec![e])?;
        if let Err(e) = committed.check_portable(&reference, snapshot) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `cargo xtask m1-golden check`: the result another machine emitted, its `json` and
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
        let reference = super::program(root, REFERENCE).map_err(|e| vec![e])?;
        if let Err(e) = golden.check_portable(&reference, foreign.1) {
            errors.push(format!("the foreign snapshot on this machine: {e}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Snapshots `program` after `k` events of an untraced run from `init`.
pub fn snapshot_after(program: &Program, k: usize) -> Result<Vec<u8>, String> {
    let mut rt = program.platform();
    rt.init().map_err(|e| e.to_string())?;
    for i in 0..k {
        rt.step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{} ended after {i} events", program.name))?;
    }
    rt.snapshot().map_err(|e| e.to_string())
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

/// Every field of a record, rendered, in file order.
fn fields(r: &Record) -> Vec<(&'static str, String)> {
    vec![
        ("elf_blake3", hex(&r.elf_blake3)),
        ("image_hash", hex(&r.image_hash)),
        ("halt", r.cause.clone()),
        ("trap_pc", word(r.trap_pc)),
        ("tval", word(r.tval)),
        ("instret", r.instret.to_string()),
        ("events", r.events.to_string()),
        ("registers", r.registers.map(word).join(",")),
        ("uart", r.uart.as_deref().map_or("null".to_owned(), hex)),
        ("StateDigest", hex(&r.state)),
        ("ExecutionDigest", hex(&r.execution)),
        ("TraceDigest", hex(&r.trace)),
    ]
}

fn differing_fields(expected: &Record, actual: &Record) -> Vec<&'static str> {
    fields(expected)
        .into_iter()
        .zip(fields(actual))
        .filter(|(a, b)| a.1 != b.1)
        .map(|(a, _)| a.0)
        .collect()
}

/// One line per difference between two golden files, for `cargo xtask m1-golden`.
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
    let short = |s: &str| {
        if s.len() > 19 {
            format!("{}..", &s[..16])
        } else {
            s.to_owned()
        }
    };
    for r in &new.programs {
        let Some(o) = old.record(&r.program) else {
            out.push(format!("+ {}", r.program));
            continue;
        };
        for ((name, before), (_, after)) in fields(o).into_iter().zip(fields(r)) {
            if before != after {
                out.push(format!(
                    "~ {}: {name} {} -> {}",
                    r.program,
                    short(&before),
                    short(&after)
                ));
            }
        }
    }
    for r in &old.programs {
        if new.record(&r.program).is_none() {
            out.push(format!("- {}", r.program));
        }
    }
    if old.mid != new.mid {
        out.push(format!(
            "~ mid snapshot: {} after {} events, {} bytes ({}..) -> {} after {} events, {} bytes ({}..)",
            old.mid.program,
            old.mid.after_events,
            old.mid.size,
            &hex(&old.mid.blake3)[..16],
            new.mid.program,
            new.mid.after_events,
            new.mid.size,
            &hex(&new.mid.blake3)[..16],
        ));
    }
    out
}

fn render_record(r: &Record) -> String {
    let registers: Vec<String> = r.registers.iter().map(|&x| q(&word(x))).collect();
    let (uart, uart_blake3) = match &r.uart {
        Some(bytes) => (q(&hex(bytes)), q(&hex(blake3::hash(bytes).as_bytes()))),
        None => ("null".to_owned(), "null".to_owned()),
    };
    format!(
        "    {{\n      \"program\": {},\n      \"elf_blake3\": {},\n      \"image_hash\": {},\n      \
         \"halt\": {},\n      \"trap_pc\": {},\n      \"tval\": {},\n      \"instret\": {},\n      \
         \"events\": {},\n      \"registers\": [{}],\n      \"uart\": {},\n      \
         \"uart_blake3\": {},\n      \"state\": {},\n      \"execution\": {},\n      \
         \"trace\": {}\n    }}",
        q(&r.program),
        q(&hex(&r.elf_blake3)),
        q(&hex(&r.image_hash)),
        q(&r.cause),
        q(&word(r.trap_pc)),
        q(&word(r.tval)),
        r.instret,
        r.events,
        registers.join(", "),
        uart,
        uart_blake3,
        q(&hex(&r.state)),
        q(&hex(&r.execution)),
        q(&hex(&r.trace)),
    )
}

fn parse_record(v: &Value) -> Result<Record, String> {
    let program = s(&v["program"])?;
    let registers: Vec<u32> = v["registers"]
        .as_array()
        .ok_or_else(|| format!("{program}: no registers"))?
        .iter()
        .map(parse_word)
        .collect::<Result<_, _>>()?;
    let registers: [u32; 32] = registers
        .try_into()
        .map_err(|r: Vec<u32>| format!("{program}: {} registers, not 32", r.len()))?;
    let uart = match &v["uart"] {
        Value::Null => None,
        other => Some(bytes(other)?),
    };
    let uart_blake3 = match &v["uart_blake3"] {
        Value::Null => None,
        other => Some(digest(other)?),
    };
    if uart.as_deref().map(|b| *blake3::hash(b).as_bytes()) != uart_blake3 {
        return Err(format!("{program}: uart_blake3 is not the BLAKE3 of uart"));
    }
    Ok(Record {
        elf_blake3: digest(&v["elf_blake3"])?,
        image_hash: digest(&v["image_hash"])?,
        cause: s(&v["halt"])?,
        trap_pc: parse_word(&v["trap_pc"])?,
        tval: parse_word(&v["tval"])?,
        instret: u(&v["instret"])?,
        events: u(&v["events"])?,
        registers,
        uart,
        state: digest(&v["state"])?,
        execution: digest(&v["execution"])?,
        trace: digest(&v["trace"])?,
        program,
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

    fn record(name: &str, n: u8) -> Record {
        Record {
            program: name.to_owned(),
            elf_blake3: [n; 32],
            image_hash: [n + 1; 32],
            cause: "EnvironmentCall".to_owned(),
            trap_pc: 0x8000_002c,
            tval: 0,
            instret: 87,
            events: 728,
            registers: std::array::from_fn(|i| i as u32 * 0x0101_0101),
            uart: (name == REFERENCE).then(|| EXPECTED_OUTPUT.to_vec()),
            state: [n + 2; 32],
            execution: [n + 3; 32],
            trace: [n + 4; 32],
        }
    }

    fn sample() -> Golden {
        Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: runner::SEED,
            programs: [REFERENCE]
                .into_iter()
                .chain(SELECTED)
                .zip(1..)
                .map(|(name, n)| record(name, n))
                .collect(),
            mid: Mid {
                program: REFERENCE.to_owned(),
                checkpoint: MID_CHECKPOINT.to_owned(),
                after_events: 341,
                seed: runner::SEED,
                compatibility_id: COMPATIBILITY_ID.to_owned(),
                image_hash: [2; 32],
                size: 8933,
                blake3: [7; 32],
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
        assert!(text.contains("\"uart\": \"48656c6c6f2c2053797374656d53636f7065210a\""));
        assert!(text.contains("\"trap_pc\": \"0x8000002c\""));
        assert!(
            text.contains("\"0x0a0a0a0a\"") && !text.contains("0x0A"),
            "lowercase hex"
        );
    }

    #[test]
    fn records_are_checked_field_by_field() {
        let golden = sample();
        for r in &golden.programs {
            assert_eq!(golden.check(r), Ok(()));
        }
        let hello = golden.programs[0].clone();
        let mut uart = hello.clone();
        uart.uart.as_mut().unwrap()[0] = b'J';
        let mut events = hello.clone();
        events.events += 1;
        let mut state = hello.clone();
        state.state[31] ^= 0x01;
        let mut image = hello.clone();
        image.image_hash[0] ^= 0x10;
        let mut reg = hello.clone();
        reg.registers[10] = 1;
        for (name, r) in [
            ("uart", uart),
            ("events", events),
            ("StateDigest", state),
            ("image_hash", image),
            ("registers", reg),
        ] {
            let err = golden.check(&r).unwrap_err();
            assert!(err.contains(name), "{name}: {err}");
        }
        let mut other = hello.clone();
        other.program = "nope".to_owned();
        assert!(golden.check(&other).is_err());
    }

    #[test]
    fn a_stale_scenario_is_refused() {
        let mut g = sample();
        g.compatibility_id = "9.9.9".to_owned();
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.mid.seed = 1;
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.programs.pop();
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.programs.swap(1, 2);
        assert!(g.ensure_current().is_err());
        let mut g = sample();
        g.mid.checkpoint = "elsewhere".to_owned();
        assert!(g.ensure_current().is_err());
    }

    #[test]
    fn a_doctored_file_is_refused() {
        let text = sample().render();
        let tampered = text.replacen("48656c6c6f", "4a656c6c6f", 1);
        let err = Golden::parse(&tampered).unwrap_err();
        assert!(err.contains("uart_blake3"), "{err}");
        assert!(Golden::parse(&text.replacen("\"0x8000002c\"", "\"0x8000002C\"", 1)).is_err());
        assert!(Golden::parse(&text.replacen("m1-reference", "m0-reference", 1)).is_err());
    }

    #[test]
    fn changes_are_described_field_by_field() {
        let old = sample();
        assert_eq!(describe_changes(Some(&old), &old), Vec::<String>::new());
        assert_eq!(describe_changes(None, &old), ["+ new golden file"]);
        let mut new = old.clone();
        new.programs[3].execution = [0xab; 32];
        new.programs[0].trace[0] ^= 1;
        new.mid.after_events += 1;
        let changes = describe_changes(Some(&old), &new);
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(
            changes[0].starts_with("~ hello: TraceDigest "),
            "{changes:?}"
        );
        let changed = format!("~ {}: ExecutionDigest ", new.programs[3].program);
        assert!(changes[1].starts_with(&changed), "{changes:?}");
        assert!(changes[1].ends_with("-> abababababababab.."), "{changes:?}");
        assert!(changes[2].starts_with("~ mid snapshot:"));
    }
}
