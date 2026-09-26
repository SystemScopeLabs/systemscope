//! M3.6: `m3-reference` and the M3 scenario (`docs/m3-design.md` §7, §8, §11, §12,
//! §17.3).
//!
//! Everything here runs the committed firmware and disks on the real `m3-reference`: the
//! host loader places only `m3-firmware.elf` in RAM; the `M3` CPU boots it, the firmware
//! enters the kernel through `kgate`, and `soc.kernel` reads the `SSX0` table and every
//! executable through `soc.blk` and `soc.disk` into staging by polled DMA, validates them
//! with `parse_user_elf32`, builds their Sv32 address spaces in simulated RAM, and runs
//! them in U-mode until the SBI shutdown. Results are read from the components' views,
//! their trace records, the dispatched events, and snapshots, never from the ELFs.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use systemscope_contracts::canonical::Decoder;
use systemscope_contracts::component::ComponentId;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_elf::{BLOCK_SIZE, LoadImage, parse_exec_table, parse_user_elf32};
use systemscope_os::kernel::{ENTER_KIND, SHUTDOWN_KIND};
use systemscope_os::procop::{
    BOOT_KIND, CREATE_KIND, EXIT_KIND, FAULT_KIND, SWITCH_KIND, SYSCALL_ENTER_KIND,
    SYSCALL_EXIT_KIND,
};
use systemscope_os::{PlanError, UserLayout, Window};
use systemscope_platform::{dma, irqc, media, mmbus, uart};
use systemscope_runtime::runtime::{Dispatched, Runtime};
use systemscope_runtime::trace::Trace;
use systemscope_rv32::m2ref;
use systemscope_rv32::m3::{
    self, FIRMWARE_FLAGS, M3_INPUTS, M3_SCRIPT, M3Manifest, M3Run, Metrics, PROGRAMS, Scenario,
    USER_FLAGS, judge, os_trace,
};
use systemscope_rv32::m3ref::{
    self, BLK, BLK_BASE, BUS, BuildError, CPU, Config, DISK, DISK_BLOCKS, IRQC, KERNEL, KGATE_BASE,
    KGATE_SIZE, MASTER_CPU, MASTER_DMA, MASTER_KERNEL, MASTERS, PATHS, POOL, POOL_FRAMES,
    POOL_SIZE, RAM, REGION_KGATE, REGION_RAM, REGIONS, SRST, STAGING, STAGING_SIZE, TRAP_FRAME,
    UART,
};
use systemscope_rv32::runner::{End, Start};
use systemscope_rv32::{BUILD_SCRIPT, RAM_BASE, RAM_SIZE, TOOLCHAIN, UART_BASE, workspace_root};
use systemscope_rv32i::cpu::EXCEPTION_KIND;

const EFAULT: u64 = (-14i32) as u32 as u64;
const EBADF: u64 = (-9i32) as u32 as u64;
const ENOSYS: u64 = (-38i32) as u32 as u64;

fn root() -> PathBuf {
    workspace_root()
}

fn fixture(scenario: Scenario) -> (LoadImage, Vec<u8>, String) {
    m3::read_fixture(&root(), scenario).unwrap()
}

fn fresh(scenario: Scenario, traced: bool) -> M3Run {
    let (firmware, disk, _) = fixture(scenario);
    m3::run(&firmware, &disk, Start::Init { traced }, Vec::new())
}

fn run_disk(disk: &[u8], config: &Config) -> M3Run {
    let (firmware, _, _) = fixture(Scenario::Reference);
    m3::run_with(
        &firmware,
        disk,
        config,
        Start::Init { traced: true },
        Vec::new(),
    )
}

fn trace(run: &M3Run) -> &Trace {
    run.finished.trace.as_ref().expect("a traced run")
}

/// The records `component` emitted with `kind`, with their positions in the trace.
fn records<'a>(
    trace: &'a Trace,
    component: ComponentId,
    kind: &str,
) -> Vec<(usize, &'a TraceRecord)> {
    trace
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.origin == TraceOrigin::Component && r.component == component && r.kind == kind
        })
        .collect()
}

fn u(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{name}: {other:?} in {r:?}"),
    }
}

fn st(r: &TraceRecord, name: &str) -> String {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::Str(v))) => v.clone(),
        other => panic!("{name}: {other:?} in {r:?}"),
    }
}

fn view_str(run: &M3Run, id: ComponentId, name: &str) -> String {
    match run.finished.views[id.0 as usize].get(name) {
        Some(Value::Str(s)) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn register(run: &M3Run, name: &str) -> u64 {
    match run.finished.views[CPU.0 as usize].get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

/// The index in `dispatched` of the event that emitted `record`.
fn event_of(dispatched: &[Dispatched], record: &TraceRecord) -> usize {
    let TraceAt::Event(key) = record.at else {
        panic!("an init record: {record:?}")
    };
    dispatched.binary_search_by(|d| d.key.cmp(&key)).unwrap()
}

// ---------------------------------------------------------------------------------------
// The fixtures.

#[test]
fn the_committed_m3_fixtures_match_their_manifest() {
    let root = root();
    let manifest = m3::verify(&root).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(manifest, M3Manifest::read(&root).unwrap());
    let programs: Vec<&str> = manifest.programs.iter().map(|p| p.file.as_str()).collect();
    let expected: Vec<String> = PROGRAMS.iter().map(|p| format!("{p}.elf")).collect();
    assert_eq!(programs, expected);
    for (scenario, disk) in Scenario::ALL.iter().zip(&manifest.disks) {
        assert_eq!(disk.file, scenario.disk_file());
        assert_eq!(disk.blocks, DISK_BLOCKS);
        assert_eq!(disk.programs, scenario.programs());
        let (_, bytes, expected) = fixture(*scenario);
        // The media is named by the disk's BLAKE3, which keys the expected trace.
        assert_eq!(m2ref::media_image(&bytes).image_hash, disk.blake3);
        assert!(expected.starts_with("os.boot entries="));
    }
    let inputs: Vec<&str> = manifest.inputs.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(inputs, M3_INPUTS);
}

/// The value of `NAME=value` or `NAME='value'` in a build script.
fn script_value<'a>(script: &'a str, name: &str) -> &'a str {
    script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name}"))
        .trim_matches('\'')
        .trim_matches('"')
}

/// The words of the bash array `NAME=( ... )`.
fn script_array<'a>(script: &'a str, name: &str) -> Vec<&'a str> {
    script
        .split_once(&format!("\n{name}=("))
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(words, _)| words.split_whitespace().collect())
        .unwrap_or_else(|| panic!("no {name} array"))
}

#[test]
fn the_build_script_pins_what_the_crate_relies_on() {
    let root = root();
    let script = fs::read_to_string(root.join(M3_SCRIPT)).unwrap();
    let rv32ui = fs::read_to_string(root.join(BUILD_SCRIPT)).unwrap();
    for name in ["GCC_VERSION", "AS_VERSION"] {
        assert_eq!(script_value(&script, name), script_value(&rv32ui, name));
    }
    assert_eq!(
        script_value(&script, "GCC_VERSION"),
        TOOLCHAIN[0].version_line
    );
    assert_eq!(
        script_value(&script, "AS_VERSION"),
        TOOLCHAIN[1].version_line
    );
    let common = script_array(&script, "COMMON");
    let with = |march: &'static str| -> Vec<&str> {
        std::iter::once(march)
            .chain(common.iter().copied())
            .collect()
    };
    assert_eq!(
        script_array(&script, "FIRMWARE_FLAGS"),
        ["-march=rv32i_zicsr", "\"${COMMON[@]}\""]
    );
    assert_eq!(
        script_array(&script, "USER_FLAGS"),
        ["-march=rv32i", "\"${COMMON[@]}\""]
    );
    assert_eq!(with("-march=rv32i_zicsr"), FIRMWARE_FLAGS);
    assert_eq!(with("-march=rv32i"), USER_FLAGS);
    let programs: Vec<&str> = script_value(&script, "USER_PROGRAMS").split(' ').collect();
    assert_eq!(programs, PROGRAMS);
    // The gp-independence and instruction checks the build enforces.
    for check in [
        "! grep -qx wfi",
        "grep -qx ecall",
        "R_RISCV_(RELAX|GPREL)",
        "__global_pointer",
        "check \"$name\" 10000 rv32i2p1 \"$RV32I\" \"\"",
        "for one in mret sret sfence.vma ecall",
        "two builds differ",
    ] {
        assert!(script.contains(check), "the script lacks {check:?}");
    }
    // Every source is assembled without relaxation.
    for name in PROGRAMS.iter().copied().chain(["firmware"]) {
        let source = fs::read_to_string(root.join(format!("tests/rv32/m3/{name}.S"))).unwrap();
        assert!(source.contains(".option norelax"), "{name}.S");
    }
}

/// The registers an RV32I instruction word names, or `None` for an opcode outside RV32I
/// as the user programs may use it.
fn registers(word: u32) -> Option<Vec<u32>> {
    let rd = (word >> 7) & 31;
    let rs1 = (word >> 15) & 31;
    let rs2 = (word >> 20) & 31;
    match word & 0x7f {
        0x37 | 0x17 | 0x6f => Some(vec![rd]),
        0x13 | 0x03 | 0x67 => Some(vec![rd, rs1]),
        0x23 | 0x63 => Some(vec![rs1, rs2]),
        0x33 => Some(vec![rd, rs1, rs2]),
        0x73 if word == 0x0000_0073 => Some(vec![]),
        _ => None,
    }
}

/// §17.1: no user program depends on `gp`. Every executable page holds only RV32I words
/// that never name `x3`, and every program validates under the M3 user layout.
#[test]
fn the_user_programs_are_gp_independent_rv32i_executables() {
    let root = root();
    for name in PROGRAMS {
        let bytes = fs::read(root.join(format!("tests/rv32/m3/{name}.elf"))).unwrap();
        let image =
            parse_user_elf32(&bytes, bytes.len() as u32, UserLayout::M3.image_range()).unwrap();
        assert_eq!(image.entry, UserLayout::M3.user_base, "{name}");
        let perms: Vec<(bool, bool, bool)> = image
            .segments
            .iter()
            .map(|s| (s.perms.read, s.perms.write, s.perms.execute))
            .collect();
        let expected = if name == "hello" {
            vec![
                (true, false, true),
                (true, false, false),
                (true, true, false),
                (true, true, false),
            ]
        } else {
            vec![(true, false, true), (true, false, false)]
        };
        assert_eq!(perms, expected, "{name}");
        let mut words = 0;
        for s in image.segments.iter().filter(|s| s.perms.execute) {
            for p in &s.pages {
                let c = p.copy.unwrap();
                let text = &bytes[c.file_offset as usize..(c.file_offset + c.len) as usize];
                for w in text.as_chunks::<4>().0 {
                    let word = u32::from_le_bytes(*w);
                    let regs = registers(word)
                        .unwrap_or_else(|| panic!("{name}: {word:#010x} is not RV32I"));
                    assert!(!regs.contains(&3), "{name}: {word:#010x} names gp");
                    words += 1;
                }
            }
        }
        assert!(words > 0, "{name}");
    }
    // The firmware is the only image the host loads: at the RAM base, inside the
    // firmware's 64 KiB below the trap frame (§11.2).
    let (firmware, _, _) = fixture(Scenario::Reference);
    assert_eq!(firmware.entry, RAM_BASE);
    for s in &firmware.segments {
        assert!(u64::from(s.offset) + s.bytes.len() as u64 <= u64::from(TRAP_FRAME - RAM_BASE));
    }
}

// ---------------------------------------------------------------------------------------
// The topology.

fn initialized(scenario: Scenario) -> Runtime {
    let (firmware, disk, _) = fixture(scenario);
    let mut rt = m3ref::build(&firmware, &disk, &Config::frozen()).unwrap();
    rt.init().unwrap();
    rt
}

#[test]
fn the_platform_is_the_frozen_m3_reference() {
    let rt = initialized(Scenario::Reference);
    assert_eq!(rt.component_paths().collect::<Vec<_>>(), PATHS);
    let components = m2ref::components(&rt.snapshot().unwrap()).unwrap();
    let schemas: Vec<u32> = components.iter().map(|c| c.schema).collect();
    // The M3 CPU writes schema 3; every platform component and the kernel schema 1.
    assert_eq!(schemas, [3, 1, 1, 1, 1, 1, 1, 1]);

    let mut d = Decoder::new(&components[BUS.0 as usize].bytes);
    let regions: Vec<(String, u64, u64)> = (0..d.len().unwrap())
        .map(|_| {
            (
                d.str().unwrap().to_owned(),
                d.u64().unwrap(),
                d.u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        regions,
        [
            ("ram".to_owned(), 0x8000_0000, 0x0100_0000),
            ("uart".to_owned(), 0x1000_0000, 0x8),
            ("irqc".to_owned(), 0x1000_1000, 0x8),
            ("blk".to_owned(), 0x1000_2000, 0x20),
            ("kgate".to_owned(), 0x1000_3000, 0x8),
        ]
    );
    let masters: Vec<String> = (0..d.len().unwrap())
        .map(|_| d.str().unwrap().to_owned())
        .collect();
    assert_eq!(masters, ["cpu", "dma0", "kernel0"]);
    assert_eq!(REGIONS, ["ram", "uart", "irqc", "blk", "kgate"]);
    assert_eq!(MASTERS, ["cpu", "dma0", "kernel0"]);
    assert_eq!((MASTER_CPU, MASTER_DMA, MASTER_KERNEL), (0, 1, 2));
    assert_eq!((irqc::SIZE, dma::SIZE, uart::SIZE), (8, 0x20, 8));

    // The RAM holds only the firmware: its image hash, and no page outside it.
    let (firmware, disk, _) = fixture(Scenario::Reference);
    let ram = &components[RAM.0 as usize].bytes;
    let mut d = Decoder::new(ram);
    assert_eq!(d.u64().unwrap(), u64::from(RAM_SIZE));
    assert_eq!(d.array::<32>().unwrap(), firmware.image_hash);
    for (page, _) in m2ref::ram_pages(ram).unwrap() {
        assert!(
            page < 0x10,
            "RAM page {page:#x} holds something besides the firmware"
        );
    }

    // The media: 256 blocks, Cycles { cpu, 16 }, no bad blocks, the disk's BLAKE3; its
    // blocks are the table and the five files.
    let mut d = Decoder::new(&components[DISK.0 as usize].bytes);
    assert_eq!(d.u64().unwrap(), 256);
    assert_eq!(
        (d.u8().unwrap(), d.u32().unwrap(), d.u64().unwrap()),
        (1, 0, 16)
    );
    assert_eq!(d.len().unwrap(), 0, "no bad blocks");
    assert_eq!(d.array::<32>().unwrap(), *blake3::hash(&disk).as_bytes());
    let block0: [u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().unwrap();
    let table = parse_exec_table(&block0, 256, STAGING_SIZE as u32).unwrap();
    let programs = m3::read_programs(&root(), Scenario::Reference).unwrap();
    assert_eq!(table.entries.len(), 5);
    for (e, p) in table.entries.iter().zip(&programs) {
        let at = e.start_lba as usize * BLOCK_SIZE;
        assert_eq!(&disk[at..at + p.len()], p.as_slice());
    }

    let frozen = Config::frozen();
    assert_eq!(
        (
            frozen.controller_blocks,
            frozen.media_blocks,
            frozen.boot.capacity_blocks
        ),
        (256, 256, 256)
    );
    assert_eq!(
        (frozen.dma_base, frozen.dma_size),
        (u64::from(RAM_BASE), u64::from(RAM_SIZE))
    );
    assert_eq!(frozen.seed, 0);
    assert!(frozen.bad_blocks.is_empty());
    assert_eq!(frozen.boot.layout, UserLayout::M3);
    let k = frozen.kernel;
    assert_eq!(k.trap_frame, 0x8001_0000);
    assert_eq!((k.staging.base, k.staging.size), (0x8010_0000, 0x10_0000));
    assert_eq!(
        (k.frame_pool.base, k.frame_pool.size),
        (0x8040_0000, 0xC0_0000)
    );
    assert_eq!(
        (u64::from(POOL), POOL_SIZE, POOL_FRAMES),
        (0x8040_0000, 0xC0_0000, 3072)
    );
    assert_eq!((u64::from(STAGING), STAGING_SIZE), (0x8010_0000, 1 << 20));
    assert_eq!(
        (k.gate.base, k.gate.size),
        (u64::from(KGATE_BASE), KGATE_SIZE)
    );
    assert_eq!((k.blk.base, k.blk.size), (u64::from(BLK_BASE), dma::SIZE));
    assert_eq!(k.uart_tx, u64::from(UART_BASE));
    assert_eq!(
        (k.ram.base, k.ram.size),
        (u64::from(RAM_BASE), u64::from(RAM_SIZE))
    );
}

/// Every link carries its traffic: each master reaches exactly the regions its role
/// allows. The kernel reaches the RAM, the UART, and the block controller, never `kgate`;
/// the DMA engine only the RAM; the CPU never the UART, which only the kernel writes.
#[test]
fn every_port_is_wired_as_frozen() {
    let run = fresh(Scenario::Reference, true);
    let t = trace(&run);
    let grants = records(t, BUS, mmbus::GRANT_KIND);
    let mut reach: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for (_, g) in &grants {
        reach
            .entry(u(g, "master"))
            .or_default()
            .insert(u(g, "region"));
    }
    assert_eq!(
        reach,
        BTreeMap::from([
            (MASTER_CPU, BTreeSet::from([REGION_RAM, REGION_KGATE])),
            (MASTER_DMA, BTreeSet::from([REGION_RAM])),
            (MASTER_KERNEL, BTreeSet::from([REGION_RAM, 1, 3])),
        ])
    );
    // One CPU access to kgate per kernel entry: the firmware's boot ENTER and one per
    // trap from U.
    let cpu_gate = grants
        .iter()
        .filter(|(_, g)| u(g, "master") == MASTER_CPU && u(g, "region") == REGION_KGATE)
        .count();
    assert_eq!(cpu_gate, records(t, KERNEL, ENTER_KIND).len());
    assert_eq!(cpu_gate, 1 + records(t, CPU, EXCEPTION_KIND).len());
    // The UART bytes are the kernel's writes, one per byte.
    let kernel_uart = grants
        .iter()
        .filter(|(_, g)| u(g, "master") == MASTER_KERNEL && u(g, "region") == 1)
        .count();
    assert_eq!(kernel_uart, Scenario::Reference.expected_output().len());
}

// ---------------------------------------------------------------------------------------
// The builder.

fn build(config: &Config, disk: &[u8]) -> Result<(), BuildError> {
    let (firmware, _, _) = fixture(Scenario::Reference);
    m3ref::build(&firmware, disk, config).map(|_| ())
}

#[test]
fn the_builder_rejects_what_the_platform_invariants_forbid() {
    let (_, disk, _) = fixture(Scenario::Reference);
    let frozen = Config::frozen();
    assert_eq!(build(&frozen, &disk), Ok(()));
    let with = |f: &dyn Fn(&mut Config)| {
        let mut c = frozen.clone();
        f(&mut c);
        build(&c, &disk)
    };
    assert_eq!(
        with(&|c| c.controller_blocks = 255),
        Err(BuildError::CapacityMismatch {
            controller: 255,
            media: 256
        })
    );
    assert_eq!(
        with(&|c| c.boot.capacity_blocks = 255),
        Err(BuildError::BootCapacityMismatch {
            kernel: 255,
            media: 256
        })
    );
    assert_eq!(
        with(&|c| c.dma_base = u64::from(RAM_BASE) - 4096),
        Err(BuildError::ApertureOutsideRam {
            base: u64::from(RAM_BASE) - 4096,
            size: u64::from(RAM_SIZE)
        })
    );
    assert_eq!(
        with(&|c| c.dma_size = u64::from(RAM_SIZE) + 1),
        Err(BuildError::ApertureOutsideRam {
            base: u64::from(RAM_BASE),
            size: u64::from(RAM_SIZE) + 1
        })
    );
    let moved = Window {
        base: u64::from(KGATE_BASE) + 8,
        size: KGATE_SIZE,
    };
    assert_eq!(
        with(&|c| c.kernel.gate = moved),
        Err(BuildError::GateMismatch(moved))
    );
    assert_eq!(
        with(&|c| c.kernel.uart_tx = u64::from(KGATE_BASE) + 4),
        Err(BuildError::GateGranted(Window {
            base: u64::from(KGATE_BASE) + 4,
            size: 1
        }))
    );
    let blk = Window {
        base: u64::from(BLK_BASE),
        size: 0x18,
    };
    assert_eq!(
        with(&|c| c.kernel.blk = blk),
        Err(BuildError::ControllerMismatch(blk))
    );
    assert_eq!(
        with(&|c| c.kernel.staging.size = 256),
        Err(BuildError::Kernel(PlanError::Disk(
            "staging cannot hold block 0"
        )))
    );
    // A disk larger than the media is the media's to refuse.
    let mut big = disk.clone();
    big.extend_from_slice(&[1; BLOCK_SIZE]);
    assert!(matches!(build(&frozen, &big), Err(BuildError::Media(_))));
    for e in [
        BuildError::GateInAperture,
        BuildError::CapacityMismatch {
            controller: 1,
            media: 2,
        },
    ] {
        assert!(!e.to_string().is_empty());
    }
}

// ---------------------------------------------------------------------------------------
// The scenario.

#[test]
fn the_m3_scenario_passes_on_m3_reference() {
    for scenario in Scenario::ALL {
        let (_, _, expected) = fixture(scenario);
        let run = fresh(scenario, true);
        judge(&run, scenario, &expected).unwrap_or_else(|e| panic!("{scenario:?}: {e}"));
        assert_eq!(
            run.finished.outcome.end,
            End::Trap {
                cause: "EnvironmentCallFromS".to_owned(),
                pc: 0x8000_01c0,
                tval: 0
            },
            "the firmware's shutdown ecall"
        );
        assert_eq!(register(&run, "x17"), u64::from(SRST));
        assert_eq!(register(&run, "x11"), u64::from(scenario.expected_reason()));
        assert_eq!(run.output.as_deref(), Ok(scenario.expected_output()));
    }
}

/// The committed trace's meaning, checked here independently of the file: boot, the
/// creations, `getpid`, the three `badptr` errors, the kill, the exits, and shutdown.
#[test]
fn the_os_records_are_the_scenario_semantics() {
    let run = fresh(Scenario::Reference, true);
    let t = trace(&run);
    let boot = records(t, KERNEL, BOOT_KIND);
    assert_eq!(boot.len(), 1);
    assert_eq!(u(boot[0].1, "entries"), 5);
    let creates = records(t, KERNEL, CREATE_KIND);
    let pids: Vec<u64> = creates.iter().map(|(_, r)| u(r, "pid")).collect();
    assert_eq!(pids, [1, 2, 3, 4, 5]);
    for (_, c) in &creates {
        assert_eq!(st(c, "error"), "");
        assert_eq!(u(c, "entry"), 0x1_0000);
        let root = u(c, "root") * 4096;
        assert!((u64::from(POOL)..u64::from(POOL) + POOL_SIZE).contains(&root));
    }
    let syscalls: Vec<(u64, u64, u64)> = records(t, KERNEL, SYSCALL_EXIT_KIND)
        .iter()
        .map(|(_, r)| (u(r, "pid"), u(r, "nr"), u(r, "ret")))
        .collect();
    assert_eq!(syscalls[0], (1, 172, 1), "getpid in pid 1");
    assert_eq!(syscalls[1], (1, 64, 17), "hello's line");
    let badptr: Vec<(u64, u64)> = syscalls
        .iter()
        .filter(|s| s.0 == 5)
        .map(|s| (s.1, s.2))
        .collect();
    assert_eq!(badptr, [(64, EFAULT), (64, EBADF), (999, ENOSYS)]);
    let enters = records(t, KERNEL, SYSCALL_ENTER_KIND);
    let efault = enters.iter().find(|(_, r)| u(r, "pid") == 5).unwrap().1;
    assert_eq!(
        (u(efault, "a0"), u(efault, "a1"), u(efault, "a2")),
        (1, u64::from(RAM_BASE), 4),
        "a kernel address"
    );
    let fault = records(t, KERNEL, FAULT_KIND);
    assert_eq!(fault.len(), 1);
    let f = fault[0].1;
    assert_eq!(
        (u(f, "pid"), st(f, "cause"), u(f, "epc"), u(f, "tval")),
        (4, "StorePageFault".to_owned(), 0x1_0020, 0x1_0000)
    );
    let exits: Vec<(u64, i64)> = records(t, KERNEL, EXIT_KIND)
        .iter()
        .map(
            |(_, r)| match r.fields.iter().find(|(n, _)| *n == "status") {
                Some((_, Value::I64(s))) => (u(r, "pid"), *s),
                other => panic!("{other:?}"),
            },
        )
        .collect();
    assert_eq!(exits, [(1, 0), (5, 0), (2, 0), (3, 0)]);
    let switches: Vec<(u64, u64)> = records(t, KERNEL, SWITCH_KIND)
        .iter()
        .map(|(_, r)| (u(r, "from"), u(r, "to")))
        .collect();
    assert_eq!(switches[0], (0, 1), "the first dispatch");
    assert!(
        switches.contains(&(2, 3)) && switches.contains(&(3, 2)),
        "ping and pong"
    );
    let shutdown = records(t, KERNEL, SHUTDOWN_KIND);
    assert_eq!(shutdown.len(), 1);
    assert_eq!(u(shutdown[0].1, "reason"), 1);
    assert_eq!(st(shutdown[0].1, "detail"), "the run queue is empty");
    let m = Metrics::of(&run);
    assert_eq!(
        (
            m.syscalls,
            m.switches,
            m.exceptions,
            m.dma_commands,
            m.uart_bytes
        ),
        (22, 11, 23, 6, 53)
    );
}

/// Executable → Storage → RAM → Process: the table and every executable reach staging
/// only through the controller's commands and the media's reads, polled, never with an
/// interrupt.
#[test]
fn every_executable_reaches_ram_through_the_block_controller() {
    let run = fresh(Scenario::Reference, true);
    let t = trace(&run);
    let (_, disk, _) = fixture(Scenario::Reference);
    let block0: [u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().unwrap();
    let table = parse_exec_table(&block0, 256, STAGING_SIZE as u32).unwrap();
    let commands: Vec<(u64, u64, u64, u64)> = records(t, BLK, dma::COMMAND_KIND)
        .iter()
        .map(|(_, r)| (u(r, "op"), u(r, "lba"), u(r, "addr"), u(r, "count")))
        .collect();
    let mut expected = vec![(1, 0, u64::from(STAGING), 1)];
    expected.extend(table.entries.iter().map(|e| {
        (
            1,
            u64::from(e.start_lba),
            u64::from(STAGING),
            u64::from(e.blocks()),
        )
    }));
    assert_eq!(commands, expected);
    let blocks: u64 = expected.iter().map(|c| c.3).sum();
    assert_eq!(records(t, DISK, media::READ_KIND).len() as u64, blocks);
    assert!(records(t, DISK, media::WRITE_KIND).is_empty());
    assert!(records(t, BLK, dma::REJECTED_KIND).is_empty());
    let dones = records(t, BLK, dma::DONE_KIND);
    assert_eq!(dones.len(), expected.len());
    assert!(dones.iter().all(|(_, r)| u(r, "error") == 0));
    // Polled: the interrupt line never rises.
    assert!(
        records(t, IRQC, irqc::LEVEL_KIND)
            .iter()
            .all(|(_, r)| r.fields.contains(&("asserted", Value::Bool(false))))
    );
    assert!(
        records(t, IRQC, irqc::MEIP_KIND)
            .iter()
            .all(|(_, r)| r.fields.contains(&("asserted", Value::Bool(false))))
    );
    // Each process is created after its executable's transfer completed.
    let creates = records(t, KERNEL, CREATE_KIND);
    for (i, (at, _)) in creates.iter().enumerate() {
        assert!(dones[i + 1].0 < *at, "entry {i}");
    }
}

/// Process → CPU → Memory → Syscall: user code runs in U, reaches the kernel only by
/// delegated `ecall`s through the trampoline, and every exception is taken into S.
#[test]
fn user_code_runs_in_u_and_enters_the_kernel_through_the_trampoline() {
    let run = fresh(Scenario::Reference, true);
    let t = trace(&run);
    let exceptions = records(t, CPU, EXCEPTION_KIND);
    let syscalls = records(t, KERNEL, SYSCALL_ENTER_KIND);
    assert_eq!(exceptions.len(), syscalls.len() + 1);
    for (_, e) in &exceptions {
        assert_eq!(
            (st(e, "from"), st(e, "to")),
            ("U".to_owned(), "S".to_owned())
        );
        assert!((0x1_0000..0x2_0000).contains(&u(e, "pc")), "{e:?}");
    }
    // Each ecall precedes the kernel's decoding of it, with the gate ENTER in between.
    let enters = records(t, KERNEL, ENTER_KIND);
    let ecalls: Vec<usize> = exceptions
        .iter()
        .filter(|(_, e)| st(e, "cause") == "EnvironmentCallFromU")
        .map(|(i, _)| *i)
        .collect();
    for (ecall, (decoded, _)) in ecalls.iter().zip(&syscalls) {
        assert!(ecall < decoded);
        assert!(enters.iter().any(|(g, _)| ecall < g && g < decoded));
    }
    let halted = view_str(&run, KERNEL, "life");
    assert_eq!(halted, "down");
    assert_eq!(view_str(&run, KERNEL, "processes").split(';').count(), 5);
}

// ---------------------------------------------------------------------------------------
// Failure paths on the real platform.

fn storage(programs: &[Vec<u8>]) -> Vec<u8> {
    m3::disk_image(programs).unwrap()
}

fn shutdown_detail(run: &M3Run) -> String {
    let s = records(trace(run), KERNEL, SHUTDOWN_KIND);
    assert_eq!(s.len(), 1);
    st(s[0].1, "detail")
}

fn expect_boot_failure(run: &M3Run, detail: &str) {
    assert_eq!(register(run, "x17"), u64::from(SRST));
    assert_eq!(register(run, "x11"), 1, "reason 1");
    assert_eq!(run.output.as_deref(), Ok(&b""[..]));
    let t = trace(run);
    assert!(records(t, KERNEL, BOOT_KIND).is_empty());
    assert!(records(t, KERNEL, CREATE_KIND).is_empty());
    assert!(records(t, CPU, EXCEPTION_KIND).is_empty());
    assert!(
        shutdown_detail(run).starts_with(detail),
        "{}",
        shutdown_detail(run)
    );
    assert_eq!(view_str(run, KERNEL, "life"), "down");
}

#[test]
fn a_media_error_on_the_table_shuts_down_with_reason_1() {
    let (_, disk, _) = fixture(Scenario::Reference);
    let config = Config {
        bad_blocks: BTreeSet::from([0]),
        ..Config::frozen()
    };
    let run = run_disk(&disk, &config);
    expect_boot_failure(
        &run,
        &format!(
            "block controller error {} reading the executable table",
            dma::MEDIA_ERROR
        ),
    );
    assert_eq!(records(trace(&run), BLK, dma::COMMAND_KIND).len(), 1);
}

#[test]
fn an_invalid_table_shuts_down_with_reason_1() {
    let (_, disk, _) = fixture(Scenario::Reference);
    // The magic, the version, the count (69 entries), and entry 0's flags word.
    for corrupt in [0usize, 4, 8, 0x18] {
        let mut bad = disk.clone();
        bad[corrupt] ^= 0x40;
        let run = run_disk(&bad, &Config::frozen());
        expect_boot_failure(&run, "invalid executable table: ");
    }
}

/// A failed entry is only its own creation's error; boot continues with the rest, which
/// run as the variant disk's programs do.
#[test]
fn a_failed_entry_fails_only_its_creation() {
    let programs = m3::read_programs(&root(), Scenario::Reference).unwrap();
    let (_, disk, _) = fixture(Scenario::Reference);
    let block0: [u8; BLOCK_SIZE] = disk[..BLOCK_SIZE].try_into().unwrap();
    let table = parse_exec_table(&block0, 256, STAGING_SIZE as u32).unwrap();
    let fault_lba = u64::from(table.entries[3].start_lba);
    let mut garbage = programs.clone();
    garbage[3] = b"not an executable".repeat(9);
    let mut writable = programs.clone();
    writable[3] = programs[3].clone();
    // e_ident[EI_CLASS] = ELFCLASS64: the header is not ELF32.
    writable[3][4] = 2;
    let cases: [(Vec<u8>, Config, &str); 3] = [
        (
            disk.clone(),
            Config {
                bad_blocks: BTreeSet::from([fault_lba]),
                ..Config::frozen()
            },
            "block controller error",
        ),
        (storage(&garbage), Config::frozen(), ""),
        (storage(&writable), Config::frozen(), ""),
    ];
    for (bytes, config, error) in cases {
        let run = run_disk(&bytes, &config);
        let t = trace(&run);
        let creates: Vec<(u64, u64, String)> = records(t, KERNEL, CREATE_KIND)
            .iter()
            .map(|(_, r)| (u(r, "pid"), u(r, "entry"), st(r, "error")))
            .collect();
        assert_eq!(creates.len(), 5);
        let (pid, entry, message) = &creates[3];
        assert_eq!((*pid, *entry), (4, 0), "{creates:?}");
        assert!(
            !message.is_empty() && message.starts_with(error),
            "{message}"
        );
        assert!(
            creates
                .iter()
                .enumerate()
                .all(|(i, c)| i == 3 || c.2.is_empty())
        );
        // The other four run exactly as the variant disk's programs, but an entry that did
        // not become a process makes the empty-queue shutdown reason 1 (§17.1).
        assert_eq!(
            run.output.as_deref(),
            Ok(Scenario::NoFault.expected_output())
        );
        assert_eq!(register(&run, "x11"), 1);
        assert_eq!(
            run.finished.views[KERNEL.0 as usize].get("free_frames"),
            Some(&Value::U64(POOL_FRAMES))
        );
    }
}

// ---------------------------------------------------------------------------------------
// Determinism.

#[test]
fn two_runs_are_identical_across_every_component() {
    for scenario in Scenario::ALL {
        let a = fresh(scenario, true);
        let b = fresh(scenario, true);
        assert_eq!(a.finished.outcome, b.finished.outcome);
        assert_eq!(a.finished.views, b.finished.views);
        assert_eq!(a.finished.dispatched, b.finished.dispatched);
        assert_eq!(a.snapshot, b.snapshot);
        assert_eq!(trace(&a).canonical_bytes(), trace(&b).canonical_bytes());
        assert_eq!(os_trace(trace(&a)), os_trace(trace(&b)));
        // Tracing observes and changes nothing.
        let untraced = fresh(scenario, false);
        let (x, y) = (&a.finished.outcome, &untraced.finished.outcome);
        assert_eq!(
            (x.state, x.execution, x.instret, x.events),
            (y.state, y.execution, y.instret, y.events)
        );
        assert_eq!(a.snapshot, untraced.snapshot);
        assert_eq!(a.finished.dispatched, untraced.finished.dispatched);
    }
}

// ---------------------------------------------------------------------------------------
// Snapshot and restore.

/// Records the kernel's operation stage and the block controller's engine after every
/// event.
struct Stages(Rc<RefCell<Vec<(String, String, String)>>>);

impl Observer for Stages {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let get =
            |id: ComponentId, name: &str| match world.inspect(id).unwrap_or_default().get(name) {
                Some(Value::Str(s)) => s.clone(),
                other => format!("{other:?}"),
            };
        self.0
            .borrow_mut()
            .push((get(KERNEL, "op"), get(KERNEL, "phase"), get(BLK, "engine")));
        Control::Continue
    }
}

fn platform(scenario: Scenario) -> Runtime {
    let (firmware, disk, _) = fixture(scenario);
    m3ref::build(&firmware, &disk, &Config::frozen()).unwrap()
}

/// §17.3: the kernel is restored at every event boundary of boot, from its first
/// instruction through the first dispatch, each time into a freshly built platform that
/// replaces the running one, and the run continues from the restored platform. Nothing
/// is reissued: the chained run dispatches exactly the uninterrupted run's events and
/// ends in its state. The boundaries include a DMA beat in flight with the kernel
/// polling `STATUS`, the held `ENTER`, and every disk stage.
#[test]
fn boot_resumes_from_every_event_without_a_reissue() {
    let scenario = Scenario::Reference;
    let stages = Rc::new(RefCell::new(Vec::new()));
    let (firmware, disk, _) = fixture(scenario);
    let reference = m3::run(
        &firmware,
        &disk,
        Start::Init { traced: true },
        vec![Box::new(Stages(Rc::clone(&stages)))],
    );
    let dispatched = &reference.finished.dispatched;
    let first_dispatch = records(trace(&reference), KERNEL, SWITCH_KIND)[0].1;
    let boot_end = event_of(dispatched, first_dispatch) + 1;

    // Coverage of the boundaries the chained run restores at.
    let stages = stages.borrow();
    let boot = &stages[..boot_end];
    let seen =
        |pred: &dyn Fn(&(String, String, String)) -> bool| boot.iter().filter(|s| pred(s)).count();
    assert!(
        seen(&|s| s.0 == "blk_poll" && s.2 == "wait_beat") > 0,
        "a beat behind a poll"
    );
    assert!(seen(&|s| s.2 == "wait_media") > 0, "the media busy");
    for stage in [
        "blk_command",
        "blk_poll",
        "blk_ack",
        "read_staging",
        "load",
        "dispatch",
    ] {
        assert!(seen(&|s| s.0 == stage) > 0, "{stage}");
    }
    assert!(seen(&|s| s.1 == "wait") > 0, "a kernel access in flight");

    let mut rt = platform(scenario);
    rt.init().unwrap();
    for (k, expected) in dispatched[..boot_end].iter().enumerate() {
        let snapshot = rt.snapshot().unwrap();
        let mut restored = platform(scenario);
        restored.restore(&snapshot).unwrap();
        assert_eq!(restored.snapshot().unwrap(), snapshot, "event {k}");
        rt = restored;
        let ev = rt.step().unwrap().unwrap();
        assert_eq!(&ev, expected, "event {k}");
    }
    let snapshot = rt.snapshot().unwrap();
    let resumed = m3::run(
        &firmware,
        &disk,
        Start::Restore {
            snapshot,
            prefix: None,
        },
        Vec::new(),
    );
    let (x, y) = (&reference.finished.outcome, &resumed.finished.outcome);
    assert_eq!(
        (x.state, x.execution, &x.end),
        (y.state, y.execution, &y.end)
    );
    assert_eq!(reference.snapshot, resumed.snapshot);
    assert_eq!(
        &dispatched[boot_end..],
        resumed.finished.dispatched.as_slice()
    );
    assert_eq!(reference.output, resumed.output);
}

/// 20 checkpoints across the whole scenario, each restored into a freshly built platform
/// and run to completion with the trace prefix: the same end, events, snapshot, and
/// trace as never stopping, and a restored run that passes §12.3.
#[test]
fn twenty_checkpoints_restore_to_the_same_end() {
    let scenario = Scenario::Reference;
    let (firmware, disk, expected) = fixture(scenario);
    let stages = Rc::new(RefCell::new(Vec::new()));
    let reference = m3::run(
        &firmware,
        &disk,
        Start::Init { traced: true },
        vec![Box::new(Stages(Rc::clone(&stages)))],
    );
    let t = trace(&reference);
    let dispatched = &reference.finished.dispatched;
    let n = dispatched.len();
    let stages = stages.borrow();
    let first = |pred: &dyn Fn(&(String, String, String)) -> bool| {
        stages.iter().position(pred).unwrap() + 1
    };
    let at =
        |component, kind: &str, i: usize| event_of(dispatched, records(t, component, kind)[i].1);
    let mut points = vec![
        1,
        first(&|s| s.0 == "blk_command"),
        first(&|s| s.0 == "blk_poll" && s.2 == "wait_beat"),
        first(&|s| s.0 == "blk_ack"),
        first(&|s| s.0 == "read_staging"),
        first(&|s| s.0 == "load"),
        first(&|s| s.0 == "load") + 40,
        at(KERNEL, SWITCH_KIND, 0) + 1,
        at(CPU, EXCEPTION_KIND, 0),
        at(KERNEL, SYSCALL_ENTER_KIND, 1) + 2,
        at(UART, uart::TX_KIND, 2),
        at(KERNEL, SWITCH_KIND, 3),
        at(KERNEL, FAULT_KIND, 0),
        at(KERNEL, EXIT_KIND, 2),
        n - 1,
    ];
    points.retain(|&p| p > 0 && p < n);
    let mut k = 1;
    while points.len() < 20 {
        points.push(k * n / 6);
        k += 1;
    }
    points.sort_unstable();
    points.dedup();
    assert_eq!(points.len(), 20, "{points:?}");
    for k in points {
        let mut rt = platform(scenario);
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let snapshot = rt.snapshot().unwrap();
        let prefix = rt.take_trace();
        let resumed = m3::run(
            &firmware,
            &disk,
            Start::Restore { snapshot, prefix },
            Vec::new(),
        );
        judge(&resumed, scenario, &expected).unwrap_or_else(|e| panic!("{k}: {e}"));
        let (x, y) = (&reference.finished.outcome, &resumed.finished.outcome);
        assert_eq!(
            (x.state, x.execution, x.trace, &x.end),
            (y.state, y.execution, y.trace, &y.end),
            "{k}"
        );
        assert_eq!(reference.snapshot, resumed.snapshot, "{k}");
        assert_eq!(
            &dispatched[k..],
            resumed.finished.dispatched.as_slice(),
            "{k}"
        );
    }
}
