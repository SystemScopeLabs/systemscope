//! `AddressBus`: routes `mem.v1` requests from one initiator to memory-mapped regions
//! (`docs/m1-design.md` §7.1).
//!
//! ```text
//! ReadReq/WriteReq @ REQUEST on cpu ─┬─ inside one region ──▶ send addr − base on that region's port @ TRANSFER
//!                                    └─ anywhere else ─────▶ Fault { AccessFault } on cpu @ COMPLETE
//! ReadResp/WriteResp on a region port ─▶ checked, then relayed on cpu in the phase it arrived in
//! ```
//!
//! # Offsets
//!
//! Targets receive offsets from their region's base, never absolute addresses, so a
//! target works at any base and only the bus knows the memory map.
//!
//! # What is an access fault, and what is a bug
//!
//! A well-formed request that no single region contains is an architectural event: the
//! bus answers it itself with [`MemFault::AccessFault`] and records `platform.bus.fault`.
//! That covers unmapped addresses, requests that run past the end of a region (even into
//! an adjacent one: the bus never splits a request), and requests whose last byte would lie
//! past `u64::MAX`.
//!
//! Everything else is a protocol violation, and faults the session with
//! [`SimError::ComponentFault`]: a zero-length request (`docs/m1-design.md` §4.2), a
//! request that reuses an outstanding `TxnId`, a request outside `Request`, a response for
//! an unknown `TxnId` (including a second response for the same request), a response on
//! the wrong region's port, and a response of the wrong kind.
//!
//! # `TxnId`s
//!
//! There is one upstream port, so the bus forwards the initiator's `TxnId` unchanged. It
//! relies only on the initiator never having two requests with one `TxnId` outstanding,
//! not on any allocation order.

use std::collections::BTreeMap;
use std::fmt;

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
use systemscope_contracts::trace::Value;

/// The upstream port: a `mem.v1` target named `cpu`.
pub const CPU_PORT: PortId = PortId(0);

/// The trace record the bus emits when it answers a request itself.
pub const FAULT_KIND: &str = "platform.bus.fault";

/// Layout of [`AddressBus`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// The port of the region at `index` in declaration order: ports follow `cpu` in region
/// order and are named after their regions.
pub const fn region_port(index: u16) -> PortId {
    PortId(index + 1)
}

/// A memory-mapped region: the bytes `[base, base + size)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Region {
    /// The region's name, which is also the name of its initiator port.
    pub name: &'static str,
    /// Address of the region's first byte.
    pub base: u64,
    /// Number of bytes; at least 1.
    pub size: u64,
}

impl Region {
    /// Address of the last byte. Valid once [`AddressBus::new`] has checked the region.
    fn last(&self) -> u64 {
        self.base + (self.size - 1)
    }
}

/// Why a list of regions cannot form an [`AddressBus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusConfigError {
    /// The region has size 0.
    ZeroSize(&'static str),
    /// The region's last byte would lie past `u64::MAX`.
    Wraps(&'static str),
    /// Two regions share at least one byte.
    Overlap(&'static str, &'static str),
    /// Two regions share a name, so their ports would too.
    DuplicateName(&'static str),
    /// The region is named `cpu`, the name of the upstream port.
    ReservedName(&'static str),
    /// There are more regions than ports a component can declare.
    TooManyRegions,
}

impl fmt::Display for BusConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusConfigError::ZeroSize(r) => write!(f, "region {r} has size 0"),
            BusConfigError::Wraps(r) => write!(f, "region {r} runs past the end of u64"),
            BusConfigError::Overlap(a, b) => write!(f, "regions {a} and {b} overlap"),
            BusConfigError::DuplicateName(r) => write!(f, "two regions are named {r}"),
            BusConfigError::ReservedName(r) => write!(f, "region name {r} is reserved"),
            BusConfigError::TooManyRegions => f.write_str("too many regions"),
        }
    }
}

impl std::error::Error for BusConfigError {}

/// A forwarded request awaiting its response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Outstanding {
    /// Index of the region it was forwarded to.
    region: u16,
    write: bool,
}

/// A single-initiator `mem.v1` interconnect over a fixed memory map.
///
/// Ports: `cpu` (target), then one initiator port per region, named after it, in region
/// order (see [`region_port`]). Region order is part of the bus's identity: it fixes the
/// port numbering and is checked on restore.
pub struct AddressBus {
    regions: Vec<Region>,
    /// Forwarded requests awaiting a response, by `TxnId`.
    outstanding: BTreeMap<TxnId, Outstanding>,
}

/// The `TxnId` of a request and whether it is a write, or `None` for a response.
fn request_txn(msg: &MemMsg) -> Option<(TxnId, bool)> {
    match msg {
        MemMsg::ReadReq { txn, .. } => Some((*txn, false)),
        MemMsg::WriteReq { txn, .. } => Some((*txn, true)),
        MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => None,
    }
}

/// A request moved to `offset`. Responses carry no address and are returned unchanged.
fn at_offset(msg: &MemMsg, offset: u64) -> MemMsg {
    match msg.clone() {
        MemMsg::ReadReq { txn, len, .. } => MemMsg::ReadReq {
            txn,
            addr: offset,
            len,
        },
        MemMsg::WriteReq { txn, data, .. } => MemMsg::WriteReq {
            txn,
            addr: offset,
            data,
        },
        resp @ (MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. }) => resp,
    }
}

impl AddressBus {
    /// Creates an idle bus over `regions`, in the given order.
    ///
    /// Rejects a region of size 0, a region that runs past `u64::MAX`, overlapping
    /// regions, two regions with one name, a region named `cpu`, and more regions than a
    /// component has ports for.
    pub fn new(regions: Vec<Region>) -> Result<AddressBus, BusConfigError> {
        if regions.len() >= usize::from(u16::MAX) {
            return Err(BusConfigError::TooManyRegions);
        }
        for (i, region) in regions.iter().enumerate() {
            if region.size == 0 {
                return Err(BusConfigError::ZeroSize(region.name));
            }
            if region.base.checked_add(region.size - 1).is_none() {
                return Err(BusConfigError::Wraps(region.name));
            }
            if region.name == "cpu" {
                return Err(BusConfigError::ReservedName(region.name));
            }
            for earlier in &regions[..i] {
                if earlier.name == region.name {
                    return Err(BusConfigError::DuplicateName(region.name));
                }
                if earlier.base <= region.last() && region.base <= earlier.last() {
                    return Err(BusConfigError::Overlap(earlier.name, region.name));
                }
            }
        }
        Ok(AddressBus {
            regions,
            outstanding: BTreeMap::new(),
        })
    }

    /// The regions, in declaration order.
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// The index of the one region holding every byte of `first..=last`.
    fn route(&self, first: u64, last: u64) -> Option<u16> {
        let index = self
            .regions
            .iter()
            .position(|r| r.base <= first && first <= r.last())?;
        let index = u16::try_from(index).expect("region count checked by new");
        (last <= self.regions[usize::from(index)].last()).then_some(index)
    }

    fn request(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let (Some((txn, write)), Some(access)) = (request_txn(msg), msg.access()) else {
            return Err(SimError::ComponentFault(
                "address bus: response on the cpu port",
            ));
        };
        if ctx.phase() != Phase::Request {
            return Err(SimError::ComponentFault(
                "address bus: request arrived after REQUEST",
            ));
        }
        if self.outstanding.contains_key(&txn) {
            return Err(SimError::ComponentFault(
                "address bus: request reuses an outstanding txn",
            ));
        }
        let routed = match access {
            Access::Empty => {
                return Err(SimError::ComponentFault("address bus: zero-length request"));
            }
            Access::OutOfRange => None,
            Access::Bytes { first, last } => self.route(first, last).map(|i| (i, first)),
        };
        match routed {
            Some((region, first)) => {
                let offset = first - self.regions[usize::from(region)].base;
                self.outstanding.insert(txn, Outstanding { region, write });
                ctx.send(
                    region_port(region),
                    at_offset(msg, offset).into(),
                    ScheduleWhen::Now,
                    Phase::Transfer,
                )
            }
            None => self.fault(msg, ctx),
        }
    }

    /// Answers a request that no region contains with an access fault.
    fn fault(&mut self, msg: &MemMsg, ctx: &mut dyn SimContext) -> Result<(), SimError> {
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
                    "address bus: response on the cpu port",
                ));
            }
        };
        ctx.send(CPU_PORT, resp.into(), ScheduleWhen::Now, Phase::Complete)?;
        ctx.trace(
            FAULT_KIND,
            vec![
                ("txn", Value::U64(txn.0)),
                ("addr", Value::U64(addr)),
                ("len", Value::U64(len)),
            ],
        );
        Ok(())
    }

    fn response(
        &mut self,
        port: PortId,
        msg: &MemMsg,
        ctx: &mut dyn SimContext,
    ) -> Result<(), SimError> {
        let (txn, write) = match msg {
            MemMsg::ReadResp { txn, .. } => (*txn, false),
            MemMsg::WriteResp { txn, .. } => (*txn, true),
            MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. } => {
                return Err(SimError::ComponentFault(
                    "address bus: request on a region port",
                ));
            }
        };
        let region = port
            .0
            .checked_sub(1)
            .filter(|&i| usize::from(i) < self.regions.len())
            .ok_or(SimError::ComponentFault("address bus: unknown port"))?;
        let expected = self.outstanding.get(&txn).ok_or(SimError::ComponentFault(
            "address bus: response for unknown txn",
        ))?;
        if expected.region != region {
            return Err(SimError::ComponentFault(
                "address bus: response on the wrong region port",
            ));
        }
        if expected.write != write {
            return Err(SimError::ComponentFault(
                "address bus: response kind mismatch",
            ));
        }
        self.outstanding.remove(&txn);
        ctx.send(CPU_PORT, msg.clone().into(), ScheduleWhen::Now, ctx.phase())
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.len(self.regions.len());
        for region in &self.regions {
            w.str(region.name);
            w.u64(region.base);
            w.u64(region.size);
        }
    }
}

impl Component for AddressBus {
    fn type_name(&self) -> &'static str {
        "platform.bus"
    }

    fn ports(&self) -> Vec<PortSpec> {
        let port = |name, role| PortSpec {
            name,
            protocol: mem_v1::PROTOCOL,
            role,
        };
        std::iter::once(port("cpu", Role::Target))
            .chain(self.regions.iter().map(|r| port(r.name, Role::Initiator)))
            .collect()
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match ev {
            Delivered::Message {
                port,
                msg: Message::MemV1(msg),
            } => {
                if *port == CPU_PORT {
                    self.request(msg, ctx)
                } else {
                    self.response(*port, msg, ctx)
                }
            }
            // Sends are protocol-checked, so only mem.v1 arrives on these ports.
            Delivered::Message {
                msg: Message::Mem(_),
                ..
            } => Err(SimError::ComponentFault("address bus: mem.v0 message")),
            Delivered::Message {
                msg: Message::Irq(_) | Message::Block(_),
                ..
            } => Err(SimError::ComponentFault("address bus: non-mem message")),
            Delivered::Wake { .. } => Err(SimError::ComponentFault("address bus: unexpected wake")),
        }
    }

    /// The number of regions and of outstanding transactions.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("regions", Value::U64(self.regions.len() as u64)),
                ("outstanding", Value::U64(self.outstanding.len() as u64)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the regions in order (`name`, `base`, `size`), which also fix the port
    /// mapping; then the outstanding transactions by ascending `TxnId` (`txn`, region
    /// index, whether it is a write).
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.len(self.outstanding.len());
        for (txn, entry) in &self.outstanding {
            w.u64(txn.0);
            w.u16(entry.region);
            w.bool(entry.write);
        }
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        let different = RestoreError::InvalidState(
            "address bus: snapshot was taken with a different memory map",
        );
        if r.len()? != self.regions.len() {
            return Err(different);
        }
        for region in &self.regions {
            if r.str()? != region.name || r.u64()? != region.base || r.u64()? != region.size {
                return Err(different);
            }
        }
        let mut outstanding = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let txn = TxnId(r.u64()?);
            if previous.is_some_and(|p| txn <= p) {
                return Err(RestoreError::InvalidState(
                    "address bus: outstanding txns out of order or duplicated",
                ));
            }
            previous = Some(txn);
            let region = r.u16()?;
            if usize::from(region) >= self.regions.len() {
                return Err(RestoreError::InvalidState(
                    "address bus: outstanding txn on an unknown region",
                ));
            }
            let write = r.bool()?;
            outstanding.insert(txn, Outstanding { region, write });
        }
        self.outstanding = outstanding;
        Ok(())
    }
}
