//! Checkpoint and resume from every event of `block_irq.elf` on `m2-reference`
//! (`docs/m2-design.md` §13.1, §13.2).
//!
//! As in M0 AT-2 and M1-A6, a checkpoint is a number of events `k`, and only the snapshot
//! bytes and the trace prefix cross from the run that took it to the run that resumes it.
//! Here every `k` from 0 to the last event is a checkpoint: no sampling. Each is
//! restored into a freshly built platform, re-encoded, and run to the end under the
//! watchdog, and the end is compared with the uninterrupted run in full.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::thread;

use systemscope_contracts::canonical::CanonicalEvent;
use systemscope_contracts::component::Delivered;
use systemscope_contracts::event::EventKey;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::BlockMsg;
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::{TraceAt, TraceOrigin};
use systemscope_platform::dma;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32::block_irq::{self, BlockIrqRun, HANDLER};
use systemscope_rv32::m2ref::{
    self, BLK, BLK_BASE, BUS, CPU, ComponentState, DISK, EVENT_BUDGET, IRQC, MASTER_CPU,
    MASTER_DMA, RAM,
};
use systemscope_rv32::runner::Start;
use systemscope_rv32i::cpu::{COMMIT_KIND, INTERRUPT_KIND};

use super::{Boundary, BoundaryWatch, Fixture, MEI_CAUSE, MIP_MEIP, MSTATUS_MIE, Work, pending};

/// The `MRET` instruction word.
pub const MRET: u64 = 0x3020_0073;

/// The uninterrupted, traced run, with what the checkpoints are compared against.
#[derive(Debug)]
pub struct Reference {
    /// The run, which passed §12.4.
    pub run: BlockIrqRun,
    /// The [`Boundary`] after each event: entry `i` is after `i + 1` events.
    pub boundaries: Vec<Boundary>,
    /// The trace's canonical bytes.
    pub trace_bytes: Vec<u8>,
    /// Records before each checkpoint: entry `k` counts the records of init and of the
    /// first `k` events.
    pub records_before: Vec<usize>,
    /// Indices of the events that took an interrupt.
    pub interrupt_events: BTreeSet<usize>,
    /// Indices of the events that retired an `MRET`.
    pub mret_events: BTreeSet<usize>,
    /// The final snapshot's component entries.
    pub final_components: Vec<ComponentState>,
}

impl Reference {
    /// The dispatched events.
    pub fn dispatched(&self) -> &[Dispatched] {
        &self.run.finished.finished.dispatched
    }

    /// The trace.
    pub fn trace(&self) -> &Trace {
        self.run
            .finished
            .finished
            .trace
            .as_ref()
            .expect("the reference is traced")
    }

    /// The final snapshot.
    pub fn final_snapshot(&self) -> &[u8] {
        self.run
            .finished
            .snapshot
            .as_deref()
            .expect("the reference ends with a snapshot")
    }

    /// Events in the whole run.
    pub fn events(&self) -> usize {
        self.dispatched().len()
    }

    /// The boundary after `k` events; `None` before the first.
    pub fn boundary(&self, k: usize) -> Option<&Boundary> {
        k.checked_sub(1).map(|i| &self.boundaries[i])
    }

    /// The trace recorded up to checkpoint `k`.
    pub fn prefix(&self, k: usize) -> Trace {
        let trace = self.trace();
        Trace {
            header: trace.header.clone(),
            records: trace.records[..self.records_before[k]].to_vec(),
        }
    }
}

/// Runs the program traced from `init` to its end under the watchdog, keeping the
/// [`Boundary`] after every event. Fails unless it passes §12.4 with the frozen work.
pub fn reference(fixture: &Fixture) -> Result<Reference, String> {
    let watch = Rc::new(RefCell::new(Vec::new()));
    let run = fixture.run(
        Start::Init { traced: true },
        vec![Box::new(BoundaryWatch(Rc::clone(&watch)))],
    );
    block_irq::judge(&run)?;
    let boundaries = watch.take().into_iter().collect::<Result<Vec<_>, _>>()?;
    let finished = &run.finished.finished;
    let trace = finished.trace.as_ref().ok_or("not traced")?;
    if Work::of(trace) != Work::expected() {
        return Err(format!("the run did other work: {:?}", Work::of(trace)));
    }
    let n = finished.dispatched.len();
    if boundaries.len() != n {
        return Err(format!("{n} events, {} boundaries", boundaries.len()));
    }
    let index: BTreeMap<EventKey, usize> = finished
        .dispatched
        .iter()
        .enumerate()
        .map(|(i, ev)| (ev.key, i))
        .collect();
    let mut records_before = Vec::with_capacity(n + 1);
    let mut interrupt_events = BTreeSet::new();
    let mut mret_events = BTreeSet::new();
    for (at, r) in trace.records.iter().enumerate() {
        if r.origin == TraceOrigin::Runtime {
            records_before.push(at);
        }
        let TraceAt::Event(key) = r.at else { continue };
        let event = *index.get(&key).ok_or("a record of an unknown event")?;
        if r.origin == TraceOrigin::Component && r.component == CPU {
            if r.kind == INTERRUPT_KIND {
                interrupt_events.insert(event);
            }
            if r.kind == COMMIT_KIND && super::u64_field(r, "insn") == Some(MRET) {
                mret_events.insert(event);
            }
        }
    }
    records_before.push(trace.records.len());
    if records_before.len() != n + 1 {
        return Err(format!(
            "{} dispatch records for {n} events",
            records_before.len() - 1
        ));
    }
    let snapshot = run
        .finished
        .snapshot
        .as_deref()
        .ok_or("no final snapshot")?;
    let final_components = m2ref::components(snapshot).map_err(|e| format!("{e:?}"))?;
    Ok(Reference {
        trace_bytes: trace.canonical_bytes(),
        boundaries,
        records_before,
        interrupt_events,
        mret_events,
        final_components,
        run,
    })
}

/// Work a checkpoint holds in flight or in progress: the event classes the snapshot's
/// queue carries across the restore, and the states the continuation must finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// A CPU request on its way to the bus.
    CpuRequest,
    /// A response on its way from the bus to the CPU.
    CpuResponse,
    /// A DMA beat request on its way from the controller to the bus.
    DmaRamRequest,
    /// A request the bus forwarded to the RAM.
    RamRequest,
    /// A beat response on its way from the bus to the controller.
    DmaResponse,
    /// A `ReadBlock` on its way to the media.
    ReadBlock,
    /// A `WriteBlock` on its way to the media.
    WriteBlock,
    /// A media result on its way to the controller.
    MediaResult,
    /// `Level(true)` on its way from the controller to the IRQ controller.
    LevelHighAtSource,
    /// `Level(true)` on its way from the IRQ controller to the CPU.
    LevelHighToCpu,
    /// `Level(false)` on its way from the controller to the IRQ controller.
    LevelLowAtSource,
    /// `Level(false)` on its way from the IRQ controller to the CPU.
    LevelLowToCpu,
    /// The handler's `ACK` store on its way to the controller.
    AckStore,
    /// The controller's engine wake-up.
    DmaWake,
    /// Just after the event that took the interrupt.
    MeiEntry,
    /// Inside the handler.
    Handler,
    /// The next event retires `MRET`.
    MretNext,
    /// A CPU request queued behind the DMA's active RAM beat.
    CpuBehindDma,
    /// A DMA beat queued behind the CPU's active RAM request.
    DmaBehindCpu,
}

impl Owner {
    /// Every class, in order.
    pub const ALL: [Owner; 19] = [
        Owner::CpuRequest,
        Owner::CpuResponse,
        Owner::DmaRamRequest,
        Owner::RamRequest,
        Owner::DmaResponse,
        Owner::ReadBlock,
        Owner::WriteBlock,
        Owner::MediaResult,
        Owner::LevelHighAtSource,
        Owner::LevelHighToCpu,
        Owner::LevelLowAtSource,
        Owner::LevelLowToCpu,
        Owner::AckStore,
        Owner::DmaWake,
        Owner::MeiEntry,
        Owner::Handler,
        Owner::MretNext,
        Owner::CpuBehindDma,
        Owner::DmaBehindCpu,
    ];
}

fn mem(ev: &CanonicalEvent) -> Option<&MemMsg> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => Some(m),
        _ => None,
    }
}

fn is_request(m: &MemMsg) -> bool {
    matches!(m, MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. })
}

fn level(ev: &CanonicalEvent) -> Option<bool> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::Irq(IrqMsg::Level { asserted }),
            ..
        } => Some(*asserted),
        _ => None,
    }
}

fn block(ev: &CanonicalEvent) -> Option<&BlockMsg> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::Block(b),
            ..
        } => Some(b),
        _ => None,
    }
}

/// The classes checkpoint `k` holds, from its queued events and the boundary.
pub fn owners(reference: &Reference, k: usize, queued: &[CanonicalEvent]) -> BTreeSet<Owner> {
    let ack = u64::from(BLK_BASE) + dma::ACK;
    let mut out = BTreeSet::new();
    for ev in queued {
        let route = (ev.source, ev.target);
        let class = match (route, mem(ev), level(ev), block(ev)) {
            ((CPU, BUS), Some(m), ..) if is_request(m) => {
                if matches!(m, MemMsg::WriteReq { addr, .. } if *addr == ack) {
                    out.insert(Owner::AckStore);
                }
                Some(Owner::CpuRequest)
            }
            ((BUS, CPU), Some(_), ..) => Some(Owner::CpuResponse),
            ((BLK, BUS), Some(m), ..) if is_request(m) => Some(Owner::DmaRamRequest),
            ((BUS, RAM), Some(m), ..) if is_request(m) => Some(Owner::RamRequest),
            ((BUS, BLK), Some(m), ..) if !is_request(m) => Some(Owner::DmaResponse),
            ((BUS, BLK), Some(MemMsg::WriteReq { addr, .. }), ..) if *addr == dma::ACK => {
                Some(Owner::AckStore)
            }
            ((BLK, DISK), _, _, Some(BlockMsg::ReadBlock { .. })) => Some(Owner::ReadBlock),
            ((BLK, DISK), _, _, Some(BlockMsg::WriteBlock { .. })) => Some(Owner::WriteBlock),
            ((DISK, BLK), _, _, Some(_)) => Some(Owner::MediaResult),
            ((BLK, IRQC), _, Some(true), _) => Some(Owner::LevelHighAtSource),
            ((IRQC, CPU), _, Some(true), _) => Some(Owner::LevelHighToCpu),
            ((BLK, IRQC), _, Some(false), _) => Some(Owner::LevelLowAtSource),
            ((IRQC, CPU), _, Some(false), _) => Some(Owner::LevelLowToCpu),
            ((BLK, BLK), ..) if matches!(ev.delivery, Delivered::Wake { .. }) => {
                Some(Owner::DmaWake)
            }
            _ => None,
        };
        out.extend(class);
    }
    if k > 0 && reference.interrupt_events.contains(&(k - 1)) {
        out.insert(Owner::MeiEntry);
    }
    if reference.mret_events.contains(&k) {
        out.insert(Owner::MretNext);
    }
    if let Some(b) = reference.boundary(k) {
        if b.in_handler() {
            out.insert(Owner::Handler);
        }
        let (active, _, queued) = &b.ram;
        if *active == Some(MASTER_DMA) && queued[MASTER_CPU as usize] > 0 {
            out.insert(Owner::CpuBehindDma);
        }
        if *active == Some(MASTER_CPU) && queued[MASTER_DMA as usize] > 0 {
            out.insert(Owner::DmaBehindCpu);
        }
    }
    out
}

/// The stress points of `docs/m2-design.md` §13.1, in its order (point 2 in both
/// directions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stress {
    /// 1. A DMA beat is pending: its request is in flight to the RAM.
    DmaBeatPending,
    /// 2. A CPU request is queued behind an active DMA beat.
    CpuQueuedBehindDma,
    /// 2. A DMA beat is queued behind an active CPU request.
    DmaQueuedBehindCpu,
    /// 3. A media result is pending.
    MediaResultPending,
    /// 4. The IRQ line is asserted at the CPU but not yet taken, the CPU mid-instruction.
    IrqAssertedNotTaken,
    /// 5. Just after entry to the handler: `FetchIssue` at `mtvec`.
    HandlerEntry,
    /// 6. The device IRQ is cleared after `ACK`, but `MRET` has not retired.
    IrqClearedBeforeMret,
    /// 7. A WRITE block is partially buffered: some of its 32 beats read.
    WritePartiallyBuffered,
}

impl Stress {
    /// Every stress point, in order.
    pub const ALL: [Stress; 8] = [
        Stress::DmaBeatPending,
        Stress::CpuQueuedBehindDma,
        Stress::DmaQueuedBehindCpu,
        Stress::MediaResultPending,
        Stress::IrqAssertedNotTaken,
        Stress::HandlerEntry,
        Stress::IrqClearedBeforeMret,
        Stress::WritePartiallyBuffered,
    ];

    /// The design's wording.
    pub fn name(self) -> &'static str {
        match self {
            Stress::DmaBeatPending => "1. DMA beat pending (request in flight to the RAM)",
            Stress::CpuQueuedBehindDma => "2. CPU request queued behind an active DMA beat",
            Stress::DmaQueuedBehindCpu => "2. DMA beat queued behind an active CPU request",
            Stress::MediaResultPending => "3. media result pending",
            Stress::IrqAssertedNotTaken => {
                "4. IRQ asserted at the CPU, not yet taken (CPU mid-instruction)"
            }
            Stress::HandlerEntry => "5. just after handler entry (FetchIssue at mtvec)",
            Stress::IrqClearedBeforeMret => "6. device IRQ cleared after ACK, MRET not retired",
            Stress::WritePartiallyBuffered => "7. WRITE block partially buffered",
        }
    }

    /// Whether checkpoint `k`, holding `owners`, is this stress point.
    pub fn holds(self, reference: &Reference, k: usize, owners: &BTreeSet<Owner>) -> bool {
        let Some(b) = reference.boundary(k) else {
            return false;
        };
        let mid_instruction = matches!(
            b.cpu_state.as_str(),
            "fetch_wait" | "mem_issue" | "mem_wait" | "commit_pending"
        );
        match self {
            Stress::DmaBeatPending => {
                b.engine == "wait_beat"
                    && (owners.contains(&Owner::DmaRamRequest)
                        || (b.ram.0 == Some(MASTER_DMA) && owners.contains(&Owner::RamRequest)))
            }
            Stress::CpuQueuedBehindDma => owners.contains(&Owner::CpuBehindDma),
            Stress::DmaQueuedBehindCpu => owners.contains(&Owner::DmaBehindCpu),
            Stress::MediaResultPending => b.engine == "wait_media",
            Stress::IrqAssertedNotTaken => {
                b.mip & MIP_MEIP != 0 && b.mstatus & MSTATUS_MIE != 0 && mid_instruction
            }
            Stress::HandlerEntry => {
                b.pc == u64::from(HANDLER)
                    && b.cpu_state == "fetch_issue"
                    && b.mcause == MEI_CAUSE
                    && b.in_handler()
            }
            Stress::IrqClearedBeforeMret => {
                b.in_handler() && !b.blk_irq && !b.irqc_out && b.mip & MIP_MEIP == 0
            }
            Stress::WritePartiallyBuffered => {
                b.command.starts_with("write")
                    && b.engine == "wait_beat"
                    && b.beat > 0
                    && b.beat < u64::from(dma::BEATS_PER_BLOCK)
            }
        }
    }
}

/// What checkpoint `k` held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// Its classes.
    pub owners: BTreeSet<Owner>,
    /// Its stress points.
    pub stress: BTreeSet<Stress>,
    /// Events queued in its snapshot.
    pub queued: usize,
    /// Both masters had a RAM request queued at once.
    pub both_queued: bool,
}

/// Restores checkpoint `k`'s `snapshot` into a freshly built platform, requires the
/// re-encoded bytes back, resumes the trace prefix, runs to the end under the watchdog,
/// and compares the end with the reference: event count, the replayed events, the final
/// snapshot byte for byte and component by component, `StateDigest`, `ExecutionDigest`,
/// the reconstructed trace's canonical bytes and `TraceDigest`, and the whole run's work
/// (three interrupts, the three media operations, 96 beats, `M2 PASS\n`). The live
/// state the restored components report after the first resumed event must also be the
/// reference's [`Boundary`] there, so state a snapshot loses is noticed even where the
/// rest of the run would not depend on it, such as a round-robin cursor.
pub fn resume_exact(
    fixture: &Fixture,
    reference: &Reference,
    k: usize,
    snapshot: Vec<u8>,
) -> Result<(), String> {
    let mut rt = fixture.platform();
    rt.restore(&snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
        return Err("snapshot -> restore -> snapshot is not the identity".to_owned());
    }
    rt.resume_trace(reference.prefix(k))
        .map_err(|e| format!("resume_trace failed: {e:?}"))?;
    let first = Rc::new(RefCell::new(None));
    rt.add_observer(Box::new(FirstBoundary(Rc::clone(&first))));
    let rest = continue_to_end(&mut rt)?;
    let mut differ = Vec::new();
    let first = first.take();
    if first.as_ref().map(|b| b.as_ref().ok()) != reference.boundaries.get(k).map(Some) {
        differ.push(format!("the state after event {}: {first:?}", k + 1));
    }
    if k + rest.len() != reference.events() {
        differ.push("event count".to_owned());
    }
    if reference.dispatched().get(k..) != Some(&rest[..]) {
        differ.push("replayed events".to_owned());
    }
    let snapshot = rt.snapshot().map_err(|e| e.to_string())?;
    if snapshot != reference.final_snapshot() {
        differ.push("final snapshot".to_owned());
        let components = m2ref::components(&snapshot).map_err(|e| format!("{e:?}"))?;
        for (id, (a, b)) in reference
            .final_components
            .iter()
            .zip(&components)
            .enumerate()
        {
            if a != b {
                differ.push(format!("component {}", m2ref::PATHS[id]));
            }
        }
    }
    let o = &reference.run.finished.finished.outcome;
    if rt.state_digest().ok() != o.state {
        differ.push("StateDigest".to_owned());
    }
    if rt.execution_digest() != o.execution {
        differ.push("ExecutionDigest".to_owned());
    }
    let trace = rt.take_trace().ok_or("the resumed run lost its trace")?;
    let bytes = trace.canonical_bytes();
    if bytes != reference.trace_bytes {
        differ.push("canonical trace bytes".to_owned());
    }
    if Some(*blake3::hash(&bytes).as_bytes()) != o.trace {
        differ.push("TraceDigest".to_owned());
    }
    let work = Work::of(&trace);
    if work != Work::expected() {
        differ.push(format!("work {work:?}"));
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(format!("differs in {}", differ.join(", ")))
    }
}

/// Keeps the [`Boundary`] after the first event it sees.
struct FirstBoundary(Rc<RefCell<Option<Result<Boundary, String>>>>);

impl Observer for FirstBoundary {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let mut first = self.0.borrow_mut();
        if first.is_none() {
            *first = Some(Boundary::read(world));
        }
        Control::Continue
    }
}

/// Runs a restored session to its end, under the same watchdog as [`m2ref::execute`]:
/// at most [`EVENT_BUDGET`] events after the restore.
pub fn continue_to_end(rt: &mut Runtime) -> Result<Vec<Dispatched>, String> {
    let mut rest = Vec::new();
    loop {
        if rest.len() as u64 >= EVENT_BUDGET {
            return Err(format!("stopped at the budget of {EVENT_BUDGET} events"));
        }
        match rt.step() {
            Ok(Some(ev)) => rest.push(ev),
            Ok(None) => break,
            Err(e) => return Err(format!("the resumed run failed: {e}")),
        }
    }
    match rt.fault() {
        Some(e) => Err(format!("the resumed run faulted: {e}")),
        None => Ok(rest),
    }
}

/// Classifies checkpoint `k` from its `snapshot`, then checks it with [`resume_exact`].
pub fn check(
    fixture: &Fixture,
    reference: &Reference,
    k: usize,
    snapshot: Vec<u8>,
) -> Result<Checked, String> {
    let queued = pending(&snapshot)?;
    let owners = owners(reference, k, &queued);
    let stress = Stress::ALL
        .into_iter()
        .filter(|s| s.holds(reference, k, &owners))
        .collect();
    let both_queued = reference
        .boundary(k)
        .is_some_and(|b| b.ram.2.iter().all(|&q| q > 0));
    resume_exact(fixture, reference, k, snapshot)?;
    Ok(Checked {
        owners,
        stress,
        queued: queued.len(),
        both_queued,
    })
}

/// The outcome of checking every checkpoint.
#[derive(Clone, Debug, Default)]
pub struct Coverage {
    /// Checkpoints checked: every `k` from 0 to the event count.
    pub checked: usize,
    /// Checkpoints per class.
    pub owners: BTreeMap<Owner, usize>,
    /// Checkpoints per stress point, and the first of each.
    pub stress: BTreeMap<Stress, (usize, usize)>,
    /// Checkpoints with both masters' RAM queues non-empty.
    pub both_queued: usize,
    /// The most events queued in one checkpoint.
    pub max_queued: usize,
    /// Every failure, as `(k, why)`.
    pub failures: Vec<(usize, String)>,
}

impl Coverage {
    fn add(&mut self, k: usize, result: Result<Checked, String>) {
        self.checked += 1;
        match result {
            Ok(c) => {
                for o in c.owners {
                    *self.owners.entry(o).or_default() += 1;
                }
                for s in c.stress {
                    let e = self.stress.entry(s).or_insert((0, k));
                    e.0 += 1;
                    e.1 = e.1.min(k);
                }
                self.both_queued += usize::from(c.both_queued);
                self.max_queued = self.max_queued.max(c.queued);
            }
            Err(e) => self.failures.push((k, e)),
        }
    }

    fn merge(&mut self, other: Coverage) {
        self.checked += other.checked;
        for (o, n) in other.owners {
            *self.owners.entry(o).or_default() += n;
        }
        for (s, (n, first)) in other.stress {
            let e = self.stress.entry(s).or_insert((0, first));
            e.0 += n;
            e.1 = e.1.min(first);
        }
        self.both_queued += other.both_queued;
        self.max_queued = self.max_queued.max(other.max_queued);
        self.failures.extend(other.failures);
    }
}

/// Checks checkpoints `range`: a traced producer run steps from `init` to the first of
/// them, snapshots at each, and requires each of its events to be the reference's.
fn check_range(
    fixture: &Fixture,
    reference: &Reference,
    range: std::ops::Range<usize>,
) -> Coverage {
    let mut coverage = Coverage::default();
    let mut producer = fixture.platform();
    let started = producer
        .start_trace()
        .and_then(|()| producer.init())
        .map_err(|e| e.to_string());
    if let Err(e) = started {
        coverage.failures.push((range.start, e));
        return coverage;
    }
    let step = |rt: &mut Runtime, k: usize| -> Result<(), String> {
        let ev = rt
            .step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the producer ended after {k} events"))?;
        if ev != reference.dispatched()[k] {
            return Err(format!("the producer's event {k} is not the reference's"));
        }
        Ok(())
    };
    for k in 0..range.start {
        if let Err(e) = step(&mut producer, k) {
            coverage.failures.push((k, e));
            return coverage;
        }
    }
    for k in range {
        match producer.snapshot() {
            Ok(snapshot) => coverage.add(k, check(fixture, reference, k, snapshot)),
            Err(e) => coverage.add(k, Err(e.to_string())),
        }
        if k < reference.events()
            && let Err(e) = step(&mut producer, k)
        {
            coverage.failures.push((k, e));
            return coverage;
        }
    }
    coverage
}

/// Checks every checkpoint, 0 through the last event, split into contiguous ranges
/// across `threads` threads. Each thread builds its own platforms; only the fixture and
/// the reference are shared.
pub fn every_event(fixture: &Fixture, reference: &Reference, threads: usize) -> Coverage {
    let total = reference.events() + 1;
    let threads = threads.clamp(1, total);
    let bounds: Vec<usize> = (0..=threads).map(|t| t * total / threads).collect();
    let mut coverage = Coverage::default();
    thread::scope(|s| {
        let handles: Vec<_> = bounds
            .windows(2)
            .map(|w| {
                let range = w[0]..w[1];
                s.spawn(move || check_range(fixture, reference, range))
            })
            .collect();
        for h in handles {
            coverage.merge(h.join().expect("a checkpoint thread panicked"));
        }
    });
    coverage.failures.sort_by_key(|f| f.0);
    coverage
}
