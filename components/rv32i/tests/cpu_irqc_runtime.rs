//! The `M2` CPU behind a `SimpleIrqController` in the real runtime (`docs/m2-design.md`
//! §7.2, M2.3): a scripted `irq.v0` source on the controller's `src0`, the controller's
//! `cpu` on the CPU's `irq`, and the controller's window on the address bus next to the
//! RAM. Every `irq.v0` link has latency `Cycles { cpu, 1 }`, as in `m2-reference` (§11);
//! no `MultiMasterBus`, block device, or DMA is involved.
//!
//! The program enables the source in `ENABLE` with a store, enables MEIE and MIE, and
//! spins; the handler reads `PENDING` and returns with `MRET`, which re-enters while the
//! line is held. The tests check that the controller's output reaches `mip.MEIP` one link
//! latency after the controller decides it, whether a source level or the `ENABLE` store's
//! acceptance decides it, and that checkpoints at every event boundary resume identically.

mod common;

use std::cell::RefCell;
use std::num::NonZeroU64;
use std::rc::Rc;

use common::asm::*;
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, Value};
use systemscope_platform::irqc::{ENABLE, MEIP_KIND, SIZE};
use systemscope_platform::{
    AddressBus, IrqControllerConfig, Ram, RamConfig, RamImage, Region, Segment, SimpleIrqController,
};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::INTERRUPT_KIND;
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: u32 = 0x1000;
const HANDLER: u32 = RAM_BASE + 0x100;
/// The controller's window, as in `m2-reference`.
const IRQC_BASE: u32 = 0x1000_1000;
/// The loop the program spins in once everything is enabled.
const LOOP: u32 = RAM_BASE + 0x28;
const TICKS_PER_CYCLE: u64 = SimulationClock::DEFAULT_TICKS_PER_SECOND / 1_000_000_000;

const CPU: ComponentId = ComponentId(0);
const IRQC: ComponentId = ComponentId(3);
const SOURCE: ComponentId = ComponentId(4);

const MRET: u32 = 0x3020_0073;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}

/// Sets `mtvec`, enables MEIE, writes `ENABLE = 1`, enables MIE, then counts in `x5`
/// forever. The handler counts in `x6`, reads `PENDING` into `x7`, and returns.
fn program() -> (Vec<u32>, Vec<u32>) {
    let main = vec![
        lui(1, RAM_BASE >> 12),
        addi(1, 1, 0x100),
        csr_word(1, 0, 1, 0x305), // csrrw x0, mtvec, x1
        addi(2, 0, 0x7ff),
        addi(2, 2, 1),
        csr_word(2, 0, 2, 0x304), // csrrs x0, mie, x2
        lui(3, IRQC_BASE >> 12),
        addi(4, 0, 1),
        sw(4, 3, ENABLE as i32),  // ENABLE = 1
        csr_word(6, 0, 8, 0x300), // csrrsi x0, mstatus, 8
        addi(5, 5, 1),            // LOOP
        jal(0, -4),
    ];
    assert_eq!(RAM_BASE + 4 * 10, LOOP);
    let handler = vec![addi(6, 6, 1), lw(7, 3, 0), MRET];
    (main, handler)
}

/// A stateless `irq.v0` source: sends `levels[i].1` at the `Complete` of cycle
/// `levels[i].0`, all scheduled during `init`.
struct LevelScript {
    clock: ClockDomainId,
    levels: Vec<(u64, bool)>,
}

impl Component for LevelScript {
    fn type_name(&self) -> &'static str {
        "test.irq_script"
    }

    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "irq",
            protocol: irq_v0::PROTOCOL,
            role: Role::Initiator,
        }]
    }

    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for &(k, asserted) in &self.levels {
            let when = ScheduleWhen::Cycles {
                domain: self.clock,
                k,
            };
            ctx.send(
                PortId(0),
                IrqMsg::Level { asserted }.into(),
                when,
                Phase::Complete,
            )?;
        }
        Ok(())
    }

    fn handle_event(&mut self, _: &Delivered, _: &mut dyn SimContext) -> Result<(), SimError> {
        Err(SimError::ComponentFault(
            "irq script: a source receives nothing",
        ))
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// The CPU's and the controller's views after every event.
struct Recorder(Rc<RefCell<Vec<(StateView, StateView)>>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0
            .borrow_mut()
            .push((world.inspect(CPU).unwrap(), world.inspect(IRQC).unwrap()));
        Control::Continue
    }
}

/// CPU, bus (RAM and controller), RAM, controller (1 source), source.
fn build(levels: &[(u64, bool)], limit: u64) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cycles = |k| LinkLatency::Cycles { domain: clock, k };
    let cpu = Rv32iCpu::new(Rv32iConfig {
        clock,
        entry: RAM_BASE,
        max_instructions: NonZeroU64::new(limit).unwrap(),
        profile: Rv32iProfile::M2,
    })
    .unwrap();
    let cpu = t.add_component("soc.cpu", Box::new(cpu));
    let bus = AddressBus::new(vec![
        Region {
            name: "ram",
            base: u64::from(RAM_BASE),
            size: u64::from(RAM_SIZE),
        },
        Region {
            name: "irqc",
            base: u64::from(IRQC_BASE),
            size: SIZE,
        },
    ])
    .unwrap();
    let bus = t.add_component("soc.bus", Box::new(bus));
    let (main, handler) = program();
    let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: cycles(1),
        },
        &RamImage {
            image_hash: [0x3c; 32],
            segments: vec![
                Segment {
                    offset: 0,
                    bytes: bytes(&main),
                },
                Segment {
                    offset: u64::from(HANDLER - RAM_BASE),
                    bytes: bytes(&handler),
                },
            ],
        },
    )
    .unwrap();
    let ram = t.add_component("soc.ram", Box::new(ram));
    let irqc = SimpleIrqController::new(IrqControllerConfig {
        sources: 1,
        latency: cycles(0),
    })
    .unwrap();
    let irqc = t.add_component("soc.irqc", Box::new(irqc));
    let source = t.add_component(
        "soc.src",
        Box::new(LevelScript {
            clock,
            levels: levels.to_vec(),
        }),
    );
    assert_eq!((cpu, irqc, source), (CPU, IRQC, SOURCE));
    t.connect((cpu, "mem"), (bus, "cpu"), None);
    t.connect((bus, "ram"), (ram, "mem"), Some(cycles(1)));
    t.connect((bus, "irqc"), (irqc, "mem"), Some(cycles(1)));
    t.connect((source, "irq"), (irqc, "src0"), Some(cycles(1)));
    t.connect((irqc, "cpu"), (cpu, "irq"), Some(cycles(1)));
    t.elaborate(SessionConfig::default()).unwrap()
}

struct Run {
    events: Vec<Dispatched>,
    views: Vec<(StateView, StateView)>,
    trace: Trace,
}

fn run(levels: &[(u64, bool)], limit: u64) -> Run {
    let views = Rc::default();
    let mut rt = build(levels, limit);
    rt.add_observer(Box::new(Recorder(Rc::clone(&views))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    Run {
        events,
        views: views.take(),
        trace: rt.take_trace().unwrap(),
    }
}

fn u(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn level_of(ev: &Dispatched) -> Option<bool> {
    match ev.delivery {
        Delivered::Message {
            msg: Message::Irq(IrqMsg::Level { asserted }),
            ..
        } => Some(asserted),
        _ => None,
    }
}

impl Run {
    /// Each controller output change: the index of the event that decided it, and the
    /// level.
    fn decisions(&self) -> Vec<(usize, bool)> {
        self.trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.component == IRQC)
            .filter(|r| r.kind == MEIP_KIND)
            .map(|r| {
                let TraceAt::Event(key) = r.at else {
                    panic!("{r:?}")
                };
                let i = self.events.iter().position(|e| e.key == key).unwrap();
                (i, r.fields[0].1 == Value::Bool(true))
            })
            .collect()
    }

    /// Each level delivered to the CPU: the event index and the level.
    fn at_cpu(&self) -> Vec<(usize, bool)> {
        self.events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.target == CPU)
            .filter_map(|(i, e)| level_of(e).map(|l| (i, l)))
            .collect()
    }

    /// The event indices of the CPU's interrupt entries.
    fn entries(&self) -> Vec<usize> {
        self.trace
            .records
            .iter()
            .filter(|r| r.component == CPU && r.kind == INTERRUPT_KIND)
            .map(|r| {
                let TraceAt::Event(key) = r.at else {
                    panic!("{r:?}")
                };
                self.events.iter().position(|e| e.key == key).unwrap()
            })
            .collect()
    }

    /// Every decision reaches the CPU one link latency later, in `Complete`, and `mip`
    /// follows the delivered level, not the decision.
    fn check_delivery(&self) {
        let decisions = self.decisions();
        let at_cpu = self.at_cpu();
        assert_eq!(decisions.len(), at_cpu.len());
        for (&(cause, level), &(got, delivered)) in decisions.iter().zip(&at_cpu) {
            assert_eq!(level, delivered);
            let (cause, got_ev) = (&self.events[cause], &self.events[got]);
            assert_eq!(got_ev.key.phase, Phase::Complete);
            assert_eq!(got_ev.key.tick.0, cause.key.tick.0 + TICKS_PER_CYCLE);
        }
        let mut meip = 0;
        for (i, (cpu, _)) in self.views.iter().enumerate() {
            if let Some(&(_, level)) = at_cpu.iter().find(|(at, _)| *at == i) {
                meip = if level { 0x800 } else { 0 };
            }
            assert_eq!(u(cpu, "mip"), meip, "after event {i}");
        }
    }
}

/// The source rises after the program has enabled it: the controller's output rises in
/// the level's own handler, the CPU sees MEIP one cycle later and enters (and re-enters
/// after every `MRET`) while the line is held; after the source falls, MEIP falls and the
/// loop runs again.
#[test]
fn a_source_level_reaches_mip_through_the_controller() {
    let r = run(&[(80, true), (200, false)], 120);
    r.check_delivery();
    let decisions = r.decisions();
    assert_eq!(
        decisions.iter().map(|d| d.1).collect::<Vec<_>>(),
        [true, false]
    );
    for &(cause, _) in &decisions {
        let ev = &r.events[cause];
        assert_eq!((ev.target, ev.key.phase), (IRQC, Phase::Complete));
        assert!(level_of(ev).is_some());
    }
    let at_cpu = r.at_cpu();
    let entries = r.entries();
    assert!(entries.len() > 1, "entries and re-entries: {entries:?}");
    assert!(
        entries.iter().all(|&e| at_cpu[0].0 < e && e < at_cpu[1].0),
        "entries only while MEIP is set"
    );
    let first = &r
        .trace
        .records
        .iter()
        .find(|r| r.kind == INTERRUPT_KIND)
        .unwrap();
    let handler = first.fields.iter().find(|f| f.0 == "handler").unwrap();
    assert_eq!(handler.1, Value::U64(u64::from(HANDLER)));
    let (cpu, irqc) = r.views.last().unwrap();
    assert_eq!(
        u(cpu, "x7"),
        1,
        "the handler read PENDING with the line held"
    );
    assert!(u(cpu, "x6") >= entries.len() as u64 - 1);
    assert_eq!(u(cpu, "mip"), 0);
    assert_eq!(
        (u(irqc, "pending"), u(irqc, "enable"), irqc.get("out")),
        (0, 1, Some(&Value::Bool(false)))
    );
    // The loop ran after the fall.
    let after = &r.views[at_cpu[1].0].0;
    assert!(u(cpu, "x5") > u(after, "x5"));
}

/// The source rises before the program enables it: the controller's output rises at the
/// acceptance of the `ENABLE` store, the CPU sees MEIP one cycle later, and enters once
/// MIE is set.
#[test]
fn an_enable_store_raises_mip_at_its_acceptance() {
    let r = run(&[(2, true), (200, false)], 120);
    r.check_delivery();
    let decisions = r.decisions();
    assert_eq!(
        decisions.iter().map(|d| d.1).collect::<Vec<_>>(),
        [true, false]
    );
    let cause = &r.events[decisions[0].0];
    assert_eq!(cause.target, IRQC);
    assert!(
        matches!(
            cause.delivery,
            Delivered::Message {
                msg: Message::MemV1(MemMsg::WriteReq { addr: ENABLE, .. }),
                ..
            }
        ),
        "{cause:?}"
    );
    // The controller's state already changed at acceptance.
    let (_, irqc) = &r.views[decisions[0].0];
    assert_eq!(irqc.get("out"), Some(&Value::Bool(true)));
    assert!(!r.entries().is_empty());
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let levels = [(40, true), (90, false)];
    let limit = 60;
    let reference = {
        let mut rt = build(&levels, limit);
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        (events, rt.state_digest().unwrap(), rt.execution_digest())
    };
    assert!(
        reference
            .0
            .iter()
            .any(|e| e.target == CPU && level_of(e).is_some())
    );
    for k in 0..=reference.0.len() {
        let mut rt = build(&levels, limit);
        rt.init().unwrap();
        for _ in 0..k {
            rt.step().unwrap().unwrap();
        }
        let bytes = rt.snapshot().unwrap();
        let mut fresh = build(&levels, limit);
        fresh.restore(&bytes).unwrap();
        let rest: Vec<_> = std::iter::from_fn(|| fresh.step().unwrap()).collect();
        assert_eq!(fresh.fault(), None);
        assert_eq!(rest, reference.0[k..], "checkpoint after {k} events");
        assert_eq!(
            (fresh.state_digest().unwrap(), fresh.execution_digest()),
            (reference.1, reference.2),
            "checkpoint after {k} events"
        );
    }
}
