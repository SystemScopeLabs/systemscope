//! `Rv32iCpu` running programs against the real address bus and RAM in the real runtime
//! (`docs/m1-design.md` §5.3–§5.7): end-to-end programs, precise commit and traps, and
//! checkpoint/restore at every event boundary.
//!
//! An observer records the CPU's `inspect` view after every event, so the tests see the
//! architectural state at each step, not only at the end.

mod common;

use std::cell::RefCell;
use std::num::NonZeroU64;
use std::rc::Rc;

use common::asm::*;
use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::event::Phase;
use systemscope_contracts::observe::{Control, EventView, Observer, StateView, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::{MemMsg, ReadOutcome, WriteOutcome};
use systemscope_contracts::time::{ClockDomainId, Frequency, Rounding, SimulationClock, Tick};
use systemscope_contracts::topology::LinkLatency;
use systemscope_contracts::trace::{TraceOrigin, TraceRecord, Value};
use systemscope_platform::{AddressBus, Ram, RamConfig, RamImage, Region, Segment};
use systemscope_runtime::runtime::{Dispatched, Runtime, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_runtime::trace::Trace;
use systemscope_rv32i::cpu::{COMMIT, COMMIT_KIND, HALT_KIND, TRAP_KIND};
use systemscope_rv32i::{Rv32iConfig, Rv32iCpu, Rv32iProfile};

/// Main memory: 16 KiB. Programs start at its base; data lives one page up.
const RAM_BASE: u32 = 0x8000_0000;
const RAM_SIZE: u32 = 0x4000;
const DATA: u32 = RAM_BASE + 0x1000;
/// Unmapped.
const GAP: u32 = 0x4000_0000;

const CPU: ComponentId = ComponentId(0);
const BUS: ComponentId = ComponentId(1);
const RAM: ComponentId = ComponentId(2);

/// `lui x10, DATA >> 12`: points x10 at the data page.
fn data_base() -> u32 {
    lui(10, DATA >> 12)
}

/// A program image and how to run it.
#[derive(Clone)]
struct Machine {
    entry: u32,
    program: Vec<u32>,
    data: Vec<u8>,
    ram_cycles: u64,
    max_instructions: u64,
}

impl Machine {
    fn new(program: Vec<u32>) -> Machine {
        Machine {
            entry: RAM_BASE,
            max_instructions: program.len() as u64,
            program,
            data: Vec::new(),
            ram_cycles: 1,
        }
    }

    fn data(mut self, data: &[u8]) -> Machine {
        self.data = data.to_vec();
        self
    }

    fn ram_cycles(mut self, k: u64) -> Machine {
        self.ram_cycles = k;
        self
    }

    fn max(mut self, n: u64) -> Machine {
        self.max_instructions = n;
        self
    }

    fn entry(mut self, entry: u32) -> Machine {
        self.entry = entry;
        self
    }

    /// CPU → bus → RAM on one 1 GHz clock, with the CPU's views recorded into `views`.
    fn build(&self, views: Rc<RefCell<Vec<StateView>>>) -> Runtime {
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
            entry: self.entry,
            max_instructions: NonZeroU64::new(self.max_instructions).unwrap(),
            profile: Rv32iProfile::M1,
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
        let code: Vec<u8> = self.program.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut segments = vec![Segment {
            offset: 0,
            bytes: code,
        }];
        if !self.data.is_empty() {
            segments.push(Segment {
                offset: u64::from(DATA - RAM_BASE),
                bytes: self.data.clone(),
            });
        }
        let ram = Ram::new(
            RamConfig {
                size: u64::from(RAM_SIZE),
                latency: cycles(clock, self.ram_cycles),
            },
            &RamImage {
                image_hash: [0x5c; 32],
                segments,
            },
        )
        .unwrap();
        let ram = t.add_component("soc.ram", Box::new(ram));
        assert_eq!((cpu, bus, ram), (CPU, BUS, RAM));
        t.connect((cpu, "mem"), (bus, "cpu"), None);
        t.connect((bus, "ram"), (ram, "mem"), Some(cycles(clock, 1)));
        let mut rt = t.elaborate(SessionConfig::default()).unwrap();
        rt.add_observer(Box::new(Recorder(views)));
        rt
    }

    /// Runs to completion.
    fn run(&self) -> Run {
        let views = Rc::default();
        let mut rt = self.build(Rc::clone(&views));
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
        assert_eq!(rt.fault(), None);
        let views = views.take();
        assert_eq!(views.len(), events.len());
        let run = Run {
            events,
            views,
            trace: rt.take_trace().unwrap(),
            reset: self.entry,
        };
        run.assert_precise();
        run
    }
}

fn cycles(domain: ClockDomainId, k: u64) -> LinkLatency {
    LinkLatency::Cycles { domain, k }
}

/// Records the CPU's view after every event.
struct Recorder(Rc<RefCell<Vec<StateView>>>);

impl Observer for Recorder {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        self.0.borrow_mut().push(world.inspect(CPU).unwrap());
        Control::Continue
    }
}

/// A finished run.
struct Run {
    events: Vec<Dispatched>,
    /// The CPU's view after each event.
    views: Vec<StateView>,
    trace: Trace,
    reset: u32,
}

fn u(view: &StateView, name: &str) -> u64 {
    match view.get(name) {
        Some(Value::U64(v)) => *v,
        other => panic!("{name}: {other:?}"),
    }
}

fn s<'a>(view: &'a StateView, name: &str) -> &'a str {
    match view.get(name) {
        Some(Value::Str(v)) => v,
        other => panic!("{name}: {other:?}"),
    }
}

/// `pc`, `x1`…`x31`, `instret`: the architectural state in a view.
fn arch(view: &StateView) -> Vec<u64> {
    let mut v = vec![u(view, "pc")];
    v.extend((1..32).map(|i| u(view, &format!("x{i}"))));
    v.push(u(view, "instret"));
    v
}

fn is_commit_wake(ev: &Dispatched) -> bool {
    ev.target == CPU && ev.delivery == Delivered::Wake { token: COMMIT }
}

fn mem(ev: &Dispatched) -> Option<&MemMsg> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } => Some(msg),
        _ => None,
    }
}

impl Run {
    fn last(&self) -> &StateView {
        self.views.last().unwrap()
    }

    /// `x[i]`; `x0` is not stored, so it is not in the view.
    fn reg(&self, i: u8) -> u32 {
        if i == 0 {
            assert_eq!(self.last().get("x0"), None);
            return 0;
        }
        u(self.last(), &format!("x{i}")) as u32
    }

    fn pc(&self) -> u32 {
        u(self.last(), "pc") as u32
    }

    fn instret(&self) -> u64 {
        u(self.last(), "instret")
    }

    /// Requests the CPU sent, in order.
    fn requests(&self) -> Vec<&MemMsg> {
        self.events
            .iter()
            .filter(|ev| ev.source == CPU && ev.target == BUS)
            .filter_map(mem)
            .collect()
    }

    /// Requests that reached the RAM, in order.
    fn at_ram(&self) -> Vec<&MemMsg> {
        self.events
            .iter()
            .filter(|ev| ev.target == RAM)
            .filter_map(mem)
            .collect()
    }

    fn cpu_records(&self) -> Vec<&TraceRecord> {
        self.trace
            .records
            .iter()
            .filter(|r| r.origin == TraceOrigin::Component && r.component == CPU)
            .collect()
    }

    fn commits(&self) -> Vec<&TraceRecord> {
        self.cpu_records()
            .into_iter()
            .filter(|r| r.kind == COMMIT_KIND)
            .collect()
    }

    fn committed_pcs(&self) -> Vec<u32> {
        self.commits()
            .iter()
            .map(|r| match r.fields[0] {
                ("pc", Value::U64(pc)) => pc as u32,
                _ => panic!("pc is the first commit field"),
            })
            .collect()
    }

    /// Precise state: the architectural state changes only in a `Wake(COMMIT)` in
    /// `Commit`, by exactly one retirement, and a trap changes nothing.
    fn assert_precise(&self) {
        let mut before = vec![u64::from(self.reset)];
        before.extend([0; 32]);
        for (ev, view) in self.events.iter().zip(&self.views) {
            let after = arch(view);
            if after != before {
                assert!(is_commit_wake(ev), "state changed outside commit: {ev:?}");
                assert_eq!(ev.key.phase, Phase::Commit);
                assert_eq!(after[32], before[32] + 1, "exactly one retirement");
            }
            if is_commit_wake(ev) && after == before {
                assert_eq!(
                    s(view, "halt"),
                    "trap",
                    "a commit that changes nothing traps"
                );
            }
            before = after;
        }
    }

    /// The single trap record's fields.
    fn trap(&self) -> Vec<(&'static str, Value)> {
        let traps: Vec<_> = self
            .cpu_records()
            .into_iter()
            .filter(|r| r.kind == TRAP_KIND)
            .collect();
        assert_eq!(traps.len(), 1);
        traps[0].fields.clone()
    }

    fn assert_trapped(&self, cause: &str, pc: u32, tval: u32) {
        let view = self.last();
        assert_eq!(s(view, "state"), "halted");
        assert_eq!(s(view, "halt"), "trap");
        assert_eq!(s(view, "cause"), cause);
        assert_eq!(u(view, "trap_pc"), u64::from(pc));
        assert_eq!(u(view, "tval"), u64::from(tval));
        // A trap leaves pc on the trapping instruction.
        assert_eq!(self.pc(), pc);
        assert!(
            self.trap()
                .contains(&("cause", Value::Str(cause.to_owned())))
        );
    }

    fn assert_limit(&self) {
        assert_eq!(s(self.last(), "state"), "halted");
        assert_eq!(s(self.last(), "halt"), "instruction_limit");
        let halts: Vec<_> = self
            .cpu_records()
            .into_iter()
            .filter(|r| r.kind == HALT_KIND)
            .map(|r| r.fields.clone())
            .collect();
        assert_eq!(halts, [vec![("instret", Value::U64(self.instret()))]]);
    }
}

fn read_resp(msg: &MemMsg) -> Option<&[u8]> {
    match msg {
        MemMsg::ReadResp {
            outcome: ReadOutcome::Data { data },
            ..
        } => Some(data),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// End-to-end programs.

/// A: ALU instructions retire in order, `x0` stays zero, and the CPU halts at the limit.
#[test]
fn a_alu_sequence_retires_in_order() {
    let program = vec![
        addi(1, 0, 5),
        addi(2, 0, -3),
        add(3, 1, 2),
        sub(4, 1, 2),
        xori(5, 2, -1),
        slli(6, 1, 4),
        lui(7, 0x12345),
        sltu(8, 1, 2),
        addi(0, 1, 1),
        FENCE,
    ];
    let n = program.len() as u32;
    let run = Machine::new(program).run();
    let expected = [5, -3i32 as u32, 2, 8, 2, 80, 0x1234_5000, 1];
    for (i, want) in (1..).zip(expected) {
        assert_eq!(run.reg(i), want, "x{i}");
    }
    assert_eq!(run.reg(0), 0);
    assert_eq!(run.instret(), u64::from(n));
    assert_eq!(run.pc(), RAM_BASE + 4 * n);
    assert_eq!(
        run.committed_pcs(),
        (0..n).map(|i| RAM_BASE + 4 * i).collect::<Vec<_>>()
    );
    // Only fetches reached memory.
    assert!(
        run.requests()
            .iter()
            .all(|m| matches!(m, MemMsg::ReadReq { len: 4, .. }))
    );
    run.assert_limit();
}

/// The `rv32.commit` fields of an ALU instruction (§5.7).
#[test]
fn commit_records_carry_the_retired_instruction() {
    let run = Machine::new(vec![addi(1, 0, 7), addi(0, 1, 1)]).run();
    let commits: Vec<_> = run.commits().iter().map(|r| r.fields.clone()).collect();
    let field = |pc: u32, insn: u32, rd: u64, v: u64| {
        vec![
            ("pc", Value::U64(u64::from(pc))),
            ("insn", Value::U64(u64::from(insn))),
            ("rd", Value::U64(rd)),
            ("rd_value", Value::U64(v)),
            ("next_pc", Value::U64(u64::from(pc + 4))),
        ]
    };
    assert_eq!(
        commits,
        [
            field(RAM_BASE, addi(1, 0, 7), 1, 7),
            // A write to x0 is reported as no write.
            field(RAM_BASE + 4, addi(0, 1, 1), 0, 0),
        ]
    );
}

/// B: a word stored and loaded back; the bytes that reach memory are little-endian.
#[test]
fn b_store_then_load_round_trips() {
    let program = vec![
        data_base(),
        lui(1, 0xdeadc),
        addi(1, 1, -0x111),
        sw(1, 10, 8),
        lw(2, 10, 8),
        lbu(3, 10, 8),
        lbu(4, 10, 11),
    ];
    let run = Machine::new(program).run();
    assert_eq!(run.reg(1), 0xdead_beef);
    assert_eq!(run.reg(2), 0xdead_beef);
    assert_eq!(run.reg(3), 0xef);
    assert_eq!(run.reg(4), 0xde);
    let writes: Vec<_> = run
        .at_ram()
        .into_iter()
        .filter(|m| matches!(m, MemMsg::WriteReq { .. }))
        .cloned()
        .collect();
    assert_eq!(writes.len(), 1);
    let MemMsg::WriteReq { addr, data, .. } = &writes[0] else {
        unreachable!()
    };
    assert_eq!(*addr, u64::from(DATA + 8 - RAM_BASE));
    assert_eq!(data, &[0xef, 0xbe, 0xad, 0xde]);
    // The store's and load's trace details.
    let commits = run.commits();
    assert_eq!(
        commits[3].fields[5..],
        [
            ("addr", Value::U64(u64::from(DATA + 8))),
            ("width", Value::U64(4)),
            ("value", Value::U64(0xdead_beef)),
        ]
    );
    assert_eq!(
        commits[4].fields[5..],
        [("addr", Value::U64(u64::from(DATA + 8)))]
    );
    run.assert_limit();
}

/// C: sub-word loads extend as their opcode says.
#[test]
fn c_sub_word_loads_extend_correctly() {
    let program = vec![
        data_base(),
        lb(1, 10, 0),
        lbu(2, 10, 0),
        lh(3, 10, 0),
        lhu(4, 10, 0),
        lh(5, 10, 2),
        lb(6, 10, 2),
        lw(7, 10, 0),
    ];
    let run = Machine::new(program).data(&[0x80, 0xff, 0x7f, 0x01]).run();
    let expected = [
        0xffff_ff80,
        0x80,
        0xffff_ff80,
        0xff80,
        0x017f,
        0x7f,
        0x017f_ff80,
    ];
    for (i, want) in (1..).zip(expected) {
        assert_eq!(run.reg(i), want, "x{i}");
    }
    // Each load asked for exactly its width.
    let lens: Vec<_> = run
        .requests()
        .iter()
        .filter_map(|m| match m {
            MemMsg::ReadReq { addr, len, .. } if *addr >= u64::from(DATA) => Some(*len),
            _ => None,
        })
        .collect();
    assert_eq!(lens, [1, 1, 2, 2, 2, 1, 4]);
}

/// D: a taken branch and a jump skip their wrong paths, which never retire.
#[test]
fn d_branches_never_retire_the_wrong_path() {
    let program = vec![
        addi(1, 0, 1),  // 0x00
        beq(1, 0, 8),   // 0x04: not taken
        bne(1, 0, 8),   // 0x08: taken to 0x10
        addi(2, 0, 99), // 0x0c: wrong path
        addi(3, 0, 7),  // 0x10
        jal(4, 8),      // 0x14: to 0x1c
        addi(5, 0, 55), // 0x18: wrong path
        addi(6, 0, 6),  // 0x1c
    ];
    let run = Machine::new(program).max(6).run();
    assert_eq!(run.reg(2), 0);
    assert_eq!(run.reg(5), 0);
    assert_eq!(run.reg(3), 7);
    assert_eq!(run.reg(4), RAM_BASE + 0x18);
    assert_eq!(run.reg(6), 6);
    let at = |o: u32| RAM_BASE + o;
    assert_eq!(
        run.committed_pcs(),
        [at(0), at(4), at(8), at(0x10), at(0x14), at(0x1c)]
    );
    // The wrong path was not even fetched.
    assert!(
        run.requests()
            .iter()
            .all(|m| !matches!(m, MemMsg::ReadReq { addr, .. } if *addr == u64::from(at(0xc)) || *addr == u64::from(at(0x18))))
    );
    run.assert_limit();
}

/// E: with a slow memory, the CPU waits: nothing retires between a request and its
/// response, and the result equals the fast-memory run's.
#[test]
fn e_the_cpu_waits_for_slow_memory() {
    let program = vec![
        data_base(),
        addi(1, 0, 0x5a),
        sw(1, 10, 0),
        lw(2, 10, 0),
        addi(3, 2, 1),
    ];
    let fast = Machine::new(program.clone()).run();
    let slow = Machine::new(program).ram_cycles(7).run();
    assert_eq!(arch(fast.last()), arch(slow.last()));
    assert_eq!(slow.reg(3), 0x5b);
    assert!(slow.events.last().unwrap().key.tick > fast.events.last().unwrap().key.tick);

    // Between each data request leaving the CPU and its response arriving, the CPU sits
    // in mem_wait with nothing retired, for at least the RAM's latency.
    let mut data_requests = 0;
    for (i, ev) in slow.events.iter().enumerate() {
        let Some(MemMsg::ReadReq { addr, .. } | MemMsg::WriteReq { addr, .. }) = mem(ev) else {
            continue;
        };
        if ev.target != BUS || *addr < u64::from(DATA) {
            continue;
        }
        data_requests += 1;
        let j = i + slow.events[i..]
            .iter()
            .position(|e| e.target == CPU && mem(e).is_some())
            .unwrap();
        assert!(
            slow.events[j].key.tick.0 - ev.key.tick.0 >= 7_000,
            "waited for the RAM"
        );
        let before = arch(&slow.views[i]);
        for view in &slow.views[i..j] {
            assert_eq!(s(view, "state"), "mem_wait");
            assert_eq!(arch(view), before);
        }
    }
    assert_eq!(data_requests, 2);
}

/// F: a load from unmapped memory is accessed, traps precisely, and leaves `rd` alone.
#[test]
fn f_load_access_fault_traps_precisely() {
    let program = vec![
        lui(10, GAP >> 12),
        addi(5, 0, 42),
        lw(5, 10, 0),
        addi(6, 0, 1),
    ];
    let run = Machine::new(program).run();
    run.assert_trapped("LoadAccessFault", RAM_BASE + 8, GAP);
    assert_eq!(run.reg(5), 42);
    assert_eq!(run.reg(6), 0);
    assert_eq!(run.instret(), 2);
    // The access was made.
    assert!(
        run.requests()
            .iter()
            .any(|m| matches!(m, MemMsg::ReadReq { addr, len: 4, .. } if *addr == u64::from(GAP)))
    );
    assert_eq!(
        run.trap(),
        [
            ("pc", Value::U64(u64::from(RAM_BASE + 8))),
            ("insn", Value::U64(u64::from(lw(5, 10, 0)))),
            ("cause", Value::Str("LoadAccessFault".to_owned())),
            ("tval", Value::U64(u64::from(GAP))),
        ]
    );
}

/// G: a store to unmapped memory traps and writes nothing anywhere.
#[test]
fn g_store_access_fault_writes_nothing() {
    let program = vec![lui(10, GAP >> 12), addi(1, 0, 1), sw(1, 10, 4)];
    let run = Machine::new(program).run();
    run.assert_trapped("StoreAccessFault", RAM_BASE + 8, GAP + 4);
    assert_eq!(run.instret(), 2);
    assert!(
        run.requests()
            .iter()
            .any(|m| matches!(m, MemMsg::WriteReq { .. }))
    );
    assert!(
        run.at_ram()
            .iter()
            .all(|m| matches!(m, MemMsg::ReadReq { .. }))
    );
    // The bus answered the store with a fault.
    assert!(run.events.iter().any(|ev| ev.target == CPU
        && matches!(
            mem(ev),
            Some(MemMsg::WriteResp {
                outcome: WriteOutcome::Fault { .. },
                ..
            })
        )));
}

/// H: misaligned loads and stores trap before any data request.
#[test]
fn h_misaligned_accesses_trap_without_a_request() {
    for (access, cause, offset) in [
        (lw(1, 10, 2), "LoadAddressMisaligned", 2),
        (lh(1, 10, 1), "LoadAddressMisaligned", 1),
        (sw(1, 10, 1), "StoreAddressMisaligned", 1),
        (sh(1, 10, 3), "StoreAddressMisaligned", 3),
    ] {
        let run = Machine::new(vec![data_base(), addi(1, 0, 9), access]).run();
        run.assert_trapped(cause, RAM_BASE + 8, DATA + offset);
        assert_eq!(run.reg(1), 9);
        // Three fetches and nothing else.
        let requests = run.requests();
        assert_eq!(requests.len(), 3, "{cause}");
        assert!(
            requests
                .iter()
                .all(|m| matches!(m, MemMsg::ReadReq { len: 4, .. }))
        );
    }
}

/// I: a load into `x0` still accesses memory and still traps on a fault, but writes
/// nothing.
#[test]
fn i_loads_into_x0_access_memory_but_write_nothing() {
    let program = vec![data_base(), lw(0, 10, 0), lui(11, GAP >> 12), lw(0, 11, 0)];
    let run = Machine::new(program).data(&[1, 2, 3, 4]).run();
    assert_eq!(run.reg(0), 0);
    let data_reads: Vec<_> = run
        .at_ram()
        .into_iter()
        .filter(
            |m| matches!(m, MemMsg::ReadReq { addr, .. } if *addr == u64::from(DATA - RAM_BASE)),
        )
        .collect();
    assert_eq!(data_reads.len(), 1);
    assert!(
        run.events
            .iter()
            .filter_map(mem)
            .any(|m| read_resp(m) == Some(&[1, 2, 3, 4]))
    );
    assert_eq!(
        run.commits()[1].fields[2..4],
        [("rd", Value::U64(0)), ("rd_value", Value::U64(0))]
    );
    run.assert_trapped("LoadAccessFault", RAM_BASE + 12, GAP);
    assert_eq!(run.instret(), 3);
}

/// A fetch from unmapped memory traps with `InstructionAccessFault`; there is no
/// instruction word, so the record's `insn` is 0.
#[test]
fn fetch_faults_trap_with_instruction_access_fault() {
    let run = Machine::new(vec![addi(1, 0, 1)]).entry(GAP).run();
    run.assert_trapped("InstructionAccessFault", GAP, GAP);
    assert_eq!(run.instret(), 0);
    assert_eq!(run.trap()[1], ("insn", Value::U64(0)));

    // Jumping out of memory faults on the next fetch, after the jump retired.
    let beyond = RAM_BASE + RAM_SIZE + 4;
    let run = Machine::new(vec![addi(1, 0, 1), jal(0, RAM_SIZE as i32)])
        .max(3)
        .run();
    run.assert_trapped("InstructionAccessFault", beyond, beyond);
    assert_eq!(run.instret(), 2);
    let run = Machine::new(vec![lui(1, GAP >> 12), jalr(0, 1, 0)])
        .max(3)
        .run();
    run.assert_trapped("InstructionAccessFault", GAP, GAP);
    assert_eq!(run.instret(), 2);
}

/// Illegal words, `ECALL`, and `EBREAK` trap with their §6 trap values.
#[test]
fn illegal_words_and_system_instructions_trap() {
    for (word, cause, tval) in [
        (0, "IllegalInstruction", 0),
        (0xffff_ffff, "IllegalInstruction", 0xffff_ffff),
        (ECALL, "EnvironmentCall", 0),
        (EBREAK, "Breakpoint", RAM_BASE + 4),
    ] {
        let run = Machine::new(vec![addi(1, 0, 3), word, addi(2, 0, 4)]).run();
        run.assert_trapped(cause, RAM_BASE + 4, tval);
        assert_eq!((run.reg(1), run.reg(2), run.instret()), (3, 0, 1));
        assert_eq!(run.trap()[1], ("insn", Value::U64(u64::from(word))));
    }
}

/// Each instruction takes its phases in order: fetch in `Request`, response in
/// `Complete`, commit in `Commit` of the same tick, next fetch at the next cycle.
#[test]
fn the_cpu_follows_its_phase_schedule() {
    let run = Machine::new(vec![data_base(), lw(1, 10, 0), addi(2, 0, 1)]).run();
    let cpu: Vec<_> = run.events.iter().filter(|ev| ev.target == CPU).collect();
    let mut last_commit = None;
    for pair in cpu.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if mem(a).is_some() && is_commit_wake(b) {
            assert_eq!(a.key.phase, Phase::Complete);
            assert_eq!(a.key.tick, b.key.tick, "commit in the response's tick");
            last_commit = Some(b.key.tick);
        }
        if a.key.phase == Phase::Commit {
            assert_eq!(b.key.phase, Phase::Request, "the next fetch is a request");
            assert_eq!(b.key.tick.0 - a.key.tick.0, 1_000, "one cycle later");
        }
    }
    assert!(last_commit.is_some());
    // The load's request goes out one cycle after its fetch completes.
    let states: Vec<_> = run.views.iter().map(|v| s(v, "state")).collect();
    assert!(states.contains(&"mem_issue"));
}

// ---------------------------------------------------------------------------------------
// Checkpoint/restore.

#[derive(Debug, PartialEq, Eq)]
struct End {
    state: [u8; 32],
    execution: [u8; 32],
    trace_bytes: Vec<u8>,
    trace: [u8; 32],
    events: Vec<Dispatched>,
}

fn end(mut rt: Runtime, events: Vec<Dispatched>) -> End {
    let trace = rt.take_trace().unwrap();
    End {
        state: rt.state_digest().unwrap(),
        execution: rt.execution_digest(),
        trace_bytes: trace.canonical_bytes(),
        trace: trace.digest(),
        events,
    }
}

fn uninterrupted(m: &Machine) -> End {
    let mut rt = m.build(Rc::default());
    rt.start_trace().unwrap();
    rt.init().unwrap();
    let events: Vec<_> = std::iter::from_fn(|| rt.step().unwrap()).collect();
    end(rt, events)
}

/// Stops after `k` events, then continues in a freshly elaborated runtime.
fn resumed(m: &Machine, k: usize) -> End {
    let (snapshot, prefix, mut events) = {
        let mut rt = m.build(Rc::default());
        rt.start_trace().unwrap();
        rt.init().unwrap();
        let events: Vec<_> = (0..k).map(|_| rt.step().unwrap().unwrap()).collect();
        (rt.snapshot().unwrap(), rt.take_trace().unwrap(), events)
    };
    let mut rt = m.build(Rc::default());
    rt.restore(&snapshot).unwrap();
    rt.resume_trace(prefix).unwrap();
    events.extend(std::iter::from_fn(|| rt.step().unwrap()));
    assert_eq!(rt.fault(), None);
    end(rt, events)
}

/// Every kind of step: ALU, store, load, a taken branch, slow memory, and a final load
/// fault, so the checkpoints cover every CPU state.
fn tour() -> Machine {
    Machine::new(vec![
        data_base(),
        addi(1, 0, 0x123),
        sw(1, 10, 4),
        lw(2, 10, 4),
        beq(1, 2, 8),
        addi(3, 0, 1),
        lhu(4, 10, 4),
        lui(11, GAP >> 12),
        lw(5, 11, 0),
    ])
    .ram_cycles(3)
}

/// Where the CPU is after `k` events of `run`: its state, and for `mem_wait`, whether
/// the outstanding request is a store.
fn label(run: &Run, k: usize) -> String {
    if k == 0 {
        return "before_fetch".to_owned();
    }
    let state = s(&run.views[k - 1], "state");
    if state != "halted" && is_commit_wake(&run.events[k - 1]) {
        return "after_commit".to_owned();
    }
    if state != "mem_wait" {
        return state.to_owned();
    }
    let store = run.events[..k]
        .iter()
        .rev()
        .find(|ev| ev.source == CPU && ev.target == BUS)
        .is_some_and(|ev| matches!(mem(ev), Some(MemMsg::WriteReq { .. })));
    if store {
        "mem_wait_store"
    } else {
        "mem_wait_load"
    }
    .to_owned()
}

#[test]
fn checkpoints_at_every_event_boundary_resume_identically() {
    let short = Machine::new(vec![addi(1, 0, 1), addi(2, 1, 1)]);
    for (m, every_state) in [(tour(), true), (short, false)] {
        let run = m.run();
        let expected = uninterrupted(&m);
        assert_eq!(expected.events, run.events);
        let mut labels = std::collections::BTreeSet::new();
        for k in 0..=run.events.len() {
            labels.insert(label(&run, k));
            assert_eq!(resumed(&m, k), expected, "checkpoint after {k} events");
        }
        if every_state {
            for want in [
                "before_fetch",
                "fetch_wait",
                "commit_pending",
                "after_commit",
                "mem_issue",
                "mem_wait_load",
                "mem_wait_store",
                "halted",
            ] {
                assert!(labels.contains(want), "no checkpoint in {want}: {labels:?}");
            }
        }
    }
}

/// A CPU restored while its store is outstanding waits for the response already in the
/// runtime's queue: the RAM receives the store exactly once, and the result matches the
/// uninterrupted run.
#[test]
fn restoring_a_pending_store_never_reissues_it() {
    let m = tour();
    let run = m.run();
    let expected = uninterrupted(&m);
    let writes = |events: &[Dispatched]| {
        events
            .iter()
            .filter(|ev| ev.target == RAM && matches!(mem(ev), Some(MemMsg::WriteReq { .. })))
            .count()
    };
    assert_eq!(writes(&expected.events), 1);
    let pending: Vec<_> = (1..=run.events.len())
        .filter(|&k| label(&run, k) == "mem_wait_store")
        .collect();
    assert!(
        pending.len() >= 3,
        "store outstanding across several events: {pending:?}"
    );
    for k in pending {
        let resumed = resumed(&m, k);
        assert_eq!(writes(&resumed.events), 1, "checkpoint after {k} events");
        let sent_by_cpu = resumed
            .events
            .iter()
            .filter(|ev| ev.source == CPU && matches!(mem(ev), Some(MemMsg::WriteReq { .. })))
            .count();
        assert_eq!(sent_by_cpu, 1);
        assert_eq!(resumed, expected);
    }
}
