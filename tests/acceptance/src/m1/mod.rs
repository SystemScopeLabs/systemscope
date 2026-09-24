//! M1 acceptance (`docs/m1-design.md` §10.1): M1-A6, M1-A7, and M1-A8 on `m1-reference`,
//! and the golden files that `cargo xtask m1-golden bless` writes.
//!
//! The committed programs are `hello.elf`, the reference workload, and the 40 selected
//! `rv32ui` tests. Each runs alone on `m1-reference` as built by
//! [`systemscope_rv32::runner::platform`]: `hello.elf` with the UART, the `rv32ui` tests
//! without it, exactly as M1-A2 and M1-A5 run them.
//!
//! As in M0, every check returns a `Result` instead of asserting, so the tests can also
//! feed it doctored inputs and prove that it notices them.

use std::path::Path;

use systemscope_contracts::component::Delivered;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::Value;
use systemscope_elf::LoadImage;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_rv32::hello::{self, HelloManifest};
use systemscope_rv32::manifest::Manifest;
use systemscope_rv32::runner::{self, BUS, CPU, End, Finished, Start, UART};

pub mod checkpoint;
pub mod golden;
pub mod observation;

/// The scenario's name, as the golden file records it.
pub const SCENARIO: &str = "m1-reference";
/// The reference workload: the program the portable snapshot comes from.
pub const REFERENCE: &str = "hello";
/// The long `rv32ui` program M1-A6 and M1-A7 run besides the reference: the one with the
/// most events, full of loads and stores.
pub const LONG: &str = "ld_st";

/// A committed program, ready to run.
#[derive(Clone, Debug)]
pub struct Program {
    /// `hello`, or the `rv32ui` test name.
    pub name: String,
    /// BLAKE3 of the ELF file.
    pub elf_blake3: [u8; 32],
    /// The loaded image.
    pub image: LoadImage,
    /// Whether it runs with the UART.
    pub uart: bool,
}

impl Program {
    /// `m1-reference` for this program, elaborated, with the session `seed`.
    pub fn platform_with_seed(&self, seed: u64) -> Runtime {
        runner::platform_with_seed(&self.image, self.uart, seed)
    }

    /// `m1-reference` for this program, elaborated.
    pub fn platform(&self) -> Runtime {
        self.platform_with_seed(runner::SEED)
    }

    /// Runs the program from `start` to its end on a fresh platform.
    pub fn execute(&self, start: Start) -> Finished {
        runner::execute(self.platform(), start, Vec::new())
    }
}

/// Every committed program under `root`, each checked against its manifest: `hello`
/// first, then the `rv32ui` selection in manifest order.
pub fn programs(root: &Path) -> Result<Vec<Program>, String> {
    let hello = HelloManifest::read(root)?;
    let mut out = vec![Program {
        name: REFERENCE.to_owned(),
        elf_blake3: hello.blake3,
        image: hello.read_image(root)?,
        uart: true,
    }];
    let manifest = Manifest::read(root)?;
    manifest.ensure_selection()?;
    for fixture in &manifest.selected {
        out.push(Program {
            name: fixture.name.clone(),
            elf_blake3: fixture.blake3,
            image: fixture.read(root)?,
            uart: false,
        });
    }
    Ok(out)
}

/// The committed program called `name`.
pub fn program(root: &Path, name: &str) -> Result<Program, String> {
    programs(root)?
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("no committed program {name}"))
}

/// What one run of a program must reproduce: the per-program entry of the golden file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The program.
    pub program: String,
    /// BLAKE3 of the ELF file.
    pub elf_blake3: [u8; 32],
    /// The RAM's `image_hash`.
    pub image_hash: [u8; 32],
    /// The halt: the trap cause's name.
    pub cause: String,
    /// The trapping instruction's `pc`.
    pub trap_pc: u32,
    /// The trap value.
    pub tval: u32,
    /// Instructions retired.
    pub instret: u64,
    /// Events dispatched.
    pub events: u64,
    /// `pc`, then `x1` to `x31`, at the end.
    pub registers: [u32; 32],
    /// The UART's whole output, for a program run with the UART.
    pub uart: Option<Vec<u8>>,
    /// `StateDigest`.
    pub state: [u8; 32],
    /// `ExecutionDigest`.
    pub execution: [u8; 32],
    /// `TraceDigest`.
    pub trace: [u8; 32],
}

impl Record {
    /// The record of a traced run of `program` from `init` to its end. Fails unless the
    /// run passed: the pass rule of §10.2 and, for `hello`, M1-A5.
    pub fn of(program: &Program, finished: &Finished) -> Result<Record, String> {
        let o = &finished.outcome;
        let name = &program.name;
        runner::judge(o).map_err(|e| format!("{name}: {e}"))?;
        let End::Trap { cause, pc, tval } = &o.end else {
            unreachable!("judge accepts only a trap");
        };
        let uart = if program.uart {
            let output = hello::uart_output(&finished.views).map_err(|e| format!("{name}: {e}"))?;
            if program.name == REFERENCE && output != hello::EXPECTED_OUTPUT {
                return Err(format!(
                    "{name}: printed {:?}",
                    String::from_utf8_lossy(&output)
                ));
            }
            Some(output)
        } else {
            None
        };
        Ok(Record {
            program: name.clone(),
            elf_blake3: program.elf_blake3,
            image_hash: program.image.image_hash,
            cause: cause.clone(),
            trap_pc: *pc,
            tval: *tval,
            instret: o.instret,
            events: o.events,
            registers: registers(finished)?,
            uart,
            state: o.state.ok_or_else(|| format!("{name}: no StateDigest"))?,
            execution: o.execution,
            trace: o.trace.ok_or_else(|| format!("{name}: not traced"))?,
        })
    }

    /// Runs `program` traced from `init` and records it.
    pub fn run(program: &Program) -> Result<Record, String> {
        Record::of(program, &program.execute(Start::Init { traced: true }))
    }
}

/// `pc`, then `x1` to `x31`, from the CPU's view after the last event.
pub fn registers(finished: &Finished) -> Result<[u32; 32], String> {
    let view = finished
        .views
        .get(CPU.0 as usize)
        .ok_or("no view of the CPU: no event ran")?;
    let mut out = [0; 32];
    for (i, reg) in out.iter_mut().enumerate() {
        let name = if i == 0 {
            "pc".to_owned()
        } else {
            format!("x{i}")
        };
        *reg = match view.get(&name) {
            Some(Value::U64(v)) => u32::try_from(*v).map_err(|e| format!("{name}: {e}"))?,
            other => return Err(format!("the CPU's view has no {name}: {other:?}")),
        };
    }
    Ok(out)
}

/// The `mem.v1` message `ev` delivered, if any.
pub fn mem(ev: &Dispatched) -> Option<&MemMsg> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } => Some(msg),
        _ => None,
    }
}

/// Indices of the events that delivered a UART write to the UART.
pub fn uart_writes(events: &[Dispatched]) -> Vec<usize> {
    (0..events.len())
        .filter(|&i| {
            events[i].source == BUS
                && events[i].target == UART
                && matches!(mem(&events[i]), Some(MemMsg::WriteReq { .. }))
        })
        .collect()
}
