//! `m2-reference`, the frozen M2 reference platform (`docs/m2-design.md` §11).
//!
//! ```text
//! soc.cpu0  Rv32iCpu, profile M2 (clock "cpu", 100 MHz)
//! soc.bus   MultiMasterBus, masters [cpu, dma0], clock cpu
//!   ├─ ram  ─▶ soc.ram   Ram                  base 0x8000_0000, size 16 MiB
//!   ├─ uart ─▶ soc.uart  SimpleUart           base 0x1000_0000, size 0x8
//!   ├─ irqc ─▶ soc.irqc  SimpleIrqController  base 0x1000_1000, size 0x8, 1 source
//!   └─ blk  ─▶ soc.blk   DmaBlockController   base 0x1000_2000, size 0x20
//! soc.disk  SimpleBlockMedia  capacity 16 blocks, latency Cycles { cpu, 16 }
//! ```
//!
//! - Components are declared in that order, so their ids are [`CPU`] to [`DISK`]; the
//!   CPU, bus, RAM, and UART keep their `m1-reference` ids.
//! - Links, in order: CPU `mem` ↔ bus `cpu`; bus `ram`/`uart`/`irqc`/`blk` ↔ each
//!   target's `mem`; `soc.blk` `dma` ↔ bus `dma0`; `soc.blk` `blk` ↔ `soc.disk` `blk`;
//!   `soc.blk` `irq` ↔ `soc.irqc` `src0`; `soc.irqc` `cpu` ↔ CPU `irq`. Every link is
//!   `Cycles { cpu, 1 }`; every target responds `Cycles { cpu, 0 }`.
//! - The DMA aperture is the RAM: `0x8000_0000`, 16 MiB.
//!
//! [`build`] is the production builder. It checks the two builder invariants of §11,
//! which belong to the platform and not to either component: the controller's capacity
//! equals the media's, and the DMA aperture lies wholly inside the RAM. Everything else
//! (overlapping windows, an image that does not fit the media, ...) is left to the
//! components, whose errors it passes on. It names the initial disk image by the BLAKE3
//! of its raw bytes ([`media_image`]), and by nothing else: no path, file name, or time.
//!
//! `m1-reference` ([`crate::runner`]) is a separate builder and stays unchanged.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU64;

use systemscope_contracts::canonical::{DecodeError, Decoder};
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::Observer;
use systemscope_contracts::time::{Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_elf::LoadImage;
use systemscope_platform::{
    BlockMediaConfig, BlockMediaConfigError, DmaBlockController, DmaBlockControllerConfig,
    DmaBlockControllerConfigError, IrqControllerConfig, IrqControllerConfigError, MediaImage,
    MultiMasterBus, MultiMasterBusConfig, MultiMasterBusConfigError, Ram, RamConfig,
    RamConfigError, RamImage, Region, Segment, SimpleBlockMedia, SimpleIrqController, SimpleUart,
    UartConfig, dma, irqc, uart,
};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

use crate::runner::{self, CPU_HZ, Finished, Start};
use crate::{MAX_INSTRUCTIONS, RAM_BASE, RAM_SIZE, UART_BASE};

/// `soc.cpu0`.
pub const CPU: ComponentId = ComponentId(0);
/// `soc.bus`.
pub const BUS: ComponentId = ComponentId(1);
/// `soc.ram`.
pub const RAM: ComponentId = ComponentId(2);
/// `soc.uart`.
pub const UART: ComponentId = ComponentId(3);
/// `soc.irqc`.
pub const IRQC: ComponentId = ComponentId(4);
/// `soc.blk`.
pub const BLK: ComponentId = ComponentId(5);
/// `soc.disk`.
pub const DISK: ComponentId = ComponentId(6);

/// The component paths, by id.
pub const PATHS: [&str; 7] = [
    "soc.cpu0", "soc.bus", "soc.ram", "soc.uart", "soc.irqc", "soc.blk", "soc.disk",
];

/// The bus masters, by master index: the CPU is master 0, the block controller master 1.
pub const MASTERS: [&str; 2] = ["cpu", "dma0"];
/// The bus regions, by region index: the order of decoding and of arbitration.
pub const REGIONS: [&str; 4] = ["ram", "uart", "irqc", "blk"];
/// The CPU's master index.
pub const MASTER_CPU: u64 = 0;
/// The block controller's master index.
pub const MASTER_DMA: u64 = 1;
/// The RAM's region index.
pub const REGION_RAM: u64 = 0;

/// The base of `soc.irqc`; its window is [`irqc::SIZE`] bytes.
pub const IRQC_BASE: u32 = 0x1000_1000;
/// The base of `soc.blk`; its window is [`dma::SIZE`] bytes.
pub const BLK_BASE: u32 = 0x1000_2000;
/// The IRQ controller's sources: source 0 is the block controller.
pub const IRQ_SOURCES: u8 = 1;
/// The capacity of `soc.disk`, and of `soc.blk`.
pub const DISK_BLOCKS: u64 = 16;
/// The media latency, in CPU cycles.
pub const DISK_LATENCY_CYCLES: u64 = 16;
/// The session seed.
pub const SEED: u64 = 0;
/// The CPU profile.
pub const PROFILE: Rv32iProfile = Rv32iProfile::M2;

/// What [`build`] can vary. [`Config::frozen`] is `m2-reference`; anything else exists
/// only so tests can check the builder rejects what it must.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The session seed.
    pub seed: u64,
    /// The RAM size; its base is [`RAM_BASE`].
    pub ram_size: u64,
    /// The UART base.
    pub uart_base: u64,
    /// The IRQ controller base.
    pub irqc_base: u64,
    /// The block controller base.
    pub blk_base: u64,
    /// The block controller's `capacity_blocks`.
    pub controller_blocks: u64,
    /// The media's `capacity_blocks`.
    pub media_blocks: u64,
    /// The first address of the DMA aperture.
    pub dma_base: u64,
    /// The size of the DMA aperture.
    pub dma_size: u64,
    /// LBAs the media fails; empty in `m2-reference`.
    pub bad_blocks: BTreeSet<u64>,
}

impl Config {
    /// `m2-reference` as §11 freezes it.
    pub fn frozen() -> Config {
        Config {
            seed: SEED,
            ram_size: u64::from(RAM_SIZE),
            uart_base: u64::from(UART_BASE),
            irqc_base: u64::from(IRQC_BASE),
            blk_base: u64::from(BLK_BASE),
            controller_blocks: DISK_BLOCKS,
            media_blocks: DISK_BLOCKS,
            dma_base: u64::from(RAM_BASE),
            dma_size: u64::from(RAM_SIZE),
            bad_blocks: BTreeSet::new(),
        }
    }
}

/// Why [`build`] refused a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// The controller validates commands against a capacity the media does not have.
    CapacityMismatch {
        /// The controller's `capacity_blocks`.
        controller: u64,
        /// The media's `capacity_blocks`.
        media: u64,
    },
    /// The DMA aperture is not wholly inside the RAM.
    ApertureOutsideRam {
        /// Its first address.
        base: u64,
        /// Its size.
        size: u64,
    },
    /// The RAM rejected its configuration or the program image.
    Ram(RamConfigError),
    /// The bus rejected its map.
    Bus(MultiMasterBusConfigError),
    /// The IRQ controller rejected its configuration.
    Irqc(IrqControllerConfigError),
    /// The block controller rejected its configuration.
    Controller(DmaBlockControllerConfigError),
    /// The media rejected its configuration or the disk image.
    Media(BlockMediaConfigError),
    /// The topology did not elaborate.
    Elaboration(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::CapacityMismatch { controller, media } => write!(
                f,
                "the block controller's capacity ({controller} blocks) differs from the \
                 media's ({media} blocks)"
            ),
            BuildError::ApertureOutsideRam { base, size } => write!(
                f,
                "the DMA aperture {base:#x} + {size:#x} is not wholly inside the RAM"
            ),
            BuildError::Ram(e) => write!(f, "soc.ram: {e}"),
            BuildError::Bus(e) => write!(f, "soc.bus: {e}"),
            BuildError::Irqc(e) => write!(f, "soc.irqc: {e}"),
            BuildError::Controller(e) => write!(f, "soc.blk: {e}"),
            BuildError::Media(e) => write!(f, "soc.disk: {e}"),
            BuildError::Elaboration(e) => write!(f, "elaboration: {e}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// The initial media image for raw disk bytes: the bytes, named by their BLAKE3.
pub fn media_image(bytes: &[u8]) -> MediaImage {
    MediaImage {
        image_hash: *blake3::hash(bytes).as_bytes(),
        bytes: bytes.to_vec(),
    }
}

/// `m2-reference` running `image` with the raw disk image `disk`, elaborated.
pub fn build(image: &LoadImage, disk: &[u8], config: &Config) -> Result<Runtime, BuildError> {
    // The builder invariants (§11), before anything is built.
    if config.controller_blocks != config.media_blocks {
        return Err(BuildError::CapacityMismatch {
            controller: config.controller_blocks,
            media: config.media_blocks,
        });
    }
    let ram_base = u64::from(RAM_BASE);
    let inside = config.dma_base >= ram_base
        && config
            .dma_base
            .checked_add(config.dma_size)
            .is_some_and(|end| end <= ram_base + config.ram_size);
    if !inside {
        return Err(BuildError::ApertureOutsideRam {
            base: config.dma_base,
            size: config.dma_size,
        });
    }

    let mut t = TopologyBuilder::new(SimulationClock::default());
    let cpu_clock = t
        .add_clock(
            Frequency::from_hz(CPU_HZ).expect("100 MHz is a valid frequency"),
            Tick::ZERO,
            Rounding::Floor,
        )
        .expect("100 MHz divides the simulation clock");
    let respond = LinkLatency::Cycles {
        domain: cpu_clock,
        k: 0,
    };
    let link = LinkLatency::Cycles {
        domain: cpu_clock,
        k: 1,
    };

    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock: cpu_clock,
        entry: image.entry,
        max_instructions: NonZeroU64::new(MAX_INSTRUCTIONS).expect("nonzero"),
        profile: PROFILE,
    })
    .expect("the loader guarantees an aligned entry");
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: MASTERS.to_vec(),
        regions: vec![
            Region {
                name: REGIONS[0],
                base: ram_base,
                size: config.ram_size,
            },
            Region {
                name: REGIONS[1],
                base: config.uart_base,
                size: uart::SIZE,
            },
            Region {
                name: REGIONS[2],
                base: config.irqc_base,
                size: irqc::SIZE,
            },
            Region {
                name: REGIONS[3],
                base: config.blk_base,
                size: dma::SIZE,
            },
        ],
        clock: cpu_clock,
    })
    .map_err(BuildError::Bus)?;
    let ram = Ram::new(
        RamConfig {
            size: config.ram_size,
            latency: respond,
        },
        &ram_image(image),
    )
    .map_err(BuildError::Ram)?;
    let device = SimpleUart::new(UartConfig { latency: respond });
    let irqc = SimpleIrqController::new(IrqControllerConfig {
        sources: IRQ_SOURCES,
        latency: respond,
    })
    .map_err(BuildError::Irqc)?;
    let blk = DmaBlockController::new(DmaBlockControllerConfig {
        clock: cpu_clock,
        latency: respond,
        capacity_blocks: config.controller_blocks,
        dma_base: config.dma_base,
        dma_size: config.dma_size,
    })
    .map_err(BuildError::Controller)?;
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: config.media_blocks,
            latency: LinkLatency::Cycles {
                domain: cpu_clock,
                k: DISK_LATENCY_CYCLES,
            },
            bad_blocks: config.bad_blocks.clone(),
        },
        &media_image(disk),
    )
    .map_err(BuildError::Media)?;

    let cpu = t.add_component(PATHS[0], Box::new(cpu));
    let bus = t.add_component(PATHS[1], Box::new(bus));
    let ram = t.add_component(PATHS[2], Box::new(ram));
    let device = t.add_component(PATHS[3], Box::new(device));
    let irqc = t.add_component(PATHS[4], Box::new(irqc));
    let blk = t.add_component(PATHS[5], Box::new(blk));
    let disk = t.add_component(PATHS[6], Box::new(disk));

    t.connect((cpu, "mem"), (bus, MASTERS[0]), Some(link));
    t.connect((bus, REGIONS[0]), (ram, "mem"), Some(link));
    t.connect((bus, REGIONS[1]), (device, "mem"), Some(link));
    t.connect((bus, REGIONS[2]), (irqc, "mem"), Some(link));
    t.connect((bus, REGIONS[3]), (blk, "mem"), Some(link));
    t.connect((blk, "dma"), (bus, MASTERS[1]), Some(link));
    t.connect((blk, "blk"), (disk, "blk"), Some(link));
    t.connect((blk, "irq"), (irqc, "src0"), Some(link));
    t.connect((irqc, "cpu"), (cpu, "irq"), Some(link));
    t.elaborate(SessionConfig {
        seed: config.seed,
        ..SessionConfig::default()
    })
    .map_err(|e| BuildError::Elaboration(format!("{e:?}")))
}

/// The load image as the RAM's initial image: the same offsets, bytes, and hash.
fn ram_image(image: &LoadImage) -> RamImage {
    RamImage {
        image_hash: image.image_hash,
        segments: image
            .segments
            .iter()
            .map(|s| Segment {
                offset: u64::from(s.offset),
                bytes: s.bytes.clone(),
            })
            .collect(),
    }
}

/// The most events [`execute`] runs after the start. `block_irq.elf` needs about 22,000
/// on `m2-reference`; a program that never finishes stops here instead of recording up
/// to [`MAX_INSTRUCTIONS`] retirements.
pub const EVENT_BUDGET: u64 = 200_000;

/// A session run to its end, with its final snapshot.
#[derive(Debug)]
pub struct M2Finished {
    /// What [`runner::execute`] reports.
    pub finished: Finished,
    /// The runtime snapshot after the last event; `None` if the session faulted.
    pub snapshot: Option<Vec<u8>>,
}

/// Starts the elaborated `rt` as `start` says, runs it to its end or for at most
/// [`EVENT_BUDGET`] events, and takes the final snapshot. `observers` are added after the
/// runner's own, which only reads.
pub fn execute(rt: Runtime, start: Start, observers: Vec<Box<dyn Observer>>) -> M2Finished {
    let (finished, rt) = runner::execute_bounded(rt, start, observers, Some(EVENT_BUDGET));
    M2Finished {
        snapshot: rt.snapshot().ok(),
        finished,
    }
}

/// One component's entry in a runtime snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentState {
    /// Its snapshot schema version.
    pub schema: u32,
    /// Its own bytes.
    pub bytes: Vec<u8>,
}

/// The component entries of a runtime snapshot, by id (`docs/m0-design.md` §7). The
/// rest of the snapshot is walked, not interpreted.
pub fn components(snapshot: &[u8]) -> Result<Vec<ComponentState>, DecodeError> {
    use systemscope_contracts::canonical::CanonicalEvent;
    use systemscope_contracts::event::EventKey;
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
        let id = d.u32()?;
        assert_eq!(id as usize, i, "component entries are by id");
        let schema = d.u32()?;
        out.push(ComponentState {
            schema,
            bytes: d.bytes()?.to_vec(),
        });
    }
    d.finish()?;
    Ok(out)
}

/// Skips a latency as the platform components write it.
fn skip_latency(d: &mut Decoder<'_>) -> Result<(), DecodeError> {
    match d.u8()? {
        0 => {
            d.u128()?;
        }
        _ => {
            d.u32()?;
            d.u64()?;
        }
    }
    Ok(())
}

/// The RAM contents, as `(page index, 4096 bytes)` for every non-zero page, from its
/// snapshot bytes (schema 1).
pub fn ram_pages(bytes: &[u8]) -> Result<Vec<(u32, Vec<u8>)>, DecodeError> {
    let mut d = Decoder::new(bytes);
    d.u64()?;
    d.raw(32)?;
    skip_latency(&mut d)?;
    let mut pages = Vec::new();
    for _ in 0..d.len()? {
        let index = d.u32()?;
        pages.push((index, d.bytes()?.to_vec()));
    }
    d.finish()?;
    Ok(pages)
}

/// `len` bytes of RAM at bus address `addr`, from the RAM's snapshot bytes.
pub fn ram_read(bytes: &[u8], addr: u32, len: usize) -> Result<Vec<u8>, DecodeError> {
    let pages = ram_pages(bytes)?;
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

/// The little-endian word of RAM at bus address `addr`, from the RAM's snapshot bytes.
pub fn ram_word(bytes: &[u8], addr: u32) -> Result<u32, DecodeError> {
    let b = ram_read(bytes, addr, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Stored blocks, as `(LBA, 512 bytes)`, by ascending LBA.
pub type Blocks = Vec<(u64, Vec<u8>)>;

/// The media contents, as `(LBA, 512 bytes)` for every non-zero block, and its image
/// hash, from its snapshot bytes (schema 1).
pub fn disk_blocks(bytes: &[u8]) -> Result<([u8; 32], Blocks), DecodeError> {
    let mut d = Decoder::new(bytes);
    d.u64()?; // capacity
    skip_latency(&mut d)?;
    for _ in 0..d.len()? {
        d.u64()?; // a bad block
    }
    let hash = d.array::<32>()?;
    let mut blocks = Vec::new();
    for _ in 0..d.len()? {
        let lba = d.u64()?;
        blocks.push((lba, d.bytes()?.to_vec()));
    }
    d.finish()?;
    Ok((hash, blocks))
}

/// Block `lba` of the media, from its snapshot bytes: all zero unless stored.
pub fn disk_block(bytes: &[u8], lba: u64) -> Result<Vec<u8>, DecodeError> {
    let (_, blocks) = disk_blocks(bytes)?;
    Ok(blocks
        .into_iter()
        .find(|(l, _)| *l == lba)
        .map_or_else(|| vec![0; 512], |(_, b)| b))
}
