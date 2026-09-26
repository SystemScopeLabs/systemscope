//! M1-A3: the `rv32ui` fixtures, and the [`progen`] programs, against Spike, retirement by
//! retirement (`docs/m1-design.md` §10.3).
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
//!
//! **Generated programs** end like the fixtures and are judged the same way
//! ([`judge_pass`]). **Trap programs** end in a misaligned access instead: Spike runs them
//! with `-l` ([`spike_trap_args`]), whose log also names the trap, and [`judge_trap`]
//! requires both sides to stop at the same instruction with the same cause and trap value.
//!
//! **M2 programs** ([`run_m2`], `docs/m2-design.md` §4) run with the M2 CPU profile, and
//! Spike with [`SPIKE_ISA_M2`] and `-l` ([`spike_m2_args`]). A [`Retire`] then also holds
//! the whitelisted CSR write: SystemScope's `csr` and `csr_value` fields, and Spike's
//! `c<number>_<name> <value>` commit tokens. Spike's `mstatush` and `tcontrol` are not
//! compared. A passing program's log ends with the `ECALL` Spike starts before its HTIF
//! exits ([`pass_end`]); a trap program's is read up to its first trap.
//!
//! **M3 programs** ([`run_m3`] and [`run_m3_sv32`], `docs/m3-design.md` §15.3) run with
//! the M3 CPU profile,
//! and Spike with `--priv=msu`, the device tree, and a budget that never ends a run
//! ([`spike_m3_args`], m3-2-spike-appendix B.0). The compared stream is an [`Event`]
//! list: each retirement with its mode, and each exception delivered to S. Spike's log
//! does not say where a trap went, so the mode of the handler's first commit does: S for
//! a delegated exception, which the stream continues through; M for the first exception
//! taken in M, where SystemScope halts and the comparison ends (appendix D11).

use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

use systemscope_contracts::observe::StateView;
use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::{COMMIT_KIND, EXCEPTION_KIND, HALT_KIND, INTERRUPT_KIND, TRAP_KIND};
use systemscope_rv32i::{Rv32iProfile, csr, privilege};

use crate::manifest::{Fixture, Manifest, load};
use crate::progen::{self, ExpectedTrap, MISALIGNED, Program};
use crate::runner::{self, CPU, End, Finished, PASS_CAUSE, Start};
use crate::{FIXTURE_DIR, MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, SELECTED, hex};
use crate::{csrgen, privgen, vmgen};

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
/// The ISA for the M2 CPU profile (`docs/m2-design.md` §4): RV32I and Zicsr.
pub const SPIKE_ISA_M2: &str = "rv32i_zicsr";
/// Where `cargo xtask spike` writes Spike's commit logs, relative to the workspace root.
pub const SPIKE_LOGS: &str = "target/spike-logs";
/// The committed log of `simple` from the pinned Spike, relative to the workspace root.
/// Tests compare SystemScope against it without Spike; `cargo xtask spike verify`
/// requires the installed Spike to write it again, byte for byte.
pub const SIMPLE_LOG: &str = "tests/rv32/spike/rv32ui-simple.log";
/// The committed log of the generated program for seed 0, like [`SIMPLE_LOG`].
pub const PROGEN_LOG: &str = "tests/rv32/spike/progen-0000000000000000.log";
/// The committed start of the `-l` log of one misaligned-access program: its
/// [`TRAP_LOG_LINES`] first lines, through the trap and into Spike's handler.
/// `cargo xtask spike verify` requires the installed Spike's log to start with them.
pub const TRAP_LOG: &str = "tests/rv32/spike/misaligned-lw-1.log";
/// The lines [`TRAP_LOG`] keeps.
pub const TRAP_LOG_LINES: usize = 16;
/// The [`MISALIGNED`] case of [`TRAP_LOG`].
pub const TRAP_LOG_CASE: usize = 2;

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
    /// The whitelisted CSR write of an M2 CSR instruction, `(csr, value stored)`: none
    /// for M1 code, and none when the write is suppressed.
    pub csr: Option<(u16, u32)>,
}

impl fmt::Display for Retire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pc {:#010x} insn {:#010x}", self.pc, self.insn)?;
        match self.reg_write {
            Some((rd, value)) => write!(f, " x{rd} = {value:#010x}")?,
            None => write!(f, " no register write")?,
        }
        match self.mem {
            Mem::None => {}
            Mem::Load { addr } => write!(f, ", load {addr:#010x}")?,
            Mem::Store { addr, width, value } => {
                write!(f, ", store {width} B {value:#x} to {addr:#010x}")?
            }
        }
        match self.csr {
            Some((csr, value)) => write!(f, ", csr {csr:#05x} = {value:#010x}"),
            None => Ok(()),
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
                let mut csr = None;
                let mem = match fields.as_slice() {
                    ["pc", "insn", "rd", "rd_value", "next_pc"] => Mem::None,
                    // An M2 CSR instruction (m2-design §6.5): its CSR, then the value stored
                    // unless the write is suppressed.
                    ["pc", "insn", "rd", "rd_value", "next_pc", "csr"] => {
                        csr_number(u32_at(5)?).ok_or_else(|| at("csr out of range"))?;
                        Mem::None
                    }
                    [
                        "pc",
                        "insn",
                        "rd",
                        "rd_value",
                        "next_pc",
                        "csr",
                        "csr_value",
                    ] => {
                        let number =
                            csr_number(u32_at(5)?).ok_or_else(|| at("csr out of range"))?;
                        csr = Some((number, u32_at(6)?));
                        Mem::None
                    }
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
                    csr,
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
    parse_commit(line, false).map(|(_, retire)| retire)
}

/// A commit line and the mode it retired in (`0` U, `1` S, `3` M). Without `m3`, only
/// machine mode and the M2 CSRs ([`csr_write`]); with it, any of the three modes and the
/// M3 CSRs ([`csr_write_m3`]).
fn parse_commit(line: &str, m3: bool) -> Result<(u8, Retire), String> {
    let (privilege, rest) = match line.strip_prefix("core   0: ") {
        Some(rest) if !m3 => (
            3,
            rest.strip_prefix("3 ")
                .ok_or("not a hart 0, machine-mode commit line")?,
        ),
        Some(rest) => match rest.as_bytes() {
            [p @ (b'0' | b'1' | b'3'), b' ', ..] => (p - b'0', &rest[2..]),
            _ => return Err("not a hart 0 commit line in U, S, or M".to_owned()),
        },
        None => return Err("not a hart 0, machine-mode commit line".to_owned()),
    };
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
    // The CSRs the instruction wrote, as `c<number>_<name> <value>`.
    let mut csrs = Vec::new();
    while let Some(token) = tokens.next_if(|t| t.starts_with('c')) {
        let (number, name) = token[1..].split_once('_').ok_or("bad CSR")?;
        let number = number
            .parse::<u32>()
            .ok()
            .and_then(csr_number)
            .ok_or("bad CSR number")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(format!("bad CSR name {name:?}"));
        }
        let value = word(tokens.next().ok_or("CSR without a value")?, 8)?;
        csrs.push((number, name, value));
    }
    if let Some(extra) = tokens.next() {
        return Err(format!("unexpected {extra:?}"));
    }
    // The tokens say what the line means; the exact spacing must be Spike's too, so a
    // change in the log's layout fails here rather than parsing by luck.
    let mut canonical = format!("core   0: {privilege} {pc:#010x} ({insn:#010x})");
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
    for (number, name, value) in &csrs {
        canonical.push_str(&format!(" c{number}_{name} {value:#010x}"));
    }
    if line != canonical {
        return Err(format!("not laid out as Spike writes it: {canonical:?}"));
    }
    let csr = if m3 {
        csr_write_m3(insn, &csrs)?
    } else {
        csr_write(insn, &csrs)?
    };
    Ok((
        privilege,
        Retire {
            pc,
            insn,
            reg_write: reg.and_then(|(rd, value)| reg_write(rd, value)),
            mem,
            csr,
        },
    ))
}

/// A CSR number, 12 bits.
fn csr_number(n: u32) -> Option<u16> {
    u16::try_from(n).ok().filter(|n| *n < 0x1000)
}

/// Spike's `mstatush`, which it writes with `mstatus`; M2 has none (m2-design §4.3).
const SPIKE_MSTATUSH: u16 = 0x310;
/// Spike's `tcontrol`, which its `MRET` writes; M2 has no trigger module.
const SPIKE_TCONTROL: u16 = 0x7a5;

/// The whitelisted CSR write among the CSRs a Spike commit line reports: `mstatush` and
/// `tcontrol` are not compared, and any other CSR outside the whitelist is an error. At
/// most one whitelisted CSR may be written, except by `MRET`: its implicit `mstatus`
/// update, which SystemScope's trace does not record (§6.5), must come exactly as
/// `mstatus`, `mstatush` = 0, `tcontrol` = 0, and is not compared here; the directed
/// programs read `mstatus` right after every `MRET` instead.
fn csr_write(insn: u32, csrs: &[(u16, &str, u32)]) -> Result<Option<(u16, u32)>, String> {
    if insn == csr::MRET {
        return match csrs {
            [
                (csr::MSTATUS, "mstatus", _),
                (SPIKE_MSTATUSH, "mstatush", 0),
                (SPIKE_TCONTROL, "tcontrol", 0),
            ] => Ok(None),
            _ => Err("MRET's CSR updates are not mstatus, mstatush = 0, tcontrol = 0".to_owned()),
        };
    }
    let mut write = None;
    for &(number, name, value) in csrs {
        if number == SPIKE_MSTATUSH || number == SPIKE_TCONTROL {
            continue;
        }
        if !csr::is_supported(number) {
            return Err(format!(
                "{name} ({number:#05x}) is outside the M2 whitelist"
            ));
        }
        if write.replace((number, value)).is_some() {
            return Err("more than one whitelisted CSR written".to_owned());
        }
    }
    Ok(write)
}

/// The M3 CSR write among the CSRs a Spike commit line reports, as [`csr_write`] does for
/// M2 but on the M3 whitelist ([`privilege::is_supported`]). Spike logs a write of
/// `sstatus` as `mstatus`: it becomes `sstatus` = the logged value's S bits, the value
/// SystemScope's trace records. `MRET` must report exactly `mstatus` and `mstatush` = 0,
/// and `SRET` exactly `mstatus`; neither update is compared here (see [`crate::privgen`]).
fn csr_write_m3(insn: u32, csrs: &[(u16, &str, u32)]) -> Result<Option<(u16, u32)>, String> {
    if insn == csr::MRET {
        return match csrs {
            [
                (csr::MSTATUS, "mstatus", _),
                (SPIKE_MSTATUSH, "mstatush", 0),
            ] => Ok(None),
            _ => Err("MRET's CSR updates are not mstatus, mstatush = 0".to_owned()),
        };
    }
    if insn == privilege::SRET {
        return match csrs {
            [(csr::MSTATUS, "mstatus", _)] => Ok(None),
            _ => Err("SRET's CSR update is not mstatus".to_owned()),
        };
    }
    let field = (insn >> 20) as u16;
    let mut write = None;
    for &(number, name, value) in csrs {
        if number == SPIKE_MSTATUSH || number == SPIKE_TCONTROL {
            continue;
        }
        if !privilege::is_supported(number) {
            return Err(format!(
                "{name} ({number:#05x}) is outside the M3 whitelist"
            ));
        }
        let stored = if number == csr::MSTATUS && field == privilege::SSTATUS {
            (privilege::SSTATUS, value & privilege::SSTATUS_MASK)
        } else {
            (number, value)
        };
        if write.replace(stored).is_some() {
            return Err("more than one whitelisted CSR written".to_owned());
        }
    }
    Ok(write)
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
pub fn compare<T: PartialEq + fmt::Display>(systemscope: &[T], spike: &[T]) -> Result<(), String> {
    let n = systemscope.len().max(spike.len());
    let Some(i) = (0..n).find(|&i| systemscope.get(i) != spike.get(i)) else {
        return Ok(());
    };
    let show = |r: Option<&T>| {
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
    let (finished, text) = run_sides(
        root,
        &image,
        &elf,
        spike,
        log,
        (Rv32iProfile::M1, false),
        executed,
    )?;
    judge_pass(&finished, &parse_spike_log(&text)?, image.entry, tohost)
}

/// Runs `image` traced on SystemScope with the CPU `profile`, then the ELF at `elf` on
/// Spike, with `-l` if `trap` or M2 ([`spike_m2_args`]), or as M3 needs
/// ([`spike_m3_args`]), writing Spike's log to `log`.
/// Returns SystemScope's run and Spike's log. Both sides run before either is judged, so a
/// divergence is reported where it starts.
fn run_sides(
    root: &Path,
    image: &LoadImage,
    elf: &str,
    spike: &Path,
    log: &str,
    (profile, trap): (Rv32iProfile, bool),
    executed: &mut (bool, bool),
) -> Result<(Finished, String), String> {
    // SystemScope: the integrated CPU, traced.
    let finished = runner::execute(
        runner::platform_with_profile(image, false, runner::SEED, profile),
        Start::Init { traced: true },
        Vec::new(),
    );
    executed.0 = true;

    // Spike.
    if let Some(dir) = Path::new(log).parent() {
        fs::create_dir_all(root.join(dir)).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let _ = fs::remove_file(root.join(log));
    let args = match (profile, trap) {
        (Rv32iProfile::M3, _) => spike_m3_args(image.entry, elf, log),
        (Rv32iProfile::M2, _) => spike_m2_args(image.entry, elf, log),
        (Rv32iProfile::M1, true) => spike_trap_args(image.entry, elf, log),
        (Rv32iProfile::M1, false) => spike_args(image.entry, elf, log),
    };
    let out = Command::new(spike)
        .args(args)
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
    Ok((finished, text))
}

/// SystemScope's traced run of a passing program.
fn our_stream(finished: &Finished) -> Result<SystemScopeStream, String> {
    finished
        .trace
        .as_ref()
        .ok_or_else(|| "SystemScope recorded no trace".to_owned())
        .and_then(from_trace)
}

/// The verdict for a program that ends at the passing `write_tohost`: the streams are
/// equal, Spike's ends at the boundary, and SystemScope then halts on the `ECALL` right
/// after it with the M1-A2 pass rule, `instret` equal to the record count, and the
/// registers Spike's writes leave. Returns the record count.
pub fn judge_pass(
    finished: &Finished,
    theirs: &[Retire],
    entry: u32,
    tohost: u32,
) -> Result<usize, String> {
    let ours = our_stream(finished)?;
    compare(&ours.retires, theirs)?;
    let n = check_boundary(theirs, entry, tohost)?;
    runner::judge(&finished.outcome).map_err(|e| format!("SystemScope: {e}"))?;
    let last = theirs[n - 1].pc;
    match (&ours.trap, &finished.outcome.end) {
        (Some((pc, cause)), End::Trap { pc: halt, .. })
            if *pc == last.wrapping_add(4) && halt == pc && cause == PASS_CAUSE => {}
        other => {
            return Err(format!(
                "SystemScope does not halt on the ECALL after {last:#010x}: {other:?}"
            ));
        }
    }
    check_end_state(finished, theirs)?;
    Ok(n)
}

/// SystemScope retired exactly `theirs`, and its registers are those Spike's writes leave.
fn check_end_state(finished: &Finished, theirs: &[Retire]) -> Result<(), String> {
    let n = theirs.len();
    if finished.outcome.instret != n as u64 {
        return Err(format!(
            "SystemScope retired {}, Spike {n}",
            finished.outcome.instret
        ));
    }
    let view = finished.views.get(CPU.0 as usize).ok_or("no CPU view")?;
    let (ours, theirs) = (registers(view)?, replay(theirs));
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
    Ok(())
}

/// Spike's command line for a trap program: [`spike_args`] with `-l`, which also logs
/// every instruction as it executes and every trap it takes.
pub fn spike_trap_args(entry: u32, elf: &str, log: &str) -> Vec<String> {
    let mut args = vec!["-l".to_owned()];
    args.extend(spike_args(entry, elf, log));
    args
}

/// Spike's command line for a program run with the M2 CPU profile: [`spike_trap_args`]
/// with [`SPIKE_ISA_M2`]. Every M2 program runs with `-l`, so a trap is always named.
pub fn spike_m2_args(entry: u32, elf: &str, log: &str) -> Vec<String> {
    let m1 = format!("--isa={SPIKE_ISA}");
    spike_trap_args(entry, elf, log)
        .into_iter()
        .map(|arg| {
            if arg == m1 {
                format!("--isa={SPIKE_ISA_M2}")
            } else {
                arg
            }
        })
        .collect()
}

/// The trap Spike reports in a `-l` log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpikeTrap {
    /// `epc`: the trapping instruction's address.
    pub pc: u32,
    /// The trapping instruction's word, from its instruction line.
    pub insn: u32,
    /// Spike's name for the cause, such as `trap_load_address_misaligned`.
    pub cause: String,
    /// The trap value.
    pub tval: u32,
}

/// Spike's name for each SystemScope trap cause (§6), as its `-l` log writes it.
pub const SPIKE_CAUSES: [(&str, &str); 9] = [
    (
        "InstructionAddressMisaligned",
        "trap_instruction_address_misaligned",
    ),
    ("InstructionAccessFault", "trap_instruction_access_fault"),
    ("IllegalInstruction", "trap_illegal_instruction"),
    ("Breakpoint", "trap_breakpoint"),
    ("LoadAddressMisaligned", "trap_load_address_misaligned"),
    ("LoadAccessFault", "trap_load_access_fault"),
    ("StoreAddressMisaligned", "trap_store_address_misaligned"),
    ("StoreAccessFault", "trap_store_access_fault"),
    ("EnvironmentCall", "trap_machine_ecall"),
];

/// Parses a pinned-Spike `-l --log-commits` log up to its first trap, strictly. Each
/// instruction is an instruction line, `core   0: <pc> (<insn>) <disassembly>`, then
/// either its commit line (as [`parse_spike_log`] reads it) for the same `pc` and word,
/// or the trap:
///
/// ```text
/// core   0: exception <cause>, epc <pc>
/// core   0:           tval <tval>
/// ```
///
/// Only the `pc` and word of an instruction line are read; Spike's disassembly is not.
/// Lines after the trap, Spike's handler at `mtvec`, are not compared (§10.3). A log with
/// no trap is an error.
pub fn parse_spike_trap_log(log: &str) -> Result<(Vec<Retire>, SpikeTrap), String> {
    match parse_spike_l_log(log)? {
        (retires, Some(trap)) => Ok((retires, trap)),
        (_, None) => Err("Spike's log has no trap".to_owned()),
    }
}

/// Spike's name for an environment call from machine mode, the one trap its `-l` log
/// writes without a tval line.
const SPIKE_ECALL: &str = "trap_machine_ecall";

/// Parses a `-l --log-commits` log as [`parse_spike_trap_log`] does, up to its first trap
/// if it has one. A program run to the passing `write_tohost` has none but the `ECALL`
/// after it, which Spike starts before its HTIF exits: see [`pass_end`].
pub fn parse_spike_l_log(log: &str) -> Result<(Vec<Retire>, Option<SpikeTrap>), String> {
    let mut lines = log.lines().enumerate();
    let mut retires = Vec::new();
    let at = |i: usize, why: &str, line: &str| format!("Spike log line {}: {why}: {line:?}", i + 1);
    while let Some((i, line)) = lines.next() {
        let (pc, insn) = instruction_line(line).map_err(|why| at(i, &why, line))?;
        let Some((k, next)) = lines.next() else {
            return Err(at(i, "the log ends after an instruction line", line));
        };
        if let Some(rest) = next.strip_prefix("core   0: exception ") {
            let (cause, epc) = rest
                .split_once(", epc ")
                .ok_or_else(|| at(k, "no epc", next))?;
            if !cause.starts_with("trap_") || epc != format!("{pc:#010x}") {
                return Err(at(
                    k,
                    &format!("not a trap of the instruction at {pc:#010x}"),
                    next,
                ));
            }
            // Spike writes no tval line for a trap without one: of the causes here, only
            // an environment call, whose `tval` is 0.
            let tval = if cause == SPIKE_ECALL {
                0
            } else {
                let (j, tval_line) = lines
                    .next()
                    .ok_or_else(|| at(k, "no tval line after the trap", next))?;
                tval_line
                    .strip_prefix("core   0:           tval ")
                    .ok_or_else(|| at(j, "not the trap's tval line", tval_line))
                    .and_then(|t| word(t, 8).map_err(|why| at(j, &why, tval_line)))?
            };
            let trap = SpikeTrap {
                pc,
                insn,
                cause: cause.to_owned(),
                tval,
            };
            return Ok((retires, Some(trap)));
        }
        let retire = parse_spike_line(next).map_err(|why| at(k, &why, next))?;
        if (retire.pc, retire.insn) != (pc, insn) {
            return Err(at(
                k,
                "not the commit of the instruction line before it",
                next,
            ));
        }
        retires.push(retire);
    }
    Ok((retires, None))
}

/// The retirements of a `-l` log of a program that must pass. With `-l`, Spike starts the
/// `ECALL` after `write_tohost` before its HTIF exits, so the log must end with exactly
/// that: the `ECALL`'s instruction line and its trap, right after the last retirement.
/// Anything else, a trap included, is an error. [`check_boundary`] then requires the
/// retirements to end with `write_tohost`, as without `-l`.
pub fn pass_end(log: &str) -> Result<Vec<Retire>, String> {
    let (retires, trap) = parse_spike_l_log(log)?;
    let last = retires.last().ok_or("Spike retired nothing")?.pc;
    let ecall = last.wrapping_add(4);
    let end = format!(
        "core   0: {ecall:#010x} (0x00000073) ecall\ncore   0: exception {SPIKE_ECALL}, epc \
         {ecall:#010x}\n"
    );
    match trap {
        Some(t)
            if (t.pc, t.insn, t.cause.as_str()) == (ecall, 0x73, SPIKE_ECALL)
                && log.ends_with(&end) =>
        {
            Ok(retires)
        }
        Some(t) => Err(format!(
            "Spike's log does not end with the ECALL at {ecall:#010x}, but traps with {t:?}"
        )),
        None => Err(format!(
            "Spike's log does not end with the ECALL at {ecall:#010x}"
        )),
    }
}

/// The `pc` and word of an instruction line, `core   0: <pc> (<insn>) <disassembly>`.
fn instruction_line(line: &str) -> Result<(u32, u32), String> {
    let rest = line
        .strip_prefix("core   0: 0x")
        .ok_or("not a hart 0 instruction line")?;
    let pc = word(&format!("0x{}", rest.get(..8).ok_or("no pc")?), 8)?;
    let insn = rest
        .get(8..)
        .and_then(|r| r.strip_prefix(" ("))
        .and_then(|r| r.get(..10))
        .ok_or("no (instruction)")?;
    let insn = word(insn, 8)?;
    let prefix = format!("core   0: {pc:#010x} ({insn:#010x}) ");
    match line.strip_prefix(&prefix) {
        Some(disassembly) if !disassembly.trim().is_empty() => Ok((pc, insn)),
        _ => Err(format!("not laid out as Spike writes it: {prefix:?}...")),
    }
}

/// The verdict for a trap program: the streams are equal up to the trap, both sides trap
/// on the same instruction with the same cause and `tval`, and that trap is `expected`.
/// SystemScope retired exactly Spike's records, with the registers they leave. Returns
/// the record count.
pub fn judge_trap(
    finished: &Finished,
    theirs: &[Retire],
    trap: &SpikeTrap,
    expected: &ExpectedTrap,
) -> Result<usize, String> {
    let ours = our_stream(finished)?;
    compare(&ours.retires, theirs)?;
    let spike_cause = SPIKE_CAUSES
        .iter()
        .find(|(ours, _)| *ours == expected.cause)
        .map(|(_, theirs)| *theirs)
        .ok_or_else(|| format!("no Spike name for {}", expected.cause))?;
    let spike_says = (trap.pc, trap.insn, trap.cause.as_str(), trap.tval);
    if spike_says != (expected.pc, expected.insn, spike_cause, expected.tval) {
        return Err(format!(
            "Spike traps with {trap:?}, not {spike_cause} at {:#010x} ({:#010x}), tval {:#010x}",
            expected.pc, expected.insn, expected.tval
        ));
    }
    match &finished.outcome.end {
        End::Trap { cause, pc, tval }
            if (cause.as_str(), *pc, *tval) == (expected.cause, expected.pc, expected.tval)
                && ours.trap.as_ref().map(|t| t.0) == Some(expected.pc) => {}
        other => {
            return Err(format!(
                "SystemScope ends with {other}, not Trap({}) at {:#010x}, tval {:#010x}",
                expected.cause, expected.pc, expected.tval
            ));
        }
    }
    check_end_state(finished, theirs)?;
    Ok(theirs.len())
}

/// Writes `program`'s ELF under `logs/progen/`, then runs it on both sides: a generated
/// program to the passing `write_tohost`, a trap program (with `expected`) to its trap.
pub fn program_differential(
    root: &Path,
    program: &Program,
    expected: Option<&ExpectedTrap>,
    spike: &Path,
    logs: &str,
) -> Diff {
    program_differential_with(root, program, expected, Rv32iProfile::M1, spike, logs)
}

/// [`program_differential`] with the CPU `profile`. An M2 program runs on Spike with
/// [`spike_m2_args`], and one that must pass must not trap. An M3 program runs with
/// [`spike_m3_args`] and must have `expected`, the exception taken in M that ends it
/// ([`judge_m3`]).
pub fn program_differential_with(
    root: &Path,
    program: &Program,
    expected: Option<&ExpectedTrap>,
    profile: Rv32iProfile,
    spike: &Path,
    logs: &str,
) -> Diff {
    let mut diff = Diff {
        name: program.name.clone(),
        blake3: *blake3::hash(&program.elf).as_bytes(),
        executed: (false, false),
        verdict: Ok(0),
    };
    diff.verdict = match (profile, expected) {
        (Rv32iProfile::M3, Some(expected)) => {
            run_m3_program(root, program, expected, spike, logs, &mut diff.executed)
        }
        (Rv32iProfile::M3, None) => {
            Err("an M3 program must end with an exception taken in M".to_owned())
        }
        _ => run_program(
            root,
            program,
            (expected, profile),
            spike,
            logs,
            &mut diff.executed,
        ),
    };
    diff
}

fn run_program(
    root: &Path,
    program: &Program,
    (expected, profile): (Option<&ExpectedTrap>, Rv32iProfile),
    spike: &Path,
    logs: &str,
    executed: &mut (bool, bool),
) -> Result<usize, String> {
    let elf = format!("{logs}/progen/{}.elf", program.name);
    let log = format!("{logs}/progen/{}.log", program.name);
    let path = root.join(&elf);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&path, &program.elf).map_err(|e| format!("{elf}: {e}"))?;
    let image = load(&program.name, &program.elf)?;
    let (finished, text) = run_sides(
        root,
        &image,
        &elf,
        spike,
        &log,
        (profile, expected.is_some()),
        executed,
    )?;
    let (theirs, trap) = match (profile, expected) {
        (Rv32iProfile::M1, None) => (parse_spike_log(&text)?, None),
        (Rv32iProfile::M1, Some(_)) => parse_spike_trap_log(&text).map(|(r, t)| (r, Some(t)))?,
        (Rv32iProfile::M2, Some(_)) => parse_spike_l_log(&text)?,
        (Rv32iProfile::M2, None) => (pass_end(&text)?, None),
        (Rv32iProfile::M3, _) => return Err("M3 programs run through run_m3_program".to_owned()),
    };
    match (expected, trap) {
        (None, _) => {
            let tohost =
                elf_symbol(&program.elf, "tohost").ok_or(format!("{elf} has no tohost symbol"))?;
            judge_pass(&finished, &theirs, image.entry, tohost)
        }
        (Some(expected), Some(trap)) => judge_trap(&finished, &theirs, &trap, expected),
        (Some(_), None) => Err("Spike's log has no trap".to_owned()),
    }
}

/// The generated programs for `seeds` against Spike (M1-A3).
pub fn run_generated(root: &Path, spike: &Path, logs: &str, seeds: &[u64]) -> DiffReport {
    let results = seeds
        .iter()
        .map(|&seed| program_differential(root, &progen::generate(seed), None, spike, logs))
        .collect();
    DiffReport {
        selected: seeds.len(),
        results,
    }
}

/// Every misaligned-access program against Spike (M1-A3): the pinned Spike must trap on
/// each, as SystemScope does.
pub fn run_misaligned(root: &Path, spike: &Path, logs: &str) -> DiffReport {
    let results = MISALIGNED
        .iter()
        .map(|case| {
            let (program, expected) = progen::misaligned(case);
            program_differential(root, &program, Some(&expected), spike, logs)
        })
        .collect();
    DiffReport {
        selected: MISALIGNED.len(),
        results,
    }
}

/// The directed M2 programs against Spike, with the M2 CPU profile and
/// [`SPIKE_ISA_M2`]: every [`csrgen::pass_programs`] program to the passing
/// `write_tohost`, then every [`csrgen::illegal_programs`] program to its trap.
pub fn run_m2(root: &Path, spike: &Path, logs: &str) -> DiffReport {
    let passing = csrgen::pass_programs()
        .into_iter()
        .map(|program| (program, None));
    let trapping = csrgen::illegal_programs()
        .into_iter()
        .map(|(program, expected)| (program, Some(expected)));
    let results: Vec<Diff> = passing
        .chain(trapping)
        .map(|(program, expected)| {
            program_differential_with(
                root,
                &program,
                expected.as_ref(),
                Rv32iProfile::M2,
                spike,
                logs,
            )
        })
        .collect();
    DiffReport {
        selected: results.len(),
        results,
    }
}

/// `--priv` for the M3 CPU profile: machine, supervisor, and user modes.
pub const SPIKE_PRIV_M3: &str = "msu";

/// Spike's instruction budget for an M3 program. Each trap ends one of Spike's step
/// chunks and costs the whole chunk, so the budget is far above any program; every run
/// ends at `write_tohost` instead (m3-2-spike-appendix B.0).
pub const SPIKE_INSTRUCTIONS_M3: u64 = 100_000_000;

/// Spike's command line for a program run with the M3 CPU profile: `-l`,
/// [`SPIKE_ISA_M2`], [`SPIKE_PRIV_M3`], and [`SPIKE_INSTRUCTIONS_M3`]. It keeps the
/// device tree, without which the pinned Spike configures no MMU and drops `medeleg`'s
/// page-fault bits; Spike then needs `dtc` on `PATH` when it starts.
pub fn spike_m3_args(entry: u32, elf: &str, log: &str) -> Vec<String> {
    vec![
        "-l".to_owned(),
        format!("--isa={SPIKE_ISA_M2}"),
        format!("--priv={SPIKE_PRIV_M3}"),
        format!("--pcs=0:{entry:#x}"),
        format!("-m{RAM_BASE:#x}:{RAM_SIZE:#x}"),
        "--log-commits".to_owned(),
        format!("--log={log}"),
        format!("--instructions={SPIKE_INSTRUCTIONS_M3}"),
        elf.to_owned(),
    ]
}

/// One step of an M3 stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A retirement, in the mode it ran in (`0` U, `1` S, `3` M).
    Retire {
        /// The mode.
        privilege: u8,
        /// The retirement.
        retire: Retire,
    },
    /// An exception delivered to S.
    Exception {
        /// The trapping instruction's address.
        pc: u32,
        /// Its word.
        insn: u32,
        /// SystemScope's M3 cause name.
        cause: String,
        /// The trap value.
        tval: u32,
    },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Retire { privilege, retire } => write!(f, "p{privilege} {retire}"),
            Event::Exception {
                pc,
                insn,
                cause,
                tval,
            } => write!(
                f,
                "exception {cause} to S at pc {pc:#010x} insn {insn:#010x}, tval {tval:#010x}"
            ),
        }
    }
}

/// The retirements of an M3 stream.
fn retirements(events: &[Event]) -> Vec<Retire> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Retire { retire, .. } => Some(*retire),
            Event::Exception { .. } => None,
        })
        .collect()
}

/// Spike's name for each SystemScope M3 trap cause the directed programs raise
/// (`docs/m3-design.md` §5.3), as its `-l` log writes it.
pub const SPIKE_CAUSES_M3: [(&str, &str); 14] = [
    (
        "InstructionAddressMisaligned",
        "trap_instruction_address_misaligned",
    ),
    ("InstructionAccessFault", "trap_instruction_access_fault"),
    ("IllegalInstruction", "trap_illegal_instruction"),
    ("Breakpoint", "trap_breakpoint"),
    ("LoadAddressMisaligned", "trap_load_address_misaligned"),
    ("LoadAccessFault", "trap_load_access_fault"),
    ("StoreAddressMisaligned", "trap_store_address_misaligned"),
    ("StoreAccessFault", "trap_store_access_fault"),
    ("EnvironmentCallFromU", "trap_user_ecall"),
    ("EnvironmentCallFromS", "trap_supervisor_ecall"),
    ("EnvironmentCallFromM", SPIKE_ECALL),
    ("InstructionPageFault", "trap_instruction_page_fault"),
    ("LoadPageFault", "trap_load_page_fault"),
    ("StorePageFault", "trap_store_page_fault"),
];

/// Parses a pinned-Spike `-l --log-commits` log of an M3 program, strictly, up to its
/// first exception taken in M. As in [`parse_spike_l_log`], each instruction is an
/// instruction line, then its commit line, now in any of the three modes, or its trap.
/// A fetch that faults (causes 1 and 12) has no instruction line: its trap line stands
/// alone, with `epc` the fetched address, and its instruction word is 0, as in
/// SystemScope's `rv32.exception` record. Spike writes no tval line for the three
/// environment calls. A trap whose handler's
/// first instruction commits in S was delegated: it is an [`Event::Exception`], and the
/// log goes on. Any other trap is the one taken in M, returned with the events before
/// it; the lines after it, Spike's handler, are not compared. A log without one is an
/// error.
pub fn parse_spike_m3_log(log: &str) -> Result<(Vec<Event>, SpikeTrap), String> {
    let lines: Vec<&str> = log.lines().collect();
    let at = |i: usize, why: &str| {
        format!(
            "Spike log line {}: {why}: {:?}",
            i + 1,
            lines.get(i).copied().unwrap_or("")
        )
    };
    let mut events = Vec::new();
    let mut k = 0;
    loop {
        let line = lines
            .get(k)
            .ok_or_else(|| "Spike's log has no exception taken in M".to_owned())?;
        if let Some(rest) = line.strip_prefix("core   0: exception ") {
            // A fetch fault: the trap line, as if after an instruction line at its epc.
            let (cause, epc) = rest.split_once(", epc ").ok_or_else(|| at(k, "no epc"))?;
            if !matches!(
                cause,
                "trap_instruction_page_fault" | "trap_instruction_access_fault"
            ) {
                return Err(at(k, "a trap without an instruction line"));
            }
            let pc = word(epc, 8).map_err(|why| at(k, &why))?;
            match trap_at(&lines, k, pc, 0, &mut events, &at)? {
                Next::Continue(j) => k = j,
                Next::Trap(trap) => return Ok((events, trap)),
            }
            continue;
        }
        let (pc, insn) = instruction_line(line).map_err(|why| at(k, &why))?;
        let next = lines
            .get(k + 1)
            .ok_or_else(|| at(k, "the log ends after an instruction line"))?;
        let Some(_) = next.strip_prefix("core   0: exception ") else {
            let (privilege, retire) = parse_commit(next, true).map_err(|why| at(k + 1, &why))?;
            if (retire.pc, retire.insn) != (pc, insn) {
                return Err(at(
                    k + 1,
                    "not the commit of the instruction line before it",
                ));
            }
            events.push(Event::Retire { privilege, retire });
            k += 2;
            continue;
        };
        match trap_at(&lines, k + 1, pc, insn, &mut events, &at)? {
            Next::Continue(j) => k = j,
            Next::Trap(trap) => return Ok((events, trap)),
        }
    }
}

/// Where [`parse_spike_m3_log`] goes after a trap.
enum Next {
    /// The trap was delegated; the log goes on at this line.
    Continue(usize),
    /// The trap was taken in M.
    Trap(SpikeTrap),
}

/// Reads the trap line `t` of the instruction at `pc` with word `insn`: its tval line,
/// unless it is an environment call, and where its handler runs.
fn trap_at(
    lines: &[&str],
    t: usize,
    pc: u32,
    insn: u32,
    events: &mut Vec<Event>,
    at: &dyn Fn(usize, &str) -> String,
) -> Result<Next, String> {
    let rest = lines[t]
        .strip_prefix("core   0: exception ")
        .ok_or_else(|| at(t, "not a trap line"))?;
    let (cause, epc) = rest.split_once(", epc ").ok_or_else(|| at(t, "no epc"))?;
    if epc != format!("{pc:#010x}") {
        return Err(at(
            t,
            &format!("not a trap of the instruction at {pc:#010x}"),
        ));
    }
    let ours = SPIKE_CAUSES_M3
        .iter()
        .find(|(_, theirs)| *theirs == cause)
        .map(|(ours, _)| *ours)
        .ok_or_else(|| at(t, "a cause no directed program raises"))?;
    let mut j = t + 1;
    let tval = if ours.starts_with("EnvironmentCall") {
        0
    } else {
        let tval = lines
            .get(j)
            .and_then(|l| l.strip_prefix("core   0:           tval "))
            .ok_or_else(|| at(j, "not the trap's tval line"))
            .and_then(|t| word(t, 8).map_err(|why| at(j, &why)))?;
        j += 1;
        tval
    };
    let delegated = match (lines.get(j), lines.get(j + 1)) {
        (Some(handler), Some(commit)) if instruction_line(handler).is_ok() => {
            matches!(parse_commit(commit, true), Ok((1, _)))
        }
        _ => false,
    };
    if !delegated {
        return Ok(Next::Trap(SpikeTrap {
            pc,
            insn,
            cause: cause.to_owned(),
            tval,
        }));
    }
    events.push(Event::Exception {
        pc,
        insn,
        cause: ours.to_owned(),
        tval,
    });
    Ok(Next::Continue(j))
}

/// SystemScope's side of an M3 run: its events, and the `rv32.trap` record that ended it,
/// as `(pc, cause)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemScopeEvents {
    /// One per `rv32.commit` or `rv32.exception` record, in order.
    pub events: Vec<Event>,
    /// The halt trap.
    pub trap: Option<(u32, String)>,
}

/// Reads SystemScope's M3 events from a traced run's canonical records
/// (`docs/m3-design.md` §5.6), as [`from_trace`] reads M1 and M2 ones: `rv32.commit` has
/// `priv` after `next_pc`, and a load or store `paddr` after `addr`; `rv32.exception` is a
/// delivered exception, always to S. An interrupt, the instruction limit, or a record
/// after the trap is an error. Spike's commit log names the virtual address, so `addr` is
/// what is compared; `paddr` must only be a 34-bit physical address with `addr`'s page
/// offset, and equal `addr` in M, which never translates (§5.2).
pub fn from_trace_m3(trace: &Trace) -> Result<SystemScopeEvents, String> {
    let mut out = SystemScopeEvents {
        events: Vec::new(),
        trap: None,
    };
    for (i, record) in trace.records.iter().enumerate() {
        if !record.kind.starts_with("rv32.") {
            continue;
        }
        let at = |why: &str| format!("trace record {i} ({}): {why}", record.kind);
        if out.trap.is_some() {
            return Err(at("after the trap"));
        }
        let fields: Vec<&str> = record.fields.iter().map(|f| f.0).collect();
        let u32_at = |n: usize| match record.fields.get(n) {
            Some((_, Value::U64(v))) => u32::try_from(*v).map_err(|_| at("value above 32 bits")),
            _ => Err(at("expected a U64 field")),
        };
        let str_at = |n: usize| match record.fields.get(n) {
            Some((_, Value::Str(s))) => Ok(s.clone()),
            _ => Err(at("expected a Str field")),
        };
        // Field 7 is a physical address for the address in field 6.
        let paddr_ok = || -> Result<bool, String> {
            let addr = u64::from(u32_at(6)?);
            let Some((_, Value::U64(paddr))) = record.fields.get(7) else {
                return Err(at("expected a U64 field"));
            };
            let in_m = u32_at(5)? == 3;
            Ok(*paddr < 1 << 34 && paddr & 0xfff == addr & 0xfff && (!in_m || *paddr == addr))
        };
        match record.kind {
            k if k == COMMIT_KIND => {
                let head = ["pc", "insn", "rd", "rd_value", "next_pc", "priv"];
                if fields.get(..6) != Some(&head[..]) {
                    return Err(at(&format!("unexpected fields {fields:?}")));
                }
                let mut csr = None;
                let mem = match &fields[6..] {
                    [] => Mem::None,
                    ["csr"] => {
                        csr_number(u32_at(6)?).ok_or_else(|| at("csr out of range"))?;
                        Mem::None
                    }
                    ["csr", "csr_value"] => {
                        let number =
                            csr_number(u32_at(6)?).ok_or_else(|| at("csr out of range"))?;
                        csr = Some((number, u32_at(7)?));
                        Mem::None
                    }
                    ["addr", "paddr"] if paddr_ok()? => Mem::Load { addr: u32_at(6)? },
                    ["addr", "paddr", "width", "value"] if paddr_ok()? => {
                        let width = match u32_at(8)? {
                            w @ (1 | 2 | 4) => w as u8,
                            w => return Err(at(&format!("store width {w}"))),
                        };
                        Mem::Store {
                            addr: u32_at(6)?,
                            width,
                            value: u32_at(9)?,
                        }
                    }
                    other => return Err(at(&format!("unexpected fields {other:?}"))),
                };
                let rd = u8::try_from(u32_at(2)?)
                    .ok()
                    .filter(|rd| *rd < 32)
                    .ok_or_else(|| at("rd out of range"))?;
                let privilege = match u32_at(5)? {
                    p @ (0 | 1 | 3) => p as u8,
                    p => return Err(at(&format!("priv {p}"))),
                };
                out.events.push(Event::Retire {
                    privilege,
                    retire: Retire {
                        pc: u32_at(0)?,
                        insn: u32_at(1)?,
                        reg_write: reg_write(rd, u32_at(3)?),
                        mem,
                        csr,
                    },
                });
            }
            k if k == EXCEPTION_KIND => {
                if fields != ["pc", "insn", "cause", "tval", "from", "to"] {
                    return Err(at(&format!("unexpected fields {fields:?}")));
                }
                if str_at(5)? != "S" {
                    return Err(at("not delivered to S"));
                }
                out.events.push(Event::Exception {
                    pc: u32_at(0)?,
                    insn: u32_at(1)?,
                    cause: str_at(2)?,
                    tval: u32_at(3)?,
                });
            }
            k if k == TRAP_KIND => {
                if fields != ["pc", "insn", "cause", "tval"] {
                    return Err(at(&format!("unexpected fields {fields:?}")));
                }
                out.trap = Some((u32_at(0)?, str_at(2)?));
            }
            k if k == INTERRUPT_KIND => return Err(at("an interrupt")),
            k if k == HALT_KIND => return Err(at("the instruction limit")),
            _ => return Err(at("unknown CPU record kind")),
        }
    }
    Ok(out)
}

/// The verdict for an M3 program: the event streams are equal up to the first exception
/// taken in M, both sides take it at the same instruction with the same cause and `tval`,
/// and it is `expected`. SystemScope halts on it, having retired exactly Spike's
/// retirements, with the registers they leave. Returns the event count.
pub fn judge_m3(
    finished: &Finished,
    theirs: &[Event],
    trap: &SpikeTrap,
    expected: &ExpectedTrap,
) -> Result<usize, String> {
    let ours = finished
        .trace
        .as_ref()
        .ok_or_else(|| "SystemScope recorded no trace".to_owned())
        .and_then(from_trace_m3)?;
    compare(&ours.events, theirs)?;
    let spike_cause = SPIKE_CAUSES_M3
        .iter()
        .find(|(ours, _)| *ours == expected.cause)
        .map(|(_, theirs)| *theirs)
        .ok_or_else(|| format!("no Spike name for {}", expected.cause))?;
    let spike_says = (trap.pc, trap.insn, trap.cause.as_str(), trap.tval);
    if spike_says != (expected.pc, expected.insn, spike_cause, expected.tval) {
        return Err(format!(
            "Spike traps in M with {trap:?}, not {spike_cause} at {:#010x} ({:#010x}), tval              {:#010x}",
            expected.pc, expected.insn, expected.tval
        ));
    }
    match &finished.outcome.end {
        End::Trap { cause, pc, tval }
            if (cause.as_str(), *pc, *tval) == (expected.cause, expected.pc, expected.tval)
                && ours.trap.as_ref().map(|t| t.0) == Some(expected.pc) => {}
        other => {
            return Err(format!(
                "SystemScope ends with {other}, not Trap({}) at {:#010x}, tval {:#010x}",
                expected.cause, expected.pc, expected.tval
            ));
        }
    }
    check_end_state(finished, &retirements(theirs))?;
    Ok(theirs.len())
}

fn run_m3_program(
    root: &Path,
    program: &Program,
    expected: &ExpectedTrap,
    spike: &Path,
    logs: &str,
    executed: &mut (bool, bool),
) -> Result<usize, String> {
    let dir = if program.name.starts_with("m3-vm-") {
        "vmgen"
    } else {
        "privgen"
    };
    let elf = format!("{logs}/{dir}/{}.elf", program.name);
    let log = format!("{logs}/{dir}/{}.log", program.name);
    let path = root.join(&elf);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&path, &program.elf).map_err(|e| format!("{elf}: {e}"))?;
    let image = load(&program.name, &program.elf)?;
    let (finished, text) = run_sides(
        root,
        &image,
        &elf,
        spike,
        &log,
        (Rv32iProfile::M3, true),
        executed,
    )?;
    let (theirs, trap) = parse_spike_m3_log(&text)?;
    judge_m3(&finished, &theirs, &trap, expected)
}

/// The directed M3.2 programs against Spike (`docs/m3-design.md` §15.3), with the M3 CPU
/// profile and [`spike_m3_args`]: every [`privgen::programs`] program, up to the exception
/// taken in M that ends it.
pub fn run_m3(root: &Path, spike: &Path, logs: &str) -> DiffReport {
    run_m3_programs(root, spike, logs, privgen::programs())
}

/// The directed M3.3 Sv32 programs against Spike, as [`run_m3`] runs the M3.2 ones: every
/// [`vmgen::programs`] program.
pub fn run_m3_sv32(root: &Path, spike: &Path, logs: &str) -> DiffReport {
    run_m3_programs(root, spike, logs, vmgen::programs())
}

fn run_m3_programs(
    root: &Path,
    spike: &Path,
    logs: &str,
    programs: Vec<(Program, ExpectedTrap)>,
) -> DiffReport {
    let results: Vec<Diff> = programs
        .into_iter()
        .map(|(program, expected)| {
            program_differential_with(
                root,
                &program,
                Some(&expected),
                Rv32iProfile::M3,
                spike,
                logs,
            )
        })
        .collect();
    DiffReport {
        selected: results.len(),
        results,
    }
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
        // A generated program, whose whole log is committed, and a trap program, whose
        // log up to and just past the trap is.
        let (lw, lw_trap) = progen::misaligned(&MISALIGNED[TRAP_LOG_CASE]);
        for (program, expected, path, lines) in [
            (progen::generate(0), None, PROGEN_LOG, None),
            (lw, Some(&lw_trap), TRAP_LOG, Some(TRAP_LOG_LINES)),
        ] {
            let name = program.name.clone();
            let smoke = program_differential(root, &program, expected, spike, SPIKE_LOGS)
                .verdict
                .and_then(|_| {
                    let log = format!("{SPIKE_LOGS}/progen/{name}.log");
                    let ours = fs::read_to_string(root.join(&log)).map_err(|e| e.to_string())?;
                    let committed =
                        fs::read_to_string(root.join(path)).map_err(|e| e.to_string())?;
                    let ours = match lines {
                        None => ours,
                        Some(n) => ours.lines().take(n).map(|l| format!("{l}\n")).collect(),
                    };
                    if ours == committed {
                        Ok(())
                    } else {
                        Err(format!("{log} differs from {path}"))
                    }
                });
            match smoke {
                Ok(()) => checked.push(format!(
                    "{name} matches, and its Spike log equals the committed one"
                )),
                Err(e) => errors.push(format!("the smoke run of {name}: {e}")),
            }
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
            csr: None,
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

    /// Commit lines of the pinned Spike with `--isa=rv32i_zicsr`: a whitelisted CSR write
    /// is kept, `mstatush` and `tcontrol` are not compared, and `MRET`'s implicit update
    /// must have exactly Spike's shape.
    #[test]
    fn csr_writes_in_spike_lines_parse() {
        let csr = |pc, insn, reg_write, csr| Retire {
            csr,
            ..retire(pc, insn, reg_write, Mem::None)
        };
        for (line, expected) in [
            (
                "core   0: 3 0x80000188 (0x340fd673) x12 0x12345678 c832_mscratch 0x0000001f",
                csr(
                    0x8000_0188,
                    0x340f_d673,
                    Some((12, 0x1234_5678)),
                    Some((0x340, 0x1f)),
                ),
            ),
            (
                "core   0: 3 0x80000008 (0x30529073) c773_mtvec 0x80000280",
                csr(0x8000_0008, 0x3052_9073, None, Some((0x305, 0x8000_0280))),
            ),
            (
                "core   0: 3 0x80000114 (0x34429073) c836_mip 0x00000000",
                csr(0x8000_0114, 0x3442_9073, None, Some((0x344, 0))),
            ),
            (
                "core   0: 3 0x8000003c (0x31029073) c784_mstatush 0x00000000",
                csr(0x8000_003c, 0x3102_9073, None, None),
            ),
            (
                "core   0: 3 0x8000006c (0x30200073) c768_mstatus 0x00001880 c784_mstatush \
                 0x00000000 c1957_tcontrol 0x00000000",
                csr(0x8000_006c, 0x3020_0073, None, None),
            ),
        ] {
            assert_eq!(parse_spike_log(line).unwrap(), [expected], "{line:?}");
        }
        for line in [
            "core   0: 3 0x80000000 (0x30129073) c769_misa 0x40000100",
            "core   0: 3 0x80000000 (0x34029073) c832_mscratch 0x00000001 c833_mepc 0x00000000",
            "core   0: 3 0x80000000 (0x34029073) c832_mscratch 0x1f",
            "core   0: 3 0x80000000 (0x34029073) c832_mscratch",
            "core   0: 3 0x80000000 (0x34029073) c832_mscratch  0x00000001",
            "core   0: 3 0x80000000 (0x34029073)  c832_mscratch 0x00000001",
            "core   0: 3 0x80000000 (0x34029073) c4096_mscratch 0x00000001",
            "core   0: 3 0x80000000 (0x34029073) c832_ 0x00000001",
            "core   0: 3 0x80000000 (0x34029073) c0832_mscratch 0x00000001",
            "core   0: 3 0x80000000 (0x34029073) c832_mscratch 0x00000001 x1  0x00000000",
            "core   0: 3 0x8000006c (0x30200073) c768_mstatus 0x00001880",
            "core   0: 3 0x8000006c (0x30200073)",
            "core   0: 3 0x8000006c (0x30200073) c768_mstatus 0x00001880 c784_mstatush \
             0x00000001 c1957_tcontrol 0x00000000",
        ] {
            assert!(parse_spike_log(line).is_err(), "{line:?}");
        }
    }

    /// The end of a pinned-Spike `-l` log of a passing M2 program: `write_tohost`, then
    /// the `ECALL`, whose trap has no tval line.
    const M2_PASS_END: &str = "\
core   0: 0x80000048 (0x00010f17) auipc   t5, 0x10
core   0: 3 0x80000048 (0x00010f17) x30 0x80010048
core   0: 0x8000004c (0xfa0f2e23) sw      zero, -68(t5)
core   0: 3 0x8000004c (0xfa0f2e23) mem 0x80010004 0x00000000
core   0: 0x80000050 (0x00000073) ecall
core   0: exception trap_machine_ecall, epc 0x80000050
";

    #[test]
    fn a_passing_l_log_ends_with_the_ecall() {
        let retires = pass_end(M2_PASS_END).unwrap();
        assert_eq!(retires.len(), 2);
        assert_eq!(
            retires[1].mem,
            Mem::Store {
                addr: 0x8001_0004,
                width: 4,
                value: 0
            }
        );
        let (_, trap) = parse_spike_l_log(M2_PASS_END).unwrap();
        assert_eq!(
            trap,
            Some(SpikeTrap {
                pc: 0x8000_0050,
                insn: 0x73,
                cause: "trap_machine_ecall".to_owned(),
                tval: 0
            })
        );
        let lines: Vec<&str> = M2_PASS_END.lines().collect();
        let join = |lines: &[&str]| lines.iter().map(|l| format!("{l}\n")).collect::<String>();
        // No ECALL, a line after it, or another trap there.
        assert!(pass_end(&join(&lines[..4])).is_err());
        assert!(pass_end(&join(&lines[..5])).is_err());
        assert!(pass_end(&format!("{M2_PASS_END}core   0: >>>>  trap_vector\n")).is_err());
        let illegal = join(
            &[
                &lines[..4],
                &[
                    "core   0: 0x80000050 (0x30202573) csrr    a0, medeleg",
                    "core   0: exception trap_illegal_instruction, epc 0x80000050",
                    "core   0:           tval 0x30202573",
                ],
            ]
            .concat(),
        );
        assert!(parse_spike_l_log(&illegal).unwrap().1.is_some());
        assert!(pass_end(&illegal).is_err());
        // Only an ECALL goes without a tval line.
        let untold = join(
            &[
                &lines[..4],
                &[
                    "core   0: 0x80000050 (0x30202573) csrr    a0, medeleg",
                    "core   0: exception trap_illegal_instruction, epc 0x80000050",
                ],
            ]
            .concat(),
        );
        assert!(parse_spike_l_log(&untold).is_err());
    }

    /// SystemScope's CSR commits read back as the whitelisted CSR writes: a suppressed
    /// write is none, and `MRET` records none.
    #[test]
    fn csr_commits_in_the_trace_are_csr_writes() {
        let p = &csrgen::pass_programs()[0];
        let image = load(&p.name, &p.elf).unwrap();
        let finished = runner::execute(
            runner::platform_with_profile(&image, false, runner::SEED, Rv32iProfile::M2),
            Start::Init { traced: true },
            Vec::new(),
        );
        let stream = our_stream(&finished).unwrap();
        assert_eq!(stream.retires.len(), p.code.len() - 1);
        for (r, &word) in stream.retires.iter().zip(&p.code) {
            let expected = match systemscope_rv32i::decode_privileged(word) {
                Some(instr @ systemscope_rv32i::PrivInstr::Csr { csr, .. }) if instr.writes() => {
                    Some(csr)
                }
                _ => None,
            };
            assert_eq!(r.csr.map(|c| c.0), expected, "{r}");
        }
        // csrrw a0, mscratch, x0 after mscratch = 0x12345678.
        assert_eq!(stream.retires[3].reg_write, Some((10, 0x1234_5678)));
        assert_eq!(stream.retires[3].csr, Some((0x340, 0)));
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

    const PROGEN_LOG: &str = include_str!("../spike/progen-0000000000000000.log");
    const TRAP_LOG: &str = include_str!("../spike/misaligned-lw-1.log");

    #[test]
    fn the_committed_program_logs_are_where_verify_looks() {
        let root = workspace_root();
        let progen = fs::read_to_string(root.join(super::PROGEN_LOG)).unwrap();
        assert_eq!(progen, PROGEN_LOG);
        let trap = fs::read_to_string(root.join(super::TRAP_LOG)).unwrap();
        assert_eq!(trap, TRAP_LOG);
        assert_eq!(TRAP_LOG.lines().count(), TRAP_LOG_LINES);
        assert_eq!(MISALIGNED[TRAP_LOG_CASE].name, "lw-1");
    }

    fn traced(program: &Program) -> Finished {
        let image = load(&program.name, &program.elf).unwrap();
        runner::execute(
            runner::platform(&image, false),
            Start::Init { traced: true },
            Vec::new(),
        )
    }

    /// Seed 0's generated program against its committed Spike log, with no Spike needed.
    #[test]
    fn generated_seed_0_matches_its_committed_spike_log() {
        let program = progen::generate(0);
        let tohost = elf_symbol(&program.elf, "tohost").unwrap();
        let theirs = parse_spike_log(PROGEN_LOG).unwrap();
        let finished = traced(&program);
        assert_eq!(
            judge_pass(&finished, &theirs, RAM_BASE, tohost),
            Ok(theirs.len())
        );
        // Another program's run is not this log's.
        let other = traced(&progen::generate(1));
        assert!(judge_pass(&other, &theirs, RAM_BASE, tohost).is_err());
        // Nor is a log cut before the boundary, or one ending at another tohost.
        let cut = &theirs[..theirs.len() - 1];
        assert!(judge_pass(&finished, cut, RAM_BASE, tohost).is_err());
        assert!(judge_pass(&finished, &theirs, RAM_BASE, tohost + 8).is_err());
        assert!(judge_pass(&finished, &theirs, RAM_BASE + 4, tohost).is_err());
        // SystemScope's end must follow: the ECALL after the boundary, `instret`, and the
        // registers Spike's writes leave.
        let mut finished = finished;
        let doctored = |finished: &mut Finished, doctor: &dyn Fn(&mut Finished)| {
            doctor(finished);
            judge_pass(finished, &theirs, RAM_BASE, tohost)
        };
        let end = finished.outcome.end.clone();
        let End::Trap { cause, pc, tval } = end.clone() else {
            panic!("{end}")
        };
        let moved = End::Trap {
            cause,
            pc: pc + 4,
            tval,
        };
        assert!(doctored(&mut finished, &|f| f.outcome.end = moved.clone()).is_err());
        finished.outcome.end = end;
        assert!(doctored(&mut finished, &|f| f.outcome.instret += 1).is_err());
        finished.outcome.instret -= 1;
        assert!(doctored(&mut finished, &|f| bump(f, "x8")).is_err());
        bump(&mut finished, "x8");
        assert!(judge_pass(&finished, &theirs, RAM_BASE, tohost).is_ok());
    }

    /// Flips bit 0 of register `name` in the CPU's final view, doing or undoing a change
    /// only the end-state check sees.
    fn bump(finished: &mut Finished, name: &str) {
        let view = &mut finished.views[CPU.0 as usize];
        let (_, value) = view.fields.iter_mut().find(|(n, _)| *n == name).unwrap();
        let Value::U64(v) = value else {
            panic!("{name}")
        };
        *v ^= 1;
    }

    fn lw_1() -> (Finished, Vec<Retire>, SpikeTrap, ExpectedTrap) {
        let (program, expected) = progen::misaligned(&MISALIGNED[TRAP_LOG_CASE]);
        let (theirs, trap) = parse_spike_trap_log(TRAP_LOG).unwrap();
        (traced(&program), theirs, trap, expected)
    }

    #[test]
    fn the_trap_log_parses_to_the_trap() {
        let (_, theirs, trap, _) = lw_1();
        assert_eq!(theirs.len(), 5);
        assert_eq!(
            theirs[4],
            retire(
                0x8000_0010,
                0x0002_a403,
                Some((8, 0x5a5)),
                Mem::Load { addr: 0x8001_0100 }
            )
        );
        assert_eq!(
            trap,
            SpikeTrap {
                pc: 0x8000_0014,
                insn: 0x0012_a383,
                cause: "trap_load_address_misaligned".to_owned(),
                tval: 0x8001_0101,
            }
        );
    }

    #[test]
    fn a_trap_log_out_of_shape_is_an_error() {
        let lines: Vec<&str> = TRAP_LOG.lines().collect();
        let join = |lines: &[&str]| lines.join("\n");
        // No trap at all, or cut inside one.
        assert!(parse_spike_trap_log(&join(&lines[..10])).is_err());
        assert!(parse_spike_trap_log(&join(&lines[..11])).is_err());
        assert!(parse_spike_trap_log(&join(&lines[..12])).is_err());
        assert!(parse_spike_trap_log("").is_err());
        let doctored = |i: usize, line: &str| {
            let mut lines = lines.clone();
            lines[i] = line;
            parse_spike_trap_log(&join(&lines))
        };
        // A commit of another instruction than the line before it.
        assert!(
            doctored(
                3,
                "core   0: 3 0x80000010 (0x0062a023) mem 0x80010100 0x000005a5"
            )
            .is_err()
        );
        // An instruction line without its disassembly, or not an instruction line.
        assert!(doctored(2, "core   0: 0x80000008 (0x5a500313) ").is_err());
        assert!(doctored(2, "core   1: 0x80000008 (0x5a500313) li t1, 1445").is_err());
        // A trap of another instruction, not a trap, or with no tval.
        assert!(
            doctored(
                11,
                "core   0: exception trap_load_address_misaligned, epc 0x80000010"
            )
            .is_err()
        );
        assert!(doctored(11, "core   0: exception interrupt_m_timer, epc 0x80000014").is_err());
        assert!(doctored(11, "core   0: exception trap_load_address_misaligned").is_err());
        assert!(doctored(12, "core   0:           epc 0x80010101").is_err());
        assert!(doctored(12, "core   0:           tval 0x8001010").is_err());
    }

    /// `lw-1` against its committed Spike log, with no Spike needed, and every way the
    /// trap verdict can go wrong.
    #[test]
    fn a_misaligned_program_matches_its_committed_trap() {
        let (finished, theirs, trap, expected) = lw_1();
        assert_eq!(judge_trap(&finished, &theirs, &trap, &expected), Ok(5));
        let wrong = |doctor: &dyn Fn(&mut SpikeTrap, &mut ExpectedTrap)| {
            let (mut trap, mut expected) = (trap.clone(), expected.clone());
            doctor(&mut trap, &mut expected);
            judge_trap(&finished, &theirs, &trap, &expected)
        };
        // Spike traps otherwise.
        assert!(wrong(&|t, _| t.cause = "trap_load_access_fault".to_owned()).is_err());
        assert!(wrong(&|t, _| t.tval += 1).is_err());
        assert!(wrong(&|t, _| t.pc += 4).is_err());
        assert!(wrong(&|t, _| t.insn ^= 1 << 20).is_err());
        // The expectation is otherwise: SystemScope's trap no longer agrees.
        let both = |cause: &'static str| {
            wrong(&move |t, e| {
                e.cause = cause;
                t.cause = SPIKE_CAUSES
                    .iter()
                    .find(|c| c.0 == cause)
                    .unwrap()
                    .1
                    .to_owned();
            })
        };
        assert!(both("LoadAccessFault").is_err());
        assert!(
            wrong(&|t, e| {
                e.tval += 4;
                t.tval += 4;
            })
            .is_err()
        );
        assert!(wrong(&|_, e| e.cause = "NoSuchCause").is_err());
        // Spike's stream is short of the trap, or stores another value: the traps and end
        // registers agree, and only the stream differs.
        assert!(judge_trap(&finished, &theirs[..4], &trap, &expected).is_err());
        let mut stored = theirs.clone();
        let Mem::Store { value, .. } = &mut stored[3].mem else {
            panic!("{}", stored[3])
        };
        *value ^= 1;
        assert!(judge_trap(&finished, &stored, &trap, &expected).is_err());
        // SystemScope's `instret` or registers disagree with Spike's stream.
        let mut finished = finished;
        finished.outcome.instret += 1;
        assert!(judge_trap(&finished, &theirs, &trap, &expected).is_err());
        finished.outcome.instret -= 1;
        bump(&mut finished, "x8");
        assert!(judge_trap(&finished, &theirs, &trap, &expected).is_err());
        bump(&mut finished, "x8");
        assert_eq!(judge_trap(&finished, &theirs, &trap, &expected), Ok(5));
        // SystemScope's run is not this program's.
        let other = traced(&progen::misaligned(&MISALIGNED[TRAP_LOG_CASE + 1]).0);
        assert!(judge_trap(&other, &theirs, &trap, &expected).is_err());
        // A passing program's run is no trap program's.
        assert!(judge_trap(&traced(&progen::generate(0)), &theirs, &trap, &expected).is_err());
    }

    /// A fetch fault has no instruction line in Spike's log: the trap line stands alone,
    /// and is read as a trap of word 0 at its `epc`, delegated or taken in M.
    #[test]
    fn a_standalone_fetch_fault_line_is_a_trap_of_word_0() {
        let log = "core   0: 0x800002ec (0x000300e7) jalr    t1
core   0: 1 0x800002ec (0x000300e7) x1  0x800002f0
core   0: exception trap_instruction_page_fault, epc 0x4040c000
core   0:           tval 0x4040c000
core   0: 0x80000374 (0x14202e73) csrr    t3, scause
core   0: 1 0x80000374 (0x14202e73) x28 0x0000000c
core   0: exception trap_instruction_access_fault, epc 0x40000000
core   0:           tval 0x40000000
core   0: 0x800002f0 (0x0ff0000f) fence   iorw,iorw
core   0: 3 0x800002f0 (0x0ff0000f)
";
        let (events, trap) = parse_spike_m3_log(log).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[1],
            Event::Exception {
                pc: 0x4040_c000,
                insn: 0,
                cause: "InstructionPageFault".to_owned(),
                tval: 0x4040_c000,
            }
        );
        assert_eq!(
            trap,
            SpikeTrap {
                pc: 0x4000_0000,
                insn: 0,
                cause: "trap_instruction_access_fault".to_owned(),
                tval: 0x4000_0000,
            }
        );
        // Only a fetch fault stands alone.
        let load = log.replace(
            "trap_instruction_page_fault, epc 0x4040c000",
            "trap_load_page_fault, epc 0x4040c000",
        );
        assert!(parse_spike_m3_log(&load).is_err());
        // Its epc must be a word address.
        let bad = log.replace("epc 0x4040c000", "epc 0x4040c00");
        assert!(parse_spike_m3_log(&bad).is_err());
    }

    #[test]
    fn every_trap_cause_has_its_spike_name() {
        let names: Vec<&str> = SPIKE_CAUSES.iter().map(|c| c.0).collect();
        for case in &MISALIGNED {
            assert!(names.contains(&case.cause), "{}", case.cause);
        }
        assert!(names.contains(&PASS_CAUSE));
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), SPIKE_CAUSES.len());
        assert!(SPIKE_CAUSES.iter().all(|c| c.1.starts_with("trap_")));
        assert_eq!(spike_trap_args(RAM_BASE, "e", "l")[0], "-l");
        assert_eq!(
            spike_trap_args(RAM_BASE, "e", "l")[1..],
            spike_args(RAM_BASE, "e", "l")
        );
    }
}
