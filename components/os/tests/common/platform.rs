//! The M3.4a minimal platform and its guest fixtures (`docs/m3-design.md` §17, M3.4a).
//!
//! ```text
//! soc.cpu0    Rv32iCpu, profile M3 (clock "cpu", 100 MHz), entry 0x8000_0000
//! soc.bus     MultiMasterBus, masters [cpu, dma0, kernel0], clock cpu
//!   ├─ ram    ─▶ soc.ram     Ram                  base 0x8000_0000, size 16 MiB
//!   ├─ uart   ─▶ soc.uart    SimpleUart           base 0x1000_0000, size 0x8
//!   ├─ irqc   ─▶ soc.irqc    SimpleIrqController  base 0x1000_1000, size 0x8, 1 source
//!   ├─ blk    ─▶ soc.blk     DmaBlockController   base 0x1000_2000, size 0x20
//!   └─ kgate  ─▶ soc.kernel  ModeledKernel (gate) base 0x1000_3000, size 0x8
//! soc.disk    SimpleBlockMedia  capacity 16 blocks, latency Cycles { cpu, 16 }
//! soc.kernel  ModeledKernel, mem ─▶ bus kernel0
//! ```
//!
//! This is the §11.1 topology with a small disk and a test program instead of the M3
//! firmware and disk fixture, which are later steps. Links follow §11.1: the M2 links in
//! M2 order, then bus `kgate` ↔ kernel `gate`, then kernel `mem` ↔ bus `kernel0`, all
//! `Cycles { cpu, 1 }`; targets respond after `Cycles { cpu, 0 }`.
//!
//! [`build`] checks the builder invariants of §16 risk 2 that belong to the platform: the
//! kernel's `kgate` window is the bus's, and it lies outside every range the kernel grants
//! and outside the DMA aperture. [`elaborate`] skips them, so tests can show what the
//! kernel's own whitelist does when a platform is built wrong.
//!
//! The guest fixtures are machine code written directly from the ISA manual's formats:
//! an M-mode stub (§7.1, with `medeleg = 0xB1FF`), S-mode code that enters the kernel
//! with one store to `kgate.ENTER`, and a trampoline (§7.3) for U-mode `ecall`s.

use std::num::NonZeroU64;

use systemscope_contracts::time::{Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_os::{KernelConfig, KernelConfigError, ModeledKernel, Window};
use systemscope_platform::{
    BlockMediaConfig, DmaBlockController, DmaBlockControllerConfig, IrqControllerConfig,
    MediaImage, MultiMasterBus, MultiMasterBusConfig, Ram, RamConfig, RamImage, Region, Segment,
    SimpleBlockMedia, SimpleIrqController, SimpleUart, UartConfig, dma, irqc, uart,
};
use systemscope_runtime::runtime::{Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::csr::{MEPC, MSTATUS};
use systemscope_rv32i::privilege::{
    MEDELEG, SATP, SCAUSE, SEPC, SRET, SSCRATCH, SSTATUS, STVAL, STVEC,
};
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

use super::layout::*;

/// Component ids, in declaration order.
pub mod id {
    use systemscope_contracts::component::ComponentId;
    pub const CPU: ComponentId = ComponentId(0);
    pub const BUS: ComponentId = ComponentId(1);
    pub const RAM: ComponentId = ComponentId(2);
    pub const UART: ComponentId = ComponentId(3);
    pub const IRQC: ComponentId = ComponentId(4);
    pub const BLK: ComponentId = ComponentId(5);
    pub const DISK: ComponentId = ComponentId(6);
    pub const KERNEL: ComponentId = ComponentId(7);
}

/// The bus masters, by index.
pub const MASTERS: [&str; 3] = ["cpu", "dma0", "kernel0"];
/// The bus regions, by index.
pub const REGIONS: [&str; 5] = ["ram", "uart", "irqc", "blk", "kgate"];
/// The RAM's and `kgate`'s region indices.
pub const REGION_RAM: u64 = 0;
pub const REGION_KGATE: u64 = 4;
/// Master indices.
pub const MASTER_CPU: u64 = 0;
pub const MASTER_DMA: u64 = 1;
pub const MASTER_KERNEL: u64 = 2;

pub const CPU_HZ: u64 = 100_000_000;
pub const DISK_BLOCKS: u64 = 16;
pub const MAX_INSTRUCTIONS: u64 = 100_000;

/// The M-mode stub, the S-mode code, the trampoline, and U code.
pub const STUB: u32 = 0x8000_0000;
pub const S_START: u32 = 0x8000_1000;
pub const TRAMPOLINE: u32 = 0x8000_2000;
pub const USER: u32 = 0x8000_3000;

/// The SBI `SRST` extension id (§7.4).
pub const SRST: u32 = 0x5352_5354;

/// A program: bytes at physical addresses.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub segments: Vec<(u64, Vec<u8>)>,
}

impl Program {
    fn code(&mut self, at: u32, words: &[u32]) {
        let bytes = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.segments.push((u64::from(at), bytes));
    }
}

/// Why [`build`] refused a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// The kernel's `kgate` window is not the bus's.
    GateMismatch,
    /// A range the kernel grants overlaps `kgate`.
    GateGranted(Window),
    /// The DMA aperture overlaps `kgate`.
    GateInAperture,
    /// The kernel rejected its configuration.
    Kernel(KernelConfigError),
}

/// The disk: block 0 and 1 hold `i mod 251` at byte `i`, the rest zeros.
pub fn disk() -> Vec<u8> {
    let mut bytes = vec![0; DISK_BLOCKS as usize * 512];
    for (i, b) in bytes.iter_mut().take(1024).enumerate() {
        *b = (i % 251) as u8;
    }
    bytes
}

/// The platform running `program` with the kernel `config`, after the builder checks.
pub fn build(program: &Program, config: KernelConfig) -> Result<Runtime, BuildError> {
    let gate = Window {
        base: KGATE_BASE,
        size: KGATE_SIZE,
    };
    if config.gate != gate {
        return Err(BuildError::GateMismatch);
    }
    if let Some(g) = config
        .grants()
        .into_iter()
        .find(|g| gate.overlaps(g.base, g.size))
    {
        return Err(BuildError::GateGranted(g));
    }
    // The DMA aperture is the RAM.
    if gate.overlaps(RAM_BASE, RAM_SIZE) {
        return Err(BuildError::GateInAperture);
    }
    elaborate(program, config)
}

/// The platform running `program` with the kernel `config`, without the builder checks.
pub fn elaborate(program: &Program, config: KernelConfig) -> Result<Runtime, BuildError> {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(CPU_HZ).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let respond = LinkLatency::Cycles {
        domain: clock,
        k: 0,
    };
    let link = LinkLatency::Cycles {
        domain: clock,
        k: 1,
    };
    let config = KernelConfig { clock, ..config };
    let kernel = ModeledKernel::new(config).map_err(BuildError::Kernel)?;
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock,
        entry: STUB,
        max_instructions: NonZeroU64::new(MAX_INSTRUCTIONS).unwrap(),
        profile: Rv32iProfile::M3,
    })
    .unwrap();
    let region = |name, base, size| Region { name, base, size };
    let bus = MultiMasterBus::new(MultiMasterBusConfig {
        masters: MASTERS.to_vec(),
        regions: vec![
            region(REGIONS[0], RAM_BASE, RAM_SIZE),
            region(REGIONS[1], UART_BASE, uart::SIZE),
            region(REGIONS[2], IRQC_BASE, irqc::SIZE),
            region(REGIONS[3], BLK_BASE, dma::SIZE),
            region(REGIONS[4], KGATE_BASE, KGATE_SIZE),
        ],
        clock,
    })
    .unwrap();
    let ram = Ram::new(
        RamConfig {
            size: RAM_SIZE,
            latency: respond,
        },
        &RamImage {
            image_hash: [0x34; 32],
            segments: program
                .segments
                .iter()
                .map(|(addr, bytes)| Segment {
                    offset: addr - RAM_BASE,
                    bytes: bytes.clone(),
                })
                .collect(),
        },
    )
    .unwrap();
    let device = SimpleUart::new(UartConfig { latency: respond });
    let irqc = SimpleIrqController::new(IrqControllerConfig {
        sources: 1,
        latency: respond,
    })
    .unwrap();
    let blk = DmaBlockController::new(DmaBlockControllerConfig {
        clock,
        latency: respond,
        capacity_blocks: DISK_BLOCKS,
        dma_base: RAM_BASE,
        dma_size: RAM_SIZE,
    })
    .unwrap();
    let disk = SimpleBlockMedia::new(
        BlockMediaConfig {
            capacity_blocks: DISK_BLOCKS,
            latency: LinkLatency::Cycles {
                domain: clock,
                k: 16,
            },
            bad_blocks: Default::default(),
        },
        &MediaImage {
            image_hash: [0x44; 32],
            bytes: disk(),
        },
    )
    .unwrap();

    let cpu = t.add_component("soc.cpu0", Box::new(cpu));
    let bus = t.add_component("soc.bus", Box::new(bus));
    let ram = t.add_component("soc.ram", Box::new(ram));
    let device = t.add_component("soc.uart", Box::new(device));
    let irqc = t.add_component("soc.irqc", Box::new(irqc));
    let blk = t.add_component("soc.blk", Box::new(blk));
    let disk = t.add_component("soc.disk", Box::new(disk));
    let kernel = t.add_component("soc.kernel", Box::new(kernel));
    assert_eq!((cpu, kernel), (id::CPU, id::KERNEL));

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
    Ok(t.elaborate(SessionConfig::default()).unwrap())
}

/// RV32I and Zicsr encodings (ISA manual, "Base Instruction Formats").
pub mod asm {
    pub const T0: u32 = 5;
    pub const T1: u32 = 6;
    pub const T2: u32 = 7;
    pub const S0: u32 = 8;
    pub const S1: u32 = 9;
    pub const A0: u32 = 10;
    pub const A1: u32 = 11;
    pub const A2: u32 = 12;
    pub const A3: u32 = 13;
    pub const A6: u32 = 16;
    pub const A7: u32 = 17;
    pub const T3: u32 = 28;
    pub const T4: u32 = 29;
    pub const T5: u32 = 30;
    pub const T6: u32 = 31;

    pub const ECALL: u32 = 0x0000_0073;
    pub const EBREAK: u32 = 0x0010_0073;
    pub const MRET: u32 = 0x3020_0073;
    pub const SFENCE_VMA: u32 = 0x1200_0073;

    fn i(imm: i32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
        ((imm as u32) & 0xfff) << 20 | rs1 << 15 | funct3 << 12 | rd << 7 | opcode
    }

    fn s(imm: i32, rs2: u32, rs1: u32, funct3: u32) -> u32 {
        let imm = imm as u32;
        (imm >> 5 & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | funct3 << 12 | (imm & 0x1f) << 7 | 0x23
    }

    fn b(imm: i32, rs2: u32, rs1: u32, funct3: u32) -> u32 {
        let imm = imm as u32;
        (imm >> 12 & 1) << 31
            | (imm >> 5 & 0x3f) << 25
            | rs2 << 20
            | rs1 << 15
            | funct3 << 12
            | (imm >> 1 & 0xf) << 8
            | (imm >> 11 & 1) << 7
            | 0x63
    }

    pub fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 0, rd, 0x13)
    }

    pub fn andi(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 7, rd, 0x13)
    }

    pub fn lw(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 2, rd, 0x03)
    }

    pub fn sw(rs2: u32, rs1: u32, imm: i32) -> u32 {
        s(imm, rs2, rs1, 2)
    }

    pub fn sb(rs2: u32, rs1: u32, imm: i32) -> u32 {
        s(imm, rs2, rs1, 0)
    }

    pub fn beq(rs1: u32, rs2: u32, offset: i32) -> u32 {
        b(offset, rs2, rs1, 0)
    }

    pub fn bne(rs1: u32, rs2: u32, offset: i32) -> u32 {
        b(offset, rs2, rs1, 1)
    }

    /// `csrrw rd, csr, rs1`.
    pub fn csrrw(rd: u32, csr: u16, rs1: u32) -> u32 {
        i(i32::from(csr), rs1, 1, rd, 0x73)
    }

    /// `csrrs rd, csr, rs1`.
    pub fn csrrs(rd: u32, csr: u16, rs1: u32) -> u32 {
        i(i32::from(csr), rs1, 2, rd, 0x73)
    }

    /// `csrrc rd, csr, rs1`.
    pub fn csrrc(rd: u32, csr: u16, rs1: u32) -> u32 {
        i(i32::from(csr), rs1, 3, rd, 0x73)
    }

    /// `lui rd, hi` then `addi rd, rd, lo`: always two words.
    pub fn li(rd: u32, value: u32) -> [u32; 2] {
        let hi = value.wrapping_add(0x800) & 0xffff_f000;
        let lo = value.wrapping_sub(hi) as i32;
        [hi | rd << 7 | 0x37, addi(rd, rd, lo)]
    }
}

use asm::*;

/// `medeleg` as the §7.1 boot stub sets it.
pub const DELEGATE: u32 = 0xB1FF;

/// The M-mode stub: `medeleg`, `stvec` at the trampoline, `sscratch` at the frame,
/// `MPP = S`, `mepc = S_START`, `MRET`.
pub fn stub() -> Vec<u32> {
    let mut w = Vec::new();
    for (csr, value) in [
        (MEDELEG, DELEGATE),
        (STVEC, TRAMPOLINE),
        (SSCRATCH, TRAP_FRAME),
        (MSTATUS, 1 << 11),
        (MEPC, S_START),
    ] {
        w.extend(li(T0, value));
        w.push(csrrw(0, csr, T0));
    }
    w.push(MRET);
    w
}

/// The SBI `SRST` shutdown with `a1` loaded from `reason_at(t6-relative)` or 0.
fn srst(w: &mut Vec<u32>, reason: u32) {
    w.extend(li(A7, SRST));
    w.push(addi(A6, 0, 0));
    w.push(addi(A0, 0, 0));
    w.push(reason);
    w.push(ECALL);
}

/// The trampoline (§7.3), plus one line of test glue: after the kernel returns, a trap
/// whose saved `scause` is 3 (a breakpoint) ends the run with the `SRST` shutdown, as a
/// kernel-chosen `Shutdown` would. The kernel's scripted operation never chooses one.
pub fn trampoline() -> Vec<u32> {
    let assemble = |shutdown: i32| {
        let mut w = vec![csrrw(T6, SSCRATCH, T6)];
        for r in 1..=30 {
            w.push(sw(r, T6, 4 * (r as i32 - 1)));
        }
        w.push(csrrs(T5, SSCRATCH, 0));
        w.push(sw(T5, T6, 0x78));
        for (csr, off) in [(SEPC, 0x7C), (SSTATUS, 0x80), (SCAUSE, 0x84), (STVAL, 0x88)] {
            w.push(csrrs(T5, csr, 0));
            w.push(sw(T5, T6, off));
        }
        w.extend(li(T5, KGATE_BASE as u32));
        w.push(sw(T6, T5, 0)); // ENTER: held until the kernel finishes
        let at = |w: &Vec<u32>| 4 * w.len() as i32;
        w.push(lw(T5, T6, 0x90));
        w.push(bne(T5, 0, shutdown - at(&w)));
        w.push(lw(T5, T6, 0x84));
        w.push(addi(T4, 0, 3));
        w.push(beq(T5, T4, shutdown - at(&w)));
        w.push(lw(T5, T6, 0x8C));
        w.push(csrrw(0, SATP, T5));
        w.push(SFENCE_VMA);
        w.push(lw(T5, T6, 0x7C));
        w.push(csrrw(0, SEPC, T5));
        w.push(lw(T5, T6, 0x80));
        w.push(csrrw(0, SSTATUS, T5));
        w.push(csrrw(0, SSCRATCH, T6));
        for r in 1..=30 {
            w.push(lw(r, T6, 4 * (r as i32 - 1)));
        }
        w.push(lw(T6, T6, 0x78));
        w.push(SRET);
        let here = at(&w);
        srst(&mut w, lw(A1, T6, 0x94));
        (w, here)
    };
    let (_, shutdown) = assemble(0);
    let (w, again) = assemble(shutdown);
    assert_eq!(again, shutdown);
    w
}

/// The initial trap frame of the S-mode probes: word `i` is `0xF000_0000 | 0x111 × i`,
/// `sepc` is [`PROBE_SEPC`], `action` and `reason` are 0.
pub fn probe_frame() -> Vec<u32> {
    let mut words: Vec<u32> = (0..38u32).map(|i| 0xF000_0000 | (0x111 * i)).collect();
    words[31] = PROBE_SEPC;
    words[36] = 0;
    words[37] = 0;
    words
}

/// The `sepc` word of [`probe_frame`].
pub const PROBE_SEPC: u32 = 0x8000_1234;

/// The address of the probe's `ENTER` store and of the instruction after it.
pub fn probe_enter_pc(dma: bool) -> u32 {
    S_START + 4 * (if dma { 10 } else { 0 } + 4)
}

/// The S-mode probe (the M3.4a integration scenario): from S, with `satp` bare, store
/// `value` to `kgate.ENTER`, then `addi a2, x0, 0x55` (the next guest instruction), load
/// the frame's `action` into `a3` and `reason` into `a1`, and end with the `SRST`
/// shutdown. With `dma`, it first starts a two-block DMA READ into staging, so the
/// DMA engine and the kernel contend for the RAM, and polls for `DONE` after the entry.
pub fn probe(value: u32, dma: bool) -> Program {
    let mut s = Vec::new();
    if dma {
        s.extend(li(T3, BLK_BASE as u32));
        s.extend(li(T1, STAGING as u32));
        s.push(sw(T1, T3, dma::MEM_ADDR as i32));
        s.push(addi(T1, 0, 2));
        s.push(sw(T1, T3, dma::BLOCK_COUNT as i32));
        s.push(sw(0, T3, dma::LBA as i32));
        s.push(addi(T1, 0, 1));
        s.push(sw(T1, T3, dma::COMMAND as i32));
    }
    s.extend(li(T0, value));
    s.extend(li(T1, KGATE_BASE as u32));
    assert_eq!(S_START + 4 * s.len() as u32, probe_enter_pc(dma));
    s.push(sw(T0, T1, 0));
    s.push(addi(A2, 0, 0x55));
    if dma {
        s.push(lw(T2, T3, dma::STATUS as i32));
        s.push(andi(T2, T2, 2));
        s.push(beq(T2, 0, -8));
    }
    s.extend(li(T4, TRAP_FRAME));
    s.push(lw(A3, T4, 0x90));
    srst(&mut s, lw(A1, T4, 0x94));
    let mut p = Program::default();
    p.code(STUB, &stub());
    p.code(S_START, &s);
    p.code(TRAMPOLINE, &trampoline());
    p.code(TRAP_FRAME, &probe_frame());
    p
}

/// The value U code loads into `x<r>` before its loop.
pub fn user_value(r: u32) -> u32 {
    r << 24 | r << 12 | (r * 3)
}

/// The address of the U loop's `ebreak`.
pub fn user_ebreak_pc() -> u32 {
    USER + 4 * (2 * 29 + 1 + 2 + 3)
}

/// The U-mode `ecall` loop (the M3.4a exit scenario): S code enters U at [`USER`], which
/// loads [`user_value`] into every register but `s0` and `s1`, then makes `ecalls`
/// `ecall`s counting in `s0` up to `s1`, and ends with `ebreak`. Every trap goes through
/// the trampoline and `kgate.ENTER`, and the kernel's scripted operation returns past it.
pub fn ecall_loop(ecalls: u32) -> Program {
    let mut s = Vec::new();
    s.extend(li(T0, USER));
    s.push(csrrw(0, SEPC, T0));
    s.extend(li(T0, 1 << 8));
    s.push(csrrc(0, SSTATUS, T0)); // SPP = U
    s.push(SRET);
    let mut u = Vec::new();
    for r in (1..=31).filter(|&r| r != S0 && r != S1) {
        u.extend(li(r, user_value(r)));
    }
    u.push(addi(S0, 0, 0));
    u.extend(li(S1, ecalls));
    u.push(ECALL);
    u.push(addi(S0, S0, 1));
    u.push(bne(S0, S1, -8));
    assert_eq!(USER + 4 * u.len() as u32, user_ebreak_pc());
    u.push(EBREAK);
    let mut p = Program::default();
    p.code(STUB, &stub());
    p.code(S_START, &s);
    p.code(TRAMPOLINE, &trampoline());
    p.code(USER, &u);
    p
}

/// M-mode code at the reset vector: `words`, then an `ecall`, which ends the run.
pub fn machine(words: &[u32]) -> Program {
    let mut w = words.to_vec();
    w.push(ECALL);
    let mut p = Program::default();
    p.code(STUB, &w);
    p
}

/// `SRET`, re-exported for the tests.
pub const SRET_WORD: u32 = SRET;
