//! `MultiMasterBus`: arbitrates `mem.v1` requests from several initiators onto
//! memory-mapped regions (`docs/m2-design.md` §10).
//!
//! ```text
//! ReadReq/WriteReq @ REQUEST on master m ─┬─ inside one region r ─▶ queues[r][m] ← request at addr − base; wake @ Now, TRANSFER
//!                                         └─ anywhere else ──────▶ Fault { AccessFault } on m @ COMPLETE
//! wake @ TRANSFER ─▶ for each idle region with a queued request: grant one master (round-robin),
//!                    send it with a fresh downstream txn on the region's port @ TRANSFER
//! ReadResp/WriteResp @ COMPLETE on region r ─▶ checked against r's active transaction, relayed to its
//!                    master with the original txn @ COMPLETE; r is freed and, if anything is queued,
//!                    the bus wakes @ Cycles { clock, 1 }, TRANSFER
//! ```
//!
//! [`AddressBus`](crate::AddressBus) stays the M1 interconnect; this bus shares its region
//! decoding rules (half-open ranges, checked arithmetic, no splitting, `AccessFault` for
//! unrouted requests) but not its code or its snapshot.
//!
//! # Identity
//!
//! Masters are indexed by their position in the configured list, and a transaction is
//! identified by `(master, txn)`: two masters may use one `TxnId` at the same time, but
//! one master reusing a `txn` that is still queued or active faults the session. Every
//! granted request gets a fresh downstream `TxnId` from one bus-owned counter, which is
//! checked and never wraps; the response is relayed with the master's original `txn`.
//!
//! # Arbitration and phases
//!
//! Each region has at most one active downstream transaction, one FIFO per master, and a
//! round-robin cursor that starts at 0 and advances only on a grant. Requests are only
//! enqueued in `Request` and regions only freed in `Complete`, so every `Transfer` wake of
//! one tick sees the same state: the first grants everything grantable and the rest find
//! nothing to do. Arbitration reads only the FIFOs, the cursors, and `active`, so it never
//! depends on the order in which requests from different masters were dispatched within
//! one `Request` phase. Requests from one master to one region keep their arrival order.
//!
//! A region freed in `Complete` is next granted in the `Transfer` of the next bus clock
//! cycle: that tick's `Transfer` has already run.
//!
//! # What is an access fault, and what is a bug
//!
//! As on `AddressBus`, a well-formed request that no single region contains is answered
//! by the bus with [`MemFault::AccessFault`] and recorded as `platform.bus.fault`.
//! Everything else faults the session with [`SimError::ComponentFault`]: a zero-length
//! request, a reused `(master, txn)`, a request outside `Request`, a response outside
//! `Complete`, a response that does not match its region's active transaction (unknown,
//! stale, duplicate, wrong kind, or on the wrong region's port), a message in the wrong
//! direction, and an exhausted downstream `TxnId` counter.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use systemscope_contracts::canonical::DecodeError;
use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::ClockDomainId;
use systemscope_contracts::trace::Value;

use crate::bus::{FAULT_KIND, Region};

/// The trace record the bus emits when it forwards a request downstream.
pub const GRANT_KIND: &str = "platform.bus.grant";

/// Layout of [`MultiMasterBus`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// The token of the arbitration wake, the only wake the bus schedules.
const ARBITRATE: u64 = 0;

/// The configuration of a [`MultiMasterBus`]. Its order is its identity: the masters fix
/// the master indices and, with the regions, the port numbering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiMasterBusConfig {
    /// The master port names; the position of a name is that master's index.
    pub masters: Vec<&'static str>,
    /// The regions, in decoding and arbitration order.
    pub regions: Vec<Region>,
    /// The bus clock, which paces grants after a completion.
    pub clock: ClockDomainId,
}

/// Why a configuration cannot form a [`MultiMasterBus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultiMasterBusConfigError {
    /// There are no masters.
    NoMasters,
    /// The region has size 0.
    ZeroSize(&'static str),
    /// The region's last byte would lie past `u64::MAX`.
    Wraps(&'static str),
    /// Two regions share at least one byte.
    Overlap(&'static str, &'static str),
    /// Two ports (masters or regions) share a name.
    DuplicateName(&'static str),
    /// There are more masters and regions than ports a component can declare.
    TooManyPorts,
}

impl fmt::Display for MultiMasterBusConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MultiMasterBusConfigError::NoMasters => f.write_str("the bus has no masters"),
            MultiMasterBusConfigError::ZeroSize(r) => write!(f, "region {r} has size 0"),
            MultiMasterBusConfigError::Wraps(r) => {
                write!(f, "region {r} runs past the end of u64")
            }
            MultiMasterBusConfigError::Overlap(a, b) => write!(f, "regions {a} and {b} overlap"),
            MultiMasterBusConfigError::DuplicateName(n) => write!(f, "two ports are named {n}"),
            MultiMasterBusConfigError::TooManyPorts => f.write_str("too many masters and regions"),
        }
    }
}

impl std::error::Error for MultiMasterBusConfigError {}

/// The one downstream transaction of a region: enough to match its response and relay it.
/// The request itself is a runtime event, never bus state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveTxn {
    master: u16,
    original_txn: TxnId,
    downstream_txn: TxnId,
    write: bool,
}

/// One region's arbitration state.
///
/// A queued request is stored as the `mem.v1` request with the region offset as its
/// address and the master's original `txn`; its master and region are the indices of the
/// FIFO holding it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegionState {
    active: Option<ActiveTxn>,
    /// One FIFO per master, indexed by master.
    queues: Vec<VecDeque<MemMsg>>,
    /// The next master to consider; always below the master count.
    rr_cursor: u16,
}

impl RegionState {
    fn idle(masters: usize) -> RegionState {
        RegionState {
            active: None,
            queues: vec![VecDeque::new(); masters],
            rr_cursor: 0,
        }
    }

    /// The master to grant: the first with a non-empty FIFO, scanning circularly from the
    /// cursor. `None` if every FIFO is empty.
    fn select(&self) -> Option<u16> {
        let masters = self.queues.len();
        (0..masters)
            .map(|k| (usize::from(self.rr_cursor) + k) % masters)
            .find(|&m| !self.queues[m].is_empty())
            .map(|m| u16::try_from(m).expect("master count checked by new"))
    }

    fn has_queued(&self) -> bool {
        self.queues.iter().any(|q| !q.is_empty())
    }
}

/// A multi-initiator `mem.v1` interconnect with per-region round-robin arbitration.
///
/// Ports: one target port per master, named and ordered as configured, then one initiator
/// port per region, named after it, in region order (see [`MultiMasterBus::master_port`]
/// and [`MultiMasterBus::region_port`]).
pub struct MultiMasterBus {
    config: MultiMasterBusConfig,
    regions: Vec<RegionState>,
    /// The next downstream `TxnId` to allocate. Every active `downstream_txn` is below it.
    next_downstream: u64,
}

/// The `TxnId` of a request and whether it is a write, or `None` for a response.
fn request_txn(msg: &MemMsg) -> Option<(TxnId, bool)> {
    match msg {
        MemMsg::ReadReq { txn, .. } => Some((*txn, false)),
        MemMsg::WriteReq { txn, .. } => Some((*txn, true)),
        MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => None,
    }
}

/// `msg` with its `txn` replaced.
fn with_txn(msg: &MemMsg, txn: TxnId) -> MemMsg {
    let mut msg = msg.clone();
    match &mut msg {
        MemMsg::ReadReq { txn: t, .. }
        | MemMsg::WriteReq { txn: t, .. }
        | MemMsg::ReadResp { txn: t, .. }
        | MemMsg::WriteResp { txn: t, .. } => *t = txn,
    }
    msg
}

/// A request moved to `offset`.
fn at_offset(msg: &MemMsg, offset: u64) -> MemMsg {
    let mut msg = msg.clone();
    match &mut msg {
        MemMsg::ReadReq { addr, .. } | MemMsg::WriteReq { addr, .. } => *addr = offset,
        MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {}
    }
    msg
}

impl MultiMasterBus {
    /// Creates an idle bus: empty FIFOs, no active transactions, every cursor at 0, and
    /// the downstream counter at 0.
    ///
    /// Rejects an empty master list, a region of size 0 or past `u64::MAX`, overlapping
    /// regions, two ports with one name, and more ports than a component can declare.
    pub fn new(config: MultiMasterBusConfig) -> Result<MultiMasterBus, MultiMasterBusConfigError> {
        if config.masters.is_empty() {
            return Err(MultiMasterBusConfigError::NoMasters);
        }
        if config.masters.len() + config.regions.len() >= usize::from(u16::MAX) {
            return Err(MultiMasterBusConfigError::TooManyPorts);
        }
        let names = config
            .masters
            .iter()
            .copied()
            .chain(config.regions.iter().map(|r| r.name));
        let mut seen = BTreeSet::new();
        for name in names {
            if !seen.insert(name) {
                return Err(MultiMasterBusConfigError::DuplicateName(name));
            }
        }
        for (i, region) in config.regions.iter().enumerate() {
            if region.size == 0 {
                return Err(MultiMasterBusConfigError::ZeroSize(region.name));
            }
            let Some(last) = region.base.checked_add(region.size - 1) else {
                return Err(MultiMasterBusConfigError::Wraps(region.name));
            };
            for earlier in &config.regions[..i] {
                if earlier.base <= last && region.base <= earlier.base + (earlier.size - 1) {
                    return Err(MultiMasterBusConfigError::Overlap(
                        earlier.name,
                        region.name,
                    ));
                }
            }
        }
        let regions = vec![RegionState::idle(config.masters.len()); config.regions.len()];
        Ok(MultiMasterBus {
            config,
            regions,
            next_downstream: 0,
        })
    }

    /// The configuration.
    pub fn config(&self) -> &MultiMasterBusConfig {
        &self.config
    }

    /// The target port of the master at `index`.
    pub fn master_port(&self, index: u16) -> PortId {
        assert!(
            usize::from(index) < self.config.masters.len(),
            "no such master"
        );
        PortId(index)
    }

    /// The initiator port of the region at `index`: region ports follow the master ports.
    pub fn region_port(&self, index: u16) -> PortId {
        assert!(
            usize::from(index) < self.config.regions.len(),
            "no such region"
        );
        PortId(self.master_count() + index)
    }

    fn master_count(&self) -> u16 {
        u16::try_from(self.config.masters.len()).expect("master count checked by new")
    }

    /// The master holding the region's active transaction, if any.
    pub fn active_master(&self, region: u16) -> Option<u16> {
        self.regions[usize::from(region)].active.map(|a| a.master)
    }

    /// The number of requests `master` has queued for `region`.
    pub fn queued(&self, region: u16, master: u16) -> usize {
        self.regions[usize::from(region)].queues[usize::from(master)].len()
    }

    /// The region's round-robin cursor.
    pub fn rr_cursor(&self, region: u16) -> u16 {
        self.regions[usize::from(region)].rr_cursor
    }

    /// The next downstream `TxnId` the bus will allocate.
    pub fn next_downstream_txn(&self) -> u64 {
        self.next_downstream
    }

    /// The index of the one region holding every byte of `first..=last`.
    fn route(&self, first: u64, last: u64) -> Option<u16> {
        let regions = &self.config.regions;
        let index = regions
            .iter()
            .position(|r| r.base <= first && first - r.base < r.size)?;
        let region = &regions[index];
        let index = u16::try_from(index).expect("region count checked by new");
        (last - region.base < region.size).then_some(index)
    }

    /// Whether `master` has a request with `txn` queued or active anywhere.
    fn is_live(&self, master: u16, txn: TxnId) -> bool {
        self.regions.iter().any(|r| {
            r.active
                .is_some_and(|a| a.master == master && a.original_txn == txn)
                || r.queues[usize::from(master)]
                    .iter()
                    .any(|q| request_txn(q).is_some_and(|(t, _)| t == txn))
        })
    }

    fn request(
        &mut self,
        master: u16,
        msg: &MemMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let (Some((txn, _)), Some(access)) = (request_txn(msg), msg.access()) else {
            return Err(SimError::ComponentFault(
                "multi-master bus: response on a master port",
            ));
        };
        if ctx.phase() != Phase::Request {
            return Err(SimError::ComponentFault(
                "multi-master bus: request arrived outside REQUEST",
            ));
        }
        if self.is_live(master, txn) {
            return Err(SimError::ComponentFault(
                "multi-master bus: master reuses a queued or active txn",
            ));
        }
        let routed = match access {
            Access::Empty => {
                return Err(SimError::ComponentFault(
                    "multi-master bus: zero-length request",
                ));
            }
            Access::OutOfRange => None,
            Access::Bytes { first, last } => self.route(first, last).map(|i| (i, first)),
        };
        match routed {
            Some((region, first)) => {
                let offset = first - self.config.regions[usize::from(region)].base;
                self.regions[usize::from(region)].queues[usize::from(master)]
                    .push_back(at_offset(msg, offset));
                ctx.wake_self(ScheduleWhen::Now, Phase::Transfer, ARBITRATE)
            }
            None => self.fault(master, msg, ctx),
        }
    }

    /// Answers a request that no region contains with an access fault.
    fn fault(
        &mut self,
        master: u16,
        msg: &MemMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let fault = MemFault::AccessFault;
        let (txn, addr, len, resp) = match msg {
            MemMsg::ReadReq { txn, addr, len } => {
                let outcome = ReadOutcome::Fault { fault };
                (
                    *txn,
                    *addr,
                    u64::from(*len),
                    MemMsg::ReadResp { txn: *txn, outcome },
                )
            }
            MemMsg::WriteReq { txn, addr, data } => {
                let outcome = WriteOutcome::Fault { fault };
                let len = data.len() as u64;
                (*txn, *addr, len, MemMsg::WriteResp { txn: *txn, outcome })
            }
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault(
                    "multi-master bus: response on a master port",
                ));
            }
        };
        ctx.send(
            self.master_port(master),
            resp.into(),
            ScheduleWhen::Now,
            Phase::Complete,
        )?;
        ctx.trace(
            FAULT_KIND,
            vec![
                ("txn", Value::U64(txn.0)),
                ("addr", Value::U64(addr)),
                ("len", Value::U64(len)),
                ("master", Value::U64(u64::from(master))),
            ],
        );
        Ok(())
    }

    /// One arbitration pass: at most one grant per idle region, in region order.
    fn arbitrate(&mut self, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if ctx.phase() != Phase::Transfer {
            return Err(SimError::ComponentFault(
                "multi-master bus: arbitration outside TRANSFER",
            ));
        }
        for index in 0..self.regions.len() {
            let region = u16::try_from(index).expect("region count checked by new");
            let state = &self.regions[index];
            if state.active.is_some() {
                continue;
            }
            let Some(master) = state.select() else {
                continue;
            };
            let downstream_txn = TxnId(self.next_downstream);
            let next = self
                .next_downstream
                .checked_add(1)
                .ok_or(SimError::ComponentFault(
                    "multi-master bus: downstream TxnId space exhausted",
                ))?;
            let request = state.queues[usize::from(master)]
                .front()
                .expect("select returns a master with a queued request");
            let (original_txn, write) = request_txn(request).expect("only requests are queued");
            ctx.send(
                self.region_port(region),
                with_txn(request, downstream_txn).into(),
                ScheduleWhen::Now,
                Phase::Transfer,
            )?;
            self.next_downstream = next;
            let masters = self.master_count();
            let state = &mut self.regions[index];
            state.queues[usize::from(master)].pop_front();
            state.active = Some(ActiveTxn {
                master,
                original_txn,
                downstream_txn,
                write,
            });
            state.rr_cursor = (master + 1) % masters;
            ctx.trace(
                GRANT_KIND,
                vec![
                    ("region", Value::U64(u64::from(region))),
                    ("master", Value::U64(u64::from(master))),
                    ("txn", Value::U64(original_txn.0)),
                    ("downstream_txn", Value::U64(downstream_txn.0)),
                ],
            );
        }
        Ok(())
    }

    fn response(
        &mut self,
        region: u16,
        msg: &MemMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let (txn, write) = match msg {
            MemMsg::ReadResp { txn, .. } => (*txn, false),
            MemMsg::WriteResp { txn, .. } => (*txn, true),
            MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. } => {
                return Err(SimError::ComponentFault(
                    "multi-master bus: request on a region port",
                ));
            }
        };
        if ctx.phase() != Phase::Complete {
            return Err(SimError::ComponentFault(
                "multi-master bus: response arrived outside COMPLETE",
            ));
        }
        let state = &self.regions[usize::from(region)];
        let active =
            state
                .active
                .filter(|a| a.downstream_txn == txn)
                .ok_or(SimError::ComponentFault(
                    "multi-master bus: response does not match the region's active txn",
                ))?;
        if active.write != write {
            return Err(SimError::ComponentFault(
                "multi-master bus: response kind mismatch",
            ));
        }
        let again = state.has_queued();
        ctx.send(
            self.master_port(active.master),
            with_txn(msg, active.original_txn).into(),
            ScheduleWhen::Now,
            Phase::Complete,
        )?;
        self.regions[usize::from(region)].active = None;
        if again {
            let when = ScheduleWhen::Cycles {
                domain: self.config.clock,
                k: 1,
            };
            ctx.wake_self(when, Phase::Transfer, ARBITRATE)?;
        }
        Ok(())
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.len(self.config.regions.len());
        for region in &self.config.regions {
            w.str(region.name);
            w.u64(region.base);
            w.u64(region.size);
        }
        w.len(self.config.masters.len());
        for master in &self.config.masters {
            w.str(master);
        }
        w.u32(self.config.clock.0);
    }

    fn read_config(&self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let different = RestoreError::InvalidState(
            "multi-master bus: snapshot was taken with a different configuration",
        );
        if r.len()? != self.config.regions.len() {
            return Err(different);
        }
        for region in &self.config.regions {
            if r.str()? != region.name || r.u64()? != region.base || r.u64()? != region.size {
                return Err(different);
            }
        }
        if r.len()? != self.config.masters.len() {
            return Err(different);
        }
        for master in &self.config.masters {
            if r.str()? != *master {
                return Err(different);
            }
        }
        if r.u32()? != self.config.clock.0 {
            return Err(different);
        }
        Ok(())
    }

    /// Reads one region's state, checking what can be checked within the region.
    fn read_region(
        &self,
        r: &mut SnapshotReader<'_>,
        region: &Region,
        next_downstream: u64,
    ) -> Result<RegionState, RestoreError> {
        let invalid = RestoreError::InvalidState;
        let masters = self.config.masters.len();
        let active = match r.u8()? {
            0 => None,
            1 => {
                let active = ActiveTxn {
                    master: r.u16()?,
                    original_txn: TxnId(r.u64()?),
                    downstream_txn: TxnId(r.u64()?),
                    write: r.bool()?,
                };
                if usize::from(active.master) >= masters {
                    return Err(invalid("multi-master bus: active txn of an unknown master"));
                }
                if active.downstream_txn.0 >= next_downstream {
                    return Err(invalid(
                        "multi-master bus: active downstream txn not yet allocated",
                    ));
                }
                Some(active)
            }
            tag => {
                return Err(RestoreError::Decode(DecodeError::InvalidTag {
                    what: "multi-master bus active txn",
                    tag,
                }));
            }
        };
        let rr_cursor = r.u16()?;
        if usize::from(rr_cursor) >= masters {
            return Err(invalid("multi-master bus: cursor past the last master"));
        }
        let mut queues = Vec::with_capacity(masters);
        for _ in 0..masters {
            let mut queue = VecDeque::new();
            for _ in 0..r.len()? {
                let msg = MemMsg::decode(r)?;
                match msg.access() {
                    None => return Err(invalid("multi-master bus: queued message is a response")),
                    Some(Access::Empty) => {
                        return Err(invalid("multi-master bus: queued request is zero-length"));
                    }
                    Some(Access::Bytes { last, .. }) if last < region.size => {}
                    Some(_) => {
                        return Err(invalid(
                            "multi-master bus: queued request outside its region",
                        ));
                    }
                }
                queue.push_back(msg);
            }
            queues.push(queue);
        }
        Ok(RegionState {
            active,
            queues,
            rr_cursor,
        })
    }
}

impl Component for MultiMasterBus {
    fn type_name(&self) -> &'static str {
        "platform.multi_master_bus"
    }

    fn ports(&self) -> Vec<PortSpec> {
        let port = |name, role| PortSpec {
            name,
            protocol: mem_v1::PROTOCOL,
            role,
        };
        let masters = self.config.masters.iter().map(|&m| port(m, Role::Target));
        let regions = self
            .config
            .regions
            .iter()
            .map(|r| port(r.name, Role::Initiator));
        masters.chain(regions).collect()
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let masters = self.master_count();
        match ev {
            Delivered::Message {
                port,
                msg: Message::MemV1(msg),
            } => {
                if port.0 < masters {
                    self.request(port.0, msg, ctx)
                } else if usize::from(port.0 - masters) < self.regions.len() {
                    self.response(port.0 - masters, msg, ctx)
                } else {
                    Err(SimError::ComponentFault("multi-master bus: unknown port"))
                }
            }
            // Sends are protocol-checked, so only mem.v1 arrives on these ports.
            Delivered::Message {
                msg: Message::Mem(_),
                ..
            } => Err(SimError::ComponentFault("multi-master bus: mem.v0 message")),
            Delivered::Message {
                msg: Message::Irq(_) | Message::Block(_),
                ..
            } => Err(SimError::ComponentFault(
                "multi-master bus: non-mem message",
            )),
            Delivered::Wake { token: ARBITRATE } => self.arbitrate(ctx),
            Delivered::Wake { .. } => Err(SimError::ComponentFault(
                "multi-master bus: unexpected wake",
            )),
        }
    }

    /// The master and region counts and the downstream counter, then one field per region,
    /// named after it: its active master (or `none`), its cursor, and its FIFO lengths in
    /// master order, as `active=<m|none> rr_cursor=<c> queued=<n0>,<n1>,...`.
    fn inspect(&self) -> StateView {
        let mut fields = vec![
            ("masters", Value::U64(self.config.masters.len() as u64)),
            ("regions", Value::U64(self.config.regions.len() as u64)),
            ("next_downstream_txn", Value::U64(self.next_downstream)),
        ];
        for (region, state) in self.config.regions.iter().zip(&self.regions) {
            let active = state
                .active
                .map_or("none".to_owned(), |a| a.master.to_string());
            let queued: Vec<String> = state.queues.iter().map(|q| q.len().to_string()).collect();
            let summary = format!(
                "active={active} rr_cursor={} queued={}",
                state.rr_cursor,
                queued.join(",")
            );
            fields.push((region.name, Value::Str(summary)));
        }
        StateView { fields }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (the regions in order as `name`, `base`, `size`; the
    /// master names in order; the clock); the downstream counter; then per region in
    /// region order: `active` (tag 0, or tag 1 then master `u16`, original `txn`,
    /// downstream `txn`, whether it is a write), the cursor (`u16`), and each master's
    /// FIFO in master order (length, then each canonical `mem.v1` request).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.u64(self.next_downstream);
        for state in &self.regions {
            match state.active {
                None => w.u8(0),
                Some(active) => {
                    w.u8(1);
                    w.u16(active.master);
                    w.u64(active.original_txn.0);
                    w.u64(active.downstream_txn.0);
                    w.bool(active.write);
                }
            }
            w.u16(state.rr_cursor);
            for queue in &state.queues {
                w.len(queue.len());
                for request in queue {
                    request.encode(w);
                }
            }
        }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let invalid = RestoreError::InvalidState;
        self.read_config(r)?;
        let next_downstream = r.u64()?;
        let mut regions = Vec::with_capacity(self.config.regions.len());
        for region in &self.config.regions {
            regions.push(self.read_region(r, region, next_downstream)?);
        }
        let mut live = BTreeSet::new();
        let mut downstream = BTreeSet::new();
        for state in &regions {
            if let Some(active) = state.active {
                if !live.insert((active.master, active.original_txn)) {
                    return Err(invalid("multi-master bus: a (master, txn) is live twice"));
                }
                if !downstream.insert(active.downstream_txn) {
                    return Err(invalid(
                        "multi-master bus: two regions share a downstream txn",
                    ));
                }
            }
            for (master, queue) in state.queues.iter().enumerate() {
                let master = u16::try_from(master).expect("master count checked by new");
                for request in queue {
                    let (txn, _) = request_txn(request).expect("checked by read_region");
                    if !live.insert((master, txn)) {
                        return Err(invalid("multi-master bus: a (master, txn) is live twice"));
                    }
                }
            }
        }
        self.regions = regions;
        self.next_downstream = next_downstream;
        Ok(())
    }
}
