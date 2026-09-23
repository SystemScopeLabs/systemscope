//! Observers, observe points, pause, and single-step (`docs/m0-design.md` §8.2).
//!
//! A `Ticker` has two events per tick, at 1000, 2000, … ps: a `Request` wake, then a
//! `Commit` wake at the same tick. That is enough to see where observe points fire
//! relative to a tick's events, and to compare observed and unobserved runs exactly.

use std::cell::RefCell;
use std::rc::Rc;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortSpec, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Duration, SimulationClock, Tick};
use systemscope_contracts::trace::{TraceRecord, Value};
use systemscope_runtime::runtime::{RunOutcome, Runtime, SessionConfig, Stop};
use systemscope_runtime::topology::TopologyBuilder;

const TICKS: u64 = 3;
const REQUEST: u64 = 0;
const COMMIT: u64 = 1;

/// Handles `2 × TICKS` events, drawing from its RNG on each so RNG state is covered.
struct Ticker {
    handled: u64,
    drawn: u64,
}

impl Component for Ticker {
    fn type_name(&self) -> &'static str {
        "test.ticker"
    }
    fn ports(&self) -> Vec<PortSpec> {
        Vec::new()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        ctx.trace("ticker.init", Vec::new());
        ctx.wake_self(
            ScheduleWhen::After(Duration::from_ps(1000)),
            Phase::Request,
            REQUEST,
        )
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.handled += 1;
        self.drawn ^= ctx.rng().next_u64();
        ctx.trace("ticker.handled", vec![("n", Value::U64(self.handled))]);
        match ev {
            Delivered::Wake { token: REQUEST } => {
                ctx.wake_self(ScheduleWhen::Now, Phase::Commit, COMMIT)
            }
            Delivered::Wake { token: COMMIT } if self.handled < 2 * TICKS => ctx.wake_self(
                ScheduleWhen::After(Duration::from_ps(1000)),
                Phase::Request,
                REQUEST,
            ),
            _ => Ok(()),
        }
    }
    fn snapshot_schema_version(&self) -> u32 {
        1
    }
    fn snapshot(&self, w: &mut SnapshotWriter) {
        w.u64(self.handled);
        w.u64(self.drawn);
    }
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        self.handled = r.u64()?;
        self.drawn = r.u64()?;
        Ok(())
    }
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![("handled", Value::U64(self.handled))],
        }
    }
}

fn build() -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "ticker",
        Box::new(Ticker {
            handled: 0,
            drawn: 0,
        }),
    );
    t.elaborate(SessionConfig {
        seed: 3,
        ..SessionConfig::default()
    })
    .unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    Event(u64, Phase),
    Observe(u64),
    Trace(&'static str),
}

/// Logs what it sees; pauses where told to.
#[derive(Default)]
struct Watcher {
    log: Rc<RefCell<Vec<Seen>>>,
    pause_on_commit: bool,
    pause_on_observe: bool,
    traces: bool,
}

impl Observer for Watcher {
    fn on_after_dispatch(&mut self, ev: &EventView<'_>, world: &WorldView<'_>) -> Control {
        // The handler has run: the target already counts this event.
        let seen = self
            .log
            .borrow()
            .iter()
            .filter(|s| matches!(s, Seen::Event(..)))
            .count();
        assert_eq!(
            world.inspect(ev.target).unwrap().get("handled"),
            Some(&Value::U64(seen as u64 + 1))
        );
        assert_eq!(world.now(), ev.key.tick);
        self.log
            .borrow_mut()
            .push(Seen::Event(ev.key.tick.0, ev.key.phase));
        if self.pause_on_commit && ev.key.phase == Phase::Commit {
            Control::Pause
        } else {
            Control::Continue
        }
    }
    fn on_observe(&mut self, now: Tick, world: &WorldView<'_>) -> Control {
        assert_eq!(world.now(), now);
        assert_eq!(world.type_name(ComponentId(0)), Some("test.ticker"));
        self.log.borrow_mut().push(Seen::Observe(now.0));
        if self.pause_on_observe {
            Control::Pause
        } else {
            Control::Continue
        }
    }
    fn on_trace(&mut self, rec: &TraceRecord) {
        if self.traces && rec.kind.starts_with("ticker.") {
            self.log.borrow_mut().push(Seen::Trace(rec.kind));
        }
    }
}

fn watched(watcher: Watcher) -> (Runtime, Rc<RefCell<Vec<Seen>>>) {
    let log = Rc::clone(&watcher.log);
    let mut rt = build();
    rt.add_observer(Box::new(watcher));
    (rt, log)
}

/// Snapshot bytes and digests of a finished run: everything the simulation is.
fn result(rt: &Runtime) -> (Vec<u8>, [u8; 32], u64) {
    (
        rt.snapshot().unwrap(),
        rt.execution_digest(),
        rt.next_sequence(),
    )
}

fn plain() -> (Vec<u8>, [u8; 32], u64) {
    let mut rt = build();
    rt.init().unwrap();
    rt.run_until(Tick(u64::MAX)).unwrap();
    result(&rt)
}

fn events(log: &[Seen]) -> usize {
    log.iter().filter(|s| matches!(s, Seen::Event(..))).count()
}

use Phase::{Commit, Request};
use Seen::{Event, Observe};

#[test]
fn points_fire_in_tick_order_after_every_event_of_their_tick() {
    let (mut rt, log) = watched(Watcher::default());
    rt.init().unwrap();
    let sequence = rt.next_sequence();
    for t in [2000, 1500, 2000, 0, 99_999] {
        rt.observe_at(Tick(t));
    }
    // Registration consumes no sequence number, and a repeated tick is one point.
    assert_eq!(rt.next_sequence(), sequence);
    assert_eq!(rt.pending_observe_points(), 4);
    let outcome = rt.run(Tick(u64::MAX)).unwrap();
    assert_eq!(
        outcome,
        RunOutcome {
            events: 2 * TICKS,
            stop: Stop::Drained
        }
    );
    assert_eq!(
        *log.borrow(),
        [
            Observe(0),
            Event(1000, Request),
            Event(1000, Commit),
            Observe(1500),
            Event(2000, Request),
            Event(2000, Commit),
            Observe(2000),
            Event(3000, Request),
            Event(3000, Commit),
            // The queue is empty and the run's horizon covers it.
            Observe(99_999),
        ]
    );
    assert_eq!(rt.pending_observe_points(), 0);
    assert_eq!(result(&rt), plain());
}

#[test]
fn a_point_past_the_last_event_waits_for_a_run_that_reaches_it() {
    let (mut rt, log) = watched(Watcher::default());
    rt.init().unwrap();
    rt.observe_at(Tick(3000));
    rt.observe_at(Tick(5000));
    while rt.step().unwrap().is_some() {}
    // Tick 3000 is finished once its last event ran; 5000 has not been reached.
    assert_eq!(log.borrow().last(), Some(&Observe(3000)));
    assert_eq!(rt.pending_observe_points(), 1);
    assert_eq!(rt.run(Tick(4999)).unwrap().stop, Stop::Drained);
    assert_eq!(rt.pending_observe_points(), 1);
    rt.run(Tick(5000)).unwrap();
    assert_eq!(log.borrow().last(), Some(&Observe(5000)));
    assert_eq!(result(&rt), plain());
}

#[test]
fn a_dispatch_pause_returns_right_after_that_event_and_resumes_exactly() {
    let (mut rt, log) = watched(Watcher {
        pause_on_commit: true,
        ..Watcher::default()
    });
    rt.init().unwrap();
    let mut pauses = 0;
    loop {
        let outcome = rt.run(Tick(u64::MAX)).unwrap();
        if outcome.stop != Stop::Paused {
            assert_eq!(outcome.events, 0);
            break;
        }
        pauses += 1;
        // Stopped on the Commit event the observer saw last, and nothing after it ran.
        let last = rt.last_dispatched().unwrap();
        assert_eq!(last.phase, Commit);
        assert_eq!(log.borrow().last(), Some(&Event(last.tick.0, Commit)));
        assert_eq!(outcome.events, 2);
    }
    assert_eq!(pauses, TICKS);
    assert_eq!(events(&log.borrow()), 2 * TICKS as usize);
    assert_eq!(result(&rt), plain());
}

#[test]
fn an_observe_pause_returns_before_the_next_event() {
    let (mut rt, log) = watched(Watcher {
        pause_on_observe: true,
        ..Watcher::default()
    });
    rt.init().unwrap();
    rt.observe_at(Tick(2000));
    rt.observe_at(Tick(2500));
    let outcome = rt.run(Tick(u64::MAX)).unwrap();
    assert_eq!(outcome.stop, Stop::Paused);
    assert_eq!(rt.last_dispatched().unwrap().tick, Tick(2000));
    assert_eq!(rt.last_dispatched().unwrap().phase, Commit);
    assert_eq!(rt.peek_key().unwrap().tick, Tick(3000));
    // The second due point fires at the start of the next call, before any event.
    let outcome = rt.run(Tick(u64::MAX)).unwrap();
    assert_eq!((outcome.events, outcome.stop), (0, Stop::Paused));
    assert_eq!(log.borrow().last(), Some(&Observe(2500)));
    assert_eq!(rt.run_until(Tick(u64::MAX)).unwrap(), 2);
    assert_eq!(result(&rt), plain());
}

#[test]
fn step_dispatches_exactly_one_event_and_ignores_pauses() {
    let (mut rt, log) = watched(Watcher {
        pause_on_commit: true,
        pause_on_observe: true,
        ..Watcher::default()
    });
    rt.init().unwrap();
    rt.observe_at(Tick(1000));
    let mut steps = 0;
    while let Some(d) = rt.step().unwrap() {
        steps += 1;
        assert_eq!(rt.last_dispatched(), Some(d.key));
        assert_eq!(events(&log.borrow()), steps);
    }
    assert_eq!(steps, 2 * TICKS as usize);
    assert!(log.borrow().contains(&Observe(1000)));
    assert_eq!(result(&rt), plain());
}

#[test]
fn observers_see_every_record_with_or_without_a_recorder() {
    let records = |recorder: bool| {
        let (mut rt, log) = watched(Watcher {
            traces: true,
            ..Watcher::default()
        });
        if recorder {
            rt.start_trace().unwrap();
        }
        rt.init().unwrap();
        rt.run_until(Tick(u64::MAX)).unwrap();
        let recorded = rt.take_trace();
        let seen = log.borrow().clone();
        (seen, recorded, result(&rt))
    };
    let (with, trace, with_result) = records(true);
    let (without, none, without_result) = records(false);
    assert!(none.is_none());
    assert_eq!(with, without);
    assert_eq!(with_result, without_result);
    // The observer saw exactly the component records the recorder kept, in order, each
    // after its event's dispatch.
    let recorded: Vec<Seen> = trace
        .unwrap()
        .records
        .iter()
        .filter(|r| r.kind.starts_with("ticker."))
        .map(|r| Seen::Trace(r.kind))
        .collect();
    let traces: Vec<Seen> = with
        .iter()
        .filter(|s| matches!(s, Seen::Trace(_)))
        .cloned()
        .collect();
    assert_eq!(traces, recorded);
    assert_eq!(with[0], Seen::Trace("ticker.init"));
    assert_eq!(
        &with[1..3],
        [Seen::Trace("ticker.handled"), Event(1000, Request)]
    );
}

#[test]
fn observed_runs_snapshot_and_digest_like_unobserved_ones() {
    let (mut rt, _) = watched(Watcher {
        pause_on_commit: true,
        pause_on_observe: true,
        traces: true,
        ..Watcher::default()
    });
    rt.init().unwrap();
    for t in (0..=4000).step_by(250) {
        rt.observe_at(Tick(t));
    }
    let mut mid = None;
    let mut runs = 0;
    while rt.run(Tick(u64::MAX)).unwrap().stop == Stop::Paused {
        runs += 1;
        if runs == 3 {
            mid = Some((rt.snapshot().unwrap(), rt.last_dispatched()));
        }
    }
    assert!(runs > TICKS);
    assert_eq!(result(&rt), plain());

    // A snapshot taken while paused is the unobserved run's snapshot at that boundary.
    let (bytes, at) = mid.unwrap();
    let mut unobserved = build();
    unobserved.init().unwrap();
    while unobserved.last_dispatched() != at {
        unobserved.step().unwrap();
    }
    assert_eq!(unobserved.snapshot().unwrap(), bytes);
}
