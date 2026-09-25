//! Platform components for SystemScope M1 (`docs/m1-design.md` §7).
//!
//! [`AddressBus`] is the only component that knows the memory map: it routes each
//! `mem.v1` request to the one region that contains it, as a region-relative offset, and
//! answers every other request with an access fault. [`Ram`] is a sparse, canonical
//! memory that serves those offsets, and [`SimpleUart`] is a transmit-only output device
//! with a TX and a STATUS register. None of them knows about the CPU.
//!
//! M2 adds [`SimpleIrqController`] (`docs/m2-design.md` §7.2), a level-sensitive
//! aggregator of `irq.v0` lines with a `PENDING` and an `ENABLE` register, and
//! [`MultiMasterBus`] (`docs/m2-design.md` §10), which arbitrates several initiators onto
//! the same kind of memory map. `AddressBus` stays the M1 interconnect, unchanged.
//!
//! Like the M0 toy components, they depend only on `systemscope-contracts`.

pub mod bus;
pub mod irqc;
pub mod mmbus;
pub mod ram;
pub mod uart;

pub use bus::{AddressBus, BusConfigError, Region};
pub use irqc::{IrqControllerConfig, IrqControllerConfigError, SimpleIrqController};
pub use mmbus::{MultiMasterBus, MultiMasterBusConfig, MultiMasterBusConfigError};
pub use ram::{Ram, RamConfig, RamConfigError, RamImage, Segment};
pub use uart::{SimpleUart, UartConfig};
