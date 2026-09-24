//! `ToyCpu`: a clocked initiator issuing a seeded stream of reads and writes.
//!
//! ```text
//! Wake(ISSUE) @ REQUEST ──send──▶ ReadReq / WriteReq
//! Read/WriteResp @ COMPLETE ──▶ queued; Wake(COMMIT) @ COMMIT (same tick)
//! Wake(COMMIT) ──▶ check reads against the shadow copy, apply writes, fold checksum
//! ```
//!
//! Every choice (operation, address, data, think time) comes from `ctx.rng()`. A read that
//! disagrees with the shadow copy faults the session.
//!
//! # Shadow-copy correctness
//!
//! The shadow copy is updated when a write's response commits, not when it is issued. That
//! is exact only because the CPU never has two operations to the same slot in flight: any
//! read to a slot is issued after the previous write to it committed, so the memory
//! accepted that write first (see `memory` ordering semantics).

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use systemscope_contracts::canonical::DecodeError;
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

/// The CPU's only port: a `mem.v0` initiator.
pub const PORT: PortId = PortId(0);

/// Wake token that issues the next operation.
pub const ISSUE: u64 = 0;

/// Wake token that commits arrived responses.
pub const COMMIT: u64 = 1;

/// Layout of [`ToyCpu`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Workload and timing of a [`ToyCpu`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToyCpuConfig {
    /// The clock the CPU runs on.
    pub clock: ClockDomainId,
    /// Total operations to issue.
    pub ops: u64,
    /// Most operations in flight at once. Must be below the number of slots.
    pub max_outstanding: u32,
    /// Think time between issues is uniform in `1..=max_think_cycles`.
    pub max_think_cycles: NonZeroU64,
    /// Bytes per access; every address is a multiple of this.
    pub access_len: u32,
    /// Number of `access_len`-sized slots, starting at address 0.
    pub slots: u32,
    /// Chance, in percent, that an operation is a write.
    pub write_percent: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Read { addr: u64 },
    Write { addr: u64, data: Vec<u8> },
}

impl Op {
    fn addr(&self) -> u64 {
        match *self {
            Op::Read { addr } | Op::Write { addr, .. } => addr,
        }
    }
}

/// A seeded memory-traffic generator.
pub struct ToyCpu {
    config: ToyCpuConfig,
    issued: u64,
    committed: u64,
    next_txn: u64,
    issue_scheduled: bool,
    commit_scheduled: bool,
    outstanding: BTreeMap<TxnId, Op>,
    /// Responses in arrival order, awaiting `COMMIT`. `Some` holds read data.
    arrived: Vec<(TxnId, Option<Vec<u8>>)>,
    /// Committed contents of every written slot.
    shadow: BTreeMap<u64, Vec<u8>>,
    checksum: u64,
}

impl ToyCpu {
    /// Creates a CPU that has issued nothing.
    pub fn new(config: ToyCpuConfig) -> ToyCpu {
        ToyCpu {
            config,
            issued: 0,
            committed: 0,
            next_txn: 0,
            issue_scheduled: false,
            commit_scheduled: false,
            outstanding: BTreeMap::new(),
            arrived: Vec::new(),
            shadow: BTreeMap::new(),
            checksum: 0,
        }
    }

    fn can_issue(&self) -> bool {
        self.issued < self.config.ops
            && self.outstanding.len() < self.config.max_outstanding as usize
    }

    fn schedule_issue(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        let think = 1 + ctx.rng().below(self.config.max_think_cycles);
        let when = ScheduleWhen::Cycles {
            domain: self.config.clock,
            k: think,
        };
        ctx.wake_self(when, Phase::Request, ISSUE)?;
        self.issue_scheduled = true;
        Ok(())
    }

    fn issue(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        let slots = NonZeroU64::new(u64::from(self.config.slots))
            .ok_or(SimError::ComponentFault("toy cpu: no slots"))?;
        let len = u64::from(self.config.access_len);
        // Probe linearly from a random slot to one with nothing in flight.
        let start = ctx.rng().below(slots);
        let busy = |addr: u64| self.outstanding.values().any(|op| op.addr() == addr);
        let addr = (0..slots.get())
            .map(|i| (start + i) % slots.get() * len)
            .find(|&addr| !busy(addr))
            .ok_or(SimError::ComponentFault("toy cpu: every slot is busy"))?;
        let hundred = NonZeroU64::new(100).expect("non-zero");
        let op = if ctx.rng().chance(self.config.write_percent, hundred) {
            let mut data = Vec::with_capacity(self.config.access_len as usize);
            while data.len() < self.config.access_len as usize {
                data.extend_from_slice(&ctx.rng().next_u64().to_le_bytes());
            }
            data.truncate(self.config.access_len as usize);
            Op::Write { addr, data }
        } else {
            Op::Read { addr }
        };

        let txn = TxnId(self.next_txn);
        self.next_txn += 1;
        let msg = match &op {
            Op::Read { addr } => MemMsg::ReadReq {
                txn,
                addr: *addr,
                len: self.config.access_len,
            },
            Op::Write { addr, data } => MemMsg::WriteReq {
                txn,
                addr: *addr,
                data: data.clone(),
            },
        };
        ctx.send(PORT, msg.into(), ScheduleWhen::Now, Phase::Request)?;
        let write = matches!(op, Op::Write { .. });
        ctx.trace(
            "toy.cpu.issue",
            vec![
                ("txn", Value::U64(txn.0)),
                ("addr", Value::U64(op.addr())),
                ("write", Value::Bool(write)),
            ],
        );
        self.outstanding.insert(txn, op);
        self.issued += 1;
        Ok(())
    }

    fn commit(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        for (txn, data) in std::mem::take(&mut self.arrived) {
            let op = self
                .outstanding
                .remove(&txn)
                .ok_or(SimError::ComponentFault(
                    "toy cpu: response for unknown txn",
                ))?;
            let (tag, addr, bytes) = match (op, data) {
                (Op::Read { addr }, Some(data)) => {
                    let zeros = vec![0; self.config.access_len as usize];
                    let expected = self.shadow.get(&addr).unwrap_or(&zeros);
                    if &data != expected {
                        return Err(SimError::ComponentFault("toy cpu: read mismatch"));
                    }
                    (0u64, addr, data)
                }
                (Op::Write { addr, data }, None) => {
                    self.shadow.insert(addr, data.clone());
                    (1, addr, data)
                }
                _ => return Err(SimError::ComponentFault("toy cpu: response kind mismatch")),
            };
            for word in [tag, txn.0, addr] {
                self.fold(word);
            }
            for byte in bytes {
                self.fold(u64::from(byte));
            }
            self.committed += 1;
            ctx.trace(
                "toy.cpu.commit",
                vec![
                    ("txn", Value::U64(txn.0)),
                    ("checksum", Value::U64(self.checksum)),
                ],
            );
        }
        Ok(())
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        let c = &self.config;
        w.u32(c.clock.0);
        w.u64(c.ops);
        w.u32(c.max_outstanding);
        w.u64(c.max_think_cycles.get());
        w.u32(c.access_len);
        w.u32(c.slots);
        w.u64(c.write_percent);
    }

    /// FNV-1a style fold over 64-bit words.
    fn fold(&mut self, word: u64) {
        self.checksum = (self.checksum ^ word).wrapping_mul(0x0000_0100_0000_01b3);
    }
}

impl Component for ToyCpu {
    fn type_name(&self) -> &'static str {
        "toy.cpu"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        if self.config.max_outstanding == 0 || self.config.max_outstanding >= self.config.slots {
            return Err(SimError::ComponentFault(
                "toy cpu: max_outstanding must be in 1..slots",
            ));
        }
        if self.config.ops > 0 {
            self.schedule_issue(ctx)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Wake { token: ISSUE } => {
                self.issue_scheduled = false;
                if self.can_issue() {
                    self.issue(ctx)?;
                }
                if self.can_issue() {
                    self.schedule_issue(ctx)?;
                }
            }
            Delivered::Wake { token: COMMIT } => {
                self.commit_scheduled = false;
                self.commit(ctx)?;
                if !self.issue_scheduled && self.can_issue() {
                    self.schedule_issue(ctx)?;
                }
            }
            // Sends are protocol-checked, so only mem.v0 arrives on this port.
            Delivered::Message {
                msg: Message::MemV1(_),
                ..
            } => {
                return Err(SimError::ComponentFault("toy cpu: mem.v1 message"));
            }
            Delivered::Message {
                msg: Message::Irq(_) | Message::Block(_),
                ..
            } => {
                return Err(SimError::ComponentFault("toy cpu: non-mem message"));
            }
            Delivered::Message {
                msg: Message::Mem(msg),
                ..
            } => {
                let arrival = match msg {
                    MemMsg::ReadResp { txn, data } => (*txn, Some(data.clone())),
                    MemMsg::WriteResp { txn } => (*txn, None),
                    _ => {
                        return Err(SimError::ComponentFault(
                            "toy cpu: request on initiator port",
                        ));
                    }
                };
                self.arrived.push(arrival);
                if !self.commit_scheduled {
                    ctx.wake_self(ScheduleWhen::Now, Phase::Commit, COMMIT)?;
                    self.commit_scheduled = true;
                }
            }
            Delivered::Wake { .. } => {
                return Err(SimError::ComponentFault("toy cpu: unknown wake token"));
            }
        }
        Ok(())
    }

    /// Counters and queue depths; cheap enough to call at every observe point.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("issued", Value::U64(self.issued)),
                ("committed", Value::U64(self.committed)),
                ("next_txn", Value::U64(self.next_txn)),
                ("outstanding", Value::U64(self.outstanding.len() as u64)),
                ("arrived", Value::U64(self.arrived.len() as u64)),
                ("checksum", Value::U64(self.checksum)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration, the counters and flags, then the outstanding, arrived,
    /// and shadow collections, then the checksum.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u64(self.issued);
        w.u64(self.committed);
        w.u64(self.next_txn);
        w.bool(self.issue_scheduled);
        w.bool(self.commit_scheduled);
        w.len(self.outstanding.len());
        for (txn, op) in &self.outstanding {
            w.u64(txn.0);
            match op {
                Op::Read { addr } => {
                    w.u8(0);
                    w.u64(*addr);
                }
                Op::Write { addr, data } => {
                    w.u8(1);
                    w.u64(*addr);
                    w.bytes(data);
                }
            }
        }
        w.len(self.arrived.len());
        for (txn, data) in &self.arrived {
            w.u64(txn.0);
            match data {
                None => w.u8(0),
                Some(data) => {
                    w.u8(1);
                    w.bytes(data);
                }
            }
        }
        w.len(self.shadow.len());
        for (addr, data) in &self.shadow {
            w.u64(*addr);
            w.bytes(data);
        }
        w.u64(self.checksum);
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let mut config = SnapshotWriter::new();
        self.write_config(&mut config);
        if r.raw(config.as_bytes().len())? != config.as_bytes() {
            return Err(RestoreError::InvalidState(
                "toy cpu: snapshot was taken with a different configuration",
            ));
        }
        self.issued = r.u64()?;
        self.committed = r.u64()?;
        self.next_txn = r.u64()?;
        self.issue_scheduled = r.bool()?;
        self.commit_scheduled = r.bool()?;
        let tag = |what, tag| RestoreError::Decode(DecodeError::InvalidTag { what, tag });
        self.outstanding = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let txn = TxnId(r.u64()?);
            if previous.is_some_and(|p| txn <= p) {
                return Err(RestoreError::InvalidState(
                    "toy cpu: outstanding transactions out of order",
                ));
            }
            previous = Some(txn);
            let op = match r.u8()? {
                0 => Op::Read { addr: r.u64()? },
                1 => Op::Write {
                    addr: r.u64()?,
                    data: r.bytes()?.to_vec(),
                },
                t => return Err(tag("toy cpu operation", t)),
            };
            self.outstanding.insert(txn, op);
        }
        self.arrived = Vec::new();
        for _ in 0..r.len()? {
            let txn = TxnId(r.u64()?);
            let data = match r.u8()? {
                0 => None,
                1 => Some(r.bytes()?.to_vec()),
                t => return Err(tag("toy cpu arrival", t)),
            };
            self.arrived.push((txn, data));
        }
        self.shadow = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let addr = r.u64()?;
            if previous.is_some_and(|p| addr <= p) {
                return Err(RestoreError::InvalidState(
                    "toy cpu: shadow slots out of order",
                ));
            }
            previous = Some(addr);
            self.shadow.insert(addr, r.bytes()?.to_vec());
        }
        self.checksum = r.u64()?;
        Ok(())
    }
}
