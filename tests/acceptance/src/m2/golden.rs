//! The M2 golden files (`docs/m2-design.md` §13.2, §15.1), in the M1 format
//! ([`crate::m1::golden`]).
//!
//! - `tests/golden/m2-reference.json`: the committed fixture's ELF, `image_hash`, and
//!   disk hashes; the halt, `instret`, event count, final `pc`, registers, and M2 CSRs;
//!   the UART output and its BLAKE3; the interrupts, media operations, DMA beats, handler
//!   entries, and final disk blocks; `StateDigest`, `ExecutionDigest`, and `TraceDigest`;
//!   then the portable snapshot's provenance.
//! - `tests/golden/m2-reference.mid.snap`: `block_irq.elf` snapshotted mid-DMA, with the
//!   WRITE to LBA 1 half buffered.
//!
//! Only `cargo xtask m2-golden bless` writes them, through [`Golden::generate`] and
//! [`Golden::render`]. Tests and `cargo xtask m2-golden verify` read the committed copies
//! and never write. The event watchdog is not part of either file.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use serde_json::Value;
use systemscope_contracts::COMPATIBILITY_ID;
use systemscope_platform::dma;
use systemscope_rv32::block_irq;
use systemscope_rv32::m2ref;
use systemscope_rv32::runner::{self, Start};

use super::{Boundary, BoundaryWatch, CSRS, Fixture, PROGRAM, Record, SCENARIO, csrs, registers};
use crate::{hex, unhex32};

/// The golden file, relative to the workspace root.
pub const GOLDEN_PATH: &str = "tests/golden/m2-reference.json";
/// The portable snapshot, relative to the workspace root.
pub const MID_SNAPSHOT_PATH: &str = "tests/golden/m2-reference.mid.snap";
/// The portable snapshot's file name, as the golden file records it.
pub const MID_FILE: &str = "m2-reference.mid.snap";
/// WRITE beats already read into the controller's buffer when the portable snapshot is
/// taken.
pub const MID_BEATS: u64 = 16;
/// Where the portable snapshot is taken.
pub const MID_CHECKPOINT: &str =
    "WRITE to LBA 1 half buffered: 16 of 32 beats read, beat 16 outstanding";

/// The portable snapshot's provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mid {
    /// The program it comes from: [`PROGRAM`].
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
    /// The media's `image_hash`: BLAKE3 of the disk fixture.
    pub media_image_hash: [u8; 32],
    /// The file's size in bytes.
    pub size: u64,
    /// BLAKE3 of the file. Guards against line-ending conversion.
    pub blake3: [u8; 32],
}

/// The contents of [`GOLDEN_PATH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    /// The contracts compatibility id the run's session carries.
    pub compatibility_id: String,
    /// The session seed.
    pub seed: u64,
    /// The program's record.
    pub record: Record,
    /// The portable snapshot.
    pub mid: Mid,
}

/// Where the portable snapshot is taken in a run whose boundaries are `boundaries`
/// (entry `i` after `i + 1` events): right after the event that left the WRITE's beat
/// [`MID_BEATS`] outstanding.
pub fn mid_after(boundaries: &[Boundary]) -> Option<usize> {
    boundaries
        .iter()
        .position(|b| {
            b.command.starts_with("write") && b.engine == "wait_beat" && b.beat == MID_BEATS
        })
        .map(|i| i + 1)
}

impl Golden {
    /// Runs the committed fixture under `root` and produces the golden file and the
    /// portable snapshot. Fails if the run fails §12.4 or does other work: a failing run
    /// is never blessed.
    pub fn generate(root: &Path) -> Result<(Golden, Vec<u8>), String> {
        let fixture = Fixture::read(root)?;
        let watch = Rc::new(RefCell::new(Vec::new()));
        let run = fixture.run(
            Start::Init { traced: true },
            vec![Box::new(BoundaryWatch(Rc::clone(&watch)))],
        );
        let record = Record::of(&fixture, &run).map_err(|e| format!("{PROGRAM}: {e}"))?;
        let boundaries = watch.take().into_iter().collect::<Result<Vec<_>, _>>()?;
        let k = mid_after(&boundaries).ok_or("the run never half buffers its WRITE")?;
        let snapshot = snapshot_after(&fixture, k)?;
        let golden = Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: m2ref::SEED,
            record,
            mid: Mid {
                program: PROGRAM.to_owned(),
                checkpoint: MID_CHECKPOINT.to_owned(),
                after_events: k as u64,
                seed: m2ref::SEED,
                compatibility_id: COMPATIBILITY_ID.to_owned(),
                image_hash: fixture.image.image_hash,
                media_image_hash: fixture.disk_blake3,
                size: snapshot.len() as u64,
                blake3: *blake3::hash(&snapshot).as_bytes(),
            },
        };
        Ok((golden, snapshot))
    }

    /// The file contents: stable JSON with a fixed field order, lowercase hex, LF line
    /// ends, and a trailing newline. Nothing host-specific: no path, time, or run id.
    pub fn render(&self) -> String {
        let r = &self.record;
        let m = &self.mid;
        let words = |ws: &[u32]| {
            ws.iter()
                .map(|&x| q(&word(x)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let interrupts: Vec<String> = r
            .interrupts
            .iter()
            .map(|&c| q(&format!("{c:#010x}")))
            .collect();
        let disk_ops: Vec<String> = r
            .disk_ops
            .iter()
            .map(|(op, lba)| format!("{{ \"op\": {}, \"lba\": {lba} }}", q(op)))
            .collect();
        let blocks: Vec<String> = r
            .disk_blocks
            .iter()
            .map(|(lba, h)| format!("{{ \"lba\": {lba}, \"blake3\": {} }}", q(&hex(h))))
            .collect();
        format!(
            "{{\n  \"scenario\": {},\n  \"compatibility_id\": {},\n  \"seed\": {},\n  \
             \"program\": {{\n    \"program\": {},\n    \"elf_blake3\": {},\n    \
             \"image_hash\": {},\n    \"disk_blake3\": {},\n    \"media_image_hash\": {},\n    \
             \"halt\": {},\n    \"trap_pc\": {},\n    \"tval\": {},\n    \"instret\": {},\n    \
             \"events\": {},\n    \"registers\": [{}],\n    \"csrs\": [{}],\n    \
             \"uart\": {},\n    \"uart_blake3\": {},\n    \"interrupts\": [{}],\n    \
             \"handler_entries\": {},\n    \"disk_ops\": [{}],\n    \"dma_beats\": {},\n    \
             \"disk_blocks\": [{}],\n    \"state\": {},\n    \"execution\": {},\n    \
             \"trace\": {}\n  }},\n  \"mid_snapshot\": {{\n    \"file\": {},\n    \
             \"program\": {},\n    \"checkpoint\": {},\n    \"after_events\": {},\n    \
             \"seed\": {},\n    \"compatibility_id\": {},\n    \"image_hash\": {},\n    \
             \"media_image_hash\": {},\n    \"size\": {},\n    \"blake3\": {}\n  }}\n}}\n",
            q(SCENARIO),
            q(&self.compatibility_id),
            self.seed,
            q(&r.program),
            q(&hex(&r.elf_blake3)),
            q(&hex(&r.image_hash)),
            q(&hex(&r.disk_blake3)),
            q(&hex(&r.media_image_hash)),
            q(&r.cause),
            q(&word(r.trap_pc)),
            q(&word(r.tval)),
            r.instret,
            r.events,
            words(&r.registers),
            words(&r.csrs),
            q(&hex(&r.uart)),
            q(&hex(blake3::hash(&r.uart).as_bytes())),
            interrupts.join(", "),
            r.entries,
            disk_ops.join(", "),
            r.dma_beats,
            blocks.join(", "),
            q(&hex(&r.state)),
            q(&hex(&r.execution)),
            q(&hex(&r.trace)),
            q(MID_FILE),
            q(&m.program),
            q(&m.checkpoint),
            m.after_events,
            m.seed,
            q(&m.compatibility_id),
            q(&hex(&m.image_hash)),
            q(&hex(&m.media_image_hash)),
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
        Ok(Golden {
            compatibility_id: s(&root["compatibility_id"])?,
            seed: u(&root["seed"])?,
            record: parse_record(&root["program"])?,
            mid: Mid {
                program: s(&m["program"])?,
                checkpoint: s(&m["checkpoint"])?,
                after_events: u(&m["after_events"])?,
                seed: u(&m["seed"])?,
                compatibility_id: s(&m["compatibility_id"])?,
                image_hash: digest(&m["image_hash"])?,
                media_image_hash: digest(&m["media_image_hash"])?,
                size: u(&m["size"])?,
                blake3: digest(&m["blake3"])?,
            },
        })
    }

    /// Fails unless the file describes today's scenario: the compatibility id, the seed,
    /// the program, and the portable snapshot's program and point.
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
        if self.seed != m2ref::SEED || self.mid.seed != m2ref::SEED {
            return Err(format!(
                "golden seeds {:#x} and {:#x} are not the m2-reference seed {:#x}",
                self.seed,
                self.mid.seed,
                m2ref::SEED
            ));
        }
        if self.record.program != PROGRAM {
            return Err(format!(
                "the golden program is {}, not {PROGRAM}",
                self.record.program
            ));
        }
        if self.mid.program != PROGRAM || self.mid.checkpoint != MID_CHECKPOINT {
            return Err(format!(
                "the portable snapshot is {:?} of {}, not {MID_CHECKPOINT:?} of {PROGRAM}",
                self.mid.checkpoint, self.mid.program
            ));
        }
        Ok(())
    }

    /// `actual` equals the golden record, field by field.
    pub fn check(&self, actual: &Record) -> Result<(), String> {
        self.ensure_current()?;
        let differ = differing_fields(&self.record, actual);
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

    /// Portability, as in M1-A6: `snapshot` has the golden size and BLAKE3, restores on a
    /// freshly built frozen platform, re-encodes to the same bytes, and runs to the golden
    /// end under the watchdog: event count, `StateDigest`, `ExecutionDigest`, registers,
    /// CSRs, the exact UART output, and the rest of the work, with the remaining 16 WRITE
    /// beats, the WRITE and the last READ, one more beat block, and the last two
    /// interrupts each happening once. The file has no trace prefix, so the full-run
    /// `TraceDigest` is checked by the checkpoint runs instead.
    pub fn check_portable(&self, fixture: &Fixture, snapshot: &[u8]) -> Result<(), String> {
        self.ensure_current()?;
        if fixture.image.image_hash != self.mid.image_hash
            || fixture.disk_blake3 != self.mid.media_image_hash
        {
            return Err(format!(
                "the portable snapshot belongs to another {} fixture",
                self.mid.program
            ));
        }
        if snapshot.len() as u64 != self.mid.size
            || *blake3::hash(snapshot).as_bytes() != self.mid.blake3
        {
            return Err("the portable snapshot's bytes differ from the golden".to_owned());
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
        let expected = &self.record;
        let finished = &resumed.finished.finished;
        let o = &finished.outcome;
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
        if registers(finished).ok() != Some(expected.registers) {
            differ.push("registers");
        }
        if csrs(finished).ok() != Some(expected.csrs) {
            differ.push("CSRs");
        }
        if resumed.output.as_ref().ok() != Some(&expected.uart) {
            differ.push("UART output");
        }
        if resumed.state.as_ref().ok().map(|s| s.entries) != Some(expected.entries) {
            differ.push("handler entries");
        }
        if block_irq::judge(&resumed).is_err() {
            differ.push("§12.4");
        }
        let rest = rest_of_work(&resumed.finished.finished.dispatched);
        if rest != MidRest::expected() {
            differ.push("work after the snapshot");
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

    /// `cargo xtask m2-golden verify`: regenerates everything under `root` on this
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
        let fixture = Fixture::read(root).map_err(|e| vec![e])?;
        if let Err(e) = committed.check_portable(&fixture, snapshot) {
            errors.push(e);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `cargo xtask m2-golden check`: the result another machine emitted, its `json` and
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
        let fixture = Fixture::read(root).map_err(|e| vec![e])?;
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

/// What happens after the portable snapshot, counted from the dispatched events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidRest {
    /// DMA beat requests the controller sends.
    pub beats: usize,
    /// `WriteBlock` messages the media receives.
    pub write_blocks: usize,
    /// `ReadBlock` messages the media receives.
    pub read_blocks: usize,
    /// `Level(true)` messages the CPU receives.
    pub levels_high: usize,
}

impl MidRest {
    /// The rest of the frozen run after [`MID_CHECKPOINT`]: WRITE beats 16 to 31 (beat
    /// 16's request is issued and still queued for the bus), the WRITE's `WriteBlock`, and
    /// transfer 3's `ReadBlock` and 32 beats; the last two completion interrupts.
    pub fn expected() -> MidRest {
        let beats = dma::BEATS_PER_BLOCK as usize;
        MidRest {
            beats: (beats - MID_BEATS as usize) + beats,
            write_blocks: 1,
            read_blocks: 1,
            levels_high: 2,
        }
    }
}

/// Counts [`MidRest`] in `events`.
pub fn rest_of_work(events: &[systemscope_runtime::runtime::Dispatched]) -> MidRest {
    use systemscope_contracts::component::Delivered;
    use systemscope_contracts::protocol::Message;
    use systemscope_contracts::protocol::block_v0::BlockMsg;
    use systemscope_contracts::protocol::irq_v0::IrqMsg;
    use systemscope_contracts::protocol::mem_v1::MemMsg;
    use systemscope_rv32::m2ref::{BLK, BUS, CPU, DISK, IRQC};
    let mut rest = MidRest {
        beats: 0,
        write_blocks: 0,
        read_blocks: 0,
        levels_high: 0,
    };
    for ev in events {
        let Delivered::Message { msg, .. } = &ev.delivery else {
            continue;
        };
        match ((ev.source, ev.target), msg) {
            ((BLK, BUS), Message::MemV1(MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. })) => {
                rest.beats += 1;
            }
            ((BLK, DISK), Message::Block(BlockMsg::WriteBlock { .. })) => rest.write_blocks += 1,
            ((BLK, DISK), Message::Block(BlockMsg::ReadBlock { .. })) => rest.read_blocks += 1,
            ((IRQC, CPU), Message::Irq(IrqMsg::Level { asserted: true })) => {
                rest.levels_high += 1;
            }
            _ => {}
        }
    }
    rest
}

/// Snapshots the frozen platform after `k` events of an untraced run from `init`.
pub fn snapshot_after(fixture: &Fixture, k: usize) -> Result<Vec<u8>, String> {
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    for i in 0..k {
        rt.step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{PROGRAM} ended after {i} events"))?;
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
    let list = |items: Vec<String>| items.join(",");
    vec![
        ("elf_blake3", hex(&r.elf_blake3)),
        ("image_hash", hex(&r.image_hash)),
        ("disk_blake3", hex(&r.disk_blake3)),
        ("media_image_hash", hex(&r.media_image_hash)),
        ("halt", r.cause.clone()),
        ("trap_pc", word(r.trap_pc)),
        ("tval", word(r.tval)),
        ("instret", r.instret.to_string()),
        ("events", r.events.to_string()),
        ("registers", list(r.registers.map(word).to_vec())),
        ("csrs", list(r.csrs.map(word).to_vec())),
        ("uart", hex(&r.uart)),
        (
            "interrupts",
            list(r.interrupts.iter().map(|c| format!("{c:#x}")).collect()),
        ),
        ("handler_entries", r.entries.to_string()),
        (
            "disk_ops",
            list(r.disk_ops.iter().map(|(o, l)| format!("{o}:{l}")).collect()),
        ),
        ("dma_beats", r.dma_beats.to_string()),
        (
            "disk_blocks",
            list(
                r.disk_blocks
                    .iter()
                    .map(|(l, h)| format!("{l}:{}", hex(h)))
                    .collect(),
            ),
        ),
        ("StateDigest", hex(&r.state)),
        ("ExecutionDigest", hex(&r.execution)),
        ("TraceDigest", hex(&r.trace)),
    ]
}

fn differing_fields(expected: &Record, actual: &Record) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = fields(expected)
        .into_iter()
        .zip(fields(actual))
        .filter(|(a, b)| a.1 != b.1)
        .map(|(a, _)| a.0)
        .collect();
    if expected.program != actual.program {
        out.insert(0, "program");
    }
    out
}

/// One line per difference between two golden files, for `cargo xtask m2-golden`.
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
    if old.record.program != new.record.program {
        out.push(format!(
            "~ program {} -> {}",
            old.record.program, new.record.program
        ));
    }
    for ((name, before), (_, after)) in fields(&old.record).into_iter().zip(fields(&new.record)) {
        if before != after {
            out.push(format!(
                "~ {}: {name} {} -> {}",
                new.record.program,
                short(&before),
                short(&after)
            ));
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

fn parse_record(v: &Value) -> Result<Record, String> {
    let program = s(&v["program"])?;
    let words = |name: &str| -> Result<Vec<u32>, String> {
        v[name]
            .as_array()
            .ok_or_else(|| format!("{program}: no {name}"))?
            .iter()
            .map(parse_word)
            .collect()
    };
    let registers: [u32; 32] = words("registers")?
        .try_into()
        .map_err(|r: Vec<u32>| format!("{program}: {} registers, not 32", r.len()))?;
    let csr_values: [u32; 8] = words("csrs")?
        .try_into()
        .map_err(|r: Vec<u32>| format!("{program}: {} CSRs, not {}", r.len(), CSRS.len()))?;
    let uart = bytes(&v["uart"])?;
    if *blake3::hash(&uart).as_bytes() != digest(&v["uart_blake3"])? {
        return Err(format!("{program}: uart_blake3 is not the BLAKE3 of uart"));
    }
    let array = |name: &str| {
        v[name]
            .as_array()
            .ok_or_else(|| format!("{program}: no {name}"))
    };
    let interrupts = array("interrupts")?
        .iter()
        .map(|c| {
            c.as_str()
                .filter(|t| t.len() == 10 && t.starts_with("0x"))
                .and_then(|t| u64::from_str_radix(&t[2..], 16).ok())
                .ok_or_else(|| format!("{c} is not an mcause"))
        })
        .collect::<Result<_, _>>()?;
    let disk_ops = array("disk_ops")?
        .iter()
        .map(|o| Ok((s(&o["op"])?, u(&o["lba"])?)))
        .collect::<Result<_, String>>()?;
    let disk_blocks = array("disk_blocks")?
        .iter()
        .map(|b| Ok((u(&b["lba"])?, digest(&b["blake3"])?)))
        .collect::<Result<_, String>>()?;
    Ok(Record {
        elf_blake3: digest(&v["elf_blake3"])?,
        image_hash: digest(&v["image_hash"])?,
        disk_blake3: digest(&v["disk_blake3"])?,
        media_image_hash: digest(&v["media_image_hash"])?,
        cause: s(&v["halt"])?,
        trap_pc: parse_word(&v["trap_pc"])?,
        tval: parse_word(&v["tval"])?,
        instret: u(&v["instret"])?,
        events: u(&v["events"])?,
        registers,
        csrs: csr_values,
        uart,
        interrupts,
        entries: u32::try_from(u(&v["handler_entries"])?).map_err(|e| e.to_string())?,
        disk_ops,
        dma_beats: u(&v["dma_beats"])?,
        disk_blocks,
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

    fn sample() -> Golden {
        Golden {
            compatibility_id: COMPATIBILITY_ID.to_owned(),
            seed: m2ref::SEED,
            record: Record {
                program: PROGRAM.to_owned(),
                elf_blake3: [1; 32],
                image_hash: [2; 32],
                disk_blake3: [3; 32],
                media_image_hash: [3; 32],
                cause: "EnvironmentCall".to_owned(),
                trap_pc: 0x8000_0100,
                tval: 0,
                instret: 2401,
                events: 22000,
                registers: std::array::from_fn(|i| i as u32 * 0x0101_0101),
                csrs: [0x88, 0x800, 0, 0x8000_0164, 0, 0x8000_014c, 0x8000_000b, 0],
                uart: b"M2 PASS\n".to_vec(),
                interrupts: vec![super::super::MEI_CAUSE; 3],
                entries: 3,
                disk_ops: vec![
                    ("read".to_owned(), 0),
                    ("write".to_owned(), 1),
                    ("read".to_owned(), 1),
                ],
                dma_beats: 96,
                disk_blocks: vec![(0, [4; 32]), (1, [5; 32])],
                state: [6; 32],
                execution: [7; 32],
                trace: [8; 32],
            },
            mid: Mid {
                program: PROGRAM.to_owned(),
                checkpoint: MID_CHECKPOINT.to_owned(),
                after_events: 9000,
                seed: m2ref::SEED,
                compatibility_id: COMPATIBILITY_ID.to_owned(),
                image_hash: [2; 32],
                media_image_hash: [3; 32],
                size: 12000,
                blake3: [9; 32],
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
        assert!(text.contains("\"uart\": \"4d3220504153530a\""));
        assert!(text.contains("\"interrupts\": [\"0x8000000b\", \"0x8000000b\", \"0x8000000b\"]"));
        assert!(text.contains("{ \"op\": \"write\", \"lba\": 1 }"));
        assert!(!text.contains("budget"), "the watchdog is not recorded");
    }

    #[test]
    fn records_are_checked_field_by_field() {
        let golden = sample();
        assert_eq!(golden.check(&golden.record), Ok(()));
        let base = golden.record.clone();
        let mut uart = base.clone();
        uart.uart[0] = b'N';
        let mut events = base.clone();
        events.events += 1;
        let mut csr = base.clone();
        csr.csrs[5] ^= 4;
        let mut irq = base.clone();
        irq.interrupts.push(super::super::MEI_CAUSE);
        let mut disk = base.clone();
        disk.disk_ops.swap(0, 1);
        let mut blocks = base.clone();
        blocks.disk_blocks[1].1[0] ^= 1;
        let mut media = base.clone();
        media.media_image_hash[0] ^= 1;
        for (name, r) in [
            ("uart", uart),
            ("events", events),
            ("csrs", csr),
            ("interrupts", irq),
            ("disk_ops", disk),
            ("disk_blocks", blocks),
            ("media_image_hash", media),
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
        g.mid.checkpoint = "elsewhere".to_owned();
        assert!(g.ensure_current().is_err());
        let text = sample().render();
        let tampered = text.replacen("\"uart\": \"4d32", "\"uart\": \"4e32", 1);
        assert!(
            Golden::parse(&tampered)
                .unwrap_err()
                .contains("uart_blake3")
        );
        assert!(Golden::parse(&text.replacen("m2-reference\"", "m1-reference\"", 1)).is_err());
        assert!(Golden::parse(&text.replacen("\"0x80000100\"", "\"0x80000100 \"", 1)).is_err());
    }

    #[test]
    fn changes_are_described_field_by_field() {
        let old = sample();
        assert_eq!(describe_changes(Some(&old), &old), Vec::<String>::new());
        assert_eq!(describe_changes(None, &old), ["+ new golden file"]);
        let mut new = old.clone();
        new.record.trace[0] ^= 1;
        new.mid.after_events += 1;
        let changes = describe_changes(Some(&old), &new);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(
            changes[0].starts_with("~ block_irq: TraceDigest "),
            "{changes:?}"
        );
        assert!(changes[1].starts_with("~ mid snapshot:"));
    }
}
