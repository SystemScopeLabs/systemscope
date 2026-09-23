// inspect takes &self, so it cannot change the component it describes.
use systemscope_contracts::component::{Component, Delivered, InitContext, PortSpec, SimContext};
use systemscope_contracts::error::SimError;
use systemscope_contracts::observe::StateView;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};

struct Counter(u64);

impl Component for Counter {
    fn type_name(&self) -> &'static str {
        "counter"
    }
    fn ports(&self) -> Vec<PortSpec> {
        Vec::new()
    }
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }
    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Ok(())
    }
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
    fn inspect(&self) -> StateView {
        self.0 += 1;
        StateView::default()
    }
}

fn main() {}
