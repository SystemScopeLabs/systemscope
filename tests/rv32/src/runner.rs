//! Runs the `rv32ui` fixtures and applies the pass rule (`docs/m1-design.md` §10.2).
//!
//! Every fixture runs alone on the same platform: `m1-reference` (§9) without the UART,
//! which no `rv32ui` test touches. `hello.elf` runs on the whole `m1-reference`, UART
//! included ([`platform`] with `uart`, as [`crate::hello`] uses it).
//!
//! ```text
//! soc.cpu0  Rv32iCpu (100 MHz, entry = ELF entry, max_instructions 10,000,000)
//!   │ mem ─▶ cpu          link Cycles { cpu, 1 }
//! soc.bus   AddressBus
//!   ├─ ram  ─▶ mem        link Cycles { cpu, 1 }
//!   └─ uart ─▶ mem        link Cycles { cpu, 1 }   (with the UART only)
//! soc.ram   Ram           base 0x8000_0000, 16 MiB, responds Cycles { cpu, 0 }, the ELF image
//! soc.uart  SimpleUart    base 0x1000_0000, 8 bytes, responds Cycles { cpu, 0 }
//! ```
//!
//! The `M2` CPU profile runs the same programs on the same platform. Its second port,
//! `irq`, is linked (`Cycles { cpu, 1 }`) to `soc.irq_low`, a [`TiedLowIrq`] added last:
//! a test-only `irq.v0` source that never asserts its line and never sends, so the CPU
//! executes exactly as without it (`docs/m2-design.md` §6.1, §7.1). The runtime requires
//! every port to be linked.
//!
//! The `M3` profile runs them too, linked the same way. It starts in M-mode, where an
//! RV32I program runs as with `M2`; only its name for the `ECALL` cause differs
//! (`EnvironmentCallFromM`, `docs/m3-design.md` §5.3), so the pass rule takes the profile.
//!
//! **Pass rule:** the CPU halts with `Trap(EnvironmentCall)` (`EnvironmentCallFromM` with
//! the `M3` profile), `gp == 1`, and `a0 == 0`.
//! Anything else fails, including the instruction limit, a runtime fault, or a run that
//! ends without a halt.

use std::cell::RefCell;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::rc::Rc;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::irq_v0;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_platform::{
    AddressBus, Ram, RamConfig, RamImage, Region, Segment, SimpleUart, UartConfig, uart,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

use crate::manifest::{Fixture, Manifest};
use crate::{MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, UART_BASE, hex};

/// The CPU clock of `m1-reference`.
pub const CPU_HZ: u64 = 100_000_000;
/// The value `gp` holds after `RVTEST_PASS`.
pub const PASS_GP: u32 = 1;
/// The value `a0` holds after `RVTEST_PASS`.
pub const PASS_A0: u32 = 0;
/// The trap cause `RVTEST_PASS` and `RVTEST_FAIL` end with.
pub const PASS_CAUSE: &str = "EnvironmentCall";
/// [`PASS_CAUSE`] as the `M3` profile names it.
pub const PASS_CAUSE_M3: &str = "EnvironmentCallFromM";

/// The pass cause of `profile`.
pub fn pass_cause(profile: Rv32iProfile) -> &'static str {
    match profile {
        Rv32iProfile::M1 | Rv32iProfile::M2 => PASS_CAUSE,
        Rv32iProfile::M3 => PASS_CAUSE_M3,
    }
}

/// The session seed of `m1-reference` (§9).
pub const SEED: u64 = 0;

/// `soc.cpu0`.
pub const CPU: ComponentId = ComponentId(0);
/// `soc.bus`.
pub const BUS: ComponentId = ComponentId(1);
/// `soc.ram`.
pub const RAM: ComponentId = ComponentId(2);
/// `soc.uart`, on a platform with the UART.
pub const UART: ComponentId = ComponentId(3);

/// A test-only `irq.v0` source whose line is always deasserted: it never sends, since a
/// source sends no initial `Level { asserted: false }` (`docs/m2-design.md` §7.1). It
/// only lets an `M2` CPU's `irq` port be linked where no interrupt source exists. It has
/// no state; any event it receives faults the session.
pub struct TiedLowIrq;

impl Component for TiedLowIrq {
    fn type_name(&self) -> &'static str {
        "test.irq_tied_low"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Err(SimError::ComponentFault(
            "tied-low irq: a source receives nothing",
        ))
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, schema: u32) -> Result<(), RestoreError> {
        if schema == 1 {
            Ok(())
        } else {
            Err(RestoreError::InvalidState("tied-low irq: unknown schema"))
        }
    }
}

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
    judge_with(outcome, Rv32iProfile::M1)
}

/// [`judge`] for a run with the CPU in `profile`.
pub fn judge_with(outcome: &Outcome, profile: Rv32iProfile) -> Result<(), String> {
    let pass = pass_cause(profile);
    match &outcome.end {
        End::Trap { cause, .. } if cause == pass => {}
        other => return Err(format!("ended with {other}, not Trap({pass})")),
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

/// Keeps every component's view after every event, and passes the rest on.
struct Probe(Rc<RefCell<Vec<StateView>>>);

impl Observer for Probe {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        *self.0.borrow_mut() = (0..world.component_count())
            .map(|i| {
                let id = ComponentId(u32::try_from(i).expect("a handful of components"));
                world.inspect(id).unwrap_or_default()
            })
            .collect();
        Control::Continue
    }
}

/// `m1-reference` (§9) for `image`, elaborated, with the UART if `uart`. The components
/// are [`CPU`], [`BUS`], [`RAM`], and [`UART`], in that order. The session seed is
/// [`SEED`].
pub fn platform(image: &LoadImage, uart: bool) -> Runtime {
    platform_with_seed(image, uart, SEED)
}

/// [`platform`] with another session seed. Nothing in `m1-reference` draws from the
/// random streams, so the seed reaches only the session information, and with it the
/// snapshot and the trace header.
pub fn platform_with_seed(image: &LoadImage, uart: bool, seed: u64) -> Runtime {
    platform_with_profile(image, uart, seed, Rv32iProfile::M1)
}

/// [`platform_with_seed`] with the CPU in `profile`. `m1-reference` is the `M1` profile;
/// the `M2` and `M3` profiles run the same RV32I programs on the same platform
/// (`docs/m2-design.md` §6.1, §15.4), with their `irq` port linked to a [`TiedLowIrq`]
/// added after the other components.
pub fn platform_with_profile(
    image: &LoadImage,
    uart: bool,
    seed: u64,
    profile: Rv32iProfile,
) -> Runtime {
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
        profile,
    })
    .expect("the loader guarantees an aligned entry");
    let cpu = t.add_component("soc.cpu0", Box::new(cpu));
    let mut regions = vec![Region {
        name: "ram",
        base: u64::from(RAM_BASE),
        size: u64::from(RAM_SIZE),
    }];
    if uart {
        regions.push(Region {
            name: "uart",
            base: u64::from(UART_BASE),
            size: uart::SIZE,
        });
    }
    let bus = AddressBus::new(regions).expect("disjoint regions");
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
    if uart {
        let device = SimpleUart::new(UartConfig {
            latency: LinkLatency::Cycles {
                domain: cpu_clock,
                k: 0,
            },
        });
        let device = t.add_component("soc.uart", Box::new(device));
        t.connect((bus, "uart"), (device, "mem"), Some(link));
    }
    if profile != Rv32iProfile::M1 {
        let irq = t.add_component("soc.irq_low", Box::new(TiedLowIrq));
        t.connect((irq, "irq"), (cpu, "irq"), Some(link));
    }
    t.elaborate(SessionConfig {
        seed,
        ..SessionConfig::default()
    })
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
    execute(platform(image, false), Start::Init { traced }, observers).outcome
}

/// How [`execute`] starts a session.
#[derive(Debug)]
pub enum Start {
    /// A new session: `init`, recording a trace if `traced`.
    Init {
        /// Whether to record a trace.
        traced: bool,
    },
    /// A checkpoint: `restore` the snapshot, then continue the trace `prefix`, if any.
    Restore {
        /// A snapshot of the same platform.
        snapshot: Vec<u8>,
        /// The trace recorded up to the snapshot.
        prefix: Option<Trace>,
    },
}

/// A session run to its end.
#[derive(Debug)]
pub struct Finished {
    /// How it ended, and its digests.
    pub outcome: Outcome,
    /// Every component's view after the last event, by `ComponentId`; empty if no event
    /// ran.
    pub views: Vec<StateView>,
    /// The events dispatched after the start, in order.
    pub dispatched: Vec<Dispatched>,
    /// The trace, if one was recorded.
    pub trace: Option<Trace>,
}

/// Starts the elaborated `rt` as `start` says and runs it until it has no more events.
/// `observers` are added after the runner's own, which only reads.
pub fn execute(rt: Runtime, start: Start, observers: Vec<Box<dyn Observer>>) -> Finished {
    execute_bounded(rt, start, observers, None).0
}

/// [`execute`], stopped with [`End::Fault`] once `budget` events have run after the
/// start, if a budget is given, and returning the session as well, so the caller can take
/// its final snapshot. A budget bounds what a diverging program records.
pub fn execute_bounded(
    mut rt: Runtime,
    start: Start,
    observers: Vec<Box<dyn Observer>>,
    budget: Option<u64>,
) -> (Finished, Runtime) {
    let last = Rc::new(RefCell::new(Vec::new()));
    rt.add_observer(Box::new(Probe(Rc::clone(&last))));
    for observer in observers {
        rt.add_observer(observer);
    }
    let mut fault = match start {
        Start::Init { traced } => {
            if traced {
                rt.start_trace().expect("tracing starts before init");
            }
            rt.init().err().map(|e| e.to_string())
        }
        Start::Restore { snapshot, prefix } => match rt.restore(&snapshot) {
            Err(e) => Some(e.to_string()),
            Ok(()) => prefix.and_then(|p| rt.resume_trace(p).err().map(|e| format!("{e:?}"))),
        },
    };
    let mut dispatched = Vec::new();
    while fault.is_none() {
        if let Some(budget) = budget.filter(|&b| dispatched.len() as u64 >= b) {
            fault = Some(format!("stopped at the budget of {budget} events"));
            break;
        }
        match rt.step() {
            Ok(Some(ev)) => dispatched.push(ev),
            Ok(None) => break,
            Err(e) => fault = Some(e.to_string()),
        }
    }
    if let Some(e) = rt.fault() {
        fault = Some(e.to_string());
    }
    let views = last.take();
    let view = views.get(CPU.0 as usize).cloned().unwrap_or_default();
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
    let state = rt.state_digest().ok();
    let execution = rt.execution_digest();
    let trace = rt.take_trace();
    let outcome = Outcome {
        state,
        execution,
        trace: trace.as_ref().map(Trace::digest),
        end,
        gp: word("x3"),
        a0: word("x10"),
        pc: word("pc"),
        instret: reg("instret"),
        events: dispatched.len() as u64,
    };
    let finished = Finished {
        outcome,
        views,
        dispatched,
        trace,
    };
    (finished, rt)
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
    run_fixture_with(root, fixture, Rv32iProfile::M1)
}

/// [`run_fixture`] with the CPU in `profile`.
pub fn run_fixture_with(
    root: &Path,
    fixture: &Fixture,
    profile: Rv32iProfile,
) -> Result<FixtureResult, String> {
    let image = fixture.read(root)?;
    let rt = platform_with_profile(&image, false, SEED, profile);
    let outcome = execute(rt, Start::Init { traced: true }, Vec::new()).outcome;
    Ok(FixtureResult {
        name: fixture.name.clone(),
        blake3: fixture.blake3,
        image_hash: image.image_hash,
        verdict: judge_with(&outcome, profile),
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
    run_suite_with(root, Rv32iProfile::M1)
}

/// [`run_suite`] with the CPU in `profile`.
pub fn run_suite_with(root: &Path, profile: Rv32iProfile) -> Result<Report, String> {
    let manifest = Manifest::read(root)?;
    manifest.ensure_selection()?;
    let mut report = Report {
        selected: manifest.selected.len(),
        results: Vec::new(),
        errors: Vec::new(),
    };
    for fixture in &manifest.selected {
        match run_fixture_with(root, fixture, profile) {
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
