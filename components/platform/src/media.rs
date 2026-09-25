//! `SimpleBlockMedia`: a sparse `block.v0` disk (`docs/m2-design.md` §8.2).
//!
//! # Ordering semantics
//!
//! The same as the [`Ram`](crate::Ram)'s:
//!
//! | Point | When |
//! |---|---|
//! | request accepted | when the request event is dispatched, in `Request` |
//! | write visible | at acceptance: every event dispatched afterwards sees it |
//! | read sampled | at acceptance |
//! | result emitted | acceptance + the configured latency, in `Complete` |
//!
//! A pending result is an event in the runtime's queue, carrying the data sampled at
//! acceptance, so the media keeps no copy and has no busy state.
//!
//! # Errors and faults
//!
//! A request for an LBA at or past `capacity_blocks` gets `Error { OutOfRange }`, and one
//! for an LBA in `bad_blocks` gets `Error { BadBlock }`. Neither reads nor writes
//! anything. A `WriteBlock` whose data is not exactly [`BLOCK_SIZE`] bytes is not an
//! access at all: it faults the session with [`SimError::ComponentFault`], sends nothing,
//! and changes nothing, as does a result arriving on the target port, a message that is
//! not `block.v0`, or a request outside `Request`.
//!
//! # Canonical storage
//!
//! The media is **one** map of 512-byte blocks by LBA. A block that is not in the map
//! reads as zero, and **no block in the map is all zero**: a write of zeros removes the
//! entry, and a write of zeros to an absent block allocates nothing. The initial image is
//! copied into the map at construction, and from then on the map is the whole state:
//! there is no fallback to the image, so a zeroed image block reads as zero. The same
//! contents therefore always have the same map, and the same snapshot bytes, whatever
//! history produced them.
//!
//! # Identity
//!
//! `image_hash` is the BLAKE3 of the raw initial image bytes as given. The builder that
//! holds the image computes it and passes it in [`MediaImage`], as the loader does for
//! [`RamImage`](crate::RamImage): components depend only on `systemscope-contracts`
//! (`docs/m1-design.md` §3), which has no hash function. It is part of the configuration,
//! so a snapshot restores only into a media with the same hash.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::StateView;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::{
    self, BLOCK_SIZE, BlockMsg, BlockReadOutcome, BlockWriteOutcome, MediaError,
};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;

/// The media's only port: a `block.v0` target named `blk`.
pub const PORT: PortId = PortId(0);

/// Layout of [`SimpleBlockMedia`]'s snapshot.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Trace kind of an accepted `ReadBlock`.
pub const READ_KIND: &str = "platform.disk.read";

/// Trace kind of an accepted `WriteBlock`.
pub const WRITE_KIND: &str = "platform.disk.write";

type Block = Box<[u8; BLOCK_SIZE]>;

/// Capacity, timing, and failing blocks of a [`SimpleBlockMedia`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockMediaConfig {
    /// Number of 512-byte blocks, at least 1. LBAs outside `0..capacity_blocks` answer
    /// `Error { OutOfRange }`.
    pub capacity_blocks: u64,
    /// Delay from accepting a request to sending its result, in the form of a link
    /// latency: a physical duration or a number of cycles of a clock domain.
    pub latency: LinkLatency,
    /// LBAs that answer `Error { BadBlock }`.
    pub bad_blocks: BTreeSet<u64>,
}

/// What a new session's media holds before the first event: a raw disk image.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MediaImage {
    /// The BLAKE3 of `bytes`, exactly as given. It is part of the media's configuration:
    /// a snapshot restores only into a media with the same hash.
    pub image_hash: [u8; 32],
    /// Block 0 onwards: a whole number of blocks, at most `capacity_blocks`. Blocks past
    /// its end are zero.
    pub bytes: Vec<u8>,
}

/// Why a [`SimpleBlockMedia`] cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockMediaConfigError {
    /// `capacity_blocks` is 0.
    ZeroCapacity,
    /// The image is this many bytes, which is not a whole number of blocks.
    PartialBlock(usize),
    /// The image has this many blocks, more than `capacity_blocks`.
    ImageTooLarge(u64),
}

impl fmt::Display for BlockMediaConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockMediaConfigError::ZeroCapacity => f.write_str("media capacity is 0 blocks"),
            BlockMediaConfigError::PartialBlock(len) => {
                write!(
                    f,
                    "media image of {len} bytes is not a whole number of blocks"
                )
            }
            BlockMediaConfigError::ImageTooLarge(blocks) => {
                write!(f, "media image of {blocks} blocks is larger than the media")
            }
        }
    }
}

impl std::error::Error for BlockMediaConfigError {}

/// A sparse block device that serves `block.v0` requests for LBAs `[0, capacity_blocks)`.
pub struct SimpleBlockMedia {
    config: BlockMediaConfig,
    image_hash: [u8; 32],
    /// Blocks holding at least one non-zero byte, by LBA.
    blocks: BTreeMap<u64, Block>,
}

impl SimpleBlockMedia {
    /// Creates a media holding `image`.
    ///
    /// Rejects a capacity of 0, an image that is not a whole number of blocks, and an
    /// image with more blocks than the capacity. The image's non-zero blocks go into the
    /// block map; it is not kept.
    pub fn new(
        config: BlockMediaConfig,
        image: &MediaImage,
    ) -> Result<SimpleBlockMedia, BlockMediaConfigError> {
        if config.capacity_blocks == 0 {
            return Err(BlockMediaConfigError::ZeroCapacity);
        }
        if !image.bytes.len().is_multiple_of(BLOCK_SIZE) {
            return Err(BlockMediaConfigError::PartialBlock(image.bytes.len()));
        }
        let image_blocks = (image.bytes.len() / BLOCK_SIZE) as u64;
        if image_blocks > config.capacity_blocks {
            return Err(BlockMediaConfigError::ImageTooLarge(image_blocks));
        }
        let mut media = SimpleBlockMedia {
            config,
            image_hash: image.image_hash,
            blocks: BTreeMap::new(),
        };
        let (chunks, _) = image.bytes.as_chunks::<BLOCK_SIZE>();
        for (lba, data) in (0..).zip(chunks) {
            media.store(lba, data);
        }
        Ok(media)
    }

    /// Why `lba` cannot be accessed, if it cannot.
    fn check(&self, lba: u64) -> Option<MediaError> {
        if lba >= self.config.capacity_blocks {
            Some(MediaError::OutOfRange)
        } else if self.config.bad_blocks.contains(&lba) {
            Some(MediaError::BadBlock)
        } else {
            None
        }
    }

    /// The block at `lba`: the map's entry, or zeros.
    fn load(&self, lba: u64) -> Vec<u8> {
        match self.blocks.get(&lba) {
            Some(block) => block.to_vec(),
            None => vec![0; BLOCK_SIZE],
        }
    }

    /// Replaces the block at `lba`, keeping the map free of all-zero blocks.
    fn store(&mut self, lba: u64, data: &[u8; BLOCK_SIZE]) {
        if data.iter().all(|&b| b == 0) {
            self.blocks.remove(&lba);
        } else {
            self.blocks.insert(lba, Box::new(*data));
        }
    }

    fn write_config(&self, w: &mut SnapshotWriter) {
        w.u64(self.config.capacity_blocks);
        self.write_latency(w);
        w.len(self.config.bad_blocks.len());
        for lba in &self.config.bad_blocks {
            w.u64(*lba);
        }
        w.raw(&self.image_hash);
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

/// The trace value of a result's outcome.
fn outcome_name(error: Option<MediaError>) -> Value {
    Value::Str(
        match error {
            None => "ok",
            Some(MediaError::OutOfRange) => "out_of_range",
            Some(MediaError::BadBlock) => "bad_block",
        }
        .to_string(),
    )
}

impl Component for SimpleBlockMedia {
    fn type_name(&self) -> &'static str {
        "platform.disk"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "blk",
            protocol: block_v0::PROTOCOL,
            role: Role::Target,
        }]
    }

    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }

    /// Accepts a request: samples a read or applies a write now, traces it, and sends the
    /// result after the configured latency, in `Complete`.
    ///
    /// Every fault is detected before anything is read, written, sent, or traced.
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            port: PORT,
            msg: Message::Block(msg),
        } = ev
        else {
            return Err(SimError::ComponentFault("disk: unexpected delivery"));
        };
        if matches!(
            msg,
            BlockMsg::ReadResult { .. } | BlockMsg::WriteResult { .. }
        ) {
            return Err(SimError::ComponentFault("disk: result on the target port"));
        }
        if ctx.phase() != Phase::Request {
            return Err(SimError::ComponentFault(
                "disk: request outside the Request phase",
            ));
        }
        let (kind, lba, error, result) = match msg {
            BlockMsg::ReadBlock { txn, lba } => {
                let error = self.check(*lba);
                let outcome = match error {
                    None => BlockReadOutcome::Data {
                        data: self.load(*lba),
                    },
                    Some(error) => BlockReadOutcome::Error { error },
                };
                (
                    READ_KIND,
                    *lba,
                    error,
                    BlockMsg::ReadResult { txn: *txn, outcome },
                )
            }
            BlockMsg::WriteBlock { txn, lba, data } => {
                let data: &[u8; BLOCK_SIZE] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| SimError::ComponentFault("disk: write data is not 512 bytes"))?;
                let error = self.check(*lba);
                let outcome = match error {
                    None => {
                        self.store(*lba, data);
                        BlockWriteOutcome::Done
                    }
                    Some(error) => BlockWriteOutcome::Error { error },
                };
                (
                    WRITE_KIND,
                    *lba,
                    error,
                    BlockMsg::WriteResult { txn: *txn, outcome },
                )
            }
            BlockMsg::ReadResult { .. } | BlockMsg::WriteResult { .. } => {
                unreachable!("results were rejected above")
            }
        };
        ctx.trace(
            kind,
            vec![("lba", Value::U64(lba)), ("outcome", outcome_name(error))],
        );
        let when = match self.config.latency {
            LinkLatency::After(d) => ScheduleWhen::After(d),
            LinkLatency::Cycles { domain, k } => ScheduleWhen::Cycles { domain, k },
        };
        ctx.send(PORT, result.into(), when, Phase::Complete)
    }

    /// The capacity, the image hash, and the number of stored blocks; never the contents.
    fn inspect(&self) -> StateView {
        StateView {
            fields: vec![
                ("capacity_blocks", Value::U64(self.config.capacity_blocks)),
                ("image_hash", Value::Bytes(self.image_hash.to_vec())),
                ("stored_blocks", Value::U64(self.blocks.len() as u64)),
            ],
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA
    }

    /// Schema 1: the configuration (`capacity_blocks`, latency, `bad_blocks` ascending,
    /// `image_hash`), then every stored block, by ascending LBA: the LBA, then its 512
    /// bytes, length-prefixed.
    ///
    /// The map holds no all-zero block, so the bytes depend only on the contents.
    fn snapshot(&self, w: &mut SnapshotWriter) {
        self.write_config(w);
        w.len(self.blocks.len());
        for (lba, block) in &self.blocks {
            w.u64(*lba);
            w.bytes(&block[..]);
        }
    }

    /// Replaces the whole block map with the snapshot's.
    ///
    /// The initial image is never re-applied: a block the snapshot omits is all zero,
    /// never "as in the initial image". Only the image's hash is compared.
    ///
    /// Rejects a different configuration, including `image_hash`, and any non-canonical
    /// block list: LBAs not strictly ascending, an LBA past the end, a block that is not
    /// 512 bytes, or an all-zero block. Nothing changes unless the whole snapshot is
    /// accepted.
    fn restore(&mut self, r: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        if r.u64()? != self.config.capacity_blocks {
            return Err(RestoreError::InvalidState(
                "disk: snapshot has a different capacity",
            ));
        }
        let mut latency = SnapshotWriter::new();
        self.write_latency(&mut latency);
        if r.raw(latency.as_bytes().len())? != latency.as_bytes() {
            return Err(RestoreError::InvalidState(
                "disk: snapshot has a different latency",
            ));
        }
        // Compared in the encoded, ascending order: a reordered or duplicated list is a
        // different configuration too.
        let count = r.len()?;
        let mut bad_blocks = Vec::new();
        for _ in 0..count.min(self.config.bad_blocks.len() + 1) {
            bad_blocks.push(r.u64()?);
        }
        if count != self.config.bad_blocks.len()
            || !bad_blocks.iter().eq(self.config.bad_blocks.iter())
        {
            return Err(RestoreError::InvalidState(
                "disk: snapshot has different bad blocks",
            ));
        }
        if r.array::<32>()? != self.image_hash {
            return Err(RestoreError::InvalidState(
                "disk: snapshot has a different image hash",
            ));
        }
        let mut blocks = BTreeMap::new();
        let mut previous = None;
        for _ in 0..r.len()? {
            let lba = r.u64()?;
            if previous.is_some_and(|p| lba <= p) {
                return Err(RestoreError::InvalidState(
                    "disk: blocks out of order or duplicated",
                ));
            }
            previous = Some(lba);
            if lba >= self.config.capacity_blocks {
                return Err(RestoreError::InvalidState("disk: block past the end"));
            }
            let bytes: [u8; BLOCK_SIZE] = r
                .bytes()?
                .try_into()
                .map_err(|_| RestoreError::InvalidState("disk: block is not 512 bytes"))?;
            if bytes.iter().all(|&b| b == 0) {
                return Err(RestoreError::InvalidState("disk: all-zero block"));
            }
            blocks.insert(lba, Box::new(bytes));
        }
        self.blocks = blocks;
        Ok(())
    }
}
