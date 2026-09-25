//! M2.8: `m2-reference` and `block_irq.elf` (`docs/m2-design.md` §11, §12).
//!
//! Everything here runs the committed ELF and disk fixture on the real `m2-reference`:
//! the loader, the `M2` CPU, the multi-master bus, the RAM, the UART, the IRQ controller,
//! the DMA block controller, and the block media, with real `mem.v1`, `irq.v0`, and
//! `block.v0` traffic. Results are read from the components' views, their trace records,
//! the dispatched events, and the final snapshot, never from the ELF.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::rc::Rc;

use systemscope_contracts::canonical::Decoder;
use systemscope_contracts::component::Delivered;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_elf::LoadImage;
use systemscope_platform::{
    BlockMediaConfigError, MultiMasterBusConfigError, dma, irqc, media, mmbus,
};
use systemscope_runtime::trace::Trace;
use systemscope_rv32::block_irq::{
    self, BLOCK_IRQ_SCRIPT, BUFFER_A, BUFFER_B, BlockIrqManifest, BlockIrqRun, EXPECTED_ENTRIES,
    FAIL_OUTPUT, FLAG, HANDLER, POLL, disk_image, judge, pattern, transformed,
};
use systemscope_rv32::m2ref::{
    self, BLK, BLK_BASE, BUS, BuildError, CPU, Config, DISK, DISK_BLOCKS, IRQC, IRQC_BASE,
    MASTER_CPU, MASTER_DMA, MASTERS, PATHS, RAM, REGION_RAM, REGIONS,
};
use systemscope_rv32::runner::Start;
use systemscope_rv32::{BUILD_SCRIPT, RAM_BASE, RAM_SIZE, TOOLCHAIN, UART_BASE, workspace_root};
use systemscope_rv32i::cpu::{COMMIT_KIND, INTERRUPT_KIND};

/// `mcause` of a machine external interrupt.
const MEI_CAUSE: u64 = 0x8000_000b;
/// The `MRET` instruction word.
const MRET: u64 = 0x3020_0073;

fn fixture() -> (LoadImage, Vec<u8>) {
    block_irq::read_fixture(&workspace_root()).unwrap()
}

fn fresh(traced: bool) -> BlockIrqRun {
    let (image, disk) = fixture();
    block_irq::run(&image, &disk, Start::Init { traced }, Vec::new())
}

fn trace(run: &BlockIrqRun) -> &Trace {
    run.finished.finished.trace.as_ref().expect("a traced run")
}

/// The records `component` emitted with `kind`, with their positions in the trace.
fn records<'a>(
    trace: &'a Trace,
    component: systemscope_contracts::component::ComponentId,
    kind: &str,
) -> Vec<(usize, &'a TraceRecord)> {
    trace
        .records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.origin == TraceOrigin::Component && r.component == component)
        .filter(|(_, r)| r.kind == kind)
        .collect()
}

fn u(r: &TraceRecord, name: &str) -> u64 {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::U64(v))) => *v,
        other => panic!("{} has no u64 {name}: {other:?}", r.kind),
    }
}

fn b(r: &TraceRecord, name: &str) -> bool {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::Bool(v))) => *v,
        other => panic!("{} has no bool {name}: {other:?}", r.kind),
    }
}

fn str_field<'a>(r: &'a TraceRecord, name: &str) -> &'a str {
    match r.fields.iter().find(|(n, _)| *n == name) {
        Some((_, Value::Str(v))) => v,
        other => panic!("{} has no string {name}: {other:?}", r.kind),
    }
}

// ---------------------------------------------------------------------------------------
// The fixture.

#[test]
fn the_committed_block_irq_elf_and_disk_match_their_manifest() {
    let root = workspace_root();
    let manifest = block_irq::verify(&root).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(manifest.entry, RAM_BASE);
    assert_eq!(manifest.image_hash, manifest.blake3);
    assert_eq!(manifest.flags, block_irq::FLAGS);
    assert_eq!(manifest.disk_blocks, DISK_BLOCKS);
    assert_eq!(
        manifest.disk_blake3,
        *blake3::hash(&disk_image()).as_bytes()
    );
    // The media names its image by the BLAKE3 of the raw bytes, the manifest's.
    let (_, disk) = fixture();
    assert_eq!(m2ref::media_image(&disk).image_hash, manifest.disk_blake3);
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

fn script_flags(script: &str) -> Vec<&str> {
    script
        .split_once("FLAGS=(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(flags, _)| flags.split_whitespace().collect())
        .expect("a FLAGS array")
}

/// The build script pins the rv32ui toolchain, the Zicsr flags, and the symbol addresses
/// this crate relies on.
#[test]
fn the_build_script_pins_what_the_crate_relies_on() {
    let root = workspace_root();
    let script = fs::read_to_string(root.join(BLOCK_IRQ_SCRIPT)).unwrap();
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
    assert_eq!(script_flags(&script), block_irq::FLAGS);
    let symbols: Vec<&str> = script_value(&script, "SYMBOLS").split(' ').collect();
    let pinned = [
        ("poll", POLL),
        ("handler", HANDLER),
        ("vars", block_irq::VARS),
        ("buffer_a", BUFFER_A),
        ("buffer_b", BUFFER_B),
    ];
    let expected: Vec<String> = pinned
        .iter()
        .map(|(name, at)| format!("{name}={at:08x}"))
        .collect();
    assert_eq!(symbols, expected);
    assert!(script.contains("! grep -qx wfi"), "the script rejects WFI");
    let manifest = BlockIrqManifest::read(&root).unwrap();
    let inputs: Vec<&str> = manifest.inputs.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(inputs, block_irq::BLOCK_IRQ_INPUTS);
}

// ---------------------------------------------------------------------------------------
// The topology.

/// The frozen platform, initialized and not run.
fn initialized() -> systemscope_runtime::runtime::Runtime {
    let (image, disk) = fixture();
    let mut rt = m2ref::build(&image, &disk, &Config::frozen()).unwrap();
    rt.init().unwrap();
    rt
}

#[test]
fn the_platform_is_the_frozen_m2_reference() {
    let rt = initialized();
    assert_eq!(rt.component_paths().collect::<Vec<_>>(), PATHS);
    let components = m2ref::components(&rt.snapshot().unwrap()).unwrap();
    let schemas: Vec<u32> = components.iter().map(|c| c.schema).collect();
    // The M2 CPU writes schema 2; every platform component schema 1.
    assert_eq!(schemas, [2, 1, 1, 1, 1, 1, 1]);

    // The bus: the region map in decoding order, then the masters, then the clock.
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
        ]
    );
    let masters: Vec<String> = (0..d.len().unwrap())
        .map(|_| d.str().unwrap().to_owned())
        .collect();
    assert_eq!(masters, ["cpu", "dma0"]);
    assert_eq!(d.u32().unwrap(), 0, "the one clock domain, cpu");
    assert_eq!(REGIONS, ["ram", "uart", "irqc", "blk"]);
    assert_eq!(MASTERS, ["cpu", "dma0"]);
    assert_eq!((MASTER_CPU, MASTER_DMA), (0, 1));
    assert_eq!(
        (u64::from(IRQC_BASE), u64::from(BLK_BASE)),
        (0x1000_1000, 0x1000_2000)
    );
    assert_eq!((irqc::SIZE, dma::SIZE), (8, 0x20));

    // The RAM: 16 MiB, the ELF's image hash, responds Cycles { cpu, 0 }.
    let (image, disk) = fixture();
    let mut d = Decoder::new(&components[RAM.0 as usize].bytes);
    assert_eq!(d.u64().unwrap(), u64::from(RAM_SIZE));
    assert_eq!(d.array::<32>().unwrap(), image.image_hash);
    assert_eq!(
        (d.u8().unwrap(), d.u32().unwrap(), d.u64().unwrap()),
        (1, 0, 0)
    );

    // The media: 16 blocks, Cycles { cpu, 16 }, no bad blocks, the fixture's BLAKE3.
    let mut d = Decoder::new(&components[DISK.0 as usize].bytes);
    assert_eq!(d.u64().unwrap(), 16);
    assert_eq!(
        (d.u8().unwrap(), d.u32().unwrap(), d.u64().unwrap()),
        (1, 0, 16)
    );
    assert_eq!(d.len().unwrap(), 0, "no bad blocks");
    assert_eq!(d.array::<32>().unwrap(), *blake3::hash(&disk).as_bytes());
    let (_, blocks) = m2ref::disk_blocks(&components[DISK.0 as usize].bytes).unwrap();
    assert_eq!(
        blocks,
        [(0, pattern())],
        "the pattern at LBA 0, zeros elsewhere"
    );

    // The controller and the IRQ controller, from their views.
    let frozen = Config::frozen();
    assert_eq!(
        (frozen.controller_blocks, frozen.media_blocks),
        (DISK_BLOCKS, DISK_BLOCKS)
    );
    assert_eq!(
        (frozen.dma_base, frozen.dma_size),
        (u64::from(RAM_BASE), u64::from(RAM_SIZE))
    );
    assert_eq!(frozen.seed, 0);
    assert_eq!(frozen.uart_base, u64::from(UART_BASE));
    assert!(frozen.bad_blocks.is_empty());
}

#[test]
fn every_port_is_wired_as_frozen() {
    // Each link carries its protocol end to end: a mis-wired port would not elaborate
    // (protocol or role mismatch) or would leave the run without its interrupt. The run
    // below needs every link: the CPU's mem and irq, the four bus regions, the DMA
    // master, the media, and the interrupt line.
    let run = fresh(true);
    judge(&run).unwrap();
    let t = trace(&run);
    let grants = records(t, BUS, mmbus::GRANT_KIND);
    let regions: BTreeSet<u64> = grants.iter().map(|(_, r)| u(r, "region")).collect();
    assert_eq!(
        regions,
        BTreeSet::from([0, 1, 2, 3]),
        "every region was used"
    );
    let masters: BTreeSet<u64> = grants.iter().map(|(_, r)| u(r, "master")).collect();
    assert_eq!(masters, BTreeSet::from([0, 1]));
    // The DMA master only ever reaches the RAM, and the CPU reaches every region.
    for (_, g) in &grants {
        if u(g, "master") == MASTER_DMA {
            assert_eq!(u(g, "region"), REGION_RAM);
        }
    }
}

// ---------------------------------------------------------------------------------------
// The builder.

fn build(config: &Config, disk: &[u8]) -> Result<(), BuildError> {
    let (image, _) = fixture();
    m2ref::build(&image, disk, config).map(|_| ())
}

#[test]
fn the_builder_requires_equal_controller_and_media_capacities() {
    let disk = disk_image();
    assert_eq!(build(&Config::frozen(), &disk), Ok(()));
    for (controller, media) in [(15, 16), (17, 16), (16, 17), (1, 16)] {
        let config = Config {
            controller_blocks: controller,
            media_blocks: media,
            ..Config::frozen()
        };
        assert_eq!(
            build(&config, &disk),
            Err(BuildError::CapacityMismatch { controller, media }),
            "{controller} vs {media}"
        );
    }
    // The capacity rule is checked first: a mismatch with a bad aperture reports it.
    let config = Config {
        controller_blocks: 8,
        dma_base: 0,
        ..Config::frozen()
    };
    assert!(matches!(
        build(&config, &disk),
        Err(BuildError::CapacityMismatch { .. })
    ));
}

#[test]
fn the_builder_requires_the_dma_aperture_inside_the_ram() {
    let disk = disk_image();
    let ram = u64::from(RAM_BASE);
    let size = u64::from(RAM_SIZE);
    for (base, len) in [
        (ram - 1, 16),              // starts below the RAM
        (ram, size + 1),            // runs one byte past it
        (ram + size - 16, 32),      // straddles its end
        (ram + size, 16),           // wholly above it
        (0x1000_2000, 0x20),        // an MMIO window
        (0, 0x1_0000_0000),         // the whole address space
        (ram + 16, u64::MAX - ram), // wraps
    ] {
        let config = Config {
            dma_base: base,
            dma_size: len,
            ..Config::frozen()
        };
        assert_eq!(
            build(&config, &disk),
            Err(BuildError::ApertureOutsideRam { base, size: len }),
            "{base:#x} + {len:#x}"
        );
    }
    // Wholly inside a smaller aperture is fine: the rule is containment, not equality.
    for (base, len) in [(ram, 512), (ram + size - 512, 512), (ram + 0x2000, 0x1000)] {
        let config = Config {
            dma_base: base,
            dma_size: len,
            ..Config::frozen()
        };
        assert_eq!(build(&config, &disk), Ok(()), "{base:#x} + {len:#x}");
    }
    // A RAM too small for the aperture.
    let config = Config {
        ram_size: size / 2,
        ..Config::frozen()
    };
    assert_eq!(
        build(&config, &disk),
        Err(BuildError::ApertureOutsideRam { base: ram, size })
    );
}

#[test]
fn overlapping_windows_are_rejected_by_the_bus() {
    let disk = disk_image();
    let config = Config {
        irqc_base: u64::from(UART_BASE) + 4,
        ..Config::frozen()
    };
    assert_eq!(
        build(&config, &disk),
        Err(BuildError::Bus(MultiMasterBusConfigError::Overlap(
            "uart", "irqc"
        )))
    );
    let config = Config {
        blk_base: u64::from(IRQC_BASE),
        ..Config::frozen()
    };
    assert_eq!(
        build(&config, &disk),
        Err(BuildError::Bus(MultiMasterBusConfigError::Overlap(
            "irqc", "blk"
        )))
    );
}

#[test]
fn a_disk_image_that_does_not_fit_is_rejected_by_the_media() {
    let mut large = disk_image();
    large.extend([0; 512]);
    assert_eq!(
        build(&Config::frozen(), &large),
        Err(BuildError::Media(BlockMediaConfigError::ImageTooLarge(17)))
    );
    assert_eq!(
        build(&Config::frozen(), &disk_image()[..511]),
        Err(BuildError::Media(BlockMediaConfigError::PartialBlock(511)))
    );
    // A shorter image is the same disk padded with zeros, under its own hash.
    assert_eq!(build(&Config::frozen(), &pattern()), Ok(()));
}

/// The production media builder names the image by the BLAKE3 of its bytes and by
/// nothing else: the same bytes from two files give the same session, and one flipped
/// byte gives another hash.
#[test]
fn the_media_image_is_named_by_the_blake3_of_its_bytes() {
    let disk = disk_image();
    let image = m2ref::media_image(&disk);
    assert_eq!(image.bytes, disk);
    assert_eq!(image.image_hash, *blake3::hash(&disk).as_bytes());

    let dir = std::env::temp_dir().join(format!("ss-m2-8-{}", std::process::id()));
    fs::create_dir_all(dir.join("elsewhere")).unwrap();
    let a = dir.join("disk.img");
    let b = dir.join("elsewhere").join("another-name.bin");
    fs::write(&a, &disk).unwrap();
    fs::write(&b, &disk).unwrap();
    let (elf, _) = fixture();
    let digest = |bytes: &[u8]| {
        let mut rt = m2ref::build(&elf, bytes, &Config::frozen()).unwrap();
        rt.init().unwrap();
        (rt.topology_hash(), rt.state_digest().unwrap())
    };
    assert_eq!(
        digest(&fs::read(&a).unwrap()),
        digest(&fs::read(&b).unwrap())
    );
    fs::remove_dir_all(&dir).unwrap();

    let mut flipped = disk.clone();
    flipped[4095] ^= 1;
    assert_ne!(m2ref::media_image(&flipped).image_hash, image.image_hash);
    assert_ne!(digest(&flipped), digest(&disk));
}

// ---------------------------------------------------------------------------------------
// End to end.

#[test]
fn block_irq_passes_on_m2_reference() {
    let run = fresh(true);
    judge(&run).unwrap_or_else(|e| panic!("{e}\n{:?}", run.finished.finished.outcome));
    let outcome = &run.finished.finished.outcome;
    assert_eq!((outcome.gp, outcome.a0), (1, 0));
    let state = run.state.as_ref().unwrap();
    assert_eq!(state.entries, EXPECTED_ENTRIES);
    assert_eq!(state.flag, 1);
    assert_eq!(state.saved_status, dma::STATUS_DONE);
    assert_eq!(state.buffer_a, transformed());
    assert_eq!(state.buffer_b, transformed());
    assert_eq!(state.lba0, pattern());
    assert_eq!(state.lba1, transformed());
    assert_eq!(state.stored, [0, 1], "no other block was written");
    assert!(!run.rejected);
    assert_eq!(
        records(trace(&run), BLK, dma::REJECTED_KIND).len(),
        0,
        "no command was rejected"
    );
}

/// The three commands, their media operations, and their completions, in order.
#[test]
fn the_three_transfers_are_commanded_served_and_completed() {
    let run = fresh(true);
    let t = trace(&run);
    let commands: Vec<(u64, u64, u64, u64, bool)> = records(t, BLK, dma::COMMAND_KIND)
        .iter()
        .map(|(_, r)| {
            (
                u(r, "op"),
                u(r, "lba"),
                u(r, "addr"),
                u(r, "count"),
                b(r, "accepted"),
            )
        })
        .collect();
    let (a, bb) = (u64::from(BUFFER_A), u64::from(BUFFER_B));
    assert_eq!(
        commands,
        [(1, 0, a, 1, true), (2, 1, a, 1, true), (1, 1, bb, 1, true)]
    );
    let dones: Vec<u64> = records(t, BLK, dma::DONE_KIND)
        .iter()
        .map(|(_, r)| u(r, "error"))
        .collect();
    assert_eq!(dones, [0, 0, 0]);
    let disk_ops: Vec<(&str, u64, &str)> = records(t, DISK, media::READ_KIND)
        .into_iter()
        .map(|(i, r)| (i, ("read", r)))
        .chain(
            records(t, DISK, media::WRITE_KIND)
                .into_iter()
                .map(|(i, r)| (i, ("write", r))),
        )
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_values()
        .map(|(kind, r)| (kind, u(r, "lba"), str_field(r, "outcome")))
        .collect();
    assert_eq!(
        disk_ops,
        [("read", 0, "ok"), ("write", 1, "ok"), ("read", 1, "ok")]
    );
}

/// Where the interrupt path of transfer `i` sits in the trace.
#[derive(Debug)]
struct Completion {
    command: usize,
    done: usize,
    level_high: usize,
    meip_high: usize,
    interrupt: usize,
    ack: usize,
    level_low: usize,
    meip_low: usize,
    flag: usize,
    mret: usize,
    resumed: usize,
}

fn completions(t: &Trace) -> Vec<Completion> {
    let commands = records(t, BLK, dma::COMMAND_KIND);
    let dones = records(t, BLK, dma::DONE_KIND);
    let levels = records(t, IRQC, irqc::LEVEL_KIND);
    let meips = records(t, IRQC, irqc::MEIP_KIND);
    let interrupts = records(t, CPU, INTERRUPT_KIND);
    let commits = records(t, CPU, COMMIT_KIND);
    let ack_addr = u64::from(BLK_BASE) + dma::ACK;
    let store_to = |addr: u64| {
        commits
            .iter()
            .filter(|(_, r)| r.fields.iter().any(|(n, _)| *n == "width"))
            .filter(move |(_, r)| u(r, "addr") == addr)
            .map(|(i, r)| (*i, *r))
            .collect::<Vec<_>>()
    };
    let acks = store_to(ack_addr);
    // The handler's stores of 1 to `flag`; `transfer` stores 0.
    let flags: Vec<_> = store_to(u64::from(FLAG))
        .into_iter()
        .filter(|(_, r)| u(r, "value") == 1)
        .collect();
    let mrets: Vec<_> = commits
        .iter()
        .filter(|(_, r)| u(r, "insn") == MRET)
        .collect();
    assert_eq!(commands.len(), 3);
    assert_eq!(dones.len(), 3);
    assert_eq!(levels.len(), 6, "{levels:?}");
    assert_eq!(meips.len(), 6, "{meips:?}");
    assert_eq!(interrupts.len(), 3, "exactly one interrupt per completion");
    assert_eq!(acks.len(), 3);
    assert_eq!(flags.len(), 3);
    assert_eq!(mrets.len(), 3);
    (0..3)
        .map(|i| {
            let mret = mrets[i].0;
            let resumed = commits
                .iter()
                .find(|(at, _)| *at > mret)
                .map(|(at, _)| *at)
                .expect("an instruction after MRET");
            Completion {
                command: commands[i].0,
                done: dones[i].0,
                level_high: levels[2 * i].0,
                meip_high: meips[2 * i].0,
                interrupt: interrupts[i].0,
                ack: acks[i].0,
                level_low: levels[2 * i + 1].0,
                meip_low: meips[2 * i + 1].0,
                flag: flags[i].0,
                mret,
                resumed,
            }
        })
        .collect()
}

#[test]
fn each_completion_raises_one_machine_external_interrupt() {
    let run = fresh(true);
    let t = trace(&run);
    for (_, r) in records(t, CPU, INTERRUPT_KIND) {
        assert_eq!(u(r, "mcause"), MEI_CAUSE);
        assert_eq!(u(r, "handler"), u64::from(HANDLER));
        // Taken in the wait loop: after its load of `flag` or after its branch.
        let mepc = u(r, "mepc");
        assert!(
            mepc == u64::from(POLL) || mepc == u64::from(POLL) + 4,
            "mepc {mepc:#x}"
        );
    }
    // The line and MEIP go high and low exactly once per completion, never twice in a
    // row, and only for source 0.
    let levels: Vec<(u64, bool)> = records(t, IRQC, irqc::LEVEL_KIND)
        .iter()
        .map(|(_, r)| (u(r, "source"), b(r, "asserted")))
        .collect();
    assert_eq!(levels, [(0, true), (0, false)].repeat(3));
    let meip: Vec<bool> = records(t, IRQC, irqc::MEIP_KIND)
        .iter()
        .map(|(_, r)| b(r, "asserted"))
        .collect();
    assert_eq!(meip, [true, false].repeat(3));
    // The CPU receives the same six levels on its irq port, and nothing else there.
    let to_cpu: Vec<bool> = run
        .finished
        .finished
        .dispatched
        .iter()
        .filter(|ev| ev.target == CPU && ev.source == IRQC)
        .map(|ev| match &ev.delivery {
            Delivered::Message {
                msg: Message::Irq(msg),
                ..
            } => {
                let systemscope_contracts::protocol::irq_v0::IrqMsg::Level { asserted } = msg;
                *asserted
            }
            other => panic!("the IRQ controller sent the CPU {other:?}"),
        })
        .collect();
    assert_eq!(to_cpu, [true, false].repeat(3));
}

/// command → media → done → line high → MEIP high → interrupt → ACK store response →
/// line low → MEIP low → flag store → MRET → back at `mepc`, then the next command.
#[test]
fn the_interrupt_path_is_causally_ordered() {
    let run = fresh(true);
    let t = trace(&run);
    let c = completions(t);
    let interrupts = records(t, CPU, INTERRUPT_KIND);
    let commits = records(t, CPU, COMMIT_KIND);
    for (i, k) in c.iter().enumerate() {
        assert!(k.command < k.done, "{i}: {k:?}");
        assert!(k.done < k.level_high, "{i}: {k:?}");
        assert!(k.level_high < k.meip_high, "{i}: {k:?}");
        assert!(k.meip_high < k.interrupt, "{i}: {k:?}");
        assert!(k.interrupt < k.level_low, "{i}: {k:?}");
        assert!(k.level_low < k.meip_low, "{i}: {k:?}");
        // MEIP is low before the ACK store commits, so the next retirement cannot
        // re-enter the handler (§12.3).
        assert!(k.meip_low < k.ack, "{i}: {k:?}");
        assert!(k.ack < k.flag, "{i}: ACK before the flag, {k:?}");
        assert!(k.flag < k.mret, "{i}: {k:?}");
        assert!(k.mret < k.resumed, "{i}: {k:?}");
        if let Some(next) = c.get(i + 1) {
            assert!(k.resumed < next.command, "{i}: {k:?}");
        }
        // No other interrupt between entry and MRET.
        let nested = interrupts
            .iter()
            .filter(|(at, _)| *at > k.interrupt && *at < k.mret)
            .count();
        assert_eq!(nested, 0, "{i}: re-entered the handler");
        // MRET returns to the interrupted instruction.
        let mepc = u(interrupts[i].1, "mepc");
        let resumed = commits.iter().find(|(at, _)| *at == k.resumed).unwrap().1;
        assert_eq!(u(resumed, "pc"), mepc, "{i}");
        // The first handler instruction is the one after the interrupt.
        let first = commits.iter().find(|(at, _)| *at > k.interrupt).unwrap().1;
        assert_eq!(u(first, "pc"), u64::from(HANDLER), "{i}");
    }
}

// ---------------------------------------------------------------------------------------
// Contention.

/// Keeps the bus's RAM-region summary after every event.
struct RamRegion(Rc<RefCell<Vec<String>>>);

impl Observer for RamRegion {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let view = world.inspect(BUS).unwrap_or_default();
        match view.get("ram") {
            Some(Value::Str(s)) => self.0.borrow_mut().push(s.clone()),
            other => panic!("the bus view has no ram summary: {other:?}"),
        }
        Control::Continue
    }
}

/// `(active, rr_cursor, queued)` from a region summary.
fn region(summary: &str) -> (Option<u64>, u64, Vec<u64>) {
    let mut parts = summary.split(' ');
    let active = parts.next().unwrap().strip_prefix("active=").unwrap();
    let cursor = parts.next().unwrap().strip_prefix("rr_cursor=").unwrap();
    let queued = parts.next().unwrap().strip_prefix("queued=").unwrap();
    (
        active.parse().ok(),
        cursor.parse().unwrap(),
        queued.split(',').map(|n| n.parse().unwrap()).collect(),
    )
}

#[test]
fn cpu_and_dma_contend_for_the_ram_under_round_robin() {
    let (image, disk) = fixture();
    let summaries = Rc::new(RefCell::new(Vec::new()));
    let run = block_irq::run(
        &image,
        &disk,
        Start::Init { traced: true },
        vec![Box::new(RamRegion(Rc::clone(&summaries)))],
    );
    judge(&run).unwrap();
    let t = trace(&run);

    // Every grant of the RAM follows the round-robin rule from the state before it: the
    // first master with a queued request, scanning from the cursor; the cursor then moves
    // past the granted master.
    let summaries = summaries.borrow();
    let mut before = (None, 0, vec![0, 0]);
    let mut behind = [0usize; 2];
    let mut grants = 0;
    for s in summaries.iter() {
        let after = region(s);
        // A request of master m queued while the other master held the RAM.
        for m in 0..2u64 {
            if after.2[m as usize] > before.2[m as usize] && after.0 == Some(1 - m) {
                behind[m as usize] += 1;
            }
        }
        if let (None, Some(granted)) = (before.0, after.0) {
            let (_, cursor, queued) = &before;
            let expected = (0..2)
                .map(|k| (cursor + k) % 2)
                .find(|&m| queued[m as usize] > 0)
                .expect("a grant needs a queued request");
            assert_eq!(granted, expected, "{before:?} -> {after:?}");
            assert_eq!(after.1, (granted + 1) % 2, "{before:?} -> {after:?}");
            grants += 1;
        }
        before = after;
    }
    // Real contention in both orders: a CPU request waited behind an active DMA beat,
    // and a DMA beat behind an active CPU request.
    assert!(behind[0] > 0 && behind[1] > 0, "{behind:?}");

    let ram_grants: Vec<&TraceRecord> = records(t, BUS, mmbus::GRANT_KIND)
        .into_iter()
        .map(|(_, r)| r)
        .filter(|r| u(r, "region") == REGION_RAM)
        .collect();
    assert_eq!(
        ram_grants.len(),
        grants,
        "every grant was seen by the observer"
    );

    // Within each transfer, the DMA moves one block in 32 beats while the CPU keeps
    // loading `flag` from the same RAM.
    let c = completions(t);
    let all = records(t, BUS, mmbus::GRANT_KIND);
    for (i, k) in c.iter().enumerate() {
        let window: Vec<&TraceRecord> = all
            .iter()
            .filter(|(at, r)| *at > k.command && *at < k.done && u(r, "region") == REGION_RAM)
            .map(|(_, r)| *r)
            .collect();
        let dma = window
            .iter()
            .filter(|r| u(r, "master") == MASTER_DMA)
            .count();
        let cpu = window
            .iter()
            .filter(|r| u(r, "master") == MASTER_CPU)
            .count();
        assert_eq!(dma, dma::BEATS_PER_BLOCK as usize, "transfer {i}");
        assert!(cpu > 0, "transfer {i}: the CPU never reached the RAM");
    }
}

/// The bus renames every downstream transaction with its own counter and restores the
/// master's id on the way back.
#[test]
fn downstream_transactions_are_renamed_and_restored() {
    let run = fresh(true);
    let t = trace(&run);
    let grants = records(t, BUS, mmbus::GRANT_KIND);
    let downstream: Vec<u64> = grants.iter().map(|(_, r)| u(r, "downstream_txn")).collect();
    assert!(
        downstream.windows(2).all(|w| w[0] < w[1]),
        "unique and increasing"
    );
    let txn = |m: &MemMsg| match m {
        MemMsg::ReadReq { txn, .. }
        | MemMsg::ReadResp { txn, .. }
        | MemMsg::WriteReq { txn, .. }
        | MemMsg::WriteResp { txn, .. } => txn.0,
    };
    let mem = |ev: &systemscope_runtime::runtime::Dispatched| match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => Some(m.clone()),
        _ => None,
    };
    let dispatched = &run.finished.finished.dispatched;
    // What the RAM received: exactly the RAM grants' downstream ids, in grant order.
    let to_ram: Vec<u64> = dispatched
        .iter()
        .filter(|ev| ev.target == RAM && ev.source == BUS)
        .filter_map(mem)
        .map(|m| txn(&m))
        .collect();
    let ram_downstream: Vec<u64> = grants
        .iter()
        .filter(|(_, r)| u(r, "region") == REGION_RAM)
        .map(|(_, r)| u(r, "downstream_txn"))
        .collect();
    assert_eq!(to_ram, ram_downstream);
    // What each master got back: its own ids, in the order it was granted.
    for (master, component, port) in [
        (MASTER_CPU, CPU, systemscope_rv32i::cpu::PORT),
        (MASTER_DMA, BLK, dma::DMA_PORT),
    ] {
        let back: Vec<u64> = dispatched
            .iter()
            .filter(|ev| ev.target == component && ev.source == BUS)
            .filter(|ev| matches!(ev.delivery, Delivered::Message { port: p, .. } if p == port))
            .filter_map(mem)
            .map(|m| txn(&m))
            .collect();
        let granted: Vec<u64> = grants
            .iter()
            .filter(|(_, r)| u(r, "master") == master)
            .map(|(_, r)| u(r, "txn"))
            .collect();
        assert_eq!(back, granted, "master {master}");
    }
}

// ---------------------------------------------------------------------------------------
// Determinism.

#[test]
fn two_runs_are_identical() {
    let a = fresh(true);
    let b = fresh(true);
    judge(&a).unwrap();
    assert_eq!(a.finished.finished.outcome, b.finished.finished.outcome);
    assert_eq!(a.finished.finished.views, b.finished.finished.views);
    assert_eq!(a.finished.snapshot, b.finished.snapshot);
    assert_eq!(a.state, b.state);
    assert_eq!(trace(&a).canonical_bytes(), trace(&b).canonical_bytes());
    assert_eq!(
        a.finished.finished.dispatched.len(),
        b.finished.finished.dispatched.len()
    );
    // Tracing observes and changes nothing.
    let untraced = fresh(false);
    let (x, y) = (
        &a.finished.finished.outcome,
        &untraced.finished.finished.outcome,
    );
    assert_eq!(
        (x.state, x.execution, x.instret, x.events),
        (y.state, y.execution, y.instret, y.events)
    );
    assert_eq!(a.finished.snapshot, untraced.finished.snapshot);
}

/// A small checkpoint smoke (the exhaustive every-event closure is M2.9): resuming from
/// a point inside each transfer's DMA window gives the same end as never stopping.
#[test]
fn resuming_inside_a_transfer_reaches_the_same_end() {
    let (image, disk) = fixture();
    let reference = fresh(true);
    let c = completions(trace(&reference));
    let dispatched = &reference.finished.finished.dispatched;
    // Event indices, in dispatch order, of the first DMA grant of each transfer.
    let grant_events: Vec<usize> = c
        .iter()
        .map(|k| {
            let key = match trace(&reference).records[k.done].at {
                systemscope_contracts::trace::TraceAt::Event(key) => key,
                systemscope_contracts::trace::TraceAt::Init => unreachable!(),
            };
            dispatched.iter().position(|ev| ev.key == key).unwrap() - 8
        })
        .collect();
    for k in grant_events {
        let mut rt = m2ref::build(&image, &disk, &Config::frozen()).unwrap();
        rt.start_trace().unwrap();
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let snapshot = rt.snapshot().unwrap();
        let prefix = rt.take_trace();
        let resumed = block_irq::run(
            &image,
            &disk,
            Start::Restore { snapshot, prefix },
            Vec::new(),
        );
        judge(&resumed).unwrap();
        let (x, y) = (
            &reference.finished.finished.outcome,
            &resumed.finished.finished.outcome,
        );
        assert_eq!(
            (x.state, x.execution, x.trace),
            (y.state, y.execution, y.trace),
            "{k}"
        );
        assert_eq!(
            reference.finished.snapshot, resumed.finished.snapshot,
            "{k}"
        );
        assert_eq!(
            &reference.finished.finished.dispatched[k..],
            resumed.finished.finished.dispatched.as_slice(),
            "{k}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// A media fault.

/// With LBA 0 bad, transfer 1 completes with `MEDIA_ERROR`; the program sees an error in
/// the saved status and takes its fail path.
#[test]
fn a_bad_block_takes_the_fail_path() {
    let (image, disk) = fixture();
    let config = Config {
        bad_blocks: BTreeSet::from([0]),
        ..Config::frozen()
    };
    let run = block_irq::run_with(
        &image,
        &disk,
        &config,
        Start::Init { traced: true },
        Vec::new(),
    );
    let outcome = &run.finished.finished.outcome;
    assert_eq!((outcome.gp, outcome.a0), (1, 1));
    assert_eq!(run.output.as_deref(), Ok(&FAIL_OUTPUT[..]));
    assert!(judge(&run).is_err());
    let state = run.state.as_ref().unwrap();
    assert_eq!(state.entries, 1);
    assert_eq!(
        state.saved_status,
        dma::STATUS_DONE | (u32::from(dma::MEDIA_ERROR) << dma::STATUS_ERROR_SHIFT)
    );
    let dones: Vec<u64> = records(trace(&run), BLK, dma::DONE_KIND)
        .iter()
        .map(|(_, r)| u(r, "error"))
        .collect();
    assert_eq!(dones, [u64::from(dma::MEDIA_ERROR)]);
    assert_eq!(state.stored, [0], "nothing was written");
}
