//! M3 snapshot, determinism, and golden acceptance (`docs/m3-design.md` §9, §15.1,
//! §17.4) on `m3-reference` running the M3 scenario, and the golden files that `cargo
//! xtask m3-golden bless` writes.
//!
//! The workload is exactly M3.6's: the committed firmware and disks, each checked against
//! the M3 manifest, on the frozen `m3-reference` built by
//! [`systemscope_rv32::m3ref::build`] and run under its event watchdog
//! ([`systemscope_rv32::m3::EVENT_BUDGET`]).
//!
//! As in M0 to M2, every check returns a `Result` instead of asserting, so the tests can
//! also feed it doctored inputs and prove that it notices them.

use std::path::Path;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::Observer;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord};
use systemscope_elf::LoadImage;
use systemscope_runtime::runtime::Runtime;
use systemscope_runtime::trace::Trace;
use systemscope_rv32::m3::{self, M3Manifest, M3Run, Scenario};
use systemscope_rv32::m3ref::{self, Config};
use systemscope_rv32::runner::Start;

pub mod checkpoint;
pub mod golden;
pub mod observation;
pub mod portable;

/// The scenario's platform, as the golden file records it.
pub const SCENARIO: &str = "m3-reference";

/// A committed M3 scenario: the firmware, its disk, and the expected `os.*` records, read
/// through the M3 manifest.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// Which disk.
    pub scenario: Scenario,
    /// BLAKE3 of the firmware ELF, as the manifest pins it.
    pub firmware_blake3: [u8; 32],
    /// The loaded firmware.
    pub firmware: LoadImage,
    /// BLAKE3 of the disk, as the manifest pins it: the media's `image_hash`.
    pub disk_blake3: [u8; 32],
    /// The raw disk.
    pub disk: Vec<u8>,
    /// The committed `os.*` records.
    pub expected: String,
}

impl Fixture {
    /// Reads the committed fixture of `scenario` under `root`; fails unless every file is
    /// exactly the one the manifest names.
    pub fn read(root: &Path, scenario: Scenario) -> Result<Fixture, String> {
        let manifest = M3Manifest::read(root)?;
        let (firmware, disk, expected) = m3::read_fixture(root, scenario)?;
        let disk_blake3 = manifest
            .disks
            .iter()
            .find(|d| d.file == scenario.disk_file())
            .ok_or_else(|| format!("the manifest has no {}", scenario.disk_file()))?
            .blake3;
        Ok(Fixture {
            scenario,
            firmware_blake3: manifest.firmware.blake3,
            firmware,
            disk_blake3,
            disk,
            expected,
        })
    }

    /// The frozen `m3-reference` with this disk, elaborated.
    ///
    /// # Panics
    ///
    /// If the frozen platform does not build, which M3.6 rules out.
    pub fn platform(&self) -> Runtime {
        m3ref::build(&self.firmware, &self.disk, &Config::frozen())
            .expect("the frozen m3-reference builds")
    }

    /// Runs the scenario from `start` to its end on a fresh frozen platform, under the
    /// watchdog. `observers` only read.
    pub fn run(&self, start: Start, observers: Vec<Box<dyn Observer>>) -> M3Run {
        m3::run(&self.firmware, &self.disk, start, observers)
    }

    /// Runs the scenario traced from `init` and judges it by §12.3, the `os.*` records
    /// included.
    pub fn judged_run(&self, observers: Vec<Box<dyn Observer>>) -> Result<M3Run, String> {
        let run = self.run(Start::Init { traced: true }, observers);
        m3::judge(&run, self.scenario, &self.expected)?;
        Ok(run)
    }
}

/// The records `component` emitted with `kind`.
pub fn records<'a>(
    trace: &'a Trace,
    component: ComponentId,
    kind: &'a str,
) -> impl Iterator<Item = &'a TraceRecord> {
    trace
        .records
        .iter()
        .filter(move |r| r.origin == TraceOrigin::Component && r.component == component)
        .filter(move |r| r.kind == kind)
}
