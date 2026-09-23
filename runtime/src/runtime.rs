//! Session lifecycle and event dispatch (`docs/m0-design.md` §4.4 and §5.1).
//!
//! ```text
//! New session:      elaborate → init (ComponentId order) → Ready → run
//! Restored session: elaborate → restore                  → Ready → run
//! Any error                                              → Faulted (terminal)
//! ```
//!
//! Observers (§8.2) watch from outside: they see each dispatched event, every trace record,
//! and due observe points through a read-only `WorldView`, and may ask the driver to pause.

use std::collections::BTreeSet;
use std::fmt;

use systemscope_contracts::canonical::{CanonicalEvent, Encoder};
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::snapshot::RestoreError;
use systemscope_contracts::time::{ClockDomain, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{
    CONTRACTS_VERSION, ComponentDecl, LinkDecl, TraceAt, TraceHeader, TraceOrigin, TraceRecord,
    Value, encode_topology,
};

use crate::rng::Xoshiro256StarStar;
use crate::scheduler::{Scheduler, SchedulerConfig};
use crate::trace::{ResumeError, Trace, check_prefix, dispatch_record};

/// Settings fixed for the lifetime of a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionConfig {
    /// Seeds every component's random stream (§5.2).
    pub seed: u64,
    /// Scheduler limits.
    pub scheduler: SchedulerConfig,
}

/// Where a session is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Elaborated; `init` has not run.
    Elaborated,
    /// Initialized or restored; events may be dispatched.
    Ready,
    /// An error occurred. Terminal: only inspection is allowed.
    Faulted,
}

/// Why a runtime call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    /// The session is faulted, by this call or an earlier one, with this error.
    Faulted(SimError),
    /// The call is not allowed in this lifecycle state.
    InvalidState(Lifecycle),
    /// A snapshot could not be restored. The session is now faulted.
    Restore(RestoreError),
    /// `restore` was called after `start_trace`. A restored session continues its trace
    /// with `resume_trace` instead.
    TraceNeedsPrefix,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Faulted(e) => write!(f, "session faulted: {e}"),
            RuntimeError::InvalidState(s) => write!(f, "not allowed in state {s:?}"),
            RuntimeError::Restore(e) => write!(f, "restore failed: {e}"),
            RuntimeError::TraceNeedsPrefix => {
                f.write_str("a restored session resumes its trace with resume_trace")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// What the runtime delivers when an event runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) source: ComponentId,
    pub(crate) target: ComponentId,
    pub(crate) delivery: Delivered,
}

/// One dispatched event, as seen by the driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatched {
    /// The event's key.
    pub key: EventKey,
    /// The component that scheduled it, stamped by the runtime.
    pub source: ComponentId,
    /// The component that handled it.
    pub target: ComponentId,
    /// What was delivered.
    pub delivery: Delivered,
}

/// Why [`Runtime::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// An observer asked for a pause. Call `run` again to resume.
    Paused,
    /// The queue is empty.
    Drained,
    /// The next event is after `until`.
    Horizon,
}

/// What a [`Runtime::run`] call did. Driver state only: never part of the simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// Events dispatched by this call.
    pub events: u64,
    /// Why it returned.
    pub stop: Stop,
}

/// The far end of a link, seen from one port.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Peer {
    pub(crate) component: ComponentId,
    pub(crate) port: PortId,
    pub(crate) latency: Option<LinkLatency>,
}

/// A component owned by the runtime, with its elaborated ports.
pub(crate) struct Slot {
    pub(crate) path: String,
    pub(crate) ports: Vec<PortSpec>,
    pub(crate) component: Box<dyn Component>,
}

/// Elaborated facts about a component, kept apart from the component itself so a
/// running component and its context never borrow the same storage.
pub(crate) struct SlotInfo {
    pub(crate) path: String,
    pub(crate) type_name: &'static str,
    pub(crate) ports: Vec<PortSpec>,
}

/// A simulation session. Owns every component; components never reach each other.
pub struct Runtime {
    pub(crate) clock: SimulationClock,
    pub(crate) domains: Vec<ClockDomain>,
    pub(crate) components: Vec<Box<dyn Component>>,
    /// One random stream per component, owned here so snapshots can capture them.
    pub(crate) rngs: Vec<Xoshiro256StarStar>,
    pub(crate) slots: Vec<SlotInfo>,
    /// `peers[component][port]` is the other end of that port's link.
    peers: Vec<Vec<Peer>>,
    /// Links in declaration order, for the trace header and the topology hash.
    links: Vec<LinkDecl>,
    pub(crate) topology_hash: [u8; 32],
    pub(crate) config: SessionConfig,
    pub(crate) scheduler: Scheduler<Pending>,
    /// Chained BLAKE3 over every dispatched event's `canonical(ev)` (§4.4).
    pub(crate) execution_digest: [u8; 32],
    pub(crate) lifecycle: Lifecycle,
    fault: Option<SimError>,
    /// Restored and not yet stepped: the only time `resume_trace` is accepted.
    pub(crate) freshly_restored: bool,
    /// Recorded trace, if tracing was started. Never read by the simulation.
    pub(crate) trace: Option<Vec<TraceRecord>>,
    /// Records emitted by the running component, awaiting observers and the recorder.
    emitted: Vec<TraceRecord>,
    /// Never snapshotted, digested, or read by the simulation (§8.2).
    observers: Vec<Box<dyn Observer>>,
    /// Observe points, outside the event queue: they consume no sequence numbers.
    observe_points: BTreeSet<Tick>,
}

/// Absorbs one event into an execution digest: `H(digest ‖ canonical(ev))`.
pub(crate) fn chain(digest: &[u8; 32], ev: &CanonicalEvent) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(digest);
    h.update(&ev.to_bytes());
    *h.finalize().as_bytes()
}

impl Runtime {
    pub(crate) fn from_elaboration(
        clock: SimulationClock,
        domains: Vec<ClockDomain>,
        slots: Vec<Slot>,
        peers: Vec<Vec<Peer>>,
        links: Vec<LinkDecl>,
        config: SessionConfig,
    ) -> Runtime {
        let rngs = slots
            .iter()
            .map(|s| Xoshiro256StarStar::for_component(config.seed, &s.path))
            .collect();
        let (slots, components): (Vec<SlotInfo>, _) = slots
            .into_iter()
            .map(|s| {
                let info = SlotInfo {
                    path: s.path,
                    type_name: s.component.type_name(),
                    ports: s.ports,
                };
                (info, s.component)
            })
            .unzip();
        let mut topology = Encoder::new();
        encode_topology(&mut topology, &component_decls(&slots), &links);
        Runtime {
            clock,
            domains,
            components,
            rngs,
            slots,
            peers,
            links,
            topology_hash: *blake3::hash(topology.as_bytes()).as_bytes(),
            config,
            scheduler: Scheduler::new(config.scheduler),
            execution_digest: [0; 32],
            lifecycle: Lifecycle::Elaborated,
            fault: None,
            freshly_restored: false,
            trace: None,
            emitted: Vec::new(),
            observers: Vec::new(),
            observe_points: BTreeSet::new(),
        }
    }

    /// `topology_hash`: BLAKE3 of the structural topology (§6).
    pub fn topology_hash(&self) -> [u8; 32] {
        self.topology_hash
    }

    /// `ExecutionDigest` so far: 32 zero bytes, chained with every dispatched event.
    pub fn execution_digest(&self) -> [u8; 32] {
        self.execution_digest
    }

    /// The current lifecycle state.
    pub fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    /// The error that faulted the session, if any.
    pub fn fault(&self) -> Option<SimError> {
        self.fault
    }

    /// The current tick.
    pub fn now(&self) -> Tick {
        self.scheduler.now()
    }

    /// Number of pending events.
    pub fn pending(&self) -> usize {
        self.scheduler.len()
    }

    /// Key of the next event to dispatch, if any.
    pub fn peek_key(&self) -> Option<EventKey> {
        self.scheduler.peek_key()
    }

    /// Key of the last dispatched event, if any.
    pub fn last_dispatched(&self) -> Option<EventKey> {
        self.scheduler.last_dispatched()
    }

    /// The sequence number the next scheduled event will receive.
    pub fn next_sequence(&self) -> u64 {
        self.scheduler.next_sequence()
    }

    /// Adds an observer (§8.2). Observers are driver state: they are never snapshotted
    /// and cannot change the simulation.
    pub fn add_observer(&mut self, observer: Box<dyn Observer>) {
        self.observers.push(observer);
    }

    /// Requests an observe point at `tick`. Points live outside the event queue, so this
    /// consumes no sequence number; registering a tick twice is the same as once.
    pub fn observe_at(&mut self, tick: Tick) {
        self.observe_points.insert(tick);
    }

    /// Observe points that have not fired yet.
    pub fn pending_observe_points(&self) -> usize {
        self.observe_points.len()
    }

    /// Paths of all components, indexed by `ComponentId`.
    pub fn component_paths(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().map(|s| s.path.as_str())
    }

    /// Starts recording a trace. Allowed only before `init`, so the trace holds every
    /// record from the start. Tracing never changes what the simulation does.
    pub fn start_trace(&mut self) -> Result<(), RuntimeError> {
        self.require(Lifecycle::Elaborated)?;
        self.trace = Some(Vec::new());
        Ok(())
    }

    /// Continues a trace across a restore (§8.1). Accepted only right after `restore`,
    /// before the first step, and only for the prefix of the run the snapshot came from:
    /// its header must equal this session's, and replaying its dispatch records must
    /// reproduce the restored `ExecutionDigest` and last dispatched key.
    pub fn resume_trace(&mut self, prefix: Trace) -> Result<(), ResumeError> {
        if self.lifecycle != Lifecycle::Ready || !self.freshly_restored {
            return Err(ResumeError::NotFreshlyRestored);
        }
        if prefix.header != self.trace_header() {
            return Err(ResumeError::HeaderMismatch);
        }
        check_prefix(
            &prefix.records,
            self.slots.len(),
            self.scheduler.last_dispatched(),
            &self.execution_digest,
        )?;
        self.trace = Some(prefix.records);
        self.freshly_restored = false;
        Ok(())
    }

    /// Stops tracing and returns what was recorded, or `None` if tracing never started.
    pub fn take_trace(&mut self) -> Option<Trace> {
        let records = self.trace.take()?;
        Some(Trace {
            header: self.trace_header(),
            records,
        })
    }

    /// The trace header describing this session.
    pub fn trace_header(&self) -> TraceHeader {
        TraceHeader {
            ticks_per_second: self.clock.ticks_per_second(),
            seed: self.config.seed,
            contracts_version: CONTRACTS_VERSION.to_owned(),
            topology_hash: self.topology_hash,
            clock_domains: self.domains.clone(),
            components: component_decls(&self.slots),
            links: self.links.clone(),
        }
    }

    /// Initializes every component in `ComponentId` order. Allowed once, when elaborated.
    ///
    /// Any failure faults the session, even though earlier components already
    /// initialized.
    pub fn init(&mut self) -> Result<(), RuntimeError> {
        self.require(Lifecycle::Elaborated)?;
        for index in 0..self.slots.len() {
            let me = ComponentId(index as u32);
            let (component, mut ctx) = self.context(me, TraceAt::Init);
            let result = component.init(&mut ctx);
            let result = ctx.error.map_or(result, Err);
            self.flush_records();
            if let Err(e) = result {
                return Err(self.enter_fault(e));
            }
        }
        self.lifecycle = Lifecycle::Ready;
        Ok(())
    }

    /// Dispatches exactly one event, with its observer callbacks and the observe points
    /// that become due, and returns it. Returns `None` when the queue is empty. Pause
    /// requests are ignored, since `step` returns anyway.
    pub fn step(&mut self) -> Result<Option<Dispatched>, RuntimeError> {
        self.require(Lifecycle::Ready)?;
        self.fire_observe_points(None, false);
        let dispatched = self.dispatch()?;
        if dispatched.is_some() {
            self.fire_observe_points(None, false);
        }
        Ok(dispatched.map(|(d, _)| d))
    }

    /// Dispatches events at or before `until` until none is left before then, or an
    /// observer asks for a pause. Resuming is calling `run` again; a paused run continues
    /// exactly as if it had never stopped.
    pub fn run(&mut self, until: Tick) -> Result<RunOutcome, RuntimeError> {
        self.require(Lifecycle::Ready)?;
        let mut events = 0;
        let outcome = |events, stop| Ok(RunOutcome { events, stop });
        loop {
            if self.fire_observe_points(Some(until), true) == Control::Pause {
                return outcome(events, Stop::Paused);
            }
            match self.scheduler.peek_key() {
                None => return outcome(events, Stop::Drained),
                Some(k) if k.tick > until => return outcome(events, Stop::Horizon),
                Some(_) => {}
            }
            let Some((_, control)) = self.dispatch()? else {
                return outcome(events, Stop::Drained);
            };
            events += 1;
            if control == Control::Pause {
                return outcome(events, Stop::Paused);
            }
        }
    }

    /// [`Runtime::run`] through any pauses. Returns how many events ran.
    pub fn run_until(&mut self, until: Tick) -> Result<u64, RuntimeError> {
        let mut total = 0;
        loop {
            let RunOutcome { events, stop } = self.run(until)?;
            total += events;
            if stop != Stop::Paused {
                return Ok(total);
            }
        }
    }

    /// Pops and dispatches the next event, then shows it to the observers. Returns the
    /// event and whether any observer asked for a pause.
    fn dispatch(&mut self) -> Result<Option<(Dispatched, Control)>, RuntimeError> {
        let event = match self.scheduler.pop() {
            Ok(Some(event)) => event,
            Ok(None) => return Ok(None),
            Err(e) => return Err(self.enter_fault(e)),
        };
        let Pending {
            source,
            target,
            delivery,
        } = event.payload;
        let ev = CanonicalEvent {
            key: event.key,
            source,
            target,
            delivery,
        };
        self.execution_digest = chain(&self.execution_digest, &ev);
        self.freshly_restored = false;
        let CanonicalEvent { delivery, .. } = ev;
        let at = TraceAt::Event(event.key);
        if self.capturing() {
            self.emitted
                .push(dispatch_record(at, source, target, &delivery));
        }
        let (component, mut ctx) = self.context(target, at);
        let result = component.handle_event(&delivery, &mut ctx);
        let result = ctx.error.map_or(result, Err);
        self.flush_records();
        if let Err(e) = result {
            return Err(self.enter_fault(e));
        }
        let dispatched = Dispatched {
            key: event.key,
            source,
            target,
            delivery,
        };
        let mut control = Control::Continue;
        if !self.observers.is_empty() {
            let view = EventView {
                key: dispatched.key,
                source,
                target,
                delivery: &dispatched.delivery,
            };
            let world = WorldView::new(dispatched.key.tick, &self.components);
            for observer in &mut self.observers {
                if observer.on_after_dispatch(&view, &world) == Control::Pause {
                    control = Control::Pause;
                }
            }
        }
        Ok(Some((dispatched, control)))
    }

    /// Fires due observe points in tick order (§8.2). A point `T` is due once tick `T` is
    /// finished: the next event is after `T`, or the queue is empty and `T` is at or
    /// before the current tick or the `run` horizon. With `stop_on_pause`, returns at the
    /// first point an observer pauses on and leaves later points for the next call.
    fn fire_observe_points(&mut self, horizon: Option<Tick>, stop_on_pause: bool) -> Control {
        let mut control = Control::Continue;
        while let Some(&point) = self.observe_points.first() {
            let now = self.scheduler.now();
            let last_finished = match self.scheduler.peek_key() {
                Some(next) => next.tick.0.checked_sub(1).map(Tick),
                None => Some(horizon.map_or(now, |h| h.max(now))),
            };
            if !last_finished.is_some_and(|last| point <= last) {
                break;
            }
            self.observe_points.pop_first();
            let world = WorldView::new(point, &self.components);
            for observer in &mut self.observers {
                if observer.on_observe(point, &world) == Control::Pause {
                    control = Control::Pause;
                }
            }
            if stop_on_pause && control == Control::Pause {
                break;
            }
        }
        control
    }

    /// Whether trace records are produced: for a recorder, observers, or both.
    fn capturing(&self) -> bool {
        self.trace.is_some() || !self.observers.is_empty()
    }

    /// Hands the running component's records to the observers, then to the recorder.
    fn flush_records(&mut self) {
        if self.emitted.is_empty() {
            return;
        }
        for record in &self.emitted {
            for observer in &mut self.observers {
                observer.on_trace(record);
            }
        }
        match &mut self.trace {
            Some(records) => records.append(&mut self.emitted),
            None => self.emitted.clear(),
        }
    }

    fn require(&self, state: Lifecycle) -> Result<(), RuntimeError> {
        match (self.lifecycle, self.fault) {
            (Lifecycle::Faulted, Some(e)) => Err(RuntimeError::Faulted(e)),
            (current, _) if current != state => Err(RuntimeError::InvalidState(current)),
            _ => Ok(()),
        }
    }

    pub(crate) fn require_state(&self, state: Lifecycle) -> Result<(), RuntimeError> {
        self.require(state)
    }

    /// Faults the session after a failed restore.
    pub(crate) fn fail_restore(&mut self, error: RestoreError) -> RuntimeError {
        self.lifecycle = Lifecycle::Faulted;
        RuntimeError::Restore(error)
    }

    fn enter_fault(&mut self, error: SimError) -> RuntimeError {
        self.lifecycle = Lifecycle::Faulted;
        self.fault = Some(error);
        RuntimeError::Faulted(error)
    }

    /// Splits the runtime into the running component and a context for it.
    fn context(&mut self, me: ComponentId, at: TraceAt) -> (&mut dyn Component, Ctx<'_>) {
        let index = me.0 as usize;
        let capturing = self.capturing();
        let ctx = Ctx {
            me,
            clock: &self.clock,
            domains: &self.domains,
            ports: &self.slots[index].ports,
            peers: &self.peers[index],
            scheduler: &mut self.scheduler,
            rng: &mut self.rngs[index],
            trace: capturing.then_some(&mut self.emitted),
            at,
            error: None,
        };
        (self.components[index].as_mut(), ctx)
    }
}

/// Components as the trace header and topology hash record them.
fn component_decls(slots: &[SlotInfo]) -> Vec<ComponentDecl> {
    slots
        .iter()
        .map(|s| ComponentDecl {
            path: s.path.clone(),
            type_name: s.type_name,
            ports: s.ports.clone(),
        })
        .collect()
}

/// Resolves a component's scheduling request to an absolute tick. Runtime only.
fn resolve(
    when: ScheduleWhen,
    now: Tick,
    clock: &SimulationClock,
    domains: &[ClockDomain],
) -> Result<Tick, SimError> {
    let tick = match when {
        ScheduleWhen::Now => now,
        ScheduleWhen::After(d) => clock.after(now, d)?,
        ScheduleWhen::Cycles { domain, k } => domains
            .get(domain.0 as usize)
            .ok_or(SimError::UnknownClockDomain(domain))?
            .cycles_after(now, k)?,
    };
    Ok(tick)
}

/// Adds a link's latency to a send tick. Runtime only.
fn add_latency(
    latency: Option<LinkLatency>,
    tick: Tick,
    clock: &SimulationClock,
    domains: &[ClockDomain],
) -> Result<Tick, SimError> {
    match latency {
        None => Ok(tick),
        Some(LinkLatency::After(d)) => resolve(ScheduleWhen::After(d), tick, clock, domains),
        Some(LinkLatency::Cycles { domain, k }) => {
            resolve(ScheduleWhen::Cycles { domain, k }, tick, clock, domains)
        }
    }
}

/// The context handed to a running component. Implements both context traits.
struct Ctx<'a> {
    me: ComponentId,
    clock: &'a SimulationClock,
    domains: &'a [ClockDomain],
    ports: &'a [PortSpec],
    peers: &'a [Peer],
    scheduler: &'a mut Scheduler<Pending>,
    rng: &'a mut Xoshiro256StarStar,
    /// Where records go, or `None` when nothing captures them. Write-only for the component.
    trace: Option<&'a mut Vec<TraceRecord>>,
    at: TraceAt,
    /// First error returned to the component. Sticky: the runtime faults on it.
    error: Option<SimError>,
}

impl Ctx<'_> {
    fn record<T>(&mut self, result: Result<T, SimError>) -> Result<T, SimError> {
        if let Err(e) = &result
            && self.error.is_none()
        {
            self.error = Some(*e);
        }
        result
    }

    fn try_send(
        &mut self,
        port: PortId,
        msg: Message,
        when: ScheduleWhen,
        phase: Phase,
    ) -> Result<(), SimError> {
        let spec = self
            .ports
            .get(usize::from(port.0))
            .ok_or(SimError::UnknownPort(port))?;
        if msg.protocol() != spec.protocol {
            return Err(SimError::ProtocolMismatch {
                port,
                expected: spec.protocol,
                actual: msg.protocol(),
            });
        }
        let peer = self.peers[usize::from(port.0)];
        let sent = resolve(when, self.scheduler.now(), self.clock, self.domains)?;
        let tick = add_latency(peer.latency, sent, self.clock, self.domains)?;
        let pending = Pending {
            source: self.me,
            target: peer.component,
            delivery: Delivered::Message {
                port: peer.port,
                msg,
            },
        };
        self.scheduler.schedule(tick, phase, pending).map(|_| ())
    }

    fn try_wake(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError> {
        let tick = resolve(when, self.scheduler.now(), self.clock, self.domains)?;
        let pending = Pending {
            source: self.me,
            target: self.me,
            delivery: Delivered::Wake { token },
        };
        self.scheduler.schedule(tick, phase, pending).map(|_| ())
    }
}

impl InitContext for Ctx<'_> {
    fn component(&self) -> ComponentId {
        self.me
    }

    fn send(
        &mut self,
        port: PortId,
        msg: Message,
        when: ScheduleWhen,
        phase: Phase,
    ) -> Result<(), SimError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let result = self.try_send(port, msg, when, phase);
        self.record(result)
    }

    fn wake_self(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let result = self.try_wake(when, phase, token);
        self.record(result)
    }

    fn rng(&mut self) -> &mut dyn SimRng {
        self.rng
    }

    fn trace(&mut self, kind: &'static str, fields: Vec<(&'static str, Value)>) {
        if let Some(records) = &mut self.trace {
            records.push(TraceRecord {
                at: self.at,
                origin: TraceOrigin::Component,
                component: self.me,
                kind,
                fields,
            });
        }
    }
}

impl SimContext for Ctx<'_> {
    fn now(&self) -> Tick {
        self.scheduler.now()
    }

    fn phase(&self) -> Phase {
        self.scheduler.phase()
    }
}
