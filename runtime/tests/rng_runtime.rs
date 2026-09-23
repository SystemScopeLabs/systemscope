//! Runtime-owned randomness: determinism and per-component independence (§5.2).

use std::cell::RefCell;
use std::rc::Rc;

use systemscope_contracts::component::{Component, Delivered, InitContext, PortSpec, SimContext};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::time::{Duration, SimulationClock};
use systemscope_runtime::runtime::SessionConfig;
use systemscope_runtime::topology::TopologyBuilder;

type Draws = Rc<RefCell<Vec<(String, u64)>>>;

/// Draws `per_event` values at init and at each of `wakes` wake-ups.
struct Drawer {
    name: &'static str,
    per_event: usize,
    wakes: u64,
    draws: Draws,
}

impl Drawer {
    fn draw(&self, rng: &mut dyn systemscope_contracts::rng::SimRng) {
        for _ in 0..self.per_event {
            let v = rng.next_u64();
            self.draws.borrow_mut().push((self.name.to_owned(), v));
        }
    }
}

impl Component for Drawer {
    fn type_name(&self) -> &'static str {
        "test.drawer"
    }
    fn ports(&self) -> Vec<PortSpec> {
        Vec::new()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        self.draw(ctx.rng());
        ctx.wake_self(ScheduleWhen::After(Duration::from_ns(1)), Phase::Request, 1)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.draw(ctx.rng());
        let Delivered::Wake { token } = *ev else {
            return Err(SimError::ComponentFault("unexpected delivery"));
        };
        if token < self.wakes {
            let after = ScheduleWhen::After(Duration::from_ns(1));
            ctx.wake_self(after, Phase::Request, token + 1)?;
        }
        Ok(())
    }
}

/// Runs components `(path, draws per event)` and returns each one's draws in order.
fn run(seed: u64, components: &[(&'static str, usize)]) -> Vec<(String, u64)> {
    let draws = Draws::default();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    for &(name, per_event) in components {
        let drawer = Drawer {
            name,
            per_event,
            wakes: 5,
            draws: draws.clone(),
        };
        t.add_component(name, Box::new(drawer));
    }
    let config = SessionConfig {
        seed,
        ..SessionConfig::default()
    };
    let mut rt = t.elaborate(config).unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    draws.borrow().clone()
}

fn only(draws: &[(String, u64)], name: &str) -> Vec<u64> {
    draws
        .iter()
        .filter(|(n, _)| n == name)
        .map(|&(_, v)| v)
        .collect()
}

#[test]
fn same_seed_same_draws_across_sessions() {
    let parts = [("soc.cpu0", 2), ("soc.dma", 3)];
    assert_eq!(run(9, &parts), run(9, &parts));
    assert_ne!(run(9, &parts), run(10, &parts));
}

#[test]
fn a_component_stream_ignores_other_components() {
    let alone = run(9, &[("soc.cpu0", 2)]);
    // A noisy neighbor, declared first so it also shifts ComponentIds, draws far more.
    let crowded = run(9, &[("soc.noisy", 50), ("soc.cpu0", 2), ("soc.late", 7)]);
    assert_eq!(only(&alone, "soc.cpu0"), only(&crowded, "soc.cpu0"));
    assert_eq!(only(&alone, "soc.cpu0").len(), 2 * 6);
}

#[test]
fn streams_are_keyed_by_path_not_id() {
    let a = run(3, &[("x", 4), ("y", 4)]);
    let b = run(3, &[("y", 4), ("x", 4)]);
    assert_eq!(only(&a, "x"), only(&b, "x"));
    assert_eq!(only(&a, "y"), only(&b, "y"));
    assert_ne!(only(&a, "x"), only(&a, "y"));
}
