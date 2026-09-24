//! Test harnesses shared by the CPU tests. Test-only: nothing here is part of the crate.
//!
//! - [`asm`] encodes the handful of RV32I instructions the test programs use, straight
//!   from the ISA manual's formats, independently of the crate's decoder.
//! - [`MockCtx`] drives the CPU directly, recording what it sends, wakes, and traces, so
//!   protocol tests can deliver responses no real memory would produce.
//! - [`mei`] is the pure machine-external-interrupt oracle (`docs/m2-design.md` §5.8).

#![allow(dead_code)]

pub mod mei;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::irq_v0::IrqMsg;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::rng::SimRng;
use systemscope_contracts::time::Tick;
use systemscope_contracts::trace::Value;

/// RV32I encodings (ISA manual, "Base Instruction Formats"). Registers are indices.
pub mod asm {
    fn r(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32) -> u32 {
        funct7 << 25 | rs2 << 20 | rs1 << 15 | funct3 << 12 | rd << 7 | 0x33
    }

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

    pub fn xori(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 4, rd, 0x13)
    }

    pub fn slli(rd: u32, rs1: u32, shamt: u32) -> u32 {
        i(shamt as i32, rs1, 1, rd, 0x13)
    }

    pub fn add(rd: u32, rs1: u32, rs2: u32) -> u32 {
        r(0, rs2, rs1, 0, rd)
    }

    pub fn sub(rd: u32, rs1: u32, rs2: u32) -> u32 {
        r(0x20, rs2, rs1, 0, rd)
    }

    pub fn sltu(rd: u32, rs1: u32, rs2: u32) -> u32 {
        r(0, rs2, rs1, 3, rd)
    }

    /// `rd = imm20 << 12`.
    pub fn lui(rd: u32, imm20: u32) -> u32 {
        imm20 << 12 | rd << 7 | 0x37
    }

    pub fn lb(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 0, rd, 0x03)
    }

    pub fn lh(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 1, rd, 0x03)
    }

    pub fn lw(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 2, rd, 0x03)
    }

    pub fn lbu(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 4, rd, 0x03)
    }

    pub fn lhu(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 5, rd, 0x03)
    }

    pub fn sb(rs2: u32, rs1: u32, imm: i32) -> u32 {
        s(imm, rs2, rs1, 0)
    }

    pub fn sh(rs2: u32, rs1: u32, imm: i32) -> u32 {
        s(imm, rs2, rs1, 1)
    }

    pub fn sw(rs2: u32, rs1: u32, imm: i32) -> u32 {
        s(imm, rs2, rs1, 2)
    }

    pub fn jalr(rd: u32, rs1: u32, imm: i32) -> u32 {
        i(imm, rs1, 0, rd, 0x67)
    }

    pub fn beq(rs1: u32, rs2: u32, imm: i32) -> u32 {
        b(imm, rs2, rs1, 0)
    }

    pub fn bne(rs1: u32, rs2: u32, imm: i32) -> u32 {
        b(imm, rs2, rs1, 1)
    }

    pub fn jal(rd: u32, imm: i32) -> u32 {
        let imm = imm as u32;
        (imm >> 20 & 1) << 31
            | (imm >> 1 & 0x3ff) << 21
            | (imm >> 11 & 1) << 20
            | (imm >> 12 & 0xff) << 12
            | rd << 7
            | 0x6f
    }

    pub const FENCE: u32 = 0x0ff0_000f;
    pub const ECALL: u32 = 0x0000_0073;
    pub const EBREAK: u32 = 0x0010_0073;
}

/// One `send` the CPU made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub port: PortId,
    pub msg: MemMsg,
    pub when: ScheduleWhen,
    pub phase: Phase,
}

/// One `wake_self` the CPU made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Woke {
    pub when: ScheduleWhen,
    pub phase: Phase,
    pub token: u64,
}

/// One `trace` record the CPU made.
pub type Traced = (&'static str, Vec<(&'static str, Value)>);

/// The CPU uses no randomness.
struct NoRng;

impl SimRng for NoRng {
    fn next_u64(&mut self) -> u64 {
        panic!("the CPU must not draw random numbers")
    }
}

/// A `SimContext` that records instead of scheduling.
pub struct MockCtx {
    pub phase: Phase,
    pub sent: Vec<Sent>,
    pub woke: Vec<Woke>,
    pub traced: Vec<Traced>,
    rng: NoRng,
}

impl MockCtx {
    pub fn new() -> MockCtx {
        MockCtx {
            phase: Phase::Request,
            sent: Vec::new(),
            woke: Vec::new(),
            traced: Vec::new(),
            rng: NoRng,
        }
    }

    /// Delivers `msg` on port 0 of `component`, in `Complete`.
    pub fn respond(&mut self, component: &mut dyn Component, msg: MemMsg) -> Result<(), SimError> {
        self.phase = Phase::Complete;
        let ev = Delivered::Message {
            port: PortId(0),
            msg: msg.into(),
        };
        component.handle_event(&ev, self)
    }

    /// Delivers `msg` on `port` of `component`, in `phase`.
    pub fn deliver(
        &mut self,
        component: &mut dyn Component,
        port: PortId,
        msg: Message,
        phase: Phase,
    ) -> Result<(), SimError> {
        self.phase = phase;
        component.handle_event(&Delivered::Message { port, msg }, self)
    }

    /// Delivers `irq.v0` `Level { asserted }` on port 1 of `component` (the `M2` CPU's
    /// `irq`), in `Complete`.
    pub fn level(&mut self, component: &mut dyn Component, asserted: bool) -> Result<(), SimError> {
        self.deliver(
            component,
            PortId(1),
            Message::Irq(IrqMsg::Level { asserted }),
            Phase::Complete,
        )
    }

    /// Delivers the wake `token` to `component`, in the phase it was scheduled for.
    pub fn wake(
        &mut self,
        component: &mut dyn Component,
        token: u64,
        phase: Phase,
    ) -> Result<(), SimError> {
        self.phase = phase;
        component.handle_event(&Delivered::Wake { token }, self)
    }

    /// The only wake scheduled since the last call, which is cleared.
    pub fn take_wake(&mut self) -> Woke {
        assert_eq!(
            self.woke.len(),
            1,
            "expected exactly one wake: {:?}",
            self.woke
        );
        self.woke.pop().unwrap()
    }

    /// The only message sent since the last call, which is cleared.
    pub fn take_sent(&mut self) -> MemMsg {
        assert_eq!(
            self.sent.len(),
            1,
            "expected exactly one send: {:?}",
            self.sent
        );
        self.sent.pop().unwrap().msg
    }
}

impl InitContext for MockCtx {
    fn component(&self) -> ComponentId {
        ComponentId(0)
    }

    fn send(
        &mut self,
        port: PortId,
        msg: Message,
        when: ScheduleWhen,
        phase: Phase,
    ) -> Result<(), SimError> {
        let Message::MemV1(msg) = msg else {
            panic!("the CPU speaks mem.v1 only");
        };
        self.sent.push(Sent {
            port,
            msg,
            when,
            phase,
        });
        Ok(())
    }

    fn wake_self(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError> {
        self.woke.push(Woke { when, phase, token });
        Ok(())
    }

    fn rng(&mut self) -> &mut dyn SimRng {
        &mut self.rng
    }

    fn trace(&mut self, kind: &'static str, fields: Vec<(&'static str, Value)>) {
        self.traced.push((kind, fields));
    }
}

impl SimContext for MockCtx {
    fn now(&self) -> Tick {
        Tick::ZERO
    }

    fn phase(&self) -> Phase {
        self.phase
    }
}
