//! Session lifecycle and event dispatch (`docs/m0-design.md` §4.4 and §5.1).
//!
//! ```text
//! New session:      elaborate → init (ComponentId order) → Ready → run
//! Restored session: elaborate → restore                  → Ready → run
//! Any error                                              → Faulted (terminal)
//! ```

use std::fmt;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::time::{ClockDomain, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;

use crate::rng::Xoshiro256StarStar;
use crate::scheduler::{Scheduler, SchedulerConfig};

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
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Faulted(e) => write!(f, "session faulted: {e}"),
            RuntimeError::InvalidState(s) => write!(f, "not allowed in state {s:?}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// What the runtime delivers when an event runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    source: ComponentId,
    target: ComponentId,
    delivery: Delivered,
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
struct SlotInfo {
    path: String,
    ports: Vec<PortSpec>,
}

/// A simulation session. Owns every component; components never reach each other.
pub struct Runtime {
    clock: SimulationClock,
    domains: Vec<ClockDomain>,
    components: Vec<Box<dyn Component>>,
    /// One random stream per component, owned here so snapshots can capture them.
    rngs: Vec<Xoshiro256StarStar>,
    slots: Vec<SlotInfo>,
    /// `peers[component][port]` is the other end of that port's link.
    peers: Vec<Vec<Peer>>,
    scheduler: Scheduler<Pending>,
    lifecycle: Lifecycle,
    fault: Option<SimError>,
}

impl Runtime {
    pub(crate) fn from_elaboration(
        clock: SimulationClock,
        domains: Vec<ClockDomain>,
        slots: Vec<Slot>,
        peers: Vec<Vec<Peer>>,
        config: SessionConfig,
    ) -> Runtime {
        let rngs = slots
            .iter()
            .map(|s| Xoshiro256StarStar::for_component(config.seed, &s.path))
            .collect();
        let (slots, components) = slots
            .into_iter()
            .map(|s| {
                let info = SlotInfo {
                    path: s.path,
                    ports: s.ports,
                };
                (info, s.component)
            })
            .unzip();
        Runtime {
            clock,
            domains,
            components,
            rngs,
            slots,
            peers,
            scheduler: Scheduler::new(config.scheduler),
            lifecycle: Lifecycle::Elaborated,
            fault: None,
        }
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

    /// Paths of all components, indexed by `ComponentId`.
    pub fn component_paths(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().map(|s| s.path.as_str())
    }

    /// Initializes every component in `ComponentId` order. Allowed once, when elaborated.
    ///
    /// Any failure faults the session, even though earlier components already
    /// initialized.
    pub fn init(&mut self) -> Result<(), RuntimeError> {
        self.require(Lifecycle::Elaborated)?;
        for index in 0..self.slots.len() {
            let me = ComponentId(index as u32);
            let (component, mut ctx) = self.context(me);
            let result = component.init(&mut ctx);
            if let Err(e) = ctx.error.map_or(result, Err) {
                return Err(self.enter_fault(e));
            }
        }
        self.lifecycle = Lifecycle::Ready;
        Ok(())
    }

    /// Dispatches the next event. Returns `None` when the queue is empty.
    pub fn step(&mut self) -> Result<Option<Dispatched>, RuntimeError> {
        self.require(Lifecycle::Ready)?;
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
        let (component, mut ctx) = self.context(target);
        let result = component.handle_event(&delivery, &mut ctx);
        if let Err(e) = ctx.error.map_or(result, Err) {
            return Err(self.enter_fault(e));
        }
        Ok(Some(Dispatched {
            key: event.key,
            source,
            target,
            delivery,
        }))
    }

    /// Dispatches every event at or before `until`. Returns how many ran.
    pub fn run_until(&mut self, until: Tick) -> Result<u64, RuntimeError> {
        self.require(Lifecycle::Ready)?;
        let mut count = 0;
        while self.scheduler.peek_key().is_some_and(|k| k.tick <= until) {
            self.step()?;
            count += 1;
        }
        Ok(count)
    }

    fn require(&self, state: Lifecycle) -> Result<(), RuntimeError> {
        match (self.lifecycle, self.fault) {
            (Lifecycle::Faulted, Some(e)) => Err(RuntimeError::Faulted(e)),
            (current, _) if current != state => Err(RuntimeError::InvalidState(current)),
            _ => Ok(()),
        }
    }

    fn enter_fault(&mut self, error: SimError) -> RuntimeError {
        self.lifecycle = Lifecycle::Faulted;
        self.fault = Some(error);
        RuntimeError::Faulted(error)
    }

    /// Splits the runtime into the running component and a context for it.
    fn context(&mut self, me: ComponentId) -> (&mut dyn Component, Ctx<'_>) {
        let index = me.0 as usize;
        let ctx = Ctx {
            me,
            clock: &self.clock,
            domains: &self.domains,
            ports: &self.slots[index].ports,
            peers: &self.peers[index],
            scheduler: &mut self.scheduler,
            rng: &mut self.rngs[index],
            error: None,
        };
        (self.components[index].as_mut(), ctx)
    }
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
}

impl SimContext for Ctx<'_> {
    fn now(&self) -> Tick {
        self.scheduler.now()
    }

    fn phase(&self) -> Phase {
        self.scheduler.phase()
    }
}
