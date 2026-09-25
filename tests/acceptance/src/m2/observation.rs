//! The M0 observation configurations O0 to O5 on `m2-reference` (`docs/m2-design.md`
//! §13.4, `docs/m0-design.md` §9 AT-3), as M1-A7 applies them to `m1-reference`.
//!
//! The configurations and the invariance checks are M0's own ([`Config`],
//! [`ensure_invariant`](crate::observation::ensure_invariant),
//! [`ensure_same_trace`](crate::observation::ensure_same_trace)). As in M1-A7, the
//! breakpoint in O2 pauses on every CPU `Commit` event, and the probe in O4 inspects
//! every component, here all seven of `m2-reference`.

use std::cell::Cell;
use std::rc::Rc;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::event::Phase;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::time::Tick;
use systemscope_runtime::export::{to_jsonl, to_perfetto};
use systemscope_runtime::runtime::Stop;
use systemscope_rv32::m2ref::CPU;

use super::Fixture;
use crate::digests::End;
use crate::observation::{Config, Observed, PROBE_INTERVAL};

/// Driver-side counts, shared with the observers.
#[derive(Default)]
struct Counts {
    pauses: Cell<u64>,
    observes: Cell<u64>,
    inspected: Cell<u64>,
}

/// Pauses on every event the CPU handles in `Commit`.
struct Breakpoint(Rc<Counts>);

impl Observer for Breakpoint {
    fn on_after_dispatch(&mut self, ev: &EventView<'_>, _: &WorldView<'_>) -> Control {
        if ev.target == CPU && ev.key.phase == Phase::Commit {
            self.0.pauses.set(self.0.pauses.get() + 1);
            Control::Pause
        } else {
            Control::Continue
        }
    }
}

struct Probe(Rc<Counts>);

impl Observer for Probe {
    fn on_observe(&mut self, _: Tick, world: &WorldView<'_>) -> Control {
        self.0.observes.set(self.0.observes.get() + 1);
        for id in 0..world.component_count() {
            let id = ComponentId(u32::try_from(id).expect("seven components"));
            let fields = world.inspect(id).map_or(0, |v| v.fields.len());
            self.0.inspected.set(self.0.inspected.get() + fields as u64);
        }
        Control::Continue
    }
}

/// Runs the program under `config` to its end. O4 places observe points from tick 0
/// through `last_tick`, the unobserved run's last event.
///
/// Callers first run the program under the watchdog ([`Fixture::run`]); only a program
/// that ends there is observed here.
///
/// # Panics
///
/// If the run faults, which the committed program does not.
pub fn observe(fixture: &Fixture, config: Config, last_tick: Tick) -> Observed {
    let counts = Rc::new(Counts::default());
    let records = matches!(config, Config::O1 | Config::O5);
    let breaks = matches!(config, Config::O2 | Config::O5);
    let steps = matches!(config, Config::O3 | Config::O5);
    let probes = matches!(config, Config::O4 | Config::O5);
    let mut rt = fixture.platform();
    if records {
        rt.start_trace().expect("tracing starts before init");
    }
    if breaks {
        rt.add_observer(Box::new(Breakpoint(Rc::clone(&counts))));
    }
    if probes {
        rt.add_observer(Box::new(Probe(Rc::clone(&counts))));
    }
    rt.init().expect("m2-reference initializes");
    let before = rt.next_sequence();
    if probes {
        for tick in (0..=last_tick.0).step_by(PROBE_INTERVAL as usize) {
            rt.observe_at(Tick(tick));
        }
    }
    let after = rt.next_sequence();

    let (mut events, mut stepped, mut resumes) = (0, 0, 0);
    if steps {
        while rt.step().expect("the program runs").is_some() {
            stepped += 1;
        }
        events = stepped;
    } else {
        loop {
            let out = rt.run(Tick(u64::MAX)).expect("the program runs");
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
        steps: stepped,
        pauses: counts.pauses.get(),
        resumes,
        observes: (counts.observes.get(), counts.inspected.get()),
        exports,
    }
}
