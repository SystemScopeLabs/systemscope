//! The kernel's configuration and its access whitelist (`docs/m3-design.md` §6.2, §6.3).
//!
//! The configuration is fixed at construction and is part of the snapshot. It names the
//! physical layout of §11.2 the kernel may touch, and the `kgate` window as the bus maps
//! it, which the kernel may never touch.
//!
//! # Whitelist
//!
//! Before sending, the kernel checks every access against the ranges its configuration
//! grants: the trap frame ([`FRAME_BYTES`] at `trap_frame`), staging, the frame pool, the
//! block controller window, and the one UART TX byte. An access is permitted only if all
//! its bytes lie inside one granted range and none lies in the `kgate` window: `kgate` is
//! never granted, whatever the other ranges say, so the kernel cannot address its own held
//! gate and deadlock (§16 risk 2). Checking that `kgate` lies outside every granted range
//! is a platform builder invariant; this check stands behind it at every send.

use std::fmt;

use systemscope_contracts::snapshot::SnapshotWriter;
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::topology::LinkLatency;

/// The size of the trap frame (§7.2): `x1`–`x31`, `sepc`, `sstatus`, `scause`, `stval`,
/// `satp`, `action`, and `reason`, 38 little-endian words.
pub const FRAME_BYTES: u64 = 0x98;
/// The offset of `sepc` in the trap frame.
pub const FRAME_SEPC: u64 = 0x7C;
/// The offset of `action` in the trap frame: 0 `Resume`, 1 `Shutdown`.
pub const FRAME_ACTION: u64 = 0x90;
/// The offset of `reason` in the trap frame, for `Shutdown`.
pub const FRAME_REASON: u64 = 0x94;

/// A physical address range, half-open: `base..base + size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    /// The first address.
    pub base: u64,
    /// The size in bytes.
    pub size: u64,
}

impl Window {
    /// Whether every byte of `addr..addr + len` lies inside the window; `false` for an
    /// empty or overflowing range.
    pub fn contains(&self, addr: u64, len: u64) -> bool {
        len != 0
            && addr >= self.base
            && addr
                .checked_add(len)
                .is_some_and(|end| end - self.base <= self.size)
    }

    /// Whether the window and `addr..addr + len` share a byte.
    pub fn overlaps(&self, addr: u64, len: u64) -> bool {
        let end = addr.saturating_add(len);
        let own_end = self.base.saturating_add(self.size);
        len != 0 && self.size != 0 && addr < own_end && self.base < end
    }
}

/// The configuration of a [`ModeledKernel`](crate::ModeledKernel).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KernelConfig {
    /// The kernel's clock: its accesses are sent in `Request` of its cycles.
    pub clock: ClockDomainId,
    /// Delay from accepting a `gate` request the kernel refuses to its fault response.
    /// The held `ENTER` is not answered after it but when its operation ends.
    pub latency: LinkLatency,
    /// The RAM region.
    pub ram: Window,
    /// The `kgate` window as the bus maps it. Never granted.
    pub gate: Window,
    /// The trap frame's physical address, 4-byte aligned. It is also the only value an
    /// `ENTER` write may carry.
    pub trap_frame: u32,
    /// The staging area, the DMA target for executables.
    pub staging: Window,
    /// The frame pool.
    pub frame_pool: Window,
    /// The block controller's register window.
    pub blk: Window,
    /// The UART's TX register.
    pub uart_tx: u64,
}

/// Why a configuration cannot form a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelConfigError {
    /// The named range is empty.
    Empty(&'static str),
    /// The named range runs past `u64::MAX`.
    Wraps(&'static str),
    /// The named range is not wholly inside the RAM.
    OutsideRam(&'static str),
    /// Two of the trap frame, staging, and the frame pool share a byte.
    Overlap(&'static str, &'static str),
    /// The trap frame is not 4-byte aligned.
    MisalignedFrame(u32),
}

impl fmt::Display for KernelConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelConfigError::Empty(r) => write!(f, "{r} is empty"),
            KernelConfigError::Wraps(r) => write!(f, "{r} runs past the end of u64"),
            KernelConfigError::OutsideRam(r) => write!(f, "{r} is not wholly inside the RAM"),
            KernelConfigError::Overlap(a, b) => write!(f, "{a} and {b} overlap"),
            KernelConfigError::MisalignedFrame(a) => {
                write!(f, "the trap frame {a:#x} is not 4-byte aligned")
            }
        }
    }
}

impl std::error::Error for KernelConfigError {}

impl KernelConfig {
    /// The trap frame as a range.
    pub fn frame(&self) -> Window {
        Window {
            base: u64::from(self.trap_frame),
            size: FRAME_BYTES,
        }
    }

    /// The granted ranges, in a fixed order: the trap frame, staging, the frame pool, the
    /// block controller, and UART TX.
    pub fn grants(&self) -> [Window; 5] {
        [
            self.frame(),
            self.staging,
            self.frame_pool,
            self.blk,
            Window {
                base: self.uart_tx,
                size: 1,
            },
        ]
    }

    /// The whitelist: whether the kernel may send an access to `addr..addr + len`.
    pub fn permits(&self, addr: u64, len: u64) -> bool {
        !self.gate.overlaps(addr, len) && self.grants().iter().any(|g| g.contains(addr, len))
    }

    /// Checks the rules of §6.2 that belong to the kernel: every range non-empty and
    /// inside `u64`; the trap frame 4-byte aligned; the trap frame, staging, and the frame
    /// pool inside the RAM and pairwise disjoint. Whether `kgate` lies outside the granted
    /// ranges is the platform builder's check.
    pub fn validate(&self) -> Result<(), KernelConfigError> {
        let named = [
            ("the RAM", self.ram),
            ("kgate", self.gate),
            ("the trap frame", self.frame()),
            ("staging", self.staging),
            ("the frame pool", self.frame_pool),
            ("the block controller", self.blk),
        ];
        for (name, w) in named {
            if w.size == 0 {
                return Err(KernelConfigError::Empty(name));
            }
            if w.base.checked_add(w.size).is_none() {
                return Err(KernelConfigError::Wraps(name));
            }
        }
        if !self.trap_frame.is_multiple_of(4) {
            return Err(KernelConfigError::MisalignedFrame(self.trap_frame));
        }
        let in_ram = [
            ("the trap frame", self.frame()),
            ("staging", self.staging),
            ("the frame pool", self.frame_pool),
        ];
        for (i, (name, w)) in in_ram.iter().enumerate() {
            if !self.ram.contains(w.base, w.size) {
                return Err(KernelConfigError::OutsideRam(name));
            }
            for (other, o) in &in_ram[..i] {
                if o.overlaps(w.base, w.size) {
                    return Err(KernelConfigError::Overlap(other, name));
                }
            }
        }
        Ok(())
    }

    /// The canonical encoding, as the snapshot stores it.
    pub(crate) fn encode(&self, w: &mut SnapshotWriter) {
        w.u32(self.clock.0);
        match self.latency {
            LinkLatency::After(d) => {
                w.u8(0);
                w.u128(d.as_femtoseconds());
            }
            LinkLatency::Cycles { domain, k } => {
                w.u8(1);
                w.u32(domain.0);
                w.u64(k);
            }
        }
        for window in [self.ram, self.gate] {
            w.u64(window.base);
            w.u64(window.size);
        }
        w.u32(self.trap_frame);
        for window in [self.staging, self.frame_pool, self.blk] {
            w.u64(window.base);
            w.u64(window.size);
        }
        w.u64(self.uart_tx);
    }
}
