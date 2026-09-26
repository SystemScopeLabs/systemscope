//! `m3-reference`, the M3 reference platform (`docs/m3-design.md` §11).
//!
//! ```text
//! soc.cpu0    Rv32iCpu, profile M3 (clock "cpu", 100 MHz), entry = firmware entry
//! soc.bus     MultiMasterBus, masters [cpu, dma0, kernel0], clock cpu
//!   ├─ ram    ─▶ soc.ram     Ram                  base 0x8000_0000, size 16 MiB
//!   ├─ uart   ─▶ soc.uart    SimpleUart           base 0x1000_0000, size 0x8
//!   ├─ irqc   ─▶ soc.irqc    SimpleIrqController  base 0x1000_1000, size 0x8, 1 source
//!   ├─ blk    ─▶ soc.blk     DmaBlockController   base 0x1000_2000, size 0x20
//!   └─ kgate  ─▶ soc.kernel  ModeledKernel (gate) base 0x1000_3000, size 0x8
//! soc.disk    SimpleBlockMedia  capacity 256 blocks, latency Cycles { cpu, 16 }
//! soc.kernel  ModeledKernel, mem ─▶ bus kernel0, booting from soc.disk
//! ```
//!
//! - Components are declared in that order, so their ids are [`CPU`] to [`KERNEL`]; the
//!   first seven keep their `m2-reference` ids.
//! - Links, in order: the `m2-reference` links in `m2-reference` order, then bus `kgate` ↔
//!   `soc.kernel` `gate`, then `soc.kernel` `mem` ↔ bus `kernel0`. Every link is
//!   `Cycles { cpu, 1 }`; every target responds `Cycles { cpu, 0 }` after acceptance,
//!   except the held `ENTER` (§6.3).
//! - The DMA aperture is the RAM. The kernel's physical layout is §11.2: the trap frame
//!   at `0x8001_0000`, staging at `0x8010_0000` (1 MiB), and the frame pool
//!   `0x8040_0000`–`0x80FF_FFFF`. The user layout is [`UserLayout::M3`].
//! - The kernel boots from the disk ([`ModeledKernel::with_disk`]): it reads the `SSX0`
//!   table at LBA 0 and every executable it names through `soc.blk` into staging, by the
//!   M2 controller's own registers and DMA (§8.2). Nothing is placed in RAM for it: the
//!   RAM's initial image is only `m3-firmware.elf`, placed by the host-side loader as a
//!   boot ROM would be (§7.1).
//!
//! [`build`] is the production builder. It checks the platform's builder invariants,
//! which belong to no single component: the controller's capacity equals the media's and
//! the kernel's boot capacity; the DMA aperture lies wholly inside the RAM; and the
//! kernel's `kgate` window is the bus's and lies outside every range the kernel grants
//! and outside the DMA aperture (§16 risk 2). Everything else is left to the components,
//! whose errors it passes on. It names the disk by the BLAKE3 of its raw bytes, and by
//! nothing else.
//!
//! [`storage_image`] is the deterministic storage builder: the executables in table
//! order, packed from LBA 1, behind their table at LBA 0, zero-filled to the capacity.
//!
//! `m1-reference` and `m2-reference` ([`crate::runner`], [`crate::m2ref`]) are separate
//! builders and stay unchanged.

use std::collections::BTreeSet;
use std::fmt;
use std::num::NonZeroU64;

use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_elf::{BLOCK_SIZE, ExecEntry, ExecTable, LoadImage, parse_exec_table};
use systemscope_os::{DiskBoot, KernelConfig, ModeledKernel, PlanError, UserLayout, Window};
use systemscope_platform::{
    BlockMediaConfig, BlockMediaConfigError, DmaBlockController, DmaBlockControllerConfig,
    DmaBlockControllerConfigError, IrqControllerConfig, IrqControllerConfigError, MultiMasterBus,
    MultiMasterBusConfig, MultiMasterBusConfigError, Ram, RamConfig, RamConfigError, RamImage,
    Region, Segment, SimpleBlockMedia, SimpleIrqController, SimpleUart, UartConfig, dma, irqc,
    uart,
};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

use systemscope_contracts::component::ComponentId;

use crate::m2ref::media_image;
use crate::runner::CPU_HZ;
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
/// `soc.kernel`.
pub const KERNEL: ComponentId = ComponentId(7);

/// The component paths, by id.
pub const PATHS: [&str; 8] = [
    "soc.cpu0",
    "soc.bus",
    "soc.ram",
    "soc.uart",
    "soc.irqc",
    "soc.blk",
    "soc.disk",
    "soc.kernel",
];

/// The bus masters, by master index.
pub const MASTERS: [&str; 3] = ["cpu", "dma0", "kernel0"];
/// The bus regions, by region index: the order of decoding and of arbitration.
pub const REGIONS: [&str; 5] = ["ram", "uart", "irqc", "blk", "kgate"];
/// The CPU's master index.
pub const MASTER_CPU: u64 = 0;
/// The block controller's master index.
pub const MASTER_DMA: u64 = 1;
/// The kernel's master index.
pub const MASTER_KERNEL: u64 = 2;
/// The RAM's region index.
pub const REGION_RAM: u64 = 0;
/// `kgate`'s region index.
pub const REGION_KGATE: u64 = 4;

/// The base of `soc.irqc`; its window is [`irqc::SIZE`] bytes.
pub const IRQC_BASE: u32 = 0x1000_1000;
/// The base of `soc.blk`; its window is [`dma::SIZE`] bytes.
pub const BLK_BASE: u32 = 0x1000_2000;
/// The base of `kgate`, the kernel's gate window.
pub const KGATE_BASE: u32 = 0x1000_3000;
/// The size of `kgate`.
pub const KGATE_SIZE: u64 = 0x8;
/// The IRQ controller's sources: source 0 is the block controller.
pub const IRQ_SOURCES: u8 = 1;
/// The capacity of `soc.disk`, of `soc.blk`, and of the kernel's boot disk.
pub const DISK_BLOCKS: u64 = 256;
/// The media latency, in CPU cycles.
pub const DISK_LATENCY_CYCLES: u64 = 16;
/// The trap frame (§11.2).
pub const TRAP_FRAME: u32 = 0x8001_0000;
/// The size of the trap frame's fields (§7.2).
pub const TRAP_FRAME_BYTES: u64 = 0x98;
/// Staging, the DMA target for executables (§11.2).
pub const STAGING: u32 = 0x8010_0000;
/// The size of staging: 1 MiB.
pub const STAGING_SIZE: u64 = 1 << 20;
/// The frame pool (§11.2).
pub const POOL: u32 = 0x8040_0000;
/// The size of the frame pool: `0x8040_0000`–`0x80FF_FFFF`, 3072 frames.
pub const POOL_SIZE: u64 = 0x00C0_0000;
/// The number of frames in the pool.
pub const POOL_FRAMES: u64 = POOL_SIZE / 4096;
/// The session seed.
pub const SEED: u64 = 0;
/// The CPU profile.
pub const PROFILE: Rv32iProfile = Rv32iProfile::M3;
/// The SBI `SRST` extension id, `a7` of the shutdown call (§7.4).
pub const SRST: u32 = 0x5352_5354;

/// What [`build`] can vary. [`Config::frozen`] is `m3-reference`; anything else exists
/// only so tests can check the builder rejects what it must, or run a failing disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The session seed.
    pub seed: u64,
    /// The block controller's `capacity_blocks`.
    pub controller_blocks: u64,
    /// The media's `capacity_blocks`.
    pub media_blocks: u64,
    /// The first address of the DMA aperture.
    pub dma_base: u64,
    /// The size of the DMA aperture.
    pub dma_size: u64,
    /// LBAs the media fails; empty in `m3-reference`.
    pub bad_blocks: BTreeSet<u64>,
    /// The kernel's configuration; [`build`] replaces its clock with the platform's.
    pub kernel: KernelConfig,
    /// The kernel's disk boot.
    pub boot: DiskBoot,
}

impl Config {
    /// `m3-reference` as §11 freezes it.
    pub fn frozen() -> Config {
        Config {
            seed: SEED,
            controller_blocks: DISK_BLOCKS,
            media_blocks: DISK_BLOCKS,
            dma_base: u64::from(RAM_BASE),
            dma_size: u64::from(RAM_SIZE),
            bad_blocks: BTreeSet::new(),
            kernel: kernel_config(ClockDomainId(0)),
            boot: DiskBoot {
                layout: UserLayout::M3,
                capacity_blocks: DISK_BLOCKS as u32,
            },
        }
    }
}

/// The kernel's configuration on `m3-reference` (§6.2, §11.2), with `clock` as its clock
/// and its refusals answered after 0 cycles.
pub fn kernel_config(clock: ClockDomainId) -> KernelConfig {
    KernelConfig {
        clock,
        latency: LinkLatency::Cycles {
            domain: clock,
            k: 0,
        },
        ram: Window {
            base: u64::from(RAM_BASE),
            size: u64::from(RAM_SIZE),
        },
        gate: Window {
            base: u64::from(KGATE_BASE),
            size: KGATE_SIZE,
        },
        trap_frame: TRAP_FRAME,
        staging: Window {
            base: u64::from(STAGING),
            size: STAGING_SIZE,
        },
        frame_pool: Window {
            base: u64::from(POOL),
            size: POOL_SIZE,
        },
        blk: Window {
            base: u64::from(BLK_BASE),
            size: dma::SIZE,
        },
        uart_tx: u64::from(UART_BASE),
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
    /// The kernel checks the executable table against a capacity the media does not have.
    BootCapacityMismatch {
        /// The kernel's boot capacity.
        kernel: u32,
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
    /// The kernel's `kgate` window is not the bus's.
    GateMismatch(Window),
    /// A range the kernel grants overlaps `kgate`.
    GateGranted(Window),
    /// The DMA aperture overlaps `kgate`.
    GateInAperture,
    /// The kernel's block controller window is not the bus's.
    ControllerMismatch(Window),
    /// The kernel rejected its configuration or disk boot.
    Kernel(PlanError),
    /// The RAM rejected its configuration or the firmware image.
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
            BuildError::BootCapacityMismatch { kernel, media } => write!(
                f,
                "the kernel's boot capacity ({kernel} blocks) differs from the media's \
                 ({media} blocks)"
            ),
            BuildError::ApertureOutsideRam { base, size } => write!(
                f,
                "the DMA aperture {base:#x} + {size:#x} is not wholly inside the RAM"
            ),
            BuildError::GateMismatch(w) => write!(
                f,
                "the kernel's gate {:#x} + {:#x} is not the bus's kgate",
                w.base, w.size
            ),
            BuildError::GateGranted(w) => write!(
                f,
                "the kernel grants {:#x} + {:#x}, which overlaps kgate",
                w.base, w.size
            ),
            BuildError::GateInAperture => write!(f, "the DMA aperture overlaps kgate"),
            BuildError::ControllerMismatch(w) => write!(
                f,
                "the kernel's block controller {:#x} + {:#x} is not the bus's blk",
                w.base, w.size
            ),
            BuildError::Kernel(e) => write!(f, "soc.kernel: {e}"),
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

/// The platform's builder invariants, before anything is built.
fn check(config: &Config) -> Result<(), BuildError> {
    if config.controller_blocks != config.media_blocks {
        return Err(BuildError::CapacityMismatch {
            controller: config.controller_blocks,
            media: config.media_blocks,
        });
    }
    if u64::from(config.boot.capacity_blocks) != config.media_blocks {
        return Err(BuildError::BootCapacityMismatch {
            kernel: config.boot.capacity_blocks,
            media: config.media_blocks,
        });
    }
    let ram_base = u64::from(RAM_BASE);
    let inside = config.dma_base >= ram_base
        && config
            .dma_base
            .checked_add(config.dma_size)
            .is_some_and(|end| end <= ram_base + u64::from(RAM_SIZE));
    if !inside {
        return Err(BuildError::ApertureOutsideRam {
            base: config.dma_base,
            size: config.dma_size,
        });
    }
    let gate = Window {
        base: u64::from(KGATE_BASE),
        size: KGATE_SIZE,
    };
    if config.kernel.gate != gate {
        return Err(BuildError::GateMismatch(config.kernel.gate));
    }
    if let Some(g) = config
        .kernel
        .grants()
        .into_iter()
        .find(|g| gate.overlaps(g.base, g.size))
    {
        return Err(BuildError::GateGranted(g));
    }
    if gate.overlaps(config.dma_base, config.dma_size) {
        return Err(BuildError::GateInAperture);
    }
    let blk = Window {
        base: u64::from(BLK_BASE),
        size: dma::SIZE,
    };
    if config.kernel.blk != blk {
        return Err(BuildError::ControllerMismatch(config.kernel.blk));
    }
    Ok(())
}

/// `m3-reference` with `firmware` in RAM and the raw disk image `disk`, elaborated.
pub fn build(firmware: &LoadImage, disk: &[u8], config: &Config) -> Result<Runtime, BuildError> {
    check(config)?;

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

    let kernel = ModeledKernel::with_disk(
        KernelConfig {
            clock: cpu_clock,
            latency: respond,
            ..config.kernel
        },
        config.boot,
    )
    .map_err(BuildError::Kernel)?;
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock: cpu_clock,
        entry: firmware.entry,
        max_instructions: NonZeroU64::new(MAX_INSTRUCTIONS).expect("nonzero"),
        profile: PROFILE,
    })
    .expect("the loader guarantees an aligned entry");
    let region = |name, base: u32, size| Region {
        name,
        base: u64::from(base),
        size,
    };
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: MASTERS.to_vec(),
        regions: vec![
            region(REGIONS[0], RAM_BASE, u64::from(RAM_SIZE)),
            region(REGIONS[1], UART_BASE, uart::SIZE),
            region(REGIONS[2], IRQC_BASE, irqc::SIZE),
            region(REGIONS[3], BLK_BASE, dma::SIZE),
            region(REGIONS[4], KGATE_BASE, KGATE_SIZE),
        ],
        clock: cpu_clock,
    })
    .map_err(BuildError::Bus)?;
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: respond,
        },
        &ram_image(firmware),
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
    let kernel = t.add_component(PATHS[7], Box::new(kernel));

    t.connect((cpu, "mem"), (bus, MASTERS[0]), Some(link));
    t.connect((bus, REGIONS[0]), (ram, "mem"), Some(link));
    t.connect((bus, REGIONS[1]), (device, "mem"), Some(link));
    t.connect((bus, REGIONS[2]), (irqc, "mem"), Some(link));
    t.connect((bus, REGIONS[3]), (blk, "mem"), Some(link));
    t.connect((blk, "dma"), (bus, MASTERS[1]), Some(link));
    t.connect((blk, "blk"), (disk, "blk"), Some(link));
    t.connect((blk, "irq"), (irqc, "src0"), Some(link));
    t.connect((irqc, "cpu"), (cpu, "irq"), Some(link));
    t.connect((bus, REGIONS[4]), (kernel, "gate"), Some(link));
    t.connect((kernel, "mem"), (bus, MASTERS[2]), Some(link));
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

/// The storage image of `files`, in table order: the `SSX0` table at LBA 0, then each
/// file from the next free block on, zero-padded to a whole block, and zeros up to
/// `capacity_blocks`. The image is a function of the file bytes and the capacity only.
/// It is checked with the kernel's own table rules for a disk of `capacity_blocks` and a
/// staging area of `staging_size` bytes, so a disk that builds is one the kernel accepts.
pub fn storage_image(
    files: &[&[u8]],
    capacity_blocks: u32,
    staging_size: u32,
) -> Result<Vec<u8>, String> {
    let mut entries = Vec::with_capacity(files.len());
    let mut next = 1u32;
    for (i, f) in files.iter().enumerate() {
        let byte_len =
            u32::try_from(f.len()).map_err(|_| format!("file {i} is larger than 4 GiB"))?;
        let entry = ExecEntry {
            start_lba: next,
            byte_len,
        };
        next = next
            .checked_add(entry.blocks())
            .ok_or_else(|| format!("file {i} does not fit"))?;
        entries.push(entry);
    }
    let table = ExecTable { entries };
    let block0 = table.encode().map_err(|e| e.to_string())?;
    let parsed = parse_exec_table(&block0, capacity_blocks, staging_size)
        .map_err(|e| format!("the table is invalid: {e}"))?;
    assert_eq!(parsed, table, "the table decodes to itself");
    let mut disk = vec![0u8; capacity_blocks as usize * BLOCK_SIZE];
    disk[..BLOCK_SIZE].copy_from_slice(&block0);
    for (f, e) in files.iter().zip(&table.entries) {
        let at = e.start_lba as usize * BLOCK_SIZE;
        disk[at..at + f.len()].copy_from_slice(f);
    }
    Ok(disk)
}
