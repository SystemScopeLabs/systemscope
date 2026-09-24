//! The `M2` CPU's machine external interrupt in the real runtime (`docs/m2-design.md` §5,
//! §6.4, §13.4, M2.2b): an `M2` CPU, the address bus, and RAM, with the CPU's `irq` port
//! driven by a test-only scripted `irq.v0` source. No interrupt controller exists yet
//! (M2.3); the source stands in for one.
//!
//! The tests check the frozen boundary against the runtime's own event order: a level
//! delivered in the same `(tick, Complete)` as an instruction's response is seen by that
//! instruction's commit whichever of the two is dispatched first; every interrupt entry
//! matches the pure oracle; architectural state changes only in `Commit`; checkpoints at
//! every event boundary resume identically; and tracing or observing changes nothing.

mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::rc::Rc;

use common::asm::*;
use common::mei::{Boundary, MEI_CAUSE, boundary};
use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase, ScheduleWhen};
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceAt, TraceOrigin, TraceRecord, Value};
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, Segment};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::{COMMIT, COMMIT_KIND, HALT_KIND, INTERRUPT_KIND};
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: u32 = 0x1000;
const HANDLER: u32 = RAM_BASE + 0x100;
/// The loop the program spins in once the interrupt is enabled.
const LOOP: u32 = RAM_BASE + 0x1c;
/// Ticks per cycle of the 1 GHz CPU clock.
const TICKS_PER_CYCLE: u64 = SimulationClock::DEFAULT_TICKS_PER_SECOND / 1_000_000_000;

const CPU: ComponentId = ComponentId(0);
const IRQ: ComponentId = ComponentId(3);

const MRET: u32 = 0x3020_0073;

fn csr_word(funct3: u32, rd: u32, field: u32, csr: u16) -> u32 {
    u32::from(csr) << 20 | field << 15 | funct3 << 12 | rd << 7 | 0x73
}

/// Sets `mtvec` to the handler, enables MEIE and MIE, then counts in `x5` forever. The
/// handler counts in `x6` and returns with `MRET`.
fn program() -> (Vec<u32>, Vec<u32>) {
    let main = vec![
        lui(1, RAM_BASE >> 12),
        addi(1, 1, 0x100),
        csr_word(1, 0, 1, 0x305), // csrrw x0, mtvec, x1
        addi(2, 0, 0x7ff),
        addi(2, 2, 1),
        csr_word(2, 0, 2, 0x304), // csrrs x0, mie, x2
        csr_word(6, 0, 8, 0x300), // csrrsi x0, mstatus, 8
        addi(5, 5, 1),            // LOOP
        jal(0, -4),
    ];
    assert_eq!(RAM_BASE + 4 * 7, LOOP);
    let handler = vec![addi(6, 6, 1), MRET];
    (main, handler)
}

/// When a scripted level is created, which decides its global sequence among the events
/// of its `(tick, Complete)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Created {
    /// At `init`, before everything else.
    Init,
    /// In the `Complete` of its own tick, after everything already scheduled there.
    Late,
}

/// One scripted level, delivered at the `Complete` of CPU cycle `cycle`.
#[derive(Clone, Copy, Debug)]
struct Pulse {
    cycle: u64,
    asserted: bool,
    created: Created,
}

/// A test-only `irq.v0` source that sends a fixed script of levels. Its only state is the
/// script, which is configuration; its wakes and messages are the runtime's.
struct ScriptedIrq {
    clock: ClockDomainId,
    script: Vec<Pulse>,
}

/// Token bit of the second, `Complete`-phase wake of a [`Created::Late`] pulse.
const LATE: u64 = 1 << 32;

impl ScriptedIrq {
    fn level(&self, i: u64) -> Message {
        Message::Irq(IrqMsg::Level {
            asserted: self.script[i as usize].asserted,
        })
    }
}

impl Component for ScriptedIrq {
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
        for (i, pulse) in self.script.iter().enumerate() {
            let at = ScheduleWhen::Cycles {
                domain: self.clock,
                k: pulse.cycle,
            };
            match pulse.created {
                Created::Init => ctx.send(PortId(0), self.level(i as u64), at, Phase::Complete)?,
                Created::Late => ctx.wake_self(at, Phase::Transfer, i as u64)?,
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        match *ev {
            Delivered::Wake { token } if token & LATE == 0 => {
                ctx.wake_self(ScheduleWhen::Now, Phase::Complete, token | LATE)
            }
            Delivered::Wake { token } => {
                let msg = self.level(token & !LATE);
                ctx.send(PortId(0), msg, ScheduleWhen::Now, Phase::Complete)
            }
            Delivered::Message { .. } => Err(SimError::ComponentFault("irq script: a message")),
        }
    }

    fn snapshot_schema_version(&self) -> u32 {
        1
    }

    fn snapshot(&self, _: &mut SnapshotWriter) {}

    fn restore(&mut self, _: &mut SnapshotReader<'_>, schema: u32) -> Result<(), RestoreError> {
        if schema == 1 {
            Ok(())
        } else {
            Err(RestoreError::InvalidState("irq script: unknown schema"))
        }
    }
}

/// Records the CPU's view after every event.
struct Recorder(Rc<RefCell<Vec<StateView>>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0.borrow_mut().push(world.inspect(CPU).unwrap());
        Control::Continue
    }
}

fn build(script: &[Pulse], limit: u64) -> Runtime {
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
        entry: RAM_BASE,
        max_instructions: NonZeroU64::new(limit).unwrap(),
        profile: Rv32iProfile::M2,
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
    let (main, handler) = program();
    let bytes = |words: &[u32]| words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let ram = Ram::new(
        RamConfig {
            size: u64::from(RAM_SIZE),
            latency: LinkLatency::Cycles {
                domain: clock,
                k: 1,
            },
        },
        &RamImage {
            image_hash: [0x3e; 32],
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
    let irq = t.add_component(
        "soc.irq",
        Box::new(ScriptedIrq {
            clock,
            script: script.to_vec(),
        }),
    );
    assert_eq!((cpu, irq), (CPU, IRQ));
    let link = LinkLatency::Cycles {
        domain: clock,
        k: 1,
    };
    t.connect((cpu, "mem"), (bus, "cpu"), None);
    t.connect((bus, "ram"), (ram, "mem"), Some(link));
    t.connect((irq, "irq"), (cpu, "irq"), None);
    t.elaborate(SessionConfig::default()).unwrap()
}

/// A finished run.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    events: Vec<Dispatched>,
    /// The CPU's view after each event.
    views: Vec<StateView>,
    trace: Trace,
    state: [u8; 32],
    execution: [u8; 32],
    limit: u64,
}

fn run(script: &[Pulse], limit: u64) -> Run {
    let views = Rc::default();
    let mut rt = build(script, limit);
    rt.add_observer(Box::new(Recorder(Rc::clone(&views))));
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(rt.fault(), None);
    Run {
        events,
        views: views.take(),
        trace: rt.take_trace().unwrap(),
        state: rt.state_digest().unwrap(),
        execution: rt.execution_digest(),
        limit,
    }
}

fn u(view: &StateView, name: &str) -> u32 {
    match view.get(name) {
        Some(Value::U64(v)) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

fn field(record: &TraceRecord, name: &str) -> u32 {
    match record.fields.iter().find(|f| f.0 == name) {
        Some((_, Value::U64(v))) => u32::try_from(*v).unwrap(),
        other => panic!("{name}: {other:?}"),
    }
}

const CSRS: [&str; 8] = [
    "mstatus", "mie", "mip", "mtvec", "mscratch", "mepc", "mcause", "mtval",
];

/// `pc`, the registers, `instret`, and every CSR but `mip`.
fn arch(view: &StateView) -> Vec<u32> {
    let mut v = vec![u(view, "pc")];
    v.extend((1..32).map(|i| u(view, &format!("x{i}"))));
    v.push(u(view, "instret"));
    v.extend(CSRS.iter().filter(|n| **n != "mip").map(|n| u(view, n)));
    v
}

fn is_level(ev: &Dispatched) -> bool {
    ev.target == CPU
        && matches!(
            ev.delivery,
            Delivered::Message {
                msg: Message::Irq(_),
                ..
            }
        )
}

fn is_fetch_response(ev: &Dispatched) -> bool {
    ev.target == CPU
        && matches!(
            ev.delivery,
            Delivered::Message {
                msg: Message::MemV1(MemMsg::ReadResp { .. }),
                ..
            }
        )
}

impl Run {
    /// The CPU's trace records, by the event that made them.
    fn cpu_records(&self) -> BTreeMap<EventKey, Vec<&TraceRecord>> {
        let mut by_event: BTreeMap<EventKey, Vec<&TraceRecord>> = BTreeMap::new();
        for r in &self.trace.records {
            if r.origin == TraceOrigin::Component && r.component == CPU {
                let TraceAt::Event(key) = r.at else {
                    panic!("the CPU traces nothing at init")
                };
                by_event.entry(key).or_default().push(r);
            }
        }
        by_event
    }

    fn cpu_record_list(&self) -> Vec<&TraceRecord> {
        self.cpu_records().into_values().flatten().collect()
    }

    /// The whole run against the frozen rules, event by event: `mip` changes only when a
    /// level is delivered, everything else only in a commit; a commit retires exactly one
    /// instruction; and every boundary after an instruction that writes no CSR is exactly
    /// the oracle's.
    fn check(&self) -> usize {
        let records = self.cpu_records();
        let mut before = self.views[0].clone();
        let mut entries = 0;
        for (ev, after) in self.events.iter().zip(&self.views).skip(1) {
            let commit = ev.target == CPU && ev.delivery == (Delivered::Wake { token: COMMIT });
            if is_level(ev) {
                assert_eq!(ev.key.phase, Phase::Complete);
                assert_eq!(arch(after), arch(&before), "a level changes only mip");
            } else {
                assert_eq!(u(after, "mip"), u(&before, "mip"), "{ev:?}");
            }
            if !commit {
                assert_eq!(arch(after), arch(&before), "state changed outside commit");
                assert!(
                    !records.contains_key(&ev.key),
                    "the CPU traces only in commits"
                );
                before = after.clone();
                continue;
            }
            assert_eq!(ev.key.phase, Phase::Commit);
            let recs = &records[&ev.key];
            assert_eq!(recs[0].kind, COMMIT_KIND);
            assert_eq!(
                u(after, "instret"),
                u(&before, "instret") + 1,
                "one retirement"
            );
            let insn = field(recs[0], "insn");
            let next = field(recs[0], "next_pc");
            let is_csr = insn & 0x7f == 0x73 && insn >> 12 & 7 != 0;
            if !is_csr {
                // The CSRs after the instruction: unchanged, or MRET's.
                let mut mstatus = u(&before, "mstatus");
                if insn == MRET {
                    mstatus = 0x1800 | (mstatus >> 7 & 1) << 3 | 0x80;
                }
                let expected = boundary(
                    u64::from(u(after, "instret")),
                    self.limit,
                    next,
                    mstatus,
                    u(&before, "mie"),
                    u(&before, "mip"),
                    u(&before, "mtvec"),
                );
                match expected {
                    Boundary::InstructionLimit => {
                        assert_eq!(recs.len(), 2);
                        assert_eq!(recs[1].kind, HALT_KIND);
                        assert_eq!(u(after, "pc"), next);
                        assert_eq!(u(after, "mstatus"), mstatus);
                    }
                    Boundary::Fetch(mei) => {
                        assert_eq!(u(after, "pc"), mei.new_pc);
                        assert_eq!(u(after, "mstatus"), mei.new_mstatus);
                        assert_eq!(u(after, "mepc"), mei.mepc.unwrap_or(u(&before, "mepc")));
                        assert_eq!(
                            u(after, "mcause"),
                            mei.mcause.unwrap_or(u(&before, "mcause"))
                        );
                        assert_eq!(u(after, "mtval"), mei.mtval.unwrap_or(u(&before, "mtval")));
                        assert_eq!(recs.len(), 1 + usize::from(mei.taken), "{recs:?}");
                    }
                }
            }
            if recs.len() == 2 && recs[1].kind != HALT_KIND {
                assert_eq!(recs[1].kind, INTERRUPT_KIND);
                assert_eq!(field(recs[1], "mepc"), next);
                assert_eq!(field(recs[1], "mcause"), MEI_CAUSE);
                assert_eq!(field(recs[1], "handler"), HANDLER);
                entries += 1;
            }
            before = after.clone();
        }
        entries
    }
}

/// The level goes up while the program loops and stays up for a while: the loop is
/// interrupted at a retirement boundary, every `MRET` re-enters while the line is held,
/// and once it falls the handler returns to the loop for good.
#[test]
fn a_held_line_interrupts_the_loop_and_every_mret_reenters() {
    let script = [
        Pulse {
            cycle: 60,
            asserted: true,
            created: Created::Init,
        },
        Pulse {
            cycle: 110,
            asserted: false,
            created: Created::Late,
        },
    ];
    let r = run(&script, 80);
    let entries = r.check();
    let records = r.cpu_record_list();
    let interrupts: Vec<_> = records
        .iter()
        .filter(|r| r.kind == INTERRUPT_KIND)
        .collect();
    assert_eq!(interrupts.len(), entries);
    assert!(
        entries >= 3,
        "one entry and at least two re-entries: {entries}"
    );
    // The first entry interrupts the loop; every later one is a re-entry after MRET,
    // with the same mepc.
    let mepc = field(interrupts[0], "mepc");
    assert!(mepc == LOOP || mepc == LOOP + 4, "{mepc:#x}");
    for i in interrupts.iter().skip(1) {
        assert_eq!(field(i, "mepc"), mepc);
    }
    let mut first = true;
    for (a, b) in records.iter().zip(records.iter().skip(1)) {
        if b.kind == INTERRUPT_KIND {
            assert_eq!(a.kind, COMMIT_KIND);
            if !first {
                assert_eq!(field(a, "insn"), MRET, "re-entries follow MRET");
            }
            first = false;
        }
    }
    // No entry before the level arrives, none after it falls, and the program returned to
    // the loop and ran to the limit.
    let level_at: Vec<EventKey> = r
        .events
        .iter()
        .filter(|e| is_level(e))
        .map(|e| e.key)
        .collect();
    assert_eq!(level_at.len(), 2);
    for (key, recs) in r.cpu_records() {
        if recs.iter().any(|r| r.kind == INTERRUPT_KIND) {
            assert!(key > level_at[0] && key < level_at[1]);
        }
    }
    let last = r.views.last().unwrap();
    assert_eq!(u(last, "instret"), 80);
    assert_eq!(u(last, "x6"), u32::try_from(entries).unwrap());
    assert_eq!(u(last, "mstatus"), 0x1888, "back in the loop with MIE set");
    let pc = u(last, "pc");
    assert!(pc == LOOP || pc == LOOP + 4, "{pc:#x}");
}

/// §5.1: a level delivered in the same `(tick, Complete)` as an instruction's fetch
/// response is seen by that instruction's commit, whether the runtime dispatches it
/// before or after the response. Only the event order differs.
#[test]
fn a_same_tick_level_is_seen_whatever_the_dispatch_order() {
    // Find the tick of the fifth loop fetch's response in a run without levels.
    let quiet = run(&[], 60);
    assert_eq!(quiet.check(), 0);
    let responses: Vec<&Dispatched> = quiet
        .events
        .iter()
        .filter(|e| is_fetch_response(e))
        .collect();
    let target = responses[7 + 5];
    let tick = target.key.tick.0;
    assert_eq!(tick % TICKS_PER_CYCLE, 0);
    let cycle = tick / TICKS_PER_CYCLE;
    let mut outcomes = Vec::new();
    for created in [Created::Init, Created::Late] {
        let r = run(
            &[Pulse {
                cycle,
                asserted: true,
                created,
            }],
            60,
        );
        r.check();
        // The level and the response share the (tick, Complete).
        let level = r.events.iter().position(is_level).unwrap();
        let response = r
            .events
            .iter()
            .position(|e| is_fetch_response(e) && e.key.tick.0 == tick)
            .unwrap();
        assert_eq!(r.events[level].key.tick, r.events[response].key.tick);
        assert_eq!(r.events[level].key.phase, r.events[response].key.phase);
        let level_first = level < response;
        // That instruction's commit takes the interrupt.
        let commit = r.events[response..]
            .iter()
            .find(|e| e.target == CPU && e.delivery == (Delivered::Wake { token: COMMIT }))
            .unwrap();
        let recs = &r.cpu_records()[&commit.key];
        assert_eq!(recs.len(), 2, "{created:?}: {recs:?}");
        assert_eq!(recs[1].kind, INTERRUPT_KIND);
        let first = r
            .cpu_record_list()
            .into_iter()
            .position(|r| r.kind == INTERRUPT_KIND)
            .unwrap();
        outcomes.push((
            level_first,
            r.cpu_record_list()
                .into_iter()
                .map(|r| (r.kind, r.fields.clone()))
                .collect::<Vec<_>>(),
            first,
            r.views.last().unwrap().clone(),
        ));
    }
    let (a, b) = (&outcomes[0], &outcomes[1]);
    assert!(a.0 && !b.0, "the two runs dispatch the level on both sides");
    assert_eq!((&a.1, a.2, &a.3), (&b.1, b.2, &b.3));
}

/// §6.4: a checkpoint after any event, including those with a pending level, mid-handler,
/// and right after an entry, resumes identically: same events, trace, and digests.
#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let script = [
        Pulse {
            cycle: 40,
            asserted: true,
            created: Created::Late,
        },
        Pulse {
            cycle: 70,
            asserted: false,
            created: Created::Init,
        },
    ];
    let limit = 40;
    let expected = run(&script, limit);
    assert!(expected.check() >= 2);
    let end = |mut rt: Runtime, mut events: Vec<Dispatched>| {
        events.extend(std::iter::from_fn(|| rt.step().unwrap()));
        assert_eq!(rt.fault(), None);
        let trace = rt.take_trace().unwrap();
        (
            events,
            trace.canonical_bytes(),
            rt.state_digest().unwrap(),
            rt.execution_digest(),
        )
    };
    let want = (
        expected.events.clone(),
        expected.trace.canonical_bytes(),
        expected.state,
        expected.execution,
    );
    for k in 0..=expected.events.len() {
        let (snapshot, prefix, events) = {
            let mut rt = build(&script, limit);
            rt.start_trace().unwrap();
            rt.init().unwrap();
            let events: Vec<_> = (0..k).map(|_| rt.step().unwrap().unwrap()).collect();
            (rt.snapshot().unwrap(), rt.take_trace().unwrap(), events)
        };
        let mut rt = build(&script, limit);
        rt.restore(&snapshot).unwrap();
        rt.resume_trace(prefix).unwrap();
        assert_eq!(end(rt, events), want, "checkpoint after {k} events");
    }
}

/// §13.4 in miniature: tracing and an extra observer change neither the events, the
/// interrupt timing, the final state, nor the digests.
#[test]
fn observation_changes_nothing() {
    let script = [Pulse {
        cycle: 50,
        asserted: true,
        created: Created::Init,
    }];
    let traced = run(&script, 50);
    traced.check();
    let mut rt = build(&script, 50);
    let count = Rc::new(RefCell::new(0u64));
    struct Counter(Rc<RefCell<u64>>);
    impl Observer for Counter {
        fn on_after_dispatch(&mut self, _: &EventView<'_>, _: &WorldView<'_>) -> Control {
            *self.0.borrow_mut() += 1;
            Control::Continue
        }
    }
    rt.add_observer(Box::new(Counter(Rc::clone(&count))));
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    assert_eq!(events, traced.events);
    assert_eq!(*count.borrow(), events.len() as u64);
    assert_eq!(rt.state_digest().unwrap(), traced.state);
    assert_eq!(rt.execution_digest(), traced.execution);
    // The same run again, traced, is identical.
    assert_eq!(run(&script, 50), traced);
}
