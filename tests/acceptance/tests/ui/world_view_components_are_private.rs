// An observer cannot reach the component list behind a WorldView.
use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};

struct Meddler;

impl Observer for Meddler {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let _ = (&world.components, ComponentId(0), Delivered::Wake { token: 0 });
        Control::Continue
    }
}

fn main() {}
