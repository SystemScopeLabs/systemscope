//! Test harnesses shared by the kernel tests. Test-only: nothing here is part of the crate.
//!
//! - [`MockCtx`] drives the kernel directly, recording what it sends, wakes, and traces,
//!   so protocol tests can deliver messages no real bus would produce.
//! - [`layout`] is the `m3-reference` physical layout (`docs/m3-design.md` §11.2) as a
//!   kernel configuration.
//! - [`oracle`] is an independent model of the scripted gate operation (§17) over a
//!   byte-addressed memory, written from the design text and sharing no code with the
//!   crate's `core`.

#![allow(dead_code)]

pub mod platform;

use std::collections::BTreeMap;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::Tick;
use systemscope_contracts::trace::Value;

/// The `m3-reference` layout (§11.1, §11.2).
pub mod layout {
    use systemscope_contracts::time::ClockDomainId;
    use systemscope_contracts::topology::LinkLatency;
    use systemscope_os::{KernelConfig, Window};

    pub const RAM_BASE: u64 = 0x8000_0000;
    pub const RAM_SIZE: u64 = 16 << 20;
    pub const UART_BASE: u64 = 0x1000_0000;
    pub const IRQC_BASE: u64 = 0x1000_1000;
    pub const BLK_BASE: u64 = 0x1000_2000;
    pub const BLK_SIZE: u64 = 0x20;
    pub const KGATE_BASE: u64 = 0x1000_3000;
    pub const KGATE_SIZE: u64 = 0x8;
    pub const TRAP_FRAME: u32 = 0x8001_0000;
    pub const STAGING: u64 = 0x8010_0000;
    pub const STAGING_SIZE: u64 = 1 << 20;
    pub const POOL: u64 = 0x8040_0000;
    /// `0x8040_0000`–`0x80FF_FFFF`, 3072 frames.
    pub const POOL_SIZE: u64 = 0x00C0_0000;

    /// The layout with `clock` as the kernel clock and every response after 0 cycles.
    pub fn config(clock: ClockDomainId) -> KernelConfig {
        KernelConfig {
            clock,
            latency: LinkLatency::Cycles {
                domain: clock,
                k: 0,
            },
            ram: Window {
                base: RAM_BASE,
                size: RAM_SIZE,
            },
            gate: Window {
                base: KGATE_BASE,
                size: KGATE_SIZE,
            },
            trap_frame: TRAP_FRAME,
            staging: Window {
                base: STAGING,
                size: STAGING_SIZE,
            },
            frame_pool: Window {
                base: POOL,
                size: POOL_SIZE,
            },
            blk: Window {
                base: BLK_BASE,
                size: BLK_SIZE,
            },
            uart_tx: UART_BASE,
        }
    }
}

/// The scripted gate operation, from the design text (§7.2, §17), over a byte memory.
pub mod oracle {
    use std::collections::BTreeMap;

    /// One access: `(write, addr, bytes)`, with the length of a read as zeros.
    pub type Access = (bool, u64, Vec<u8>);

    /// The trap frame is 38 words; `sepc` is word 31.
    const WORDS: u64 = 38;
    const SEPC_WORD: u64 = 31;

    /// The pieces of `[base, base + len)`: a new piece starts at every 16th byte of the
    /// current piece and at every 4 KiB boundary.
    fn pieces(base: u64, len: u64) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for addr in base..base + len {
            match out.last_mut() {
                Some((_, n)) if *n < 16 && addr % 4096 != 0 => *n += 1,
                _ => out.push((addr, 1)),
            }
        }
        out
    }

    /// Runs the scripted operation on `mem` and returns its accesses in order.
    pub fn script(frame: u64, uart_tx: u64, mem: &mut BTreeMap<u64, u8>) -> Vec<Access> {
        let len = WORDS * 4;
        let read = |a: u64, m: &BTreeMap<u64, u8>| m.get(&a).copied().unwrap_or(0);
        let mut accesses = Vec::new();
        let mut copy = Vec::new();
        for (addr, n) in pieces(frame, len) {
            accesses.push((false, addr, vec![0; n as usize]));
            copy.extend((addr..addr + n).map(|a| read(a, mem)));
        }
        let at = (SEPC_WORD * 4) as usize;
        let sepc = u32::from(copy[at])
            | u32::from(copy[at + 1]) << 8
            | u32::from(copy[at + 2]) << 16
            | u32::from(copy[at + 3]) << 24;
        let sepc = sepc.wrapping_add(4);
        for k in 0..4 {
            copy[at + k] = (sepc >> (8 * k)) as u8;
        }
        for (addr, n) in pieces(frame, len) {
            let bytes: Vec<u8> = (addr..addr + n)
                .map(|a| copy[(a - frame) as usize])
                .collect();
            for (a, b) in (addr..).zip(&bytes) {
                mem.insert(a, *b);
            }
            accesses.push((true, addr, bytes));
        }
        accesses.push((true, uart_tx, vec![b'k']));
        accesses
    }

    /// The shutdown operation: `action = 1`, `reason = 1` at frame + 0x90.
    pub fn shutdown(frame: u64, mem: &mut BTreeMap<u64, u8>) -> Vec<Access> {
        let words = [1, 0, 0, 0, 1, 0, 0, 0];
        let mut out = Vec::new();
        for (addr, n) in pieces(frame + 0x90, 8) {
            let off = (addr - frame - 0x90) as usize;
            let bytes = words[off..off + n as usize].to_vec();
            for (a, b) in (addr..).zip(&bytes) {
                mem.insert(a, *b);
            }
            out.push((true, addr, bytes));
        }
        out
    }
}

/// The snapshot bytes of `component`.
pub fn snapshot_of(component: &dyn Component) -> Vec<u8> {
    let mut w = SnapshotWriter::new();
    component.snapshot(&mut w);
    w.into_bytes()
}

/// Restores `bytes` into `component`, requiring every byte to be read.
pub fn restore_into(component: &mut dyn Component, bytes: &[u8]) -> Result<(), RestoreError> {
    let mut r = SnapshotReader::new(bytes);
    let schema = component.snapshot_schema_version();
    component.restore(&mut r, schema)?;
    r.finish().map_err(RestoreError::Decode)
}

/// A byte memory with every unwritten byte 0.
pub type Mem = BTreeMap<u64, u8>;

/// One `send` the kernel made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub port: PortId,
    pub msg: MemMsg,
    pub when: ScheduleWhen,
    pub phase: Phase,
}

/// One `wake_self` the kernel made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Woke {
    pub when: ScheduleWhen,
    pub phase: Phase,
    pub token: u64,
}

/// One `trace` record the kernel made.
pub type Traced = (&'static str, Vec<(&'static str, Value)>);

/// The kernel uses no randomness.
struct NoRng;

impl SimRng for NoRng {
    fn next_u64(&mut self) -> u64 {
        panic!("the kernel must not draw random numbers")
    }
}

/// A `SimContext` that records instead of scheduling.
pub struct MockCtx {
    pub phase: Phase,
    pub sent: Vec<Sent>,
    pub woke: Vec<Woke>,
    pub traced: Vec<Traced>,
    rng: NoRng,
}

impl Default for MockCtx {
    fn default() -> MockCtx {
        MockCtx::new()
    }
}

impl MockCtx {
    pub fn new() -> MockCtx {
        MockCtx {
            phase: Phase::Request,
            sent: Vec::new(),
            woke: Vec::new(),
            traced: Vec::new(),
            rng: NoRng,
        }
    }

    /// Delivers `msg` on `port` of `component`, in `phase`.
    pub fn deliver(
        &mut self,
        component: &mut dyn Component,
        port: PortId,
        msg: MemMsg,
        phase: Phase,
    ) -> Result<(), SimError> {
        self.phase = phase;
        let ev = Delivered::Message {
            port,
            msg: msg.into(),
        };
        component.handle_event(&ev, self)
    }

    /// Delivers the wake `token` to `component`, in `phase`.
    pub fn wake(
        &mut self,
        component: &mut dyn Component,
        token: u64,
        phase: Phase,
    ) -> Result<(), SimError> {
        self.phase = phase;
        component.handle_event(&Delivered::Wake { token }, self)
    }

    /// Every send since the last call, which are cleared.
    pub fn take_sent(&mut self) -> Vec<Sent> {
        std::mem::take(&mut self.sent)
    }

    /// Every wake since the last call, which are cleared.
    pub fn take_woke(&mut self) -> Vec<Woke> {
        std::mem::take(&mut self.woke)
    }
}

impl InitContext for MockCtx {
    fn component(&self) -> ComponentId {
        ComponentId(0)
    }

    fn send(
        &mut self,
        port: PortId,
        msg: Message,
        when: ScheduleWhen,
        phase: Phase,
    ) -> Result<(), SimError> {
        let Message::MemV1(msg) = msg else {
            panic!("the kernel speaks mem.v1 only");
        };
        self.sent.push(Sent {
            port,
            msg,
            when,
            phase,
        });
        Ok(())
    }

    fn wake_self(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError> {
        self.woke.push(Woke { when, phase, token });
        Ok(())
    }

    fn rng(&mut self) -> &mut dyn SimRng {
        &mut self.rng
    }

    fn trace(&mut self, kind: &'static str, fields: Vec<(&'static str, Value)>) {
        self.traced.push((kind, fields));
    }
}

impl SimContext for MockCtx {
    fn now(&self) -> Tick {
        Tick::ZERO
    }

    fn phase(&self) -> Phase {
        self.phase
    }
}

/// A runtime snapshot's component entries and the RAM contents they hold, walked by the
/// snapshot format of `docs/m0-design.md` §7 and the RAM's schema 1. Test-only reading.
pub mod snap {
    use systemscope_contracts::canonical::{CanonicalEvent, DecodeError, Decoder};
    use systemscope_contracts::event::EventKey;

    use super::layout::RAM_BASE;

    /// Every component's `(schema, bytes)`, by id.
    pub fn components(snapshot: &[u8]) -> Result<Vec<(u32, Vec<u8>)>, DecodeError> {
        let mut d = Decoder::new(snapshot);
        d.raw(8)?; // magic
        d.u32()?; // format version
        d.u64()?; // seed
        d.u64()?; // ticks per second
        d.u64()?; // max events per phase
        d.str()?; // contracts version
        for _ in 0..d.len()? {
            d.u32()?;
            d.raw(3 * 8 + 1)?;
        }
        d.raw(32)?; // topology hash
        if d.u8()? == 1 {
            EventKey::decode(&mut d)?;
        }
        d.u64()?; // dispatched in phase
        d.u64()?; // next sequence
        d.raw(32)?; // execution digest
        for _ in 0..d.len()? {
            CanonicalEvent::decode(&mut d)?;
        }
        for _ in 0..d.len()? {
            d.raw(4 * 8)?;
        }
        let mut out = Vec::new();
        for i in 0..d.len()? {
            assert_eq!(d.u32()? as usize, i, "component entries are by id");
            let schema = d.u32()?;
            out.push((schema, d.bytes()?.to_vec()));
        }
        d.finish()?;
        Ok(out)
    }

    /// `len` bytes of RAM at bus address `addr`, from the RAM's snapshot bytes.
    pub fn ram_read(bytes: &[u8], addr: u64, len: usize) -> Result<Vec<u8>, DecodeError> {
        let mut d = Decoder::new(bytes);
        d.u64()?; // size
        d.raw(32)?; // image hash
        match d.u8()? {
            0 => {
                d.u128()?;
            }
            _ => {
                d.u32()?;
                d.u64()?;
            }
        }
        let mut pages = Vec::new();
        for _ in 0..d.len()? {
            let index = d.u32()?;
            pages.push((index, d.bytes()?.to_vec()));
        }
        d.finish()?;
        let offset = (addr - RAM_BASE) as usize;
        Ok((offset..offset + len)
            .map(|at| {
                let index = (at / 4096) as u32;
                pages
                    .iter()
                    .find(|(i, _)| *i == index)
                    .map_or(0, |(_, page)| page[at % 4096])
            })
            .collect())
    }
}
