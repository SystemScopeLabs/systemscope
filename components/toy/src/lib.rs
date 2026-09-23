//! Toy components used by the M0 reference scenario (`docs/m0-design.md` §9.1).
//!
//! They depend only on `systemscope-contracts`: randomness comes from `ctx.rng()` and time
//! only from `ScheduleWhen`, never from ticks.

pub mod bus;
pub mod cpu;
pub mod dma;
pub mod memory;

pub use bus::{ToyBus, ToyBusConfig};
pub use cpu::{ToyCpu, ToyCpuConfig};
pub use dma::{ToyDma, ToyDmaConfig};
pub use memory::{ToyMemory, ToyMemoryConfig};
