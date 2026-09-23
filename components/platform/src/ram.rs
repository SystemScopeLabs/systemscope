//! `Ram`: a sparse, byte-addressable `mem.v1` memory (`docs/m1-design.md` §7.2).
//!
//! # Addresses are offsets
//!
//! The RAM does not know where it is mapped. Every address it receives is an offset from
//! its own start, as [`AddressBus`](crate::AddressBus) forwards it, and it serves
//! `[0, size)`.
//!
//! # Ordering semantics
//!
//! The same as M0's `ToyMemory`:
//!
//! | Point | When |
//! |---|---|
//! | request accepted | when the request event is dispatched |
//! | write visible | at acceptance: every event dispatched afterwards sees it |
//! | read sampled | at acceptance |
//! | response emitted | acceptance + the configured latency, in `Complete` |
//!
//! When a response arrives has no effect on visibility. A pending response is an event
//! in the runtime's queue, carrying the data sampled at acceptance, so the runtime's
//! snapshot holds it and the RAM keeps no copy.
//!
//! # Faults
//!
//! A well-formed request that reaches past `size` gets a [`MemFault::AccessFault`]
//! response and changes nothing. The bus never sends one, but the RAM does not rely on
//! that. A zero-length request is a protocol violation, not an access, and faults the
//! session with [`SimError::ComponentFault`].
//!
//! # Canonical storage
//!
//! Memory is a map of 4096-byte pages, indexed by `offset / 4096`. A page that is not in
//! the map reads as zero, and **no page in the map is all zero**: a write that zeroes a
//! page's last non-zero byte removes the page, and a write of zeros to an absent page
//! allocates nothing. The same contents therefore always have the same pages, and the
//! same snapshot bytes, whatever history produced them.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{
    self, Access, MemFault, MemMsg, ReadOutcome, WriteOutcome,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// The RAM's only port: a `mem.v1` target named `mem`.
pub const PORT: PortId = PortId(0);

/// Bytes per page.
pub const PAGE_SIZE: usize = 4096;

/// The largest RAM: the 32-bit address space.
pub const MAX_SIZE: u64 = 1 << 32;

/// Layout of [`Ram`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

const PAGE: u64 = PAGE_SIZE as u64;

type Page = Box<[u8; PAGE_SIZE]>;

/// Size and timing of a [`Ram`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RamConfig {
    /// Capacity in bytes, in `1..=MAX_SIZE`. Offsets outside `0..size` fault.
    pub size: u64,
    /// Delay from accepting a request to sending its response, in the form of a link
    /// latency: a physical duration or a number of cycles of a clock domain.
    pub latency: LinkLatency,
}

/// Bytes placed at an offset of the initial image.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Segment {
    /// Offset of the first byte from the start of the RAM.
    pub offset: u64,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// What a new session's RAM holds before the first event: a program image.
///
/// Offsets are RAM-relative. The ELF loader produces one from a program's loadable
/// segments, after subtracting the RAM region's base (`docs/m1-design.md` §8).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RamImage {
    /// Identifies the program, such as the BLAKE3 hash of its ELF file. It is part of the
    /// RAM's configuration: a snapshot restores only into a RAM with the same hash.
    pub image_hash: [u8; 32],
    /// The bytes to place; they must not overlap. Unlisted bytes are zero.
    pub segments: Vec<Segment>,
}

/// Why a [`Ram`] cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamConfigError {
    /// The size is 0.
    ZeroSize,
    /// The size is larger than [`MAX_SIZE`].
    TooLarge(u64),
    /// The segment at this index reaches past the end of the RAM.
    SegmentOutOfRange(usize),
    /// The segments at these indices share at least one byte.
    SegmentsOverlap(usize, usize),
}

impl fmt::Display for RamConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RamConfigError::ZeroSize => f.write_str("RAM size is 0"),
            RamConfigError::TooLarge(size) => write!(f, "RAM size {size} is too large"),
            RamConfigError::SegmentOutOfRange(i) => {
                write!(f, "image segment {i} reaches past the end of the RAM")
            }
            RamConfigError::SegmentsOverlap(a, b) => {
                write!(f, "image segments {a} and {b} overlap")
            }
        }
    }
}

impl std::error::Error for RamConfigError {}

/// A sparse RAM that serves `mem.v1` requests at offsets `[0, size)`.
pub struct Ram {
    config: RamConfig,
    image_hash: [u8; 32],
    /// Pages holding at least one non-zero byte, by page index.
    pages: BTreeMap<u32, Page>,
}

impl Ram {
    /// Creates a RAM holding `image`.
    ///
    /// Rejects a size of 0 or above [`MAX_SIZE`], a segment that reaches past `size`, and
    /// overlapping segments. Empty segments are allowed and place nothing.
    pub fn new(config: RamConfig, image: &RamImage) -> Result<Ram, RamConfigError> {
        if config.size == 0 {
            return Err(RamConfigError::ZeroSize);
        }
        if config.size > MAX_SIZE {
            return Err(RamConfigError::TooLarge(config.size));
        }
        // Half-open spans, in bounds, of the non-empty segments.
        let mut spans = Vec::new();
        for (i, segment) in image.segments.iter().enumerate() {
            let end = u64::try_from(segment.bytes.len())
                .ok()
                .and_then(|len| segment.offset.checked_add(len))
                .filter(|&end| end <= config.size)
                .ok_or(RamConfigError::SegmentOutOfRange(i))?;
            if end > segment.offset {
                spans.push((segment.offset, end, i));
            }
        }
        spans.sort_unstable();
        for pair in spans.windows(2) {
            let ((_, end, a), (start, _, b)) = (pair[0], pair[1]);
            if start < end {
                return Err(RamConfigError::SegmentsOverlap(a.min(b), a.max(b)));
            }
        }
        let mut ram = Ram {
            config,
            image_hash: image.image_hash,
            pages: BTreeMap::new(),
        };
        for segment in &image.segments {
            ram.store(segment.offset, &segment.bytes);
        }
        Ok(ram)
    }

    /// The bytes `offset..offset + len`, which the caller has checked are in range.
    fn load(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut data = vec![0; len];
        let mut done = 0;
        while done < len {
            let at = offset + done as u64;
            let start = (at % PAGE) as usize;
            let n = (PAGE_SIZE - start).min(len - done);
            if let Some(page) = self.pages.get(&page_index(at)) {
                data[done..done + n].copy_from_slice(&page[start..start + n]);
            }
            done += n;
        }
        data
    }

    /// Writes `data` at `offset`, which the caller has checked is in range, keeping the
    /// map free of all-zero pages.
    fn store(&mut self, offset: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let start = (at % PAGE) as usize;
            let n = (PAGE_SIZE - start).min(data.len() - done);
            let chunk = &data[done..done + n];
            match self.pages.entry(page_index(at)) {
                Entry::Occupied(mut page) => {
                    page.get_mut()[start..start + n].copy_from_slice(chunk);
                    if page.get().iter().all(|&b| b == 0) {
                        page.remove();
                    }
                }
                Entry::Vacant(slot) => {
                    if chunk.iter().any(|&b| b != 0) {
                        let mut page: Page = Box::new([0; PAGE_SIZE]);
                        page[start..start + n].copy_from_slice(chunk);
                        slot.insert(page);
                    }
                }
            }
            done += n;
        }
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u64(self.config.size);
        w.raw(&self.image_hash);
        self.write_latency(w);
    }

    fn write_latency(&self, w: &mut SnapshotWriter) {
        match self.config.latency {
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
    }
}

/// The index of the page holding `offset`, which is below [`MAX_SIZE`].
fn page_index(offset: u64) -> u32 {
    u32::try_from(offset / PAGE).expect("offsets are below MAX_SIZE")
}

impl Component for Ram {
    fn type_name(&self) -> &'static str {
        "platform.ram"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem_v1::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    /// Accepts a request: samples a read or applies a write now, and sends the response
    /// after the configured latency, in `Complete`.
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } = ev
        else {
            return Err(SimError::ComponentFault("ram: unexpected delivery"));
        };
        let in_range = match msg.access() {
            None => {
                return Err(SimError::ComponentFault("ram: response on the target port"));
            }
            Some(Access::Empty) => {
                return Err(SimError::ComponentFault("ram: zero-length request"));
            }
            Some(Access::OutOfRange) => false,
            Some(Access::Bytes { last, .. }) => last < self.config.size,
        };
        let fault = MemFault::AccessFault;
        let resp = match msg {
            MemMsg::ReadReq { txn, addr, len } => MemMsg::ReadResp {
                txn: *txn,
                outcome: if in_range {
                    ReadOutcome::Data {
                        data: self.load(*addr, *len as usize),
                    }
                } else {
                    ReadOutcome::Fault { fault }
                },
            },
            MemMsg::WriteReq { txn, addr, data } => MemMsg::WriteResp {
                txn: *txn,
                outcome: if in_range {
                    self.store(*addr, data);
                    WriteOutcome::Done
                } else {
                    WriteOutcome::Fault { fault }
                },
            },
            MemMsg::ReadResp { .. } | MemMsg::WriteResp { .. } => {
                return Err(SimError::ComponentFault("ram: response on the target port"));
            }
        };
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(PORT, resp.into(), when, Phase::Complete)
    }

    /// The size, the image hash, and the number of non-zero pages; never the contents.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("size", Value::U64(self.config.size)),
                ("image_hash", Value::Bytes(self.image_hash.to_vec())),
                ("non_zero_pages", Value::U64(self.pages.len() as u64)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (`size`, `image_hash`, latency), then every page with a
    /// non-zero byte, by ascending index: the index, then its 4096 bytes, length-prefixed.
    ///
    /// All-zero pages are never written, so the bytes depend only on the contents.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.len(self.pages.len());
        for (index, page) in &self.pages {
            w.u32(*index);
            w.bytes(&page[..]);
        }
    }

    /// Replaces the whole memory with the snapshot's.
    ///
    /// The RAM is cleared first, including every page the initial image placed at
    /// construction, and then only the snapshot's pages are inserted: a page the snapshot
    /// omits is all zero, never "as in the initial image". The image itself is not read;
    /// only its hash is compared.
    ///
    /// Rejects a different size, image hash, or latency, and any non-canonical page list:
    /// indices not strictly ascending, a page past the end of the RAM, a page that is not
    /// 4096 bytes, an all-zero page, or non-zero bytes past `size` in the last page.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        if r.u64()? != self.config.size {
            return Err(RestoreError::InvalidState(
                "ram: snapshot has a different size",
            ));
        }
        if r.array::<32>()? != self.image_hash {
            return Err(RestoreError::InvalidState(
                "ram: snapshot has a different image hash",
            ));
        }
        let mut latency = SnapshotWriter::new();
        self.write_latency(&mut latency);
        if r.raw(latency.as_bytes().len())? != latency.as_bytes() {
            return Err(RestoreError::InvalidState(
                "ram: snapshot has a different latency",
            ));
        }
        let mut pages = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let index = r.u32()?;
            if previous.is_some_and(|p| index <= p) {
                return Err(RestoreError::InvalidState(
                    "ram: pages out of order or duplicated",
                ));
            }
            previous = Some(index);
            let start = u64::from(index) * PAGE;
            if start >= self.config.size {
                return Err(RestoreError::InvalidState("ram: page past the end"));
            }
            let bytes: [u8; PAGE_SIZE] = r
                .bytes()?
                .try_into()
                .map_err(|_| RestoreError::InvalidState("ram: page is not 4096 bytes"))?;
            if bytes.iter().all(|&b| b == 0) {
                return Err(RestoreError::InvalidState("ram: all-zero page"));
            }
            let valid =
                usize::try_from((self.config.size - start).min(PAGE)).expect("at most one page");
            if bytes[valid..].iter().any(|&b| b != 0) {
                return Err(RestoreError::InvalidState(
                    "ram: non-zero bytes past the end",
                ));
            }
            pages.insert(index, Box::new(bytes));
        }
        self.pages = pages;
        Ok(())
    }
}
