//! M2 snapshot, determinism, and golden acceptance (`docs/m2-design.md` §13, §15.1) on
//! `m2-reference` running `block_irq.elf`, and the golden files that `cargo xtask
//! m2-golden bless` writes.
//!
//! The workload is exactly M2.8's: the committed `block_irq.elf` and disk fixture, each
//! checked against its manifest, on the frozen `m2-reference` built by
//! [`systemscope_rv32::m2ref::build`] and run under its event watchdog
//! ([`systemscope_rv32::m2ref::EVENT_BUDGET`]).
//!
//! As in M0 and M1, every check returns a `Result` instead of asserting, so the tests can
//! also feed it doctored inputs and prove that it notices them.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use systemscope_contracts::canonical::{CanonicalEvent, Decoder};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_elf::LoadImage;
use systemscope_platform::{dma, media, mmbus, uart};
use systemscope_runtime::runtime::Runtime;
use systemscope_runtime::trace::Trace;
use systemscope_rv32::block_irq::{self, BlockIrqManifest, BlockIrqRun};
use systemscope_rv32::m2ref::{self, BLK, BUS, CPU, Config, DISK, IRQC, MASTER_DMA, REGION_RAM};
use systemscope_rv32::runner::{End, Finished, Start};
use systemscope_rv32i::cpu::INTERRUPT_KIND;

use crate::layout::Layout;

pub mod golden;

/// The scenario's name, as the golden file records it.
pub const SCENARIO: &str = "m2-reference";
/// The program, as the golden file records it.
pub const PROGRAM: &str = "block_irq";
/// The M2 CSRs the CPU's view lists, in view order.
pub const CSRS: [&str; 8] = [
    "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval",
];
/// `mstatus.MIE`.
pub const MSTATUS_MIE: u64 = 1 << 3;
/// `mstatus.MPIE`.
pub const MSTATUS_MPIE: u64 = 1 << 7;
/// `mip.MEIP`.
pub const MIP_MEIP: u64 = 1 << 11;
/// `mcause` of a machine external interrupt.
pub const MEI_CAUSE: u64 = 0x8000_000b;

/// The committed `block_irq.elf` and disk fixture, read through their manifest.
#[derive(Clone, Debug)]
pub struct Fixture {
    /// BLAKE3 of the ELF file, as the manifest pins it.
    pub elf_blake3: [u8; 32],
    /// BLAKE3 of the disk fixture, as the manifest pins it: the media's `image_hash`.
    pub disk_blake3: [u8; 32],
    /// The loaded image.
    pub image: LoadImage,
    /// The raw disk image.
    pub disk: Vec<u8>,
}

impl Fixture {
    /// Reads the committed fixture under `root`; fails unless both files are exactly the
    /// ones the manifest names.
    pub fn read(root: &Path) -> Result<Fixture, String> {
        let manifest = BlockIrqManifest::read(root)?;
        Ok(Fixture {
            elf_blake3: manifest.blake3,
            disk_blake3: manifest.disk_blake3,
            image: manifest.read_image(root)?,
            disk: manifest.read_disk(root)?,
        })
    }

    /// `m2-reference` configured as `config`, elaborated.
    pub fn platform_with(&self, config: &Config) -> Result<Runtime, m2ref::BuildError> {
        m2ref::build(&self.image, &self.disk, config)
    }

    /// The frozen `m2-reference`, elaborated.
    ///
    /// # Panics
    ///
    /// If the frozen platform does not build, which M2.8 rules out.
    pub fn platform(&self) -> Runtime {
        self.platform_with(&Config::frozen())
            .expect("the frozen m2-reference builds")
    }

    /// Runs the program from `start` to its end on a fresh frozen platform, under the
    /// watchdog. `observers` only read.
    pub fn run(&self, start: Start, observers: Vec<Box<dyn Observer>>) -> BlockIrqRun {
        block_irq::run(&self.image, &self.disk, start, observers)
    }
}

/// What the tests read of the world after one event: the fields of the CPU, the bus, the
/// block controller, and the IRQ controller views that locate the §13.1 stress points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    /// The CPU's execution state name.
    pub cpu_state: String,
    /// `pc`.
    pub pc: u64,
    /// `mstatus`.
    pub mstatus: u64,
    /// `mip`.
    pub mip: u64,
    /// `mcause`.
    pub mcause: u64,
    /// The bus's RAM region: the active master, the round-robin cursor, and each
    /// master's queue length.
    pub ram: (Option<u64>, u64, Vec<u64>),
    /// The bus's next downstream `txn`.
    pub next_downstream: u64,
    /// The controller's latched command, as its view prints it.
    pub command: String,
    /// The controller's engine position.
    pub engine: String,
    /// The controller's beat index.
    pub beat: u64,
    /// The controller's IRQ line.
    pub blk_irq: bool,
    /// The IRQ controller's output.
    pub irqc_out: bool,
}

impl Boundary {
    /// Reads the boundary from the views.
    pub fn read(world: &WorldView<'_>) -> Result<Boundary, String> {
        let view = |id: ComponentId| world.inspect(id).unwrap_or_default();
        let (cpu, bus, blk, irqc) = (view(CPU), view(BUS), view(BLK), view(IRQC));
        Ok(Boundary {
            cpu_state: text(&cpu, "state")?,
            pc: number(&cpu, "pc")?,
            mstatus: number(&cpu, "mstatus")?,
            mip: number(&cpu, "mip")?,
            mcause: number(&cpu, "mcause")?,
            ram: region(&text(&bus, "ram")?)?,
            next_downstream: number(&bus, "next_downstream_txn")?,
            command: text(&blk, "command")?,
            engine: text(&blk, "engine")?,
            beat: number(&blk, "beat")?,
            blk_irq: flag(&blk, "irq")?,
            irqc_out: flag(&irqc, "out")?,
        })
    }

    /// Inside the handler: entered from the main program with interrupts on, so
    /// `mstatus.MIE` is 0 and `mstatus.MPIE` is 1 until `MRET` retires.
    pub fn in_handler(&self) -> bool {
        self.mstatus & (MSTATUS_MIE | MSTATUS_MPIE) == MSTATUS_MPIE
    }
}

/// Keeps the [`Boundary`] after every event.
pub struct BoundaryWatch(pub Rc<RefCell<Vec<Result<Boundary, String>>>>);

impl Observer for BoundaryWatch {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0.borrow_mut().push(Boundary::read(world));
        Control::Continue
    }
}

fn field<'a>(view: &'a StateView, name: &str) -> Result<&'a Value, String> {
    view.get(name)
        .ok_or_else(|| format!("no view field {name}"))
}

fn number(view: &StateView, name: &str) -> Result<u64, String> {
    match field(view, name)? {
        Value::U64(v) => Ok(*v),
        other => Err(format!("{name} is {other:?}")),
    }
}

fn flag(view: &StateView, name: &str) -> Result<bool, String> {
    match field(view, name)? {
        Value::Bool(v) => Ok(*v),
        other => Err(format!("{name} is {other:?}")),
    }
}

fn text(view: &StateView, name: &str) -> Result<String, String> {
    match field(view, name)? {
        Value::Str(v) => Ok(v.clone()),
        other => Err(format!("{name} is {other:?}")),
    }
}

/// `(active, rr_cursor, queued)` from a bus region summary.
fn region(summary: &str) -> Result<(Option<u64>, u64, Vec<u64>), String> {
    let bad = || format!("bad region summary {summary:?}");
    let mut parts = summary.split(' ');
    let mut part = |prefix: &str| {
        parts
            .next()
            .and_then(|p| p.strip_prefix(prefix))
            .map(str::to_owned)
            .ok_or_else(bad)
    };
    let active = part("active=")?;
    let cursor = part("rr_cursor=")?;
    let queued = part("queued=")?;
    Ok((
        active.parse().ok(),
        cursor.parse().map_err(|_| bad())?,
        queued
            .split(',')
            .map(|n| n.parse().map_err(|_| bad()))
            .collect::<Result<_, _>>()?,
    ))
}

/// The events queued in a runtime snapshot, in the order it stores them.
pub fn pending(snapshot: &[u8]) -> Result<Vec<CanonicalEvent>, String> {
    let layout = Layout::parse(snapshot).map_err(|e| format!("{e:?}"))?;
    layout
        .queue
        .iter()
        .map(|&at| {
            CanonicalEvent::decode(&mut Decoder::new(&snapshot[at..])).map_err(|e| format!("{e:?}"))
        })
        .collect()
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

/// A record's `u64` field.
pub fn u64_field(r: &TraceRecord, name: &str) -> Option<u64> {
    r.fields.iter().find_map(|(n, v)| match v {
        Value::U64(x) if *n == name => Some(*x),
        _ => None,
    })
}

/// What a whole `block_irq.elf` trace must show (§12.4, M2.8): its interrupts, media
/// operations, DMA beats, and UART bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    /// `mcause` of every interrupt taken.
    pub interrupts: Vec<u64>,
    /// Every media operation, in order: `("read" | "write", lba)`.
    pub disk_ops: Vec<(&'static str, u64)>,
    /// RAM grants to the DMA master: one per beat.
    pub dma_beats: usize,
    /// Every byte the UART transmitted.
    pub uart: Vec<u8>,
}

impl Work {
    /// The work a trace records.
    pub fn of(trace: &Trace) -> Work {
        let mut disk_ops = Vec::new();
        for r in &trace.records {
            if r.origin != TraceOrigin::Component || r.component != DISK {
                continue;
            }
            let lba = u64_field(r, "lba").unwrap_or(u64::MAX);
            if r.kind == media::READ_KIND {
                disk_ops.push(("read", lba));
            } else if r.kind == media::WRITE_KIND {
                disk_ops.push(("write", lba));
            }
        }
        Work {
            interrupts: records(trace, CPU, INTERRUPT_KIND)
                .map(|r| u64_field(r, "mcause").unwrap_or(0))
                .collect(),
            disk_ops,
            dma_beats: records(trace, BUS, mmbus::GRANT_KIND)
                .filter(|r| {
                    u64_field(r, "master") == Some(MASTER_DMA)
                        && u64_field(r, "region") == Some(REGION_RAM)
                })
                .count(),
            uart: records(trace, m2ref::UART, uart::TX_KIND)
                .map(|r| u64_field(r, "byte").unwrap_or(0x100) as u8)
                .collect(),
        }
    }

    /// The work of the frozen run: three interrupts, READ LBA 0, WRITE LBA 1, READ LBA 1,
    /// 32 beats each, and `M2 PASS\n`.
    pub fn expected() -> Work {
        Work {
            interrupts: vec![MEI_CAUSE; block_irq::EXPECTED_ENTRIES as usize],
            disk_ops: vec![("read", 0), ("write", 1), ("read", 1)],
            dma_beats: 3 * dma::BEATS_PER_BLOCK as usize,
            uart: block_irq::EXPECTED_OUTPUT.to_vec(),
        }
    }
}

/// What the run must reproduce: the golden file's program entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// [`PROGRAM`].
    pub program: String,
    /// BLAKE3 of the ELF file.
    pub elf_blake3: [u8; 32],
    /// The RAM's `image_hash`.
    pub image_hash: [u8; 32],
    /// BLAKE3 of the disk fixture.
    pub disk_blake3: [u8; 32],
    /// The media's `image_hash`, from its final state.
    pub media_image_hash: [u8; 32],
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
    /// The M2 CSRs of [`CSRS`], at the end.
    pub csrs: [u32; 8],
    /// The UART's whole output.
    pub uart: Vec<u8>,
    /// `mcause` of every interrupt taken.
    pub interrupts: Vec<u64>,
    /// The handler's entry counter.
    pub entries: u32,
    /// Every media operation, in order.
    pub disk_ops: Vec<(String, u64)>,
    /// DMA beats.
    pub dma_beats: u64,
    /// Every non-zero disk block at the end, with the BLAKE3 of its bytes.
    pub disk_blocks: Vec<(u64, [u8; 32])>,
    /// `StateDigest`.
    pub state: [u8; 32],
    /// `ExecutionDigest`.
    pub execution: [u8; 32],
    /// `TraceDigest`.
    pub trace: [u8; 32],
}

impl Record {
    /// The record of a traced run from `init` to its end. Fails unless the run passed
    /// §12.4 and did exactly the frozen work.
    pub fn of(fixture: &Fixture, run: &BlockIrqRun) -> Result<Record, String> {
        block_irq::judge(run)?;
        let finished = &run.finished.finished;
        let o = &finished.outcome;
        let End::Trap { cause, pc, tval } = &o.end else {
            unreachable!("judge accepts only a trap");
        };
        let trace = finished.trace.as_ref().ok_or("not traced")?;
        let work = Work::of(trace);
        if work != Work::expected() {
            return Err(format!("the run did other work: {work:?}"));
        }
        let snapshot = run.finished.snapshot.as_ref().ok_or("no final snapshot")?;
        let components = m2ref::components(snapshot).map_err(|e| format!("{e:?}"))?;
        let (media_image_hash, blocks) =
            m2ref::disk_blocks(&components[DISK.0 as usize].bytes).map_err(|e| format!("{e:?}"))?;
        let state = run.state.as_ref().map_err(Clone::clone)?;
        Ok(Record {
            program: PROGRAM.to_owned(),
            elf_blake3: fixture.elf_blake3,
            image_hash: fixture.image.image_hash,
            disk_blake3: fixture.disk_blake3,
            media_image_hash,
            cause: cause.clone(),
            trap_pc: *pc,
            tval: *tval,
            instret: o.instret,
            events: o.events,
            registers: registers(finished)?,
            csrs: csrs(finished)?,
            uart: run.output.clone()?,
            interrupts: work.interrupts,
            entries: state.entries,
            disk_ops: work
                .disk_ops
                .into_iter()
                .map(|(op, lba)| (op.to_owned(), lba))
                .collect(),
            dma_beats: work.dma_beats as u64,
            disk_blocks: blocks
                .iter()
                .map(|(lba, bytes)| (*lba, *blake3::hash(bytes).as_bytes()))
                .collect(),
            state: o.state.ok_or("no StateDigest")?,
            execution: o.execution,
            trace: o.trace.ok_or("not traced")?,
        })
    }

    /// Runs the program traced from `init` and records it.
    pub fn run(fixture: &Fixture) -> Result<Record, String> {
        Record::of(
            fixture,
            &fixture.run(Start::Init { traced: true }, Vec::new()),
        )
    }
}

fn cpu_value(finished: &Finished, name: &str) -> Result<u32, String> {
    let view = finished
        .views
        .get(CPU.0 as usize)
        .ok_or("no view of the CPU: no event ran")?;
    match view.get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).map_err(|e| format!("{name}: {e}")),
        other => Err(format!("the CPU's view has no {name}: {other:?}")),
    }
}

/// `pc`, then `x1` to `x31`, from the CPU's view after the last event.
pub fn registers(finished: &Finished) -> Result<[u32; 32], String> {
    let mut out = [0; 32];
    for (i, reg) in out.iter_mut().enumerate() {
        let name = if i == 0 {
            "pc".to_owned()
        } else {
            format!("x{i}")
        };
        *reg = cpu_value(finished, &name)?;
    }
    Ok(out)
}

/// The [`CSRS`], from the CPU's view after the last event.
pub fn csrs(finished: &Finished) -> Result<[u32; 8], String> {
    let mut out = [0; 8];
    for (value, name) in out.iter_mut().zip(CSRS) {
        *value = cpu_value(finished, name)?;
    }
    Ok(out)
}
