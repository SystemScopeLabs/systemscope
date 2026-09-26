//! The SystemScope M3 modeled OS backend (`docs/m3-design.md` §6), at the M3.4a kernel
//! gate prototype.
//!
//! [`ModeledKernel`] is an architectural state machine behind two `mem.v1` ports: `gate`,
//! the `kgate` MMIO window whose `ENTER` store it holds until an operation finishes, and
//! `mem`, a bus master through which it reaches memory. It never sees the CPU: a guest
//! enters it with an ordinary store that the bus routes to `kgate`, and everything it does
//! reaches the hart through memory and the store's delayed completion.
//!
//! - [`config`] is the configuration and the access whitelist, which never grants
//!   `kgate`.
//! - [`core`] is the pure core: what an operation sends next and how a completion
//!   advances it. M3.4a has only prototype operations: the scripted gate operation and
//!   the shutdown for a bad `ENTER` value. Processes, the frame allocator, loading, and
//!   syscalls are later steps (§17).
//! - [`kernel`] is the component: the held entry, the Issue/Wait engine, its snapshot
//!   (schema 1), inspect, and trace.
//!
//! Like the platform components, it depends only on `systemscope-contracts`.

pub mod config;
pub mod core;
pub mod frames;
pub mod image;
pub mod kernel;
pub mod process;
pub mod procop;
pub mod pte;
pub mod space;

pub use config::{KernelConfig, KernelConfigError, Window};
pub use image::{BootImage, PlanError, ProcessPlan, UserLayout};
pub use kernel::ModeledKernel;
