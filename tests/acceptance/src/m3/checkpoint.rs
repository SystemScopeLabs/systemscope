//! Checkpoint and resume on the M3 scenario (`docs/m3-design.md` §9.2, §17.4).
//!
//! As in M0 to M2, a checkpoint is a number of events `k`, and only the snapshot bytes
//! and the trace prefix cross from the run that took it to the run that resumes it. Three
//! passes cover every event:
//!
//! - [`chained`]: one run that, at every event boundary, checks the snapshot with the
//!   independent reader ([`super::portable`]), classifies it, restores it into a freshly
//!   built platform, requires the re-encoded bytes back, and continues from the restored
//!   platform, so every event of the scenario is dispatched by a platform restored just
//!   before it;
//! - [`every_event`]: every checkpoint `k` from 0 to the last event, each restored into
//!   its own fresh platform and run to the end, compared event by event and in its final
//!   state with the uninterrupted run;
//! - [`resume_exact`]: the traced resume of a checkpoint, with the trace prefix, judged
//!   by §12.3 and compared in full, at every §9.2 stress point.

use std::collections::{BTreeMap, BTreeSet};
use std::thread;

use systemscope_contracts::component::Delivered;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::BlockMsg;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::TraceOrigin;
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32::m3::{self, EVENT_BUDGET, M3Run, Metrics};
use systemscope_rv32::m3ref::{BLK, BUS, DISK};
use systemscope_rv32::runner::Start;

use super::Fixture;
use super::portable::{
    self, Accounting, At, CpuState, Engine, MASTERS, Place, Platform, Purpose, Stage, TARGETS,
};

/// The uninterrupted, traced run the checkpoints are compared against.
#[derive(Debug)]
pub struct Reference {
    /// The run, which passed §12.3.
    pub run: M3Run,
    /// Its metrics.
    pub metrics: Metrics,
    /// The trace's canonical bytes.
    pub trace_bytes: Vec<u8>,
    /// Records before each checkpoint: entry `k` counts the records of init and of the
    /// first `k` events.
    pub records_before: Vec<usize>,
}

impl Reference {
    /// The dispatched events.
    pub fn dispatched(&self) -> &[Dispatched] {
        &self.run.finished.dispatched
    }

    /// The trace.
    pub fn trace(&self) -> &Trace {
        self.run
            .finished
            .trace
            .as_ref()
            .expect("the reference is traced")
    }

    /// The final snapshot.
    pub fn final_snapshot(&self) -> &[u8] {
        self.run
            .snapshot
            .as_deref()
            .expect("the reference ends with a snapshot")
    }

    /// Events in the whole run.
    pub fn events(&self) -> usize {
        self.dispatched().len()
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

/// Runs the scenario traced from `init` to its end under the watchdog; fails unless it
/// passes §12.3.
pub fn reference(fixture: &Fixture) -> Result<Reference, String> {
    let run = fixture.judged_run(Vec::new())?;
    let finished = &run.finished;
    let trace = finished.trace.as_ref().ok_or("not traced")?;
    let n = finished.dispatched.len();
    let mut records_before = Vec::with_capacity(n + 1);
    for (at, r) in trace.records.iter().enumerate() {
        if r.origin == TraceOrigin::Runtime {
            records_before.push(at);
        }
    }
    records_before.push(trace.records.len());
    if records_before.len() != n + 1 {
        return Err(format!(
            "{} dispatch records for {n} events",
            records_before.len() - 1
        ));
    }
    Ok(Reference {
        metrics: Metrics::of(&run),
        trace_bytes: trace.canonical_bytes(),
        records_before,
        run,
    })
}

/// Whether a kernel access is part of boot or of a syscall.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum During {
    /// Reading the table or an executable, or creating a process.
    Boot,
    /// A `write` syscall.
    Syscall,
}

/// The stress points of §9.2, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stress {
    /// 1. A PTE read in flight at `level` for `purpose`.
    MidWalk(Purpose, u8),
    /// 2. A delegated exception entry just taken: `FetchIssue` at `stvec`.
    ExceptionEntry,
    /// 3. `ENTER` held with a kernel access in flight.
    HeldAccess(During),
    /// 4. A DMA beat in flight during an executable load, with a kernel `STATUS` poll
    ///    outstanding behind it.
    BeatBehindPoll,
    /// 5. A partial frame zeroing or segment copy.
    PartialCopy,
    /// 6. A `write` with some bytes already at the UART.
    PartialWrite,
    /// 7. Between a context switch's trap-frame write and the trampoline's `SRET`.
    FrameWrittenBeforeSret,
    /// 8. After `SRET`, before the first U-mode fetch of a new process.
    SretBeforeFirstFetch,
}

impl Stress {
    /// Every stress point, in order: point 1 for each purpose and level, point 3 at boot
    /// and in a syscall.
    pub const ALL: [Stress; 14] = [
        Stress::MidWalk(Purpose::Fetch, 1),
        Stress::MidWalk(Purpose::Fetch, 0),
        Stress::MidWalk(Purpose::Load, 1),
        Stress::MidWalk(Purpose::Load, 0),
        Stress::MidWalk(Purpose::Store, 1),
        Stress::MidWalk(Purpose::Store, 0),
        Stress::ExceptionEntry,
        Stress::HeldAccess(During::Boot),
        Stress::HeldAccess(During::Syscall),
        Stress::BeatBehindPoll,
        Stress::PartialCopy,
        Stress::PartialWrite,
        Stress::FrameWrittenBeforeSret,
        Stress::SretBeforeFirstFetch,
    ];

    /// The design's wording.
    pub fn name(self) -> String {
        match self {
            Stress::MidWalk(p, l) => format!("1. mid-walk: level-{l} PTE read in flight, {p:?}"),
            Stress::ExceptionEntry => "2. delegated exception entry just taken".to_owned(),
            Stress::HeldAccess(d) => format!("3. ENTER held, kernel access in flight, {d:?}"),
            Stress::BeatBehindPoll => {
                "4. DMA beat in flight in an executable load, STATUS poll behind it".to_owned()
            }
            Stress::PartialCopy => "5. partial frame zeroing or segment copy".to_owned(),
            Stress::PartialWrite => "6. write with some bytes at the UART".to_owned(),
            Stress::FrameWrittenBeforeSret => {
                "7. context switch: trap frame written, SRET not retired".to_owned()
            }
            Stress::SretBeforeFirstFetch => {
                "8. after SRET, before a new process's first U fetch".to_owned()
            }
        }
    }
}

/// Whether the CPU is about to fetch at `pc` and has sent nothing for it yet: `FetchIssue`
/// without translation, or, under Sv32, the first step of the fetch's walk, which is where
/// a translated fetch starts (§5.4, no TLB). §9.2 names both "`FetchIssue`".
pub fn fetch_not_issued(state: &CpuState) -> bool {
    match state {
        CpuState::FetchIssue { pa: None } => true,
        CpuState::WalkIssue(walk) => walk.insn.is_none() && walk.level == 1,
        _ => false,
    }
}

/// What the classifier remembers from one boundary to the next.
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    prev_priv: Option<u8>,
    /// The PID the last dispatch put in the trap frame, until its `SRET`.
    dispatched: Option<u32>,
    /// A dispatch stage was seen and has ended.
    dispatch_done: bool,
    /// The processes that have reached U.
    ran: BTreeSet<u32>,
}

impl Tracker {
    /// The stress points boundary `p` is, given everything before it.
    pub fn classify(&mut self, p: &Platform, acc: &Accounting) -> BTreeSet<Stress> {
        let mut out = BTreeSet::new();
        let cpu = &p.cpu;
        let k = &p.kernel;
        let stage = k.op.as_ref().map(|o| &o.stage);
        if let CpuState::WalkWait { walk, .. } = &cpu.state
            && let Some(purpose) = walk.purpose()
        {
            out.insert(Stress::MidWalk(purpose, walk.level));
        }
        if fetch_not_issued(&cpu.state)
            && cpu.privilege == 1
            && self.prev_priv == Some(0)
            && cpu.pc == cpu.stvec
            && cpu.scause >> 31 == 0
        {
            out.insert(Stress::ExceptionEntry);
        }
        if let Some(s) = stage
            && k.held.is_some()
            && acc.kernel.is_some()
        {
            if s.is_boot() {
                out.insert(Stress::HeldAccess(During::Boot));
            }
            if s.is_syscall() {
                out.insert(Stress::HeldAccess(During::Syscall));
            }
        }
        if let Some(Stage::Poll(cursor)) = stage
            && cursor.table.is_some()
            && acc.kernel.is_some()
            && matches!(p.blk.engine, Engine::WaitBeat(_))
            && acc.dma.is_some()
        {
            out.insert(Stress::BeatBehindPoll);
        }
        if let (Some(Stage::Load { .. } | Stage::Create { .. }), Some(op)) = (stage, &k.op)
            && op.step > 0
        {
            out.insert(Stress::PartialCopy);
        }
        if let Some(Stage::Output { buffer, done, .. }) = stage
            && *done > 0
            && *done < buffer.n
        {
            out.insert(Stress::PartialWrite);
        }
        match stage {
            Some(Stage::Dispatch { pid }) => {
                self.dispatched = Some(*pid);
                self.dispatch_done = false;
            }
            Some(_) => self.dispatch_done = false,
            None => {
                if self.dispatched.is_some() {
                    self.dispatch_done = true;
                }
            }
        }
        if self.dispatch_done && cpu.privilege == 1 && stage.is_none() {
            out.insert(Stress::FrameWrittenBeforeSret);
        }
        if cpu.privilege == 0
            && let Some(pid) = self.dispatched.take()
        {
            self.dispatch_done = false;
            let new = self.ran.insert(pid);
            if new && fetch_not_issued(&cpu.state) {
                out.insert(Stress::SretBeforeFirstFetch);
            }
        }
        self.prev_priv = Some(cpu.privilege);
        out
    }
}

/// What the whole chained run saw.
#[derive(Clone, Debug, Default)]
pub struct Coverage {
    /// Boundaries checked: every `k` from 0 to the event count.
    pub checked: usize,
    /// Every `k` at each stress point.
    pub stress: BTreeMap<Stress, Vec<usize>>,
    /// Boundaries per combination of outstanding master transactions' places.
    pub places: BTreeMap<Vec<(usize, Place)>, usize>,
    /// Boundaries with a media operation outstanding.
    pub media: usize,
    /// The most events in flight at one boundary.
    pub max_queued: usize,
    /// The largest snapshot.
    pub max_bytes: usize,
}

/// §9.2 and §17.4: the chained run. At every boundary from init to the end, the snapshot
/// passes [`portable::read`], is classified, is restored into a freshly built platform,
/// and re-encodes to the same bytes; the restored platform dispatches the next event,
/// which must be the reference's. The end must be the reference's: the event count, the
/// final snapshot, `StateDigest`, `ExecutionDigest`, and the UART output.
pub fn chained(fixture: &Fixture, reference: &Reference) -> Result<Coverage, String> {
    let mut coverage = Coverage::default();
    let mut tracker = Tracker::default();
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    let dispatched = reference.dispatched();
    for k in 0..=dispatched.len() {
        let snapshot = rt.snapshot().map_err(|e| format!("{k}: {e}"))?;
        coverage.record(k, &snapshot, &mut tracker)?;
        let mut restored = fixture.platform();
        restored
            .restore(&snapshot)
            .map_err(|e| format!("boundary {k}: restore failed: {e}"))?;
        if restored.snapshot().map_err(|e| e.to_string())? != snapshot {
            return Err(format!(
                "boundary {k}: restore -> snapshot is not the identity"
            ));
        }
        rt = restored;
        match rt.step().map_err(|e| format!("event {k}: {e}"))? {
            Some(ev) if Some(&ev) == dispatched.get(k) => {}
            Some(_) => return Err(format!("event {k} is not the reference's")),
            None if k == dispatched.len() => {}
            None => return Err(format!("the chained run ended after {k} events")),
        }
    }
    let end = rt.snapshot().map_err(|e| e.to_string())?;
    let o = &reference.run.finished.outcome;
    if end != reference.final_snapshot()
        || rt.state_digest().ok() != o.state
        || rt.execution_digest() != o.execution
    {
        return Err("the chained run ends in another state".to_owned());
    }
    let (p, _) = portable::read(&end)?;
    if Ok(&p.uart) != reference.run.output.as_ref() {
        return Err("the chained run printed something else".to_owned());
    }
    Ok(coverage)
}

impl Coverage {
    /// Checks boundary `k`'s `snapshot` with [`portable::read`] and counts it.
    fn record(&mut self, k: usize, snapshot: &[u8], tracker: &mut Tracker) -> Result<(), String> {
        let (p, acc) = portable::read(snapshot).map_err(|e| format!("boundary {k}: {e}"))?;
        self.checked += 1;
        for s in tracker.classify(&p, &acc) {
            self.stress.entry(s).or_default().push(k);
        }
        let key: Vec<(usize, Place)> = acc.masters().iter().map(|m| (m.0, m.2)).collect();
        *self.places.entry(key).or_default() += 1;
        self.media += usize::from(acc.media.is_some());
        self.max_queued = self.max_queued.max(p.queue.len());
        self.max_bytes = self.max_bytes.max(snapshot.len());
        Ok(())
    }

    /// The instances of each stress point the exact resumes cover: the first, the middle,
    /// and the last boundary at it.
    pub fn exact_points(&self) -> Result<Vec<(Stress, usize)>, String> {
        let mut out = Vec::new();
        for s in Stress::ALL {
            let ks = self
                .stress
                .get(&s)
                .filter(|ks| !ks.is_empty())
                .ok_or_else(|| format!("stress point {} is never reached", s.name()))?;
            let mut picked = vec![ks[0], ks[ks.len() / 2], ks[ks.len() - 1]];
            picked.dedup();
            out.extend(picked.into_iter().map(|k| (s, k)));
        }
        Ok(out)
    }

    /// The first boundary [`is_mid`] accepts.
    pub fn mid_checkpoint(&self) -> Result<usize, String> {
        let held = self.stress.get(&Stress::HeldAccess(During::Syscall));
        let partial = self.stress.get(&Stress::PartialWrite);
        held.into_iter()
            .flatten()
            .find(|k| partial.is_some_and(|p| p.contains(k)))
            .copied()
            .ok_or_else(|| "no boundary holds ENTER in a partial write".to_owned())
    }
}

/// Whether boundary stress set `points` is the portable snapshot's checkpoint (§9.2): stress
/// point 3 during a syscall where the `write` already has bytes at the UART (point 6), so
/// one snapshot holds the held `ENTER`, the kernel's access, a partial output, and a user
/// process with its address space.
pub fn is_mid(points: &BTreeSet<Stress>) -> bool {
    points.contains(&Stress::HeldAccess(During::Syscall)) && points.contains(&Stress::PartialWrite)
}

/// The portable snapshot's checkpoint in an untraced run from `init`: the first boundary
/// [`is_mid`] accepts, classified from its snapshot through [`portable::read`], and that
/// snapshot.
pub fn mid_point(fixture: &Fixture) -> Result<(usize, Vec<u8>), String> {
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    let mut tracker = Tracker::default();
    for k in 0.. {
        let snapshot = rt.snapshot().map_err(|e| e.to_string())?;
        let (p, acc) = portable::read(&snapshot).map_err(|e| format!("boundary {k}: {e}"))?;
        if is_mid(&tracker.classify(&p, &acc)) {
            return Ok((k, snapshot));
        }
        if rt.step().map_err(|e| e.to_string())?.is_none() {
            break;
        }
    }
    Err("no boundary holds ENTER in a partial write".to_owned())
}

/// [`chained`]'s checks without the restores: every boundary of an untraced run of
/// `fixture` from `init`, read by [`portable::read`] and classified.
pub fn survey(fixture: &Fixture) -> Result<Coverage, String> {
    let mut coverage = Coverage::default();
    let mut tracker = Tracker::default();
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    for k in 0.. {
        let snapshot = rt.snapshot().map_err(|e| e.to_string())?;
        coverage.record(k, &snapshot, &mut tracker)?;
        if rt.step().map_err(|e| format!("event {k}: {e}"))?.is_none() {
            break;
        }
    }
    Ok(coverage)
}

/// Runs a restored session to its end under the watchdog: at most [`EVENT_BUDGET`]
/// events after the restore.
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

/// A mem request's or response's `TxnId`, with `true` for a request.
fn mem(ev: &Dispatched) -> Option<(u64, bool)> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => Some(match m {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => (txn.0, true),
            MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => (txn.0, false),
        }),
        _ => None,
    }
}

fn media(ev: &Dispatched) -> Option<(u64, bool)> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::Block(b),
            ..
        } => Some(match b {
            BlockMsg::ReadBlock { txn, .. } | BlockMsg::WriteBlock { txn, .. } => (txn.0, true),
            BlockMsg::ReadResult { txn, .. } | BlockMsg::WriteResult { txn, .. } => (txn.0, false),
        }),
        _ => None,
    }
}

/// Checks one issuer's requests after a checkpoint: requests below its counter at the
/// checkpoint are exactly the ones `in_flight` there, each dispatched once; new ones
/// start at the counter and rise by one.
fn issued(what: &str, requests: &[u64], counter: u64, in_flight: &[u64]) -> Result<(), String> {
    let old: Vec<u64> = requests.iter().copied().filter(|&t| t < counter).collect();
    let mut expected = in_flight.to_vec();
    expected.sort_unstable();
    let mut sorted = old.clone();
    sorted.sort_unstable();
    if sorted != expected {
        return Err(format!(
            "{what}: requests {old:?} before the counter {counter} after the checkpoint, \
             with {in_flight:?} in flight: a reissue"
        ));
    }
    let new: Vec<u64> = requests.iter().copied().filter(|&t| t >= counter).collect();
    if new.iter().zip(counter..).any(|(&t, want)| t != want) {
        return Err(format!(
            "{what}: new requests {:?}.. do not continue the counter {counter}",
            &new[..new.len().min(4)]
        ));
    }
    Ok(())
}

/// §17.4 no-reissue and `TxnId` determinism: after checkpoint `p` (holding `acc`), the
/// events `rest` never send an outstanding request again, complete each outstanding
/// master transaction exactly once, and allocate every new `TxnId` from the checkpoint's
/// counters, per issuer: the CPU, the block controller's DMA beats and media operations,
/// the kernel, and the bus's downstream transfers.
pub fn no_reissue(p: &Platform, acc: &Accounting, rest: &[Dispatched]) -> Result<(), String> {
    let counters = [p.cpu.next_txn, p.blk.dma_txn, p.kernel.next_txn];
    let names = ["cpu", "dma", "kernel"];
    let masters = acc.masters();
    for (m, &master) in MASTERS.iter().enumerate() {
        let from = |ev: &&Dispatched| ev.source == master && ev.target == BUS;
        let requests: Vec<u64> = rest
            .iter()
            .filter(from)
            .filter_map(mem)
            .filter(|&(_, req)| req)
            .map(|(t, _)| t)
            .collect();
        let in_flight: Vec<u64> = masters
            .iter()
            .filter(|o| o.0 == m && o.2 == Place::Request)
            .map(|o| o.1)
            .collect();
        issued(names[m], &requests, counters[m], &in_flight)?;
        if let Some(&(_, t, _)) = masters.iter().find(|o| o.0 == m) {
            let completions = rest
                .iter()
                .filter(|ev| ev.source == BUS && ev.target == master)
                .filter_map(mem)
                .filter(|&(txn, req)| txn == t && !req)
                .count();
            if completions != 1 {
                return Err(format!(
                    "{} txn {t} completes {completions} times after the checkpoint",
                    names[m]
                ));
            }
        }
    }
    let downstream: Vec<u64> = rest
        .iter()
        .filter(|ev| ev.source == BUS && TARGETS.contains(&ev.target))
        .filter_map(mem)
        .filter(|&(_, req)| req)
        .map(|(t, _)| t)
        .collect();
    let down_in_flight: Vec<u64> = p
        .bus
        .regions
        .iter()
        .filter_map(|r| r.active)
        .filter(|a| {
            masters.iter().any(|o| {
                usize::from(a.master) == o.0
                    && a.original == o.1
                    && o.2 == Place::Active(At::Request)
            })
        })
        .map(|a| a.downstream)
        .collect();
    issued(
        "bus downstream",
        &downstream,
        p.bus.next_downstream,
        &down_in_flight,
    )?;
    let block: Vec<u64> = rest
        .iter()
        .filter(|ev| ev.source == BLK && ev.target == DISK)
        .filter_map(media)
        .filter(|&(_, req)| req)
        .map(|(t, _)| t)
        .collect();
    let media_in_flight: Vec<u64> = acc
        .media
        .iter()
        .filter(|&&(_, result)| !result)
        .map(|&(t, _)| t)
        .collect();
    issued("media", &block, p.blk.blk_txn, &media_in_flight)?;
    Ok(())
}

/// Restores checkpoint `k`'s `snapshot` into a freshly built platform, requires the
/// re-encoded bytes back, resumes the trace prefix, runs to the end under the watchdog,
/// and compares the end with the reference in full: §12.3 with the committed `os.*`
/// records, the replayed events, [`no_reissue`], the final snapshot, `StateDigest`,
/// `ExecutionDigest`, the trace's canonical bytes and `TraceDigest`, and the metrics.
pub fn resume_exact(
    fixture: &Fixture,
    reference: &Reference,
    k: usize,
    snapshot: Vec<u8>,
) -> Result<(), String> {
    let (p, acc) = portable::read(&snapshot)?;
    let mut rt = fixture.platform();
    rt.restore(&snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
        return Err("snapshot -> restore -> snapshot is not the identity".to_owned());
    }
    let resumed = fixture.run(
        Start::Restore {
            snapshot,
            prefix: Some(reference.prefix(k)),
        },
        Vec::new(),
    );
    let mut differ = Vec::new();
    if let Err(e) = m3::judge(&resumed, fixture.scenario, &fixture.expected) {
        differ.push(format!("§12.3 ({e})"));
    }
    let rest = &resumed.finished.dispatched;
    if reference.dispatched().get(k..) != Some(&rest[..]) {
        differ.push("replayed events".to_owned());
    }
    if let Err(e) = no_reissue(&p, &acc, rest) {
        differ.push(e);
    }
    if resumed.snapshot.as_deref() != Some(reference.final_snapshot()) {
        differ.push("final snapshot".to_owned());
    }
    let (x, y) = (&reference.run.finished.outcome, &resumed.finished.outcome);
    if x.state != y.state {
        differ.push("StateDigest".to_owned());
    }
    if x.execution != y.execution {
        differ.push("ExecutionDigest".to_owned());
    }
    if x.trace != y.trace {
        differ.push("TraceDigest".to_owned());
    }
    let bytes = resumed.finished.trace.as_ref().map(Trace::canonical_bytes);
    if bytes.as_ref() != Some(&reference.trace_bytes) {
        differ.push("canonical trace bytes".to_owned());
    }
    // A resumed run counts the events after its restore.
    let mut metrics = Metrics::of(&resumed);
    metrics.events += k as u64;
    if metrics != reference.metrics {
        differ.push("metrics".to_owned());
    }
    if resumed.output != reference.run.output {
        differ.push("UART output".to_owned());
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(format!("checkpoint {k} differs in {}", differ.join(", ")))
    }
}

/// Snapshots the frozen platform after `k` events of an untraced run from `init`.
pub fn snapshot_after(fixture: &Fixture, k: usize) -> Result<Vec<u8>, String> {
    let mut rt = fixture.platform();
    rt.init().map_err(|e| e.to_string())?;
    for i in 0..k {
        rt.step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the scenario ended after {i} events"))?;
    }
    rt.snapshot().map_err(|e| e.to_string())
}

/// The untraced resume of checkpoint `k`: restore into a freshly built platform, the
/// re-encoded bytes back, then every remaining event the reference's in order, no
/// reissue, and the reference's final snapshot, `StateDigest`, and `ExecutionDigest`.
pub fn resume_untraced(
    fixture: &Fixture,
    reference: &Reference,
    k: usize,
    snapshot: &[u8],
) -> Result<(), String> {
    let (p, acc) = portable::read(snapshot)?;
    let mut rt = fixture.platform();
    rt.restore(snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
        return Err("snapshot -> restore -> snapshot is not the identity".to_owned());
    }
    let rest = continue_to_end(&mut rt)?;
    if reference.dispatched().get(k..) != Some(&rest[..]) {
        return Err(format!("checkpoint {k}: the remaining events differ"));
    }
    no_reissue(&p, &acc, &rest).map_err(|e| format!("checkpoint {k}: {e}"))?;
    let o = &reference.run.finished.outcome;
    if rt.snapshot().map_err(|e| e.to_string())? != reference.final_snapshot()
        || rt.state_digest().ok() != o.state
        || rt.execution_digest() != o.execution
    {
        return Err(format!(
            "checkpoint {k}: the resumed run ends in another state"
        ));
    }
    Ok(())
}

/// The outcome of [`every_event`].
#[derive(Clone, Debug, Default)]
pub struct Sweep {
    /// Checkpoints resumed.
    pub checked: usize,
    /// Every failure, as `(k, why)`.
    pub failures: Vec<(usize, String)>,
}

fn sweep_range(fixture: &Fixture, reference: &Reference, range: std::ops::Range<usize>) -> Sweep {
    let mut sweep = Sweep::default();
    let mut producer = fixture.platform();
    if let Err(e) = producer.init() {
        sweep.failures.push((range.start, e.to_string()));
        return sweep;
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
            sweep.failures.push((k, e));
            return sweep;
        }
    }
    for k in range {
        sweep.checked += 1;
        let result = producer
            .snapshot()
            .map_err(|e| e.to_string())
            .and_then(|s| resume_untraced(fixture, reference, k, &s));
        if let Err(e) = result {
            sweep.failures.push((k, e));
        }
        if k < reference.events()
            && let Err(e) = step(&mut producer, k)
        {
            sweep.failures.push((k, e));
            return sweep;
        }
    }
    sweep
}

/// Checkpoints `range` (at most 0 through `events`) of a run of `events` events, split
/// into at most `parts` contiguous, non-empty ranges of about equal cost: resuming
/// checkpoint `k` runs the `events - k` remaining events, plus the restore itself.
pub fn balanced(
    range: std::ops::Range<usize>,
    events: usize,
    parts: usize,
) -> Vec<std::ops::Range<usize>> {
    let range = range.start..range.end.min(events + 1);
    let cost = |k: usize| (events - k + 1) as u128;
    let total: u128 = range.clone().map(cost).sum();
    let parts = parts.clamp(1, range.len().max(1)) as u128;
    let mut out = Vec::new();
    let (mut start, mut spent, mut part) = (range.start, 0u128, 1u128);
    for k in range.clone() {
        spent += cost(k);
        if part < parts && spent * parts >= part * total {
            out.push(start..k + 1);
            start = k + 1;
            part += 1;
        }
    }
    if start < range.end {
        out.push(start..range.end);
    }
    out
}

/// §9.2 "resume from every event": every checkpoint `k` in `range` (at most 0 through
/// the last event) resumed with [`resume_untraced`], split [`balanced`] across
/// `threads` threads. Each thread builds its own platforms; only the fixture and the
/// reference are shared.
pub fn every_event(
    fixture: &Fixture,
    reference: &Reference,
    range: std::ops::Range<usize>,
    threads: usize,
) -> Sweep {
    let parts = balanced(range, reference.events(), threads);
    let mut sweep = Sweep::default();
    thread::scope(|s| {
        let handles: Vec<_> = parts
            .into_iter()
            .map(|part| s.spawn(move || sweep_range(fixture, reference, part)))
            .collect();
        for h in handles {
            let part = h.join().expect("a checkpoint thread panicked");
            sweep.checked += part.checked;
            sweep.failures.extend(part.failures);
        }
    });
    sweep.failures.sort_by_key(|f| f.0);
    sweep
}
