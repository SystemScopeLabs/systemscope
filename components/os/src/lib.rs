//! The SystemScope M3 modeled OS backend (`docs/m3-design.md` §6).
//!
//! [`ModeledKernel`] is an architectural state machine behind two `mem.v1` ports: `gate`,
//! the `kgate` MMIO window whose `ENTER` store it holds until an operation finishes, and
//! `mem`, a bus master through which it reaches memory. It never sees the CPU: a guest
//! enters it with an ordinary store that the bus routes to `kgate`, and everything it does
//! reaches the hart through memory and the store's delayed completion.
//!
//! - [`config`] is the configuration and the access whitelist, which never grants
//!   `kgate`.
//! - [`core`] is the M3.4a prototype core: the scripted gate operation and the shutdown
//!   for a bad `ENTER` value.
//! - [`image`], [`frames`], [`space`], [`pte`], and [`process`] are the process model:
//!   validated boot images, the frame pool, address spaces, PTE encoding, and the PCBs
//!   and run queue.
//! - [`syscall`] is the syscall ABI (§6.5) and the kernel's user-copy walk.
//! - [`procop`] is the process-mode pure core: boot, traps, syscalls, dispatch, and
//!   shutdown as chains of bus accesses.
//! - [`kernel`] is the component: the held entry, the Issue/Wait engine, its snapshot
//!   (schema 1), inspect, and trace.
//!
//! Like the platform components, it depends only on `systemscope-contracts`, plus the
//! pure `systemscope-elf` types of the validated images it is given.

pub mod config;
pub mod core;
pub mod frames;
pub mod image;
pub mod kernel;
pub mod process;
pub mod procop;
pub mod pte;
pub mod space;
pub mod syscall;

pub use config::{KernelConfig, KernelConfigError, Window};
pub use image::{BootImage, PlanError, ProcessPlan, UserLayout};
pub use kernel::ModeledKernel;
