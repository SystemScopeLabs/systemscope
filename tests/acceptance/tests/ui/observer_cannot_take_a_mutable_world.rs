// Observer callbacks receive the world by shared reference only.
use systemscope_contracts::observe::{Control, Observer, WorldView};
use systemscope_contracts::time::Tick;

struct Meddler;

impl Observer for Meddler {
    fn on_observe(&mut self, _: Tick, _: &mut WorldView<'_>) -> Control {
        Control::Continue
    }
}

fn main() {}
