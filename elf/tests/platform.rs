//! A load image handed to the real RAM and CPU (`docs/m1-design.md` §8):
//!
//! ```text
//! synthetic ELF bytes → load_elf32 → LoadImage → Ram::new / Rv32iCpu::new → bytes read, program run
//! ```
//!
//! The loader depends on neither crate. The `LoadSegment → Segment` step below is the whole
//! adapter: the same offsets and bytes, widened from `u32` to `u64`, and the same hash.

mod common;

use std::cell::RefCell;
use std::num::NonZeroU64;
use std::rc::Rc;

use common::asm::*;
use common::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemMsg, ReadOutcome, TxnId};
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::Value;
use systemscope_elf::{LoadImage, load_elf32};
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, Segment};
use systemscope_runtime::runtime::SessionConfig;
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_rv32i::{CpuConfigError, Rv32iConfig, Rv32iCpu};

const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: u32 = 0x4000;
const TEXT: u32 = RAM_BASE + 0x100;
const DATA: u32 = RAM_BASE + 0x2000;
const DATA_WORD: u32 = 0xdead_beef;

/// The mechanical adapter from a load image to the RAM's initial image.
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

/// Code at `TEXT` (entry), then one data word and 12 bytes of `.bss` at `DATA`. The
/// program loads the data word and the first `.bss` word, adds them, and stores the sum
/// in the last `.bss` word.
fn program() -> Elf {
    let code = code(&[
        lui(10, DATA >> 12),
        lw(1, 10, 0),
        lw(2, 10, 4),
        add(3, 1, 2),
        sw(3, 10, 12),
        lw(4, 10, 12),
        addi(5, 0, 7),
    ]);
    Elf::new(
        TEXT,
        vec![
            other(PT_NOTE, 0, &[0x55; 8], 8),
            load(DATA, &DATA_WORD.to_le_bytes(), 16),
            load(TEXT, &code, code.len() as u32),
        ],
    )
}

const PROGRAM_LEN: u64 = 7;

// ---- RAM ----

struct NoRng;

impl SimRng for NoRng {
    fn next_u64(&mut self) -> u64 {
        unreachable!("the RAM draws no random numbers")
    }
}

/// Delivers requests straight to one component and keeps what it sends back.
struct Direct {
    rng: NoRng,
    sent: Vec<MemMsg>,
}

impl InitContext for Direct {
    fn component(&self) -> ComponentId {
        ComponentId(0)
    }

    fn send(&mut self, _: PortId, msg: Message, _: ScheduleWhen, _: Phase) -> Result<(), SimError> {
        let Message::MemV1(msg) = msg else {
            panic!("the RAM speaks mem.v1");
        };
        self.sent.push(msg);
        Ok(())
    }

    fn wake_self(&mut self, _: ScheduleWhen, _: Phase, _: u64) -> Result<(), SimError> {
        unreachable!("the RAM never wakes itself")
    }

    fn rng(&mut self) -> &mut dyn SimRng {
        &mut self.rng
    }

    fn trace(&mut self, _: &'static str, _: Vec<(&'static str, Value)>) {}
}

impl SimContext for Direct {
    fn now(&self) -> Tick {
        Tick::ZERO
    }

    fn phase(&self) -> Phase {
        Phase::Request
    }
}

/// A RAM for [`read`]: the latency only sets the response's schedule, which `Direct`
/// ignores.
fn direct_config() -> RamConfig {
    RamConfig {
        size: u64::from(RAM_SIZE),
        latency: LinkLatency::Cycles {
            domain: ClockDomainId(0),
            k: 1,
        },
    }
}

/// Reads `len` bytes at RAM offset `offset` with a `mem.v1` request.
fn read(ram: &mut Ram, offset: u32, len: u32) -> Vec<u8> {
    let mut ctx = Direct {
        rng: NoRng,
        sent: Vec::new(),
    };
    let req = MemMsg::ReadReq {
        txn: TxnId(1),
        addr: u64::from(offset),
        len,
    };
    let ev = Delivered::Message {
        port: PortId(0),
        msg: req.into(),
    };
    ram.handle_event(&ev, &mut ctx).unwrap();
    match ctx.sent.as_slice() {
        [
            MemMsg::ReadResp {
                txn: TxnId(1),
                outcome: ReadOutcome::Data { data },
            },
        ] => data.clone(),
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn the_ram_holds_the_loaded_text_data_and_zeroed_bss() {
    let elf = program();
    let file = elf.build();
    let image = load_elf32(&file, RAM_BASE, RAM_SIZE).unwrap();
    let config = direct_config();
    let mut ram = Ram::new(config, &ram_image(&image)).unwrap();

    // Text at its RAM-relative offset, not at its absolute address.
    let code = &elf.phdrs[2].data;
    assert_eq!(read(&mut ram, TEXT - RAM_BASE, code.len() as u32), *code);
    // Initialized data, then .bss reading as zero.
    assert_eq!(read(&mut ram, DATA - RAM_BASE, 4), DATA_WORD.to_le_bytes());
    assert_eq!(read(&mut ram, DATA - RAM_BASE + 4, 12), vec![0; 12]);
    // Bytes no segment covers are zero, including where the PT_NOTE's address points.
    assert_eq!(read(&mut ram, 0, 0x100), vec![0; 0x100]);
    assert_eq!(read(&mut ram, DATA - RAM_BASE + 16, 16), vec![0; 16]);

    // The RAM is configured with the hash of the ELF file itself.
    assert_eq!(image.image_hash, *blake3::hash(&file).as_bytes());
    assert_eq!(
        ram.inspect().get("image_hash"),
        Some(&Value::Bytes(image.image_hash.to_vec()))
    );
}

#[test]
fn the_ram_accepts_every_image_the_loader_accepts() {
    // Segments right at both ends of the RAM, adjacent to each other.
    let elf = Elf::new(
        RAM_BASE,
        vec![
            load(RAM_BASE + RAM_SIZE - 8, &[7; 8], 8),
            load(RAM_BASE, &[1; 16], 16),
            load(RAM_BASE + 16, &[], 32),
        ],
    );
    let image = load_elf32(&elf.build(), RAM_BASE, RAM_SIZE).unwrap();
    let config = direct_config();
    let mut ram = Ram::new(config, &ram_image(&image)).unwrap();
    assert_eq!(read(&mut ram, RAM_SIZE - 8, 8), vec![7; 8]);
    assert_eq!(read(&mut ram, 0, 16), vec![1; 16]);
    assert_eq!(read(&mut ram, 16, 32), vec![0; 32]);
}

// ---- CPU ----

#[test]
fn the_entry_point_is_a_valid_cpu_entry() {
    let image = load_elf32(&program().build(), RAM_BASE, RAM_SIZE).unwrap();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let config = |entry| Rv32iConfig {
        clock,
        entry,
        max_instructions: NonZeroU64::new(1).unwrap(),
    };
    assert!(Rv32iCpu::new(config(image.entry)).is_ok());
    // What the loader rules out, the CPU would too: the two agree on alignment.
    assert_eq!(
        Rv32iCpu::new(config(image.entry + 2)).err(),
        Some(CpuConfigError::MisalignedEntry(image.entry + 2))
    );
}

/// Keeps the CPU's view after the last event.
struct Last(Rc<RefCell<Option<StateView>>>);

impl Observer for Last {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        *self.0.borrow_mut() = world.inspect(ComponentId(0));
        Control::Continue
    }
}

fn reg(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

#[test]
fn a_loaded_program_runs_from_its_entry_point_on_the_bus_and_ram() {
    let image = load_elf32(&program().build(), RAM_BASE, RAM_SIZE).unwrap();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock,
        entry: image.entry,
        max_instructions: NonZeroU64::new(PROGRAM_LEN).unwrap(),
    })
    .unwrap();
    let cpu = t.add_component("soc.cpu", Box::new(cpu));
    let bus = AddressBus::new(vec![Region {
        name: "ram",
        base: u64::from(RAM_BASE),
        size: u64::from(RAM_SIZE),
    }])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let latency = LinkLatency::Cycles {
        domain: clock,
        k: 1,
    };
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency,
        },
        &ram_image(&image),
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    assert_eq!(cpu, ComponentId(0));
    t.connect((cpu, "mem"), (bus, "cpu"), None);
    t.connect((bus, "ram"), (ram, "mem"), Some(latency));
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    let last = Rc::new(RefCell::new(None));
    rt.add_observer(Box::new(Last(Rc::clone(&last))));
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    assert_eq!(rt.fault(), None);

    let view = last.take().unwrap();
    assert_eq!(
        view.get("halt"),
        Some(&Value::Str("instruction_limit".to_owned()))
    );
    assert_eq!(reg(&view, "instret"), PROGRAM_LEN);
    assert_eq!(reg(&view, "pc"), u64::from(TEXT) + 4 * PROGRAM_LEN);
    assert_eq!(reg(&view, "x1"), u64::from(DATA_WORD), "initialized data");
    assert_eq!(reg(&view, "x2"), 0, "bss reads as zero");
    assert_eq!(
        reg(&view, "x4"),
        u64::from(DATA_WORD),
        "bss is writable RAM"
    );
    assert_eq!(reg(&view, "x5"), 7);
}
