// WorldView has no way to lend out a component mutably, even through `&mut WorldView`.
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::WorldView;

fn meddle(world: &mut WorldView<'_>) {
    let _ = world.get_mut(ComponentId(0));
}

fn main() {}
