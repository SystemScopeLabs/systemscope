//! M1-A3: the `rv32ui` fixtures against Spike, retirement by retirement
//! (`docs/m1-design.md` §10.3).
//!
//! ```text
//! committed rv32ui-*.elf ──▶ Rv32iCpu on m1-reference ──rv32.commit trace──▶ [Retire] ─┐
//!                       └──▶ pinned Spike --log-commits ──commit log────────▶ [Retire] ─┴▶ equal
//! ```
//!
//! Both sides run the same committed bytes, and each side's evidence is read as it is
//! written: SystemScope's canonical `rv32.commit` records, and Spike's commit log. Neither
//! is re-interpreted with the other's decoder or execution code.
//!
//! **Normalized retirement** ([`Retire`]): `pc`, the raw instruction word, the register
//! write, and the memory access. A write to `x0` changes nothing and counts as no write
//! ([`reg_write`]); SystemScope already records it as `rd = 0`, and Spike leaves it out.
//!
//! **Boundary** ([`check_boundary`]): Spike's log is the whole compared stream. It starts
//! at the ELF entry and ends with the environment's `write_tohost` (§10.2): a word store
//! of `1` to the ELF's `tohost` symbol, then a word store of `0` to `tohost + 4`. Spike's
//! HTIF exits on it; nothing after it retires, because the `ECALL` that follows traps on
//! both sides. SystemScope's stream must have exactly as many records, and then halts on
//! that `ECALL` with the M1-A2 pass rule.
//!
//! **Spike's exit status is not a verdict.** Spike exits 0 on its instruction limit too,
//! so a run counts only with exit 0, no output, and the boundary above.

use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

use systemscope_contracts::observe::StateView;
use systemscope_contracts::trace::Value;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::{COMMIT_KIND, HALT_KIND, TRAP_KIND};

use crate::manifest::{Fixture, Manifest};
use crate::runner::{self, CPU, End, PASS_CAUSE, Start};
use crate::{FIXTURE_DIR, MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, SELECTED, hex};

/// The upstream Spike repository.
pub const SPIKE_REPO: &str = "https://github.com/riscv-software-src/riscv-isa-sim.git";
/// The pinned Spike commit.
pub const SPIKE_COMMIT: &str = "19609434bb3d83448eec8796e8f0367c868efbda";
/// The first line of the pinned Spike's `--help`.
pub const SPIKE_VERSION_LINE: &str = "Spike RISC-V ISA Simulator 1.1.1-dev";
/// Spike's license. Spike is fetched and built, never vendored or committed.
pub const SPIKE_LICENSE: &str = "BSD-3-Clause, LICENSE in the Spike repository";
/// The build script, relative to the workspace root.
pub const SPIKE_SCRIPT: &str = "tests/rv32/build-spike.sh";
/// Where `cargo xtask spike build` installs Spike, relative to the workspace root.
pub const SPIKE_DIR: &str = "target/spike";
/// The stamp the build script writes into its directory.
pub const SPIKE_STAMP: &str = "spike.stamp";
/// The ISA: RV32I and nothing else.
pub const SPIKE_ISA: &str = "rv32i";
/// The privilege modes: machine mode only, the least Spike runs an ELF with.
pub const SPIKE_PRIV: &str = "m";
/// Where `cargo xtask spike` writes Spike's commit logs, relative to the workspace root.
pub const SPIKE_LOGS: &str = "target/spike-logs";
/// The committed log of `simple` from the pinned Spike, relative to the workspace root.
/// Tests compare SystemScope against it without Spike; `cargo xtask spike verify`
/// requires the installed Spike to write it again, byte for byte.
pub const SIMPLE_LOG: &str = "tests/rv32/spike/rv32ui-simple.log";

/// The Spike command line for one fixture, after the executable. `--pcs` starts hart 0 at
/// the ELF entry: Spike sets the hart's `pc` directly, so its boot ROM at `0x1000` never
/// runs, and `--disable-dtb` keeps the device tree out of memory. The memory is the
/// `m1-reference` RAM; the instruction limit is its `max_instructions`.
pub fn spike_args(entry: u32, elf: &str, log: &str) -> Vec<String> {
    vec![
        format!("--isa={SPIKE_ISA}"),
        format!("--priv={SPIKE_PRIV}"),
        format!("--pcs=0:{entry:#x}"),
        format!("-m{RAM_BASE:#x}:{RAM_SIZE:#x}"),
        "--disable-dtb".to_owned(),
        "--log-commits".to_owned(),
        format!("--log={log}"),
        format!("--instructions={MAX_INSTRUCTIONS}"),
        elf.to_owned(),
    ]
}

/// A retired instruction's memory access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mem {
    /// No data access.
    None,
    /// A load from `addr`.
    Load {
        /// The effective address.
        addr: u32,
    },
    /// A store of `width` bytes of `value` to `addr`.
    Store {
        /// The effective address.
        addr: u32,
        /// 1, 2, or 4.
        width: u8,
        /// The bytes written, little-endian.
        value: u32,
    },
}

/// One retired instruction, normalized. Its retirement index is its position in the
/// stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retire {
    /// The instruction's address.
    pub pc: u32,
    /// The raw instruction word.
    pub insn: u32,
    /// The architectural register write, `(rd, value)`, never to `x0`.
    pub reg_write: Option<(u8, u32)>,
    /// The data access.
    pub mem: Mem,
}

impl fmt::Display for Retire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pc {:#010x} insn {:#010x}", self.pc, self.insn)?;
        match self.reg_write {
            Some((rd, value)) => write!(f, " x{rd} = {value:#010x}")?,
            None => write!(f, " no register write")?,
        }
        match self.mem {
            Mem::None => Ok(()),
            Mem::Load { addr } => write!(f, ", load {addr:#010x}"),
            Mem::Store { addr, width, value } => {
                write!(f, ", store {width} B {value:#x} to {addr:#010x}")
            }
        }
    }
}

/// The architectural register write for "`rd` gets `value`": none for `x0`, which never
/// changes.
pub fn reg_write(rd: u8, value: u32) -> Option<(u8, u32)> {
    (rd != 0).then_some((rd, value))
}

/// SystemScope's side: the retirements and how the CPU halted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemScopeStream {
    /// One per `rv32.commit` record, in order.
    pub retires: Vec<Retire>,
    /// The `rv32.trap` record that ended the run, as `(pc, cause)`, if any.
    pub trap: Option<(u32, String)>,
}

/// Reads SystemScope's retirements from a traced run's canonical records (§5.7). Only
/// `rv32.*` records are read; any other `rv32.*` kind, a field out of place, or a record
/// after the trap is an error.
pub fn from_trace(trace: &Trace) -> Result<SystemScopeStream, String> {
    let mut stream = SystemScopeStream {
        retires: Vec::new(),
        trap: None,
    };
    for (i, record) in trace.records.iter().enumerate() {
        if !record.kind.starts_with("rv32.") {
            continue;
        }
        let at = |why: &str| format!("trace record {i} ({}): {why}", record.kind);
        if stream.trap.is_some() {
            return Err(at("after the trap"));
        }
        let fields: Vec<&str> = record.fields.iter().map(|f| f.0).collect();
        let u32_at = |n: usize| match record.fields.get(n) {
            Some((_, Value::U64(v))) => u32::try_from(*v).map_err(|_| at("value above 32 bits")),
            _ => Err(at("expected a U64 field")),
        };
        match record.kind {
            k if k == COMMIT_KIND => {
                let mem = match fields.as_slice() {
                    ["pc", "insn", "rd", "rd_value", "next_pc"] => Mem::None,
                    ["pc", "insn", "rd", "rd_value", "next_pc", "addr"] => {
                        Mem::Load { addr: u32_at(5)? }
                    }
                    [
                        "pc",
                        "insn",
                        "rd",
                        "rd_value",
                        "next_pc",
                        "addr",
                        "width",
                        "value",
                    ] => {
                        let width = match u32_at(6)? {
                            w @ (1 | 2 | 4) => w as u8,
                            w => return Err(at(&format!("store width {w}"))),
                        };
                        Mem::Store {
                            addr: u32_at(5)?,
                            width,
                            value: u32_at(7)?,
                        }
                    }
                    other => return Err(at(&format!("unexpected fields {other:?}"))),
                };
                let rd = u8::try_from(u32_at(2)?)
                    .ok()
                    .filter(|rd| *rd < 32)
                    .ok_or_else(|| at("rd out of range"))?;
                stream.retires.push(Retire {
                    pc: u32_at(0)?,
                    insn: u32_at(1)?,
                    reg_write: reg_write(rd, u32_at(3)?),
                    mem,
                });
            }
            k if k == TRAP_KIND => {
                if fields != ["pc", "insn", "cause", "tval"] {
                    return Err(at(&format!("unexpected fields {fields:?}")));
                }
                let cause = match &record.fields[2].1 {
                    Value::Str(c) => c.clone(),
                    _ => return Err(at("cause is not a string")),
                };
                stream.trap = Some((u32_at(0)?, cause));
            }
            k if k == HALT_KIND => return Err(at("the instruction limit")),
            _ => return Err(at("unknown CPU record kind")),
        }
    }
    Ok(stream)
}

/// Parses a pinned-Spike commit log (`--log-commits`), strictly. Every line must be one
/// retirement of hart 0 in machine mode:
///
/// ```text
/// core   0: 3 <pc> (<insn>)[ x<rd> <value>][ mem <addr>[ <value>]]
/// ```
///
/// A `mem` access with a value is a store, whose width is the value's digit count (2, 4,
/// or 8); without one, it is a load. Anything else, including an empty line, is an error:
/// a format change must fail the parse, not be skipped.
pub fn parse_spike_log(log: &str) -> Result<Vec<Retire>, String> {
    log.lines()
        .enumerate()
        .map(|(i, line)| {
            parse_spike_line(line)
                .map_err(|why| format!("Spike log line {}: {why}: {line:?}", i + 1))
        })
        .collect()
}

fn parse_spike_line(line: &str) -> Result<Retire, String> {
    let rest = line
        .strip_prefix("core   0: 3 ")
        .ok_or("not a hart 0, machine-mode commit line")?;
    let mut tokens = rest.split(' ').filter(|t| !t.is_empty()).peekable();
    let pc = word(tokens.next().ok_or("no pc")?, 8)?;
    let insn = tokens
        .next()
        .and_then(|t| t.strip_prefix('(')?.strip_suffix(')'))
        .ok_or("no (instruction)")?;
    let insn = word(insn, 8)?;
    let mut reg = None;
    if let Some(n) = tokens.peek().and_then(|t| t.strip_prefix('x')) {
        let rd: u8 = n.parse().ok().filter(|rd| *rd < 32).ok_or("bad register")?;
        tokens.next();
        let value = word(tokens.next().ok_or("register without a value")?, 8)?;
        reg = Some((rd, value));
    }
    let mut mem = Mem::None;
    if tokens.peek() == Some(&"mem") {
        tokens.next();
        let addr = word(tokens.next().ok_or("mem without an address")?, 8)?;
        mem = match tokens.next() {
            None => Mem::Load { addr },
            Some(v) => {
                let digits = v.strip_prefix("0x").map_or(0, str::len);
                let width = match digits {
                    2 => 1,
                    4 => 2,
                    8 => 4,
                    _ => return Err(format!("store value {v} is not 1, 2, or 4 bytes")),
                };
                Mem::Store {
                    addr,
                    width,
                    value: word(v, digits)?,
                }
            }
        };
    }
    if let Some(extra) = tokens.next() {
        return Err(format!("unexpected {extra:?}"));
    }
    // The tokens say what the line means; the exact spacing must be Spike's too, so a
    // change in the log's layout fails here rather than parsing by luck.
    let mut canonical = format!("core   0: 3 {pc:#010x} ({insn:#010x})");
    if let Some((rd, value)) = reg {
        canonical.push_str(&format!(" {:<3} {value:#010x}", format!("x{rd}")));
    }
    match mem {
        Mem::None => {}
        Mem::Load { addr } => canonical.push_str(&format!(" mem {addr:#010x}")),
        Mem::Store { addr, width, value } => canonical.push_str(&format!(
            " mem {addr:#010x} 0x{value:0digits$x}",
            digits = usize::from(width) * 2
        )),
    }
    if line != canonical {
        return Err(format!("not laid out as Spike writes it: {canonical:?}"));
    }
    Ok(Retire {
        pc,
        insn,
        reg_write: reg.and_then(|(rd, value)| reg_write(rd, value)),
        mem,
    })
}

/// `0x` and exactly `digits` lowercase hex digits.
fn word(token: &str, digits: usize) -> Result<u32, String> {
    token
        .strip_prefix("0x")
        .filter(|h| h.len() == digits && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .ok_or_else(|| format!("{token:?} is not 0x and {digits} hex digits"))
}

/// The agreed end of the compared stream: Spike's log starts at `entry` and ends with
/// `write_tohost` for a pass, a word store of `1` to `tohost`, one instruction, then a
/// word store of `0` to `tohost + 4`. Returns the record count.
pub fn check_boundary(spike: &[Retire], entry: u32, tohost: u32) -> Result<usize, String> {
    let n = spike.len();
    let first = spike.first().ok_or("Spike retired nothing")?;
    if first.pc != entry {
        return Err(format!(
            "Spike's first retirement is at {:#010x}, not the ELF entry {entry:#010x}",
            first.pc
        ));
    }
    let store = |i: usize, addr: u32, value: u32| {
        n >= 3
            && spike[i].mem
                == Mem::Store {
                    addr,
                    width: 4,
                    value,
                }
    };
    if !(store(n.wrapping_sub(3), tohost, 1) && store(n - 1, tohost.wrapping_add(4), 0)) {
        let tail: Vec<String> = spike[n.saturating_sub(3)..]
            .iter()
            .map(ToString::to_string)
            .collect();
        return Err(format!(
            "Spike's log does not end with the passing write_tohost (1 to {tohost:#010x}, then \
             0 to {:#010x}); it ends with: {}",
            tohost.wrapping_add(4),
            tail.join("; ")
        ));
    }
    Ok(n)
}

/// Compares the two streams record by record, over their whole length. The first
/// difference, or the first record only one side has, is an error showing that index,
/// both records, and up to three records before and after it.
pub fn compare(systemscope: &[Retire], spike: &[Retire]) -> Result<(), String> {
    let n = systemscope.len().max(spike.len());
    let Some(i) = (0..n).find(|&i| systemscope.get(i) != spike.get(i)) else {
        return Ok(());
    };
    let show = |r: Option<&Retire>| {
        r.map_or(
            "(none: the stream has ended)".to_owned(),
            ToString::to_string,
        )
    };
    let why = match (systemscope.get(i), spike.get(i)) {
        (Some(_), None) => "Spike's stream ends first",
        (None, Some(_)) => "SystemScope's stream ends first",
        _ => "the records differ",
    };
    let mut msg = format!(
        "retirement {i} of {} (SystemScope) and {} (Spike): {why}\n  SystemScope: {}\n  Spike:       {}",
        systemscope.len(),
        spike.len(),
        show(systemscope.get(i)),
        show(spike.get(i)),
    );
    for (label, range) in [
        ("before", i.saturating_sub(3)..i),
        ("after", i + 1..(i + 4).min(n)),
    ] {
        for j in range {
            msg.push_str(&format!(
                "\n  {label} [{j}]: SystemScope {} | Spike {}",
                show(systemscope.get(j)),
                show(spike.get(j))
            ));
        }
    }
    Err(msg)
}

/// The registers after `stream`, from all-zero registers: every register write, replayed.
pub fn replay(stream: &[Retire]) -> [u32; 32] {
    let mut regs = [0; 32];
    for (rd, value) in stream.iter().filter_map(|r| r.reg_write) {
        regs[usize::from(rd)] = value;
    }
    regs
}

/// The value of symbol `name` in a little-endian ELF32 file's symbol table, as Spike's
/// HTIF finds `tohost`.
pub fn elf_symbol(elf: &[u8], name: &str) -> Option<u32> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(elf.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(elf.get(at..at + 4)?.try_into().ok()?));
    if elf.get(..6)? != b"\x7fELF\x01\x01" {
        return None;
    }
    let shoff = u32_at(0x20)? as usize;
    let shentsize = usize::from(u16_at(0x2e)?);
    let shnum = usize::from(u16_at(0x30)?);
    let section = |i: usize| shoff + i * shentsize;
    for i in 0..shnum {
        let sh = section(i);
        if u32_at(sh + 4)? != 2 {
            continue; // not SHT_SYMTAB
        }
        let (offset, size, link) = (
            u32_at(sh + 16)? as usize,
            u32_at(sh + 20)? as usize,
            u32_at(sh + 24)? as usize,
        );
        let strtab = u32_at(section(link) + 16)? as usize;
        for sym in (offset..offset + size).step_by(16) {
            let start = strtab + u32_at(sym)? as usize;
            let end = start + elf.get(start..)?.iter().position(|&b| b == 0)?;
            if &elf[start..end] == name.as_bytes() {
                return u32_at(sym + 4);
            }
        }
    }
    None
}

/// SystemScope's final `x0`..`x31` from the CPU's view.
fn registers(view: &StateView) -> Result<[u32; 32], String> {
    let mut regs = [0; 32];
    for (i, reg) in regs.iter_mut().enumerate().skip(1) {
        *reg = match view.get(&format!("x{i}")) {
            Some(Value::U64(v)) => u32::try_from(*v).map_err(|_| format!("x{i} above 32 bits"))?,
            _ => return Err(format!("the CPU view has no x{i}")),
        };
    }
    Ok(regs)
}

/// One fixture's differential result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    /// The test name.
    pub name: String,
    /// The ELF's BLAKE3.
    pub blake3: [u8; 32],
    /// Whether SystemScope ran it, and whether Spike did.
    pub executed: (bool, bool),
    /// Retirements compared, or why the fixture failed.
    pub verdict: Result<usize, String>,
}

/// Runs `fixture` on SystemScope and, through `spike` (a Spike executable), on Spike,
/// writing Spike's log to `log`, and compares them. Paths are relative to `root`, which
/// both run in.
pub fn differential(root: &Path, fixture: &Fixture, spike: &Path, log: &str) -> Diff {
    let mut diff = Diff {
        name: fixture.name.clone(),
        blake3: fixture.blake3,
        executed: (false, false),
        verdict: Ok(0),
    };
    diff.verdict = run_both(root, fixture, spike, log, &mut diff.executed);
    diff
}

fn run_both(
    root: &Path,
    fixture: &Fixture,
    spike: &Path,
    log: &str,
    executed: &mut (bool, bool),
) -> Result<usize, String> {
    // The committed bytes, checked against the manifest, for both sides.
    let image = fixture.read(root)?;
    let elf = format!("{FIXTURE_DIR}/{}", fixture.elf);
    let bytes = fs::read(root.join(&elf)).map_err(|e| format!("{elf}: {e}"))?;
    let tohost = elf_symbol(&bytes, "tohost").ok_or(format!("{elf} has no tohost symbol"))?;

    // SystemScope: the integrated CPU, traced. Both sides run before either is judged,
    // so a divergence is reported where it starts.
    let finished = runner::execute(
        runner::platform(&image, false),
        Start::Init { traced: true },
        Vec::new(),
    );
    executed.0 = true;
    let ours = finished
        .trace
        .as_ref()
        .ok_or_else(|| "SystemScope recorded no trace".to_owned())
        .and_then(from_trace);

    // Spike.
    if let Some(dir) = Path::new(log).parent() {
        fs::create_dir_all(root.join(dir)).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let _ = fs::remove_file(root.join(log));
    let out = Command::new(spike)
        .args(spike_args(image.entry, &elf, log))
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", spike.display()))?;
    executed.1 = true;
    if !out.status.success() || !out.stdout.is_empty() || !out.stderr.is_empty() {
        return Err(format!(
            "Spike: {}, stdout {:?}, stderr {:?}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = fs::read_to_string(root.join(log)).map_err(|e| format!("{log}: {e}"))?;
    let theirs = parse_spike_log(&text)?;

    // The streams, then where Spike's ends, then SystemScope's own pass rule (M1-A2).
    let ours = ours?;
    compare(&ours.retires, &theirs)?;
    let n = check_boundary(&theirs, image.entry, tohost)?;
    runner::judge(&finished.outcome).map_err(|e| format!("SystemScope: {e}"))?;
    // SystemScope then halts on the ECALL right after the boundary, with the same
    // registers Spike's writes leave.
    let last = theirs[n - 1].pc;
    match (&ours.trap, &finished.outcome.end) {
        (Some((pc, cause)), End::Trap { .. })
            if *pc == last.wrapping_add(4) && cause == PASS_CAUSE => {}
        other => {
            return Err(format!(
                "SystemScope does not halt on the ECALL after {last:#010x}: {other:?}"
            ));
        }
    }
    if finished.outcome.instret != n as u64 {
        return Err(format!(
            "SystemScope retired {}, Spike {n}",
            finished.outcome.instret
        ));
    }
    let view = finished.views.get(CPU.0 as usize).ok_or("no CPU view")?;
    let (ours, theirs) = (registers(view)?, replay(&theirs));
    if ours != theirs {
        let differ: Vec<String> = (0..32)
            .filter(|&i| ours[i] != theirs[i])
            .map(|i| {
                format!(
                    "x{i}: SystemScope {:#010x}, Spike {:#010x}",
                    ours[i], theirs[i]
                )
            })
            .collect();
        return Err(format!(
            "registers at the boundary differ: {}",
            differ.join(", ")
        ));
    }
    Ok(n)
}

/// The whole differential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffReport {
    /// Tests the manifest selects.
    pub selected: usize,
    /// One result per selected test, in manifest order.
    pub results: Vec<Diff>,
}

impl DiffReport {
    /// Tests SystemScope ran.
    pub fn systemscope_executed(&self) -> usize {
        self.results.iter().filter(|d| d.executed.0).count()
    }

    /// Tests Spike ran.
    pub fn spike_executed(&self) -> usize {
        self.results.iter().filter(|d| d.executed.1).count()
    }

    /// Tests whose streams matched.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|d| d.verdict.is_ok()).count()
    }

    /// Retirements compared over every passing test.
    pub fn compared(&self) -> usize {
        self.results
            .iter()
            .filter_map(|d| d.verdict.as_ref().ok())
            .sum()
    }

    /// M1-A3: selected, run by SystemScope, run by Spike, and passed all equal `expected`,
    /// with no mismatch.
    pub fn accept(&self, expected: usize) -> Result<(), String> {
        let counts = (
            self.selected,
            self.systemscope_executed(),
            self.spike_executed(),
            self.passed(),
        );
        if counts == (expected, expected, expected, expected) && self.results.len() == expected {
            return Ok(());
        }
        let mut msg = format!(
            "expected {expected} selected, run by SystemScope and Spike, and matched; got \
             selected {}, SystemScope {}, Spike {}, matched {}, mismatched {}",
            counts.0,
            counts.1,
            counts.2,
            counts.3,
            self.results.len() - counts.3
        );
        for d in self.results.iter().filter(|d| d.verdict.is_err()) {
            msg.push_str(&format!("\n{}", d.line()));
        }
        Err(msg)
    }
}

impl Diff {
    /// The verdict, with everything needed to reproduce a failure.
    pub fn line(&self) -> String {
        match &self.verdict {
            Ok(n) => format!("{:<6} PASS: {n} retirements equal", self.name),
            Err(why) => format!(
                "{:<6} FAIL (elf {}):\n  {}",
                self.name,
                hex(&self.blake3),
                why.replace('\n', "\n  ")
            ),
        }
    }
}

/// Checks a Spike installed by the build script in `dir`, run as `spike`: the stamp, the
/// source checkout at the pin, and the version line; then runs `simple` through the
/// differential and requires its log to equal the committed [`SIMPLE_LOG`]. Returns what
/// it checked, or every failure.
pub fn verify_install(root: &Path, dir: &Path, spike: &Path) -> Result<Vec<String>, Vec<String>> {
    let mut checked = Vec::new();
    let mut errors = Vec::new();

    let stamp = dir.join(SPIKE_STAMP);
    let expected = format!(
        "commit {SPIKE_COMMIT}
version {SPIKE_VERSION_LINE}
"
    );
    match fs::read_to_string(&stamp) {
        Ok(text) if text == expected => {
            checked.push(format!("{}: {SPIKE_COMMIT}", stamp.display()))
        }
        Ok(text) => errors.push(format!("{}: {text:?}, not {expected:?}", stamp.display())),
        Err(e) => errors.push(format!("{}: {e}", stamp.display())),
    }

    let src = dir.join("src");
    // The checkout is compared byte for byte, whatever the user's line-ending setting.
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .output()
            .map_err(|e| e.to_string())
            .and_then(|o| {
                if o.status.success() {
                    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
                } else {
                    Err(String::from_utf8_lossy(&o.stderr).trim().to_owned())
                }
            })
    };
    match (git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"])) {
        (Ok(head), Ok(status)) if head.trim() == SPIKE_COMMIT && status.is_empty() => {
            checked.push(format!(
                "{}: a clean checkout of {SPIKE_COMMIT}",
                src.display()
            ));
        }
        (Ok(head), Ok(status)) => errors.push(format!(
            "{}: HEAD {}, status {status:?}; expected a clean checkout of {SPIKE_COMMIT}",
            src.display(),
            head.trim()
        )),
        (Err(e), _) | (_, Err(e)) => errors.push(format!("{}: git: {e}", src.display())),
    }

    match fs::read(dir.join("bin/spike")) {
        Ok(bytes) => checked.push(format!(
            "{}: BLAKE3 {}",
            dir.join("bin/spike").display(),
            blake3::hash(&bytes).to_hex()
        )),
        Err(e) => errors.push(format!("{}: {e}", dir.join("bin/spike").display())),
    }

    match Command::new(spike).arg("--help").output() {
        Ok(out) => {
            let mut text = out.stdout;
            text.extend(out.stderr);
            let text = String::from_utf8_lossy(&text);
            match text.lines().next() {
                Some(line) if line == SPIKE_VERSION_LINE => {
                    checked.push(format!("{}: {line}", spike.display()));
                }
                line => errors.push(format!(
                    "{} --help starts with {line:?}, not {SPIKE_VERSION_LINE:?}",
                    spike.display()
                )),
            }
        }
        Err(e) => errors.push(format!("cannot run {}: {e}", spike.display())),
    }

    // The smoke run: the pinned Spike's log of `simple` is the committed one.
    if errors.is_empty() {
        let log = format!("{SPIKE_LOGS}/simple.log");
        let smoke = Manifest::read(root).and_then(|m| {
            let fixture = m
                .selected
                .iter()
                .find(|f| f.name == "simple")
                .ok_or("the manifest does not select simple")?;
            differential(root, fixture, spike, &log).verdict?;
            let (ours, committed) = (fs::read(root.join(&log)), fs::read(root.join(SIMPLE_LOG)));
            match (ours, committed) {
                (Ok(a), Ok(b)) if a == b => Ok(()),
                (Ok(_), Ok(_)) => Err(format!("{log} differs from {SIMPLE_LOG}")),
                (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
            }
        });
        match smoke {
            Ok(()) => checked.push(format!(
                "simple matches, and its Spike log equals {SIMPLE_LOG}"
            )),
            Err(e) => errors.push(format!("the smoke run of simple: {e}")),
        }
    }

    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors)
    }
}

/// Runs the differential for every test the committed manifest under `root` selects,
/// after checking the selection, with Spike's logs under `logs` (relative to `root`).
pub fn run_differential(root: &Path, spike: &Path, logs: &str) -> Result<DiffReport, String> {
    let manifest = Manifest::read(root)?;
    manifest.ensure_selection()?;
    if manifest.selected.len() != SELECTED.len() {
        return Err(format!(
            "the manifest selects {} tests",
            manifest.selected.len()
        ));
    }
    let results = manifest
        .selected
        .iter()
        .map(|f| differential(root, f, spike, &format!("{logs}/{}.log", f.name)))
        .collect();
    Ok(DiffReport {
        selected: manifest.selected.len(),
        results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_root;

    const SIMPLE_LOG: &str = include_str!("../spike/rv32ui-simple.log");

    #[test]
    fn the_committed_simple_log_is_where_verify_looks() {
        let committed = fs::read_to_string(workspace_root().join(super::SIMPLE_LOG)).unwrap();
        assert_eq!(committed, SIMPLE_LOG);
    }
    const EXCERPTS: &str = include_str!("../spike/excerpts.log");

    fn retire(pc: u32, insn: u32, reg_write: Option<(u8, u32)>, mem: Mem) -> Retire {
        Retire {
            pc,
            insn,
            reg_write,
            mem,
        }
    }

    #[test]
    fn the_build_script_pins_the_same_spike() {
        let script = include_str!("../build-spike.sh");
        assert!(script.contains(&format!("SPIKE_REPO={SPIKE_REPO}\n")));
        assert!(script.contains(&format!("SPIKE_COMMIT={SPIKE_COMMIT}\n")));
        assert_eq!(SPIKE_COMMIT.len(), 40);
    }

    #[test]
    fn the_command_line_bypasses_the_boot_rom_and_matches_the_reference_platform() {
        let args = spike_args(0x8000_0000, "a.elf", "a.log");
        assert_eq!(
            args,
            [
                "--isa=rv32i",
                "--priv=m",
                "--pcs=0:0x80000000",
                "-m0x80000000:0x1000000",
                "--disable-dtb",
                "--log-commits",
                "--log=a.log",
                "--instructions=10000000",
                "a.elf",
            ]
        );
    }

    /// Real lines from the pinned Spike, one per kind of retirement.
    #[test]
    fn each_kind_of_spike_line_parses() {
        let r = parse_spike_log(EXCERPTS).unwrap();
        let expected = [
            // addi x14, x0, 0: an ALU write.
            retire(0x8000_0034, 0x0000_0713, Some((14, 0)), Mem::None),
            // addi x0, x0, 0 and lui x0: writes to x0, which Spike leaves out.
            retire(0x8000_029c, 0x0000_0013, None, Mem::None),
            retire(0x8000_00d0, 0x8000_0037, None, Mem::None),
            // beq: no write.
            retire(0x8000_008c, 0x0020_8663, None, Mem::None),
            // jal x4 and jal x0.
            retire(0x8000_0088, 0x0100_026f, Some((4, 0x8000_008c)), Mem::None),
            retire(0x8000_00ac, 0x0140_006f, None, Mem::None),
            // jalr x0.
            retire(0x8000_014c, 0xffc3_0067, None, Mem::None),
            // lb, lh, lw.
            retire(
                0x8000_0090,
                0x0001_0703,
                Some((14, 0xffff_ffff)),
                Mem::Load { addr: 0x8000_2000 },
            ),
            retire(
                0x8000_0090,
                0x0001_1703,
                Some((14, 0xff)),
                Mem::Load { addr: 0x8000_2000 },
            ),
            retire(
                0x8000_0094,
                0x0001_2703,
                Some((14, 0x00ff_00ff)),
                Mem::Load { addr: 0x8000_2000 },
            ),
            // sb, sh, sw: the width is the value's digit count.
            retire(
                0x8000_0098,
                0x0011_0023,
                None,
                Mem::Store {
                    addr: 0x8000_2000,
                    width: 1,
                    value: 0xaa,
                },
            ),
            retire(
                0x8000_0098,
                0x0011_1023,
                None,
                Mem::Store {
                    addr: 0x8000_2000,
                    width: 2,
                    value: 0xaa,
                },
            ),
            retire(
                0x8000_009c,
                0x0011_2023,
                None,
                Mem::Store {
                    addr: 0x8000_2000,
                    width: 4,
                    value: 0x00aa_00aa,
                },
            ),
            // A one-digit register, padded to two columns.
            retire(0x8000_0000, 0x0000_0093, Some((1, 0)), Mem::None),
            // jalr x5: a link.
            retire(0x8000_0090, 0x0003_02e7, Some((5, 0x8000_0094)), Mem::None),
        ];
        assert_eq!(r, expected);
    }

    #[test]
    fn a_write_to_x0_is_no_write_on_both_sides() {
        assert_eq!(reg_write(0, 0), None);
        assert_eq!(reg_write(0, 5), None);
        assert_eq!(reg_write(1, 0), Some((1, 0)));
        // Spike leaves x0 out, but a line that showed it would mean the same.
        let shown = parse_spike_log("core   0: 3 0x8000029c (0x00000013) x0  0x00000000").unwrap();
        let hidden = parse_spike_log("core   0: 3 0x8000029c (0x00000013)").unwrap();
        assert_eq!(shown, hidden);
        assert_eq!(shown[0].reg_write, None);
    }

    #[test]
    fn unexpected_lines_are_errors() {
        for line in [
            "core   1: 3 0x80000000 (0x00000093) x1  0x00000000",
            "core   0: 1 0x80000000 (0x00000093) x1  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1  0x0000000",
            "core   0: 3 0x80000000 (0x00000093) x32 0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x01 0x00000000",
            "core   0: 3 0x80000000 (0x0093) x1  0x00000000",
            "core   0: 3 0x0000000080000000 (0x00000093) x1  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1  0x00000000 f1  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1",
            "core   0: 3 0x80000098 (0x00110023) mem 0x80002000 0xaaa",
            "core   0: 3 0x80000098 (0x00110023) mem",
            "core   0: 3 0x80000098 (0x00110023) mem 0x80002000 0xaa 0xbb",
            "core   0: 3 0x80000000 (0x00000093) X1  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1  0x0000000A",
            "core   0: 3 0x80000000 (0x00000093) x1 0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1   0x00000000",
            "core   0: 3  0x80000000 (0x00000093) x1  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x14  0x00000000",
            "core   0: 3 0x80000000 (0x00000093) x1  0x00000000 ",
            "core   0: 3 0x80000098 (0x00110023) mem  0x80002000 0xaa",
            "core   0: exception trap_illegal_instruction, epc 0x80000000",
            "*** FAILED *** (tohost = 3)",
        ] {
            assert!(parse_spike_log(line).is_err(), "{line:?}");
        }
        // One bad line fails the whole log.
        let log = format!("{SIMPLE_LOG}garbage\n");
        let err = parse_spike_log(&log).unwrap_err();
        assert!(err.contains("line 41"), "{err}");
        // So does a blank one; an empty log parses, and the boundary rejects it.
        let err = parse_spike_log(&format!(
            "{SIMPLE_LOG}
"
        ))
        .unwrap_err();
        assert!(err.contains("line 41"), "{err}");
        assert_eq!(parse_spike_log(""), Ok(Vec::new()));
    }

    fn simple() -> (Vec<Retire>, u32) {
        let root = workspace_root();
        let bytes = fs::read(root.join(FIXTURE_DIR).join("rv32ui-simple.elf")).unwrap();
        (
            parse_spike_log(SIMPLE_LOG).unwrap(),
            elf_symbol(&bytes, "tohost").unwrap(),
        )
    }

    #[test]
    fn the_tohost_symbol_is_read_from_the_elf() {
        let (_, tohost) = simple();
        assert_eq!(tohost, 0x8000_1000);
        assert_eq!(elf_symbol(b"not an elf", "tohost"), None);
        let bytes = fs::read(workspace_root().join(FIXTURE_DIR).join("rv32ui-add.elf")).unwrap();
        assert_eq!(elf_symbol(&bytes, "no_such_symbol"), None);
    }

    #[test]
    fn a_whole_spike_run_ends_at_the_passing_write_tohost() {
        let (spike, tohost) = simple();
        assert_eq!(check_boundary(&spike, RAM_BASE, tohost), Ok(40));
        // The first retirement is the ELF entry's instruction: no boot ROM.
        assert_eq!(spike[0].pc, RAM_BASE);
        assert!(spike.iter().all(|r| r.pc >= RAM_BASE));
    }

    #[test]
    fn a_wrong_start_or_end_is_not_the_boundary() {
        let (spike, tohost) = simple();
        // A boot ROM before the entry.
        let mut rom = vec![retire(0x1000, 0x0000_0297, Some((5, 0x1000)), Mem::None)];
        rom.extend(&spike);
        assert!(check_boundary(&rom, RAM_BASE, tohost).is_err());
        // Cut short, as by the instruction limit, or a trap before the end.
        assert!(check_boundary(&spike[..39], RAM_BASE, tohost).is_err());
        assert!(check_boundary(&spike[..20], RAM_BASE, tohost).is_err());
        assert!(check_boundary(&[], RAM_BASE, tohost).is_err());
        // RVTEST_FAIL writes gp = (TESTNUM << 1) | 1 to tohost.
        let mut fail = spike.clone();
        fail[37].mem = Mem::Store {
            addr: tohost,
            width: 4,
            value: 5,
        };
        let err = check_boundary(&fail, RAM_BASE, tohost).unwrap_err();
        assert!(err.contains("write_tohost"), "{err}");
        // Another tohost.
        assert!(check_boundary(&spike, RAM_BASE, tohost + 8).is_err());
    }

    /// The committed Spike log of `simple` and SystemScope's own run of the same ELF are
    /// equal, with no Spike needed: the differential path on one fixture.
    #[test]
    fn simple_matches_its_committed_spike_log() {
        let root = workspace_root();
        let manifest = Manifest::read(&root).unwrap();
        let fixture = manifest
            .selected
            .iter()
            .find(|f| f.name == "simple")
            .unwrap();
        let image = fixture.read(&root).unwrap();
        let finished = runner::execute(
            runner::platform(&image, false),
            Start::Init { traced: true },
            Vec::new(),
        );
        runner::judge(&finished.outcome).unwrap();
        let ours = from_trace(finished.trace.as_ref().unwrap()).unwrap();
        let (spike, _) = simple();
        assert_eq!(compare(&ours.retires, &spike), Ok(()));
        assert_eq!(ours.trap, Some((spike[39].pc + 4, PASS_CAUSE.to_owned())));
        assert_eq!(
            registers(&finished.views[CPU.0 as usize]).unwrap(),
            replay(&spike)
        );
    }

    #[test]
    fn every_difference_is_a_mismatch() {
        let (spike, _) = simple();
        assert_eq!(compare(&spike, &spike), Ok(()));
        let mut store = spike.clone();
        type Doctor = Box<dyn Fn(&mut Vec<Retire>)>;
        let cases: Vec<(&str, Doctor)> = vec![
            ("pc", Box::new(|s| s[10].pc += 4)),
            ("insn", Box::new(|s| s[10].insn ^= 1 << 20)),
            ("rd", Box::new(|s| s[10].reg_write = Some((12, 0)))),
            ("rd value", Box::new(|s| s[10].reg_write = Some((11, 1)))),
            ("a write dropped", Box::new(|s| s[10].reg_write = None)),
            (
                "record deleted",
                Box::new(|s| {
                    s.remove(10);
                }),
            ),
            ("record added", Box::new(|s| s.insert(10, s[10]))),
            ("extra record at the end", Box::new(|s| s.push(s[39]))),
            (
                "last record missing",
                Box::new(|s| {
                    s.pop();
                }),
            ),
            (
                "store value",
                Box::new(|s| {
                    if let Mem::Store { value, .. } = &mut s[37].mem {
                        *value = 3;
                    }
                }),
            ),
            (
                "store width",
                Box::new(|s| {
                    if let Mem::Store { width, .. } = &mut s[37].mem {
                        *width = 2;
                    }
                }),
            ),
            (
                "store address",
                Box::new(|s| {
                    if let Mem::Store { addr, .. } = &mut s[37].mem {
                        *addr += 8;
                    }
                }),
            ),
            (
                "store becomes a load",
                Box::new(|s| s[37].mem = Mem::Load { addr: 0x8000_1000 }),
            ),
            (
                "x0 write kept as a write",
                Box::new(|s| s[0].reg_write = Some((0, 0))),
            ),
        ];
        assert_eq!(
            spike[10].reg_write,
            Some((11, 0)),
            "the record the cases change"
        );
        for (what, doctor) in &cases {
            store.clone_from(&spike);
            doctor(&mut store);
            assert_ne!(store, spike, "{what}: the doctoring changed nothing");
            let err = compare(&store, &spike).unwrap_err();
            let err2 = compare(&spike, &store).unwrap_err();
            assert!(
                err.starts_with("retirement ") && err2.starts_with("retirement "),
                "{what}"
            );
        }
    }

    #[test]
    fn a_mismatch_shows_the_index_both_records_and_their_neighbours() {
        let (spike, _) = simple();
        let mut ours = spike.clone();
        ours[10].reg_write = Some((11, 7));
        let err = compare(&ours, &spike).unwrap_err();
        assert!(
            err.starts_with("retirement 10 of 40 (SystemScope) and 40 (Spike): the records differ"),
            "{err}"
        );
        assert!(
            err.contains("SystemScope: pc 0x80000028 insn 0x00000593 x11 = 0x00000007"),
            "{err}"
        );
        assert!(
            err.contains("Spike:       pc 0x80000028 insn 0x00000593 x11 = 0x00000000"),
            "{err}"
        );
        for j in [7, 8, 9, 11, 12, 13] {
            assert!(err.contains(&format!("[{j}]")), "{j}: {err}");
        }
        let short = compare(&spike[..30], &spike).unwrap_err();
        assert!(short.contains("SystemScope's stream ends first"), "{short}");
        let long = compare(&spike, &spike[..30]).unwrap_err();
        assert!(long.contains("Spike's stream ends first"), "{long}");
    }

    #[test]
    fn the_report_needs_all_forty_run_and_matched() {
        let diff = |i: usize, executed, verdict| Diff {
            name: format!("t{i}"),
            blake3: [0; 32],
            executed,
            verdict,
        };
        let report = |results: Vec<Diff>, selected| DiffReport { selected, results };
        let all = |n| {
            (0..n)
                .map(|i| diff(i, (true, true), Ok(10)))
                .collect::<Vec<_>>()
        };
        assert_eq!(report(all(40), 40).accept(40), Ok(()));
        assert_eq!(report(all(40), 40).compared(), 400);
        assert!(report(all(39), 39).accept(40).is_err());
        assert!(report(all(39), 40).accept(40).is_err());
        let mut one_failed = all(40);
        one_failed[3].verdict = Err("differs".to_owned());
        let err = report(one_failed, 40).accept(40).unwrap_err();
        assert!(err.contains("mismatched 1") && err.contains("t3"), "{err}");
        let mut spike_missing = all(40);
        spike_missing[5].executed.1 = false;
        assert!(report(spike_missing, 40).accept(40).is_err());
    }
}
