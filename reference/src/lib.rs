//! The M0 reference scenario `m0-reference` (`docs/m0-design.md` §9.1).
//!
//! ```text
//! ToyCpu (cpu, 3 GHz)   ──┐
//!                         ├─▶ ToyBus (bus, 1 GHz) ──▶ ToyMemory (read 50 ns, write 30 ns)
//! ToyDma (io, 1.5 GHz)  ──┘
//! ```
//!
//! [`build`] declares everything in the order the design fixes, so every caller gets the
//! same `topology_hash`. Only the seed and the operation counts vary.

use std::num::NonZeroU64;

use systemscope_contracts::component::ComponentId;
use systemscope_contracts::time::{Duration, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_runtime::runtime::{Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_toy::{
    ToyBus, ToyBusConfig, ToyCpu, ToyCpuConfig, ToyDma, ToyDmaConfig, ToyMemory, ToyMemoryConfig,
};

/// `soc.cpu0`.
pub const CPU: ComponentId = ComponentId(0);
/// `soc.dma0`.
pub const DMA: ComponentId = ComponentId(1);
/// `soc.bus`.
pub const BUS: ComponentId = ComponentId(2);
/// `soc.mem`.
pub const MEM: ComponentId = ComponentId(3);

/// CPU region: slots of [`CPU_ACCESS`] bytes from address 0.
pub const CPU_SLOTS: u32 = 64;
/// Bytes per CPU access.
pub const CPU_ACCESS: u32 = 8;
/// DMA region: slots of [`DMA_ACCESS`] bytes from [`DMA_BASE`].
pub const DMA_SLOTS: u32 = 64;
/// Bytes per DMA write.
pub const DMA_ACCESS: u32 = 16;
/// First address of the DMA region, right after the CPU region.
pub const DMA_BASE: u64 = CPU_SLOTS as u64 * CPU_ACCESS as u64;
/// Memory size: both regions and nothing else.
pub const MEMORY_SIZE: u32 = CPU_SLOTS * CPU_ACCESS + DMA_SLOTS * DMA_ACCESS;

/// Operations per initiator in the full CI scenario.
pub const FULL_OPS: u64 = 100_000;

/// Simulated time after which the run stops even if work remains.
pub const T_END: Duration = Duration::from_ms(10);

/// What varies between runs of the scenario.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceConfig {
    /// Session seed.
    pub seed: u64,
    /// Operations the CPU issues.
    pub cpu_ops: u64,
    /// Writes the DMA issues.
    pub dma_ops: u64,
}

impl ReferenceConfig {
    /// The full CI scenario for `seed`.
    pub const fn full(seed: u64) -> ReferenceConfig {
        ReferenceConfig {
            seed,
            cpu_ops: FULL_OPS,
            dma_ops: FULL_OPS,
        }
    }
}

/// Builds the scenario, elaborated but not initialized.
pub fn build(config: ReferenceConfig) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let domain = |t: &mut TopologyBuilder, num, den| {
        let f = Frequency::new(num, den).expect("valid frequency");
        t.add_clock(f, Tick::ZERO, Rounding::Floor)
            .expect("representable at the default resolution")
    };
    let cpu_clock = domain(&mut t, 3_000_000_000, 1);
    let io_clock = domain(&mut t, 3_000_000_000, 2);
    let bus_clock = domain(&mut t, 1_000_000_000, 1);

    let cpu = t.add_component(
        "soc.cpu0",
        Box::new(ToyCpu::new(ToyCpuConfig {
            clock: cpu_clock,
            ops: config.cpu_ops,
            max_outstanding: 4,
            max_think_cycles: NonZeroU64::new(8).expect("non-zero"),
            access_len: CPU_ACCESS,
            slots: CPU_SLOTS,
            write_percent: 40,
        })),
    );
    let dma = t.add_component(
        "soc.dma0",
        Box::new(ToyDma::new(ToyDmaConfig {
            clock: io_clock,
            ops: config.dma_ops,
            max_outstanding: 8,
            max_burst: NonZeroU64::new(8).expect("non-zero"),
            max_gap_cycles: NonZeroU64::new(128).expect("non-zero"),
            base: DMA_BASE,
            access_len: DMA_ACCESS,
            slots: DMA_SLOTS,
        })),
    );
    let bus = t.add_component(
        "soc.bus",
        Box::new(ToyBus::new(ToyBusConfig { clock: bus_clock })),
    );
    let mem = t.add_component(
        "soc.mem",
        Box::new(ToyMemory::new(ToyMemoryConfig {
            size: MEMORY_SIZE,
            read_latency: Duration::from_ns(50),
            write_latency: Duration::from_ns(30),
        })),
    );
    assert_eq!([cpu, dma, bus, mem], [CPU, DMA, BUS, MEM]);

    let link = Some(LinkLatency::Cycles {
        domain: bus_clock,
        k: 1,
    });
    t.connect((cpu, "mem"), (bus, "cpu"), link);
    t.connect((dma, "mem"), (bus, "dma"), link);
    t.connect((bus, "mem"), (mem, "mem"), link);
    let session = SessionConfig {
        seed: config.seed,
        ..SessionConfig::default()
    };
    t.elaborate(session)
        .expect("the reference topology is valid")
}

/// Runs until the queue is empty or [`T_END`] has passed. Returns how many events ran.
pub fn run(rt: &mut Runtime) -> Result<u64, RuntimeError> {
    let end = SimulationClock::default()
        .after(Tick::ZERO, T_END)
        .expect("T_END fits the default resolution");
    rt.run_until(end)
}
