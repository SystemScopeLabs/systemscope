//! Toy components used by the M0 reference scenario (`docs/m0-design.md` §9.1).
//!
//! They depend only on `systemscope-contracts`: randomness comes from `ctx.rng()` and time
//! only from `ScheduleWhen`, never from ticks.

pub mod cpu;
pub mod memory;

pub use cpu::{ToyCpu, ToyCpuConfig};
pub use memory::{ToyMemory, ToyMemoryConfig};
