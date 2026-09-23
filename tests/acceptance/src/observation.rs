//! AT-3: observation configurations O0 to O5 (`docs/m0-design.md` §9).

use std::cell::Cell;
use std::rc::Rc;

use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::MemMsg;
use systemscope_contracts::time::{SimulationClock, Tick};
use systemscope_reference::{ReferenceConfig, T_END, build};
use systemscope_runtime::export::{to_jsonl, to_perfetto};
use systemscope_runtime::runtime::Stop;

use crate::digests::{Digests, End, ensure_same};
use crate::layout::Layout;

/// Ticks between O4's observe points.
pub const PROBE_INTERVAL: u64 = 1_000;

/// An observation configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    /// No observers, no sinks.
    O0,
    /// The canonical recorder, with JSONL and Perfetto exports of its trace.
    O1,
    /// A breakpoint pausing on every `ReadResp`; the driver resumes at once.
    O2,
    /// One event per call, to the end.
    O3,
    /// An observe point every [`PROBE_INTERVAL`] ticks, inspecting every component.
    O4,
    /// O1, O2, O3, and O4 together.
    O5,
}

impl Config {
    /// Every configuration, in order.
    pub const ALL: [Config; 6] = [
        Config::O0,
        Config::O1,
        Config::O2,
        Config::O3,
        Config::O4,
        Config::O5,
    ];

    fn records(self) -> bool {
        matches!(self, Config::O1 | Config::O5)
    }
    fn breaks(self) -> bool {
        matches!(self, Config::O2 | Config::O5)
    }
    fn steps(self) -> bool {
        matches!(self, Config::O3 | Config::O5)
    }
    fn probes(self) -> bool {
        matches!(self, Config::O4 | Config::O5)
    }
}

/// Driver-side counts, shared with the observers.
#[derive(Default)]
struct Counts {
    pauses: Cell<u64>,
    observes: Cell<u64>,
    inspected: Cell<u64>,
}

struct Breakpoint(Rc<Counts>);

impl Observer for Breakpoint {
    fn on_after_dispatch(&mut self, ev: &EventView<'_>, _: &WorldView<'_>) -> Control {
        match ev.delivery {
            Delivered::Message {
                msg: Message::Mem(MemMsg::ReadResp { .. }),
                ..
            } => {
                self.0.pauses.set(self.0.pauses.get() + 1);
                Control::Pause
            }
            _ => Control::Continue,
        }
    }
}

struct Probe(Rc<Counts>);

impl Observer for Probe {
    fn on_observe(&mut self, _: Tick, world: &WorldView<'_>) -> Control {
        self.0.observes.set(self.0.observes.get() + 1);
        for id in 0..world.component_count() {
            let id = ComponentId(u32::try_from(id).expect("few components"));
            let fields = world.inspect(id).map_or(0, |v| v.fields.len());
            self.0.inspected.set(self.0.inspected.get() + fields as u64);
        }
        Control::Continue
    }
}

/// An observed run's results. The trace itself is dropped after hashing.
#[derive(Debug)]
pub struct Observed {
    /// The configuration.
    pub config: Config,
    /// The run's digests.
    pub digests: Digests,
    /// The final snapshot.
    pub snapshot: Vec<u8>,
    /// The tick of the last event.
    pub last_tick: Tick,
    /// `next_sequence` right before and right after registering observe points.
    pub sequence_around_points: (u64, u64),
    /// Observe points left unfired at the end.
    pub points_left: usize,
    /// `step` calls that dispatched an event.
    pub steps: u64,
    /// Pauses the breakpoint requested.
    pub pauses: u64,
    /// Times the driver resumed a paused `run`.
    pub resumes: u64,
    /// Observe points fired, and fields inspected at them.
    pub observes: (u64, u64),
    /// BLAKE3 of the JSONL and Perfetto exports.
    pub exports: Option<([u8; 32], [u8; 32])>,
}

/// Runs the full reference for `seed` under `config`. O4 places observe points from tick
/// 0 through `last_tick`, the reference run's last event.
///
/// # Panics
///
/// If the run faults, which the reference never does.
pub fn observe(seed: u64, config: Config, last_tick: Tick) -> Observed {
    let counts = Rc::new(Counts::default());
    let mut rt = build(ReferenceConfig::full(seed));
    if config.records() {
        rt.start_trace().expect("tracing starts before init");
    }
    if config.breaks() {
        rt.add_observer(Box::new(Breakpoint(Rc::clone(&counts))));
    }
    if config.probes() {
        rt.add_observer(Box::new(Probe(Rc::clone(&counts))));
    }
    rt.init().expect("the reference initializes");
    let before = rt.next_sequence();
    if config.probes() {
        for tick in (0..=last_tick.0).step_by(PROBE_INTERVAL as usize) {
            rt.observe_at(Tick(tick));
        }
    }
    let after = rt.next_sequence();

    let end = SimulationClock::default()
        .after(Tick::ZERO, T_END)
        .expect("T_END fits");
    let (mut events, mut steps, mut resumes) = (0, 0, 0);
    if config.steps() {
        while rt.step().expect("the reference runs").is_some() {
            steps += 1;
        }
        events = steps;
    } else {
        loop {
            let out = rt.run(end).expect("the reference runs");
            events += out.events;
            if out.stop != Stop::Paused {
                break;
            }
            resumes += 1;
        }
    }
    let points_left = rt.pending_observe_points();
    let finished = End::finish(&mut rt, events);
    let digests = finished.digests();
    let exports = finished.trace.as_ref().map(|trace| {
        let hash = |text: String| *blake3::hash(text.as_bytes()).as_bytes();
        (hash(to_jsonl(trace)), hash(to_perfetto(trace)))
    });
    Observed {
        config,
        digests,
        snapshot: finished.snapshot,
        last_tick: finished.last.map_or(Tick::ZERO, |k| k.tick),
        sequence_around_points: (before, after),
        points_left,
        steps,
        pauses: counts.pauses.get(),
        resumes,
        observes: (counts.observes.get(), counts.inspected.get()),
        exports,
    }
}

/// AT-3: `observed` matches the unobserved run `base` in everything the simulation owns.
pub fn ensure_invariant(base: &Observed, observed: &Observed) -> Result<(), String> {
    let name = format!("{:?}", observed.config);
    ensure_same(&base.digests.untraced(), &observed.digests.untraced())
        .map_err(|m| format!("{name} {m} from O0"))?;
    let (before, after) = observed.sequence_around_points;
    if before != after {
        return Err(format!(
            "{name}: registering observe points moved next_sequence {before} -> {after}"
        ));
    }
    if observed.points_left != 0 {
        return Err(format!(
            "{name}: {} observe points never fired",
            observed.points_left
        ));
    }
    let base_layout = Layout::parse(&base.snapshot).map_err(|e| e.to_string())?;
    let layout = Layout::parse(&observed.snapshot).map_err(|e| e.to_string())?;
    if layout.rng_states != base_layout.rng_states {
        return Err(format!("{name}: RNG states differ from O0"));
    }
    if layout.next_sequence_value != base_layout.next_sequence_value {
        return Err(format!("{name}: next_sequence differs from O0"));
    }
    for (id, (a, b)) in base_layout
        .components
        .iter()
        .zip(&layout.components)
        .enumerate()
    {
        if base.snapshot[a.state.clone()] != observed.snapshot[b.state.clone()] {
            return Err(format!(
                "{name}: component {id}'s final state differs from O0"
            ));
        }
    }
    if observed.snapshot != base.snapshot {
        return Err(format!("{name}: final snapshot differs from O0"));
    }
    Ok(())
}

/// AT-3: O1 and O5 record the same trace and export the same files.
pub fn ensure_same_trace(o1: &Observed, o5: &Observed) -> Result<(), String> {
    if o1.digests.trace.is_none() || o1.digests.trace != o5.digests.trace {
        return Err("O1 and O5 TraceDigests differ".to_owned());
    }
    if o1.exports.is_none() || o1.exports != o5.exports {
        return Err("O1 and O5 exports differ".to_owned());
    }
    Ok(())
}
