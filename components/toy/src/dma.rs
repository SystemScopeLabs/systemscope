//! `ToyDma`: a clocked initiator issuing seeded bursts of writes to its own region.
//!
//! ```text
//! Wake(ISSUE) @ REQUEST ──send──▶ WriteReq, then Wake(ISSUE) one cycle later (in a burst)
//!                                 or after a random gap (burst done)
//! WriteResp @ COMPLETE ──▶ retire; resume a paused burst one cycle later
//! ```
//!
//! Every choice (burst start, length, data, gap) comes from `ctx.rng()`. The DMA writes
//! only its own region (`docs/m0-design.md` §9.1), so it never invalidates the CPU's
//! shadow copy.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;

/// The DMA's only port: a `mem.v0` initiator.
pub const PORT: PortId = PortId(0);

/// Wake token that issues the next write.
pub const ISSUE: u64 = 0;

/// Layout of [`ToyDma`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Workload and timing of a [`ToyDma`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToyDmaConfig {
    /// The clock the DMA runs on.
    pub clock: ClockDomainId,
    /// Total writes to issue.
    pub ops: u64,
    /// Most writes in flight at once.
    pub max_outstanding: u32,
    /// Burst length is uniform in `1..=max_burst`.
    pub max_burst: NonZeroU64,
    /// Idle time after a burst is uniform in `1..=max_gap_cycles`.
    pub max_gap_cycles: NonZeroU64,
    /// First address of the DMA's region.
    pub base: u64,
    /// Bytes per write; slot `i` is at `base + i × access_len`.
    pub access_len: u32,
    /// Number of slots in the region.
    pub slots: u32,
}

/// A seeded burst-write generator.
pub struct ToyDma {
    config: ToyDmaConfig,
    issued: u64,
    completed: u64,
    next_txn: u64,
    /// Slot of the current burst's next write.
    burst_slot: u32,
    /// Writes left in the current burst.
    burst_left: u64,
    wake_scheduled: bool,
    /// Writes in flight, by `TxnId`, with their addresses.
    outstanding: BTreeMap<TxnId, u64>,
    checksum: u64,
}

impl ToyDma {
    /// Creates a DMA that has issued nothing.
    pub fn new(config: ToyDmaConfig) -> ToyDma {
        ToyDma {
            config,
            issued: 0,
            completed: 0,
            next_txn: 0,
            burst_slot: 0,
            burst_left: 0,
            wake_scheduled: false,
            outstanding: BTreeMap::new(),
            checksum: 0,
        }
    }

    fn wake_in(&mut self, ctx: &mut dyn InitContext, cycles: u64) -> Result<(), SimError> {
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: cycles,
        };
        ctx.wake_self(when, Phase::Request, ISSUE)?;
        self.wake_scheduled = true;
        Ok(())
    }

    fn schedule_gap(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        let gap = 1 + ctx.rng().below(self.config.max_gap_cycles);
        self.wake_in(ctx, gap)
    }

    fn full(&self) -> bool {
        self.outstanding.len() >= self.config.max_outstanding as usize
    }

    fn issue(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let slots = NonZeroU64::new(u64::from(self.config.slots))
            .ok_or(SimError::ComponentFault("toy dma: no slots"))?;
        if self.burst_left == 0 {
            let len = 1 + ctx.rng().below(self.config.max_burst);
            self.burst_left = len.min(self.config.ops - self.issued);
            self.burst_slot = ctx.rng().below(slots) as u32;
        }
        let len = self.config.access_len as usize;
        let mut data = Vec::with_capacity(len);
        while data.len() < len {
            data.extend_from_slice(&ctx.rng().next_u64().to_le_bytes());
        }
        data.truncate(len);
        let addr =
            self.config.base + u64::from(self.burst_slot) * u64::from(self.config.access_len);
        let txn = TxnId(self.next_txn);
        self.next_txn += 1;
        let msg = MemMsg::WriteReq { txn, addr, data };
        ctx.send(PORT, msg.into(), ScheduleWhen::Now, Phase::Request)?;
        ctx.trace(
            "toy.dma.issue",
            vec![("txn", Value::U64(txn.0)), ("addr", Value::U64(addr))],
        );
        self.outstanding.insert(txn, addr);
        self.issued += 1;
        self.burst_left -= 1;
        self.burst_slot = (self.burst_slot + 1) % self.config.slots;
        Ok(())
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        let c = &self.config;
        w.u32(c.clock.0);
        w.u64(c.ops);
        w.u32(c.max_outstanding);
        w.u64(c.max_burst.get());
        w.u64(c.max_gap_cycles.get());
        w.u64(c.base);
        w.u32(c.access_len);
        w.u32(c.slots);
    }

    /// FNV-1a style fold over 64-bit words.
    fn fold(&mut self, word: u64) {
        self.checksum = (self.checksum ^ word).wrapping_mul(0x0000_0100_0000_01b3);
    }
}

impl Component for ToyDma {
    fn type_name(&self) -> &'static str {
        "toy.dma"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        if self.config.max_outstanding == 0 || self.config.slots == 0 {
            return Err(SimError::ComponentFault(
                "toy dma: max_outstanding and slots must be non-zero",
            ));
        }
        if self.config.ops > 0 {
            self.schedule_gap(ctx)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Wake { token: ISSUE } => {
                self.wake_scheduled = false;
                if self.issued < self.config.ops && !self.full() {
                    self.issue(ctx)?;
                }
                if self.issued < self.config.ops && !self.full() {
                    if self.burst_left > 0 {
                        self.wake_in(ctx, 1)?;
                    } else {
                        self.schedule_gap(ctx)?;
                    }
                }
            }
            Delivered::Message {
                msg: Message::Mem(MemMsg::WriteResp { txn }),
                ..
            } => {
                let addr = self
                    .outstanding
                    .remove(txn)
                    .ok_or(SimError::ComponentFault(
                        "toy dma: response for unknown txn",
                    ))?;
                self.completed += 1;
                self.fold(txn.0);
                self.fold(addr);
                ctx.trace(
                    "toy.dma.done",
                    vec![
                        ("txn", Value::U64(txn.0)),
                        ("checksum", Value::U64(self.checksum)),
                    ],
                );
                // Paused on the outstanding limit: a burst resumes one cycle later, and a
                // finished burst starts its gap now.
                if !self.wake_scheduled && self.issued < self.config.ops {
                    if self.burst_left > 0 {
                        self.wake_in(ctx, 1)?;
                    } else {
                        self.schedule_gap(ctx)?;
                    }
                }
            }
            Delivered::Message { .. } => {
                return Err(SimError::ComponentFault(
                    "toy dma: unexpected message on initiator port",
                ));
            }
            Delivered::Wake { .. } => {
                return Err(SimError::ComponentFault("toy dma: unknown wake token"));
            }
        }
        Ok(())
    }

    /// Counters and queue depths; cheap enough to call at every observe point.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("issued", Value::U64(self.issued)),
                ("completed", Value::U64(self.completed)),
                ("next_txn", Value::U64(self.next_txn)),
                ("outstanding", Value::U64(self.outstanding.len() as u64)),
                ("burst_left", Value::U64(self.burst_left)),
                ("checksum", Value::U64(self.checksum)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration, the counters, the burst position, the wake flag, the
    /// outstanding writes, then the checksum.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u64(self.issued);
        w.u64(self.completed);
        w.u64(self.next_txn);
        w.u32(self.burst_slot);
        w.u64(self.burst_left);
        w.bool(self.wake_scheduled);
        w.len(self.outstanding.len());
        for (txn, addr) in &self.outstanding {
            w.u64(txn.0);
            w.u64(*addr);
        }
        w.u64(self.checksum);
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(RestoreError::InvalidState(
                "toy dma: snapshot was taken with a different configuration",
            ));
        }
        self.issued = r.u64()?;
        self.completed = r.u64()?;
        self.next_txn = r.u64()?;
        self.burst_slot = r.u32()?;
        self.burst_left = r.u64()?;
        self.wake_scheduled = r.bool()?;
        if self.burst_slot >= self.config.slots {
            return Err(RestoreError::InvalidState(
                "toy dma: burst slot out of range",
            ));
        }
        self.outstanding = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let txn = TxnId(r.u64()?);
            if previous.is_some_and(|p| txn <= p) || txn.0 >= self.next_txn {
                return Err(RestoreError::InvalidState(
                    "toy dma: outstanding writes out of order",
                ));
            }
            previous = Some(txn);
            self.outstanding.insert(txn, r.u64()?);
        }
        self.checksum = r.u64()?;
        Ok(())
    }
}
