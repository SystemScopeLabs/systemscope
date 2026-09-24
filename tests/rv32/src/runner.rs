//! Runs the `rv32ui` fixtures and applies the pass rule (`docs/m1-design.md` §10.2).
//!
//! Every fixture runs alone on the same platform: `m1-reference` (§9) without the UART,
//! which no `rv32ui` test touches.
//!
//! ```text
//! soc.cpu0  Rv32iCpu (100 MHz, entry = ELF entry, max_instructions 10,000,000)
//!   │ mem ─▶ cpu          link Cycles { cpu, 1 }
//! soc.bus   AddressBus
//!   └─ ram ─▶ mem         link Cycles { cpu, 1 }
//! soc.ram   Ram           base 0x8000_0000, 16 MiB, responds Cycles { cpu, 0 }, the ELF image
//! ```
//!
//! **Pass rule:** the CPU halts with `Trap(EnvironmentCall)`, `gp == 1`, and `a0 == 0`.
//! Anything else fails, including the instruction limit, a runtime fault, or a run that
//! ends without a halt.

use std::cell::RefCell;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::rc::Rc;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::time::{Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, Segment};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu};

use crate::manifest::{Fixture, Manifest};
use crate::{MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, hex};

/// The CPU clock of `m1-reference`.
pub const CPU_HZ: u64 = 100_000_000;
/// The value `gp` holds after `RVTEST_PASS`.
pub const PASS_GP: u32 = 1;
/// The value `a0` holds after `RVTEST_PASS`.
pub const PASS_A0: u32 = 0;
/// The trap cause `RVTEST_PASS` and `RVTEST_FAIL` end with.
pub const PASS_CAUSE: &str = "EnvironmentCall";

/// How the run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// The CPU halted on a trap.
    Trap {
        /// The trap cause's name, such as `EnvironmentCall`.
        cause: String,
        /// The trapping instruction's `pc`.
        pc: u32,
        /// The trap value.
        tval: u32,
    },
    /// The CPU retired [`MAX_INSTRUCTIONS`] instructions.
    InstructionLimit,
    /// The session faulted.
    Fault(String),
    /// The runtime ran out of events without the CPU halting.
    NotHalted,
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            End::Trap { cause, pc, tval } => {
                write!(f, "Trap({cause}) at pc {pc:#010x}, tval {tval:#010x}")
            }
            End::InstructionLimit => write!(f, "InstructionLimit"),
            End::Fault(e) => write!(f, "fault: {e}"),
            End::NotHalted => write!(f, "no halt"),
        }
    }
}

/// What a run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// How it ended.
    pub end: End,
    /// `gp` (`x3`) at the end.
    pub gp: u32,
    /// `a0` (`x10`) at the end.
    pub a0: u32,
    /// `pc` at the end.
    pub pc: u32,
    /// Instructions retired.
    pub instret: u64,
    /// Events dispatched.
    pub events: u64,
    /// `StateDigest` at the end; `None` if the session faulted.
    pub state: Option<[u8; 32]>,
    /// `ExecutionDigest` at the end.
    pub execution: [u8; 32],
    /// `TraceDigest`, if the run was traced.
    pub trace: Option<[u8; 32]>,
}

/// The pass rule: `Ok` if the test passed, otherwise why it failed.
pub fn judge(outcome: &Outcome) -> Result<(), String> {
    match &outcome.end {
        End::Trap { cause, .. } if cause == PASS_CAUSE => {}
        other => return Err(format!("ended with {other}, not Trap({PASS_CAUSE})")),
    }
    if outcome.gp != PASS_GP || outcome.a0 != PASS_A0 {
        return Err(format!(
            "gp = {:#x}, a0 = {:#x} (RVTEST_FAIL at test number {})",
            outcome.gp,
            outcome.a0,
            outcome.a0 >> 1
        ));
    }
    Ok(())
}

/// Keeps the CPU's view after every event, and passes the rest on.
struct Probe(Rc<RefCell<Option<StateView>>>);

impl Observer for Probe {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        *self.0.borrow_mut() = world.inspect(ComponentId(0));
        Control::Continue
    }
}

/// The platform for `image`, elaborated. The CPU is `ComponentId(0)`.
fn platform(image: &LoadImage) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let cpu_clock = t
        .add_clock(
            Frequency::from_hz(CPU_HZ).expect("100 MHz is a valid frequency"),
            Tick::ZERO,
            Rounding::Floor,
        )
        .expect("100 MHz divides the simulation clock");
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock: cpu_clock,
        entry: image.entry,
        max_instructions: NonZeroU64::new(MAX_INSTRUCTIONS).expect("nonzero"),
    })
    .expect("the loader guarantees an aligned entry");
    let cpu = t.add_component("soc.cpu0", Box::new(cpu));
    let bus = AddressBus::new(vec![Region {
        name: "ram",
        base: u64::from(RAM_BASE),
        size: u64::from(RAM_SIZE),
    }])
    .expect("one region");
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: LinkLatency::Cycles {
                domain: cpu_clock,
                k: 0,
            },
        },
        &ram_image(image),
    )
    .expect("the loader checked the image against the RAM");
    let ram = t.add_component("soc.ram", Box::new(ram));
    let link = LinkLatency::Cycles {
        domain: cpu_clock,
        k: 1,
    };
    t.connect((cpu, "mem"), (bus, "cpu"), Some(link));
    t.connect((bus, "ram"), (ram, "mem"), Some(link));
    t.elaborate(SessionConfig::default())
        .expect("the platform elaborates")
}

/// The load image as the RAM's initial image: the same offsets, bytes, and hash.
fn ram_image(image: &LoadImage) -> RamImage {
    RamImage {
        image_hash: image.image_hash,
        segments: image
            .segments
            .iter()
            .map(|s| Segment {
                offset: u64::from(s.offset),
                bytes: s.bytes.clone(),
            })
            .collect(),
    }
}

/// Runs `image` until the runtime has no more events. `traced` records a trace;
/// `observers` are added after the runner's own, which only reads.
pub fn run(image: &LoadImage, traced: bool, observers: Vec<Box<dyn Observer>>) -> Outcome {
    let mut rt = platform(image);
    let last = Rc::new(RefCell::new(None));
    rt.add_observer(Box::new(Probe(Rc::clone(&last))));
    for observer in observers {
        rt.add_observer(observer);
    }
    if traced {
        rt.start_trace().expect("tracing starts before init");
    }
    let mut events = 0;
    let mut fault = rt.init().err().map(|e| e.to_string());
    while fault.is_none() {
        match rt.step() {
            Ok(Some(_)) => events += 1,
            Ok(None) => break,
            Err(e) => fault = Some(e.to_string()),
        }
    }
    if let Some(e) = rt.fault() {
        fault = Some(e.to_string());
    }
    let view = last.take().unwrap_or_default();
    let reg = |name: &str| match view.get(name) {
        Some(Value::U64(v)) => *v,
        _ => 0,
    };
    let word = |name: &str| u32::try_from(reg(name)).expect("32-bit register");
    let end = match (fault, view.get("halt")) {
        (Some(e), _) => End::Fault(e),
        (None, Some(Value::Str(h))) if h == "trap" => End::Trap {
            cause: match view.get("cause") {
                Some(Value::Str(c)) => c.clone(),
                other => format!("{other:?}"),
            },
            pc: word("trap_pc"),
            tval: word("tval"),
        },
        (None, Some(Value::Str(h))) if h == "instruction_limit" => End::InstructionLimit,
        (None, _) => End::NotHalted,
    };
    Outcome {
        state: rt.state_digest().ok(),
        execution: rt.execution_digest(),
        trace: rt.take_trace().map(|t| t.digest()),
        end,
        gp: word("x3"),
        a0: word("x10"),
        pc: word("pc"),
        instret: reg("instret"),
        events,
    }
}

/// One fixture's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureResult {
    /// The test name.
    pub name: String,
    /// The ELF's BLAKE3.
    pub blake3: [u8; 32],
    /// The RAM's `image_hash`.
    pub image_hash: [u8; 32],
    /// What the traced run produced.
    pub outcome: Outcome,
    /// The pass rule's verdict.
    pub verdict: Result<(), String>,
}

impl FixtureResult {
    /// One line: the verdict and everything needed to diagnose a failure.
    pub fn line(&self) -> String {
        let o = &self.outcome;
        let verdict = match &self.verdict {
            Ok(()) => "PASS".to_owned(),
            Err(why) => format!("FAIL ({why})"),
        };
        format!(
            "{:<6} {verdict}: {}; gp {:#x}, a0 {:#x}, pc {:#010x}, instret {}, events {}, \
             elf {}..",
            self.name,
            o.end,
            o.gp,
            o.a0,
            o.pc,
            o.instret,
            o.events,
            &hex(&self.blake3)[..16],
        )
    }
}

/// Reads `fixture` under `root`, checks it against its manifest entry, and runs it traced.
pub fn run_fixture(root: &Path, fixture: &Fixture) -> Result<FixtureResult, String> {
    let image = fixture.read(root)?;
    let outcome = run(&image, true, Vec::new());
    Ok(FixtureResult {
        name: fixture.name.clone(),
        blake3: fixture.blake3,
        image_hash: image.image_hash,
        verdict: judge(&outcome),
        outcome,
    })
}

/// The whole suite's results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Tests the manifest selects.
    pub selected: usize,
    /// One result per test run, in manifest order.
    pub results: Vec<FixtureResult>,
    /// Tests that could not be run, with why.
    pub errors: Vec<String>,
}

impl Report {
    /// Tests that ran.
    pub fn executed(&self) -> usize {
        self.results.len()
    }

    /// Tests that passed.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.verdict.is_ok()).count()
    }

    /// Tests that failed.
    pub fn failed(&self) -> usize {
        self.executed() - self.passed()
    }

    /// M1-A2: `selected == executed == passed == expected` and nothing failed or was
    /// skipped. A test that silently did not run fails this as surely as one that failed.
    pub fn accept(&self, expected: usize) -> Result<(), String> {
        let counts = (self.selected, self.executed(), self.passed(), self.failed());
        if counts == (expected, expected, expected, 0) && self.errors.is_empty() {
            return Ok(());
        }
        let mut msg = format!(
            "expected {expected} selected, executed, and passed; got selected {}, executed {}, \
             passed {}, failed {}",
            counts.0, counts.1, counts.2, counts.3
        );
        for r in self.results.iter().filter(|r| r.verdict.is_err()) {
            msg.push_str(&format!(
                "\n  {} (image_hash {})",
                r.line(),
                hex(&r.image_hash)
            ));
        }
        for e in &self.errors {
            msg.push_str(&format!("\n  not run: {e}"));
        }
        Err(msg)
    }
}

/// Runs every test the committed manifest under `root` selects, after checking the
/// selection itself.
pub fn run_suite(root: &Path) -> Result<Report, String> {
    let manifest = Manifest::read(root)?;
    manifest.ensure_selection()?;
    let mut report = Report {
        selected: manifest.selected.len(),
        results: Vec::new(),
        errors: Vec::new(),
    };
    for fixture in &manifest.selected {
        match run_fixture(root, fixture) {
            Ok(result) => report.results.push(result),
            Err(e) => report.errors.push(e),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> Outcome {
        Outcome {
            end: End::Trap {
                cause: PASS_CAUSE.to_owned(),
                pc: RAM_BASE + 0xa0,
                tval: 0,
            },
            gp: 1,
            a0: 0,
            pc: RAM_BASE + 0xa0,
            instret: 100,
            events: 500,
            state: Some([1; 32]),
            execution: [2; 32],
            trace: Some([3; 32]),
        }
    }

    #[test]
    fn the_pass_sequence_passes() {
        assert_eq!(judge(&passing()), Ok(()));
    }

    #[test]
    fn a_failing_test_number_fails() {
        // RVTEST_FAIL at test 5: gp = a0 = (5 << 1) | 1.
        let o = Outcome {
            gp: 11,
            a0: 11,
            ..passing()
        };
        let err = judge(&o).unwrap_err();
        assert!(err.contains("test number 5"), "{err}");
    }

    #[test]
    fn gp_and_a0_are_both_checked() {
        let o = Outcome { gp: 0, ..passing() };
        assert!(judge(&o).is_err(), "a0 == 0 alone is not a pass");
        let o = Outcome { a0: 3, ..passing() };
        assert!(judge(&o).is_err(), "gp == 1 alone is not a pass");
    }

    #[test]
    fn only_an_environment_call_ends_a_test() {
        let trap = |cause: &str| Outcome {
            end: End::Trap {
                cause: cause.to_owned(),
                pc: RAM_BASE,
                tval: 0,
            },
            ..passing()
        };
        for cause in ["Breakpoint", "IllegalInstruction", "LoadAccessFault"] {
            assert!(judge(&trap(cause)).is_err(), "{cause}");
        }
        for end in [
            End::InstructionLimit,
            End::NotHalted,
            End::Fault("x".to_owned()),
        ] {
            let o = Outcome { end, ..passing() };
            assert!(judge(&o).is_err(), "{o:?}");
        }
    }

    fn report(passed: usize, failed: usize, selected: usize) -> Report {
        let result = |i: usize, verdict| FixtureResult {
            name: format!("t{i}"),
            blake3: [0; 32],
            image_hash: [0; 32],
            outcome: passing(),
            verdict,
        };
        Report {
            selected,
            results: (0..passed)
                .map(|i| result(i, Ok(())))
                .chain((0..failed).map(|i| result(passed + i, Err("no".to_owned()))))
                .collect(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn acceptance_needs_every_selected_test_run_and_passed() {
        assert_eq!(report(40, 0, 40).accept(40), Ok(()));
        // A test that did not run, however green the rest.
        assert!(report(39, 0, 40).accept(40).is_err());
        assert!(report(39, 0, 39).accept(40).is_err());
        // A failure.
        assert!(report(39, 1, 40).accept(40).is_err());
        // A test that could not be read.
        let mut r = report(40, 0, 40);
        r.errors.push("missing".to_owned());
        assert!(r.accept(40).is_err());
    }
}
