//! End-to-end event flow, elaboration, and lifecycle tests (`docs/m0-design.md` §5–§6).

use std::cell::RefCell;
use std::rc::Rc;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_runtime::runtime::{Dispatched, Lifecycle, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::scheduler::SchedulerConfig;
use systemscope_runtime::topology::{ElaborationError, TopologyBuilder};

type Log = Rc<RefCell<Vec<String>>>;

const MEM_INIT: PortSpec = PortSpec {
    name: "mem",
    protocol: mem::PROTOCOL,
    role: Role::Initiator,
};
const MEM_TARGET: PortSpec = PortSpec {
    name: "mem",
    protocol: mem::PROTOCOL,
    role: Role::Target,
};

fn read(txn: u64) -> Message {
    MemMsg::ReadReq {
        txn: TxnId(txn),
        addr: 0x100,
        len: 4,
    }
    .into()
}

/// Sends one read at init and logs the response.
struct Pinger {
    log: Log,
}

impl Component for Pinger {
    fn type_name(&self) -> &'static str {
        "test.pinger"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![MEM_INIT]
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        let me = ctx.component().0;
        ctx.send(
            PortId(0),
            read(u64::from(me)),
            ScheduleWhen::Now,
            Phase::Request,
        )
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.log
            .borrow_mut()
            .push(format!("pinger@{}:{} {ev:?}", ctx.now(), ctx.phase()));
        Ok(())
    }
}

/// Answers every read after 50 ns, in COMPLETE.
struct Echo {
    log: Log,
}

impl Component for Echo {
    fn type_name(&self) -> &'static str {
        "test.echo"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![MEM_TARGET]
    }
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        self.log
            .borrow_mut()
            .push(format!("echo@{}:{}", ctx.now(), ctx.phase()));
        let Delivered::Message {
            port,
            msg: Message::Mem(MemMsg::ReadReq { txn, len, .. }),
        } = ev
        else {
            return Err(SimError::ComponentFault("echo expects ReadReq"));
        };
        let resp = MemMsg::ReadResp {
            txn: *txn,
            data: vec![0xAB; *len as usize],
        };
        ctx.send(
            *port,
            resp.into(),
            ScheduleWhen::After(Duration::from_ns(50)),
            Phase::Complete,
        )
    }
}

type InitFn = Box<dyn FnMut(&mut dyn InitContext) -> Result<(), SimError>>;
type EventFn = Box<dyn FnMut(&Delivered, &mut dyn SimContext) -> Result<(), SimError>>;

/// A component whose behavior is a closure, for fault and edge-case tests.
struct Scripted {
    ports: Vec<PortSpec>,
    on_init: InitFn,
    on_event: EventFn,
}

impl Component for Scripted {
    fn type_name(&self) -> &'static str {
        "test.scripted"
    }
    fn ports(&self) -> Vec<PortSpec> {
        self.ports.clone()
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        (self.on_init)(ctx)
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        (self.on_event)(ev, ctx)
    }
}

fn scripted(
    on_init: impl FnMut(&mut dyn InitContext) -> Result<(), SimError> + 'static,
    on_event: impl FnMut(&Delivered, &mut dyn SimContext) -> Result<(), SimError> + 'static,
) -> Box<Scripted> {
    Box::new(Scripted {
        ports: Vec::new(),
        on_init: Box::new(on_init),
        on_event: Box::new(on_event),
    })
}

fn ping_echo(latency: Option<LinkLatency>) -> (Runtime, Log) {
    let log = Log::default();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let cpu = t.add_clock(
        Frequency::from_hz(3_000_000_000).unwrap(),
        Tick::ZERO,
        Rounding::Floor,
    );
    assert_eq!(cpu, Ok(ClockDomainId(0)));
    let p = t.add_component("soc.pinger", Box::new(Pinger { log: log.clone() }));
    let e = t.add_component("soc.echo", Box::new(Echo { log: log.clone() }));
    t.connect((p, "mem"), (e, "mem"), latency);
    let rt = t.elaborate(SessionConfig::default()).unwrap();
    (rt, log)
}

fn run_all(rt: &mut Runtime) -> Vec<Dispatched> {
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

#[test]
fn request_and_response_flow_through_link_with_latency() {
    let (mut rt, log) = ping_echo(Some(LinkLatency::After(Duration::from_ns(1))));
    rt.init().unwrap();
    let trace = run_all(&mut rt);

    // Request: sent at 0, +1 ns link. Response: +50 ns, +1 ns link.
    let when: Vec<_> = trace.iter().map(|d| (d.key.tick, d.key.phase)).collect();
    assert_eq!(
        when,
        [
            (Tick(1_000), Phase::Request),
            (Tick(52_000), Phase::Complete)
        ]
    );
    // Sources are stamped by the runtime; targets follow the link.
    let hops: Vec<_> = trace.iter().map(|d| (d.source, d.target)).collect();
    assert_eq!(
        hops,
        [
            (ComponentId(0), ComponentId(1)),
            (ComponentId(1), ComponentId(0))
        ]
    );
    assert_eq!(
        *log.borrow(),
        [
            "echo@1000:REQUEST".to_owned(),
            format!(
                "pinger@52000:COMPLETE {:?}",
                Delivered::Message {
                    port: PortId(0),
                    msg: MemMsg::ReadResp {
                        txn: TxnId(0),
                        data: vec![0xAB; 4]
                    }
                    .into()
                }
            ),
        ]
    );
    assert_eq!(rt.lifecycle(), Lifecycle::Ready);
}

#[test]
fn cycle_latency_counts_from_next_edge_of_send_tick() {
    let latency = LinkLatency::Cycles {
        domain: ClockDomainId(0),
        k: 1,
    };
    let (mut rt, _) = ping_echo(Some(latency));
    rt.init().unwrap();
    let trace = run_all(&mut rt);
    // 3 GHz edges: 0, 333, …; response sent at 333 + 50 000 = 50 333 → next edge 50 333
    // (edge 151) → +1 cycle = 50 666.
    assert_eq!(trace[0].key.tick, Tick(333));
    assert_eq!(trace[1].key.tick, Tick(50_666));
}

#[test]
fn zero_latency_delivery_is_a_separate_later_event() {
    let order = Log::default();
    let o1 = order.clone();
    let o2 = order.clone();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let a = t.add_component(
        "a",
        Box::new(Scripted {
            ports: vec![MEM_INIT],
            on_init: Box::new(|ctx| ctx.wake_self(ScheduleWhen::Now, Phase::Request, 0)),
            on_event: Box::new(move |_, ctx| {
                ctx.send(PortId(0), read(1), ScheduleWhen::Now, Phase::Request)?;
                // The receiver has not run yet: send only enqueued.
                o1.borrow_mut().push("a returned".to_owned());
                Ok(())
            }),
        }),
    );
    let b = t.add_component(
        "b",
        Box::new(Scripted {
            ports: vec![MEM_TARGET],
            on_init: Box::new(|_| Ok(())),
            on_event: Box::new(move |_, _| {
                o2.borrow_mut().push("b ran".to_owned());
                Ok(())
            }),
        }),
    );
    t.connect((a, "mem"), (b, "mem"), None);
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    let trace = run_all(&mut rt);

    assert_eq!(*order.borrow(), ["a returned", "b ran"]);
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].key.tick, trace[1].key.tick);
    assert_eq!(trace[0].key.phase, trace[1].key.phase);
    assert!(trace[0].key.sequence < trace[1].key.sequence);
}

#[test]
fn init_runs_in_component_id_order() {
    let build = |names: [&'static str; 3]| {
        let log = Log::default();
        let mut t = TopologyBuilder::new(SimulationClock::default());
        for name in names {
            let l = log.clone();
            t.add_component(
                name,
                scripted(
                    move |ctx| {
                        l.borrow_mut().push(format!("{name}={}", ctx.component().0));
                        ctx.wake_self(ScheduleWhen::Now, Phase::Request, 0)
                    },
                    |_, _| Ok(()),
                ),
            );
        }
        let mut rt = t.elaborate(SessionConfig::default()).unwrap();
        rt.init().unwrap();
        let targets: Vec<_> = run_all(&mut rt)
            .iter()
            .map(|d| (d.key.sequence, d.target.0))
            .collect();
        let init_order = log.borrow().clone();
        (init_order, targets)
    };
    let (order, targets) = build(["x", "y", "z"]);
    assert_eq!(order, ["x=0", "y=1", "z=2"]);
    assert_eq!(targets, [(0, 0), (1, 1), (2, 2)]);
    // Declaration order decides ids, and ids decide initial sequence numbers.
    let (order, _) = build(["z", "x", "y"]);
    assert_eq!(order, ["z=0", "x=1", "y=2"]);
}

#[test]
fn init_may_schedule_any_phase_but_observe_at_tick_zero() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "c",
        scripted(
            |ctx| {
                for (token, phase) in [Phase::Commit, Phase::Request].into_iter().enumerate() {
                    ctx.wake_self(ScheduleWhen::Now, phase, token as u64)?;
                }
                Ok(())
            },
            |_, _| Ok(()),
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    let phases: Vec<_> = run_all(&mut rt).iter().map(|d| d.key.phase).collect();
    assert_eq!(phases, [Phase::Request, Phase::Commit]);

    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "c",
        scripted(
            |ctx| ctx.wake_self(ScheduleWhen::Now, Phase::Observe, 0),
            |_, _| Ok(()),
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    assert!(matches!(
        rt.init(),
        Err(RuntimeError::Faulted(SimError::PhaseViolation { .. }))
    ));
}

#[test]
fn swallowed_context_error_still_faults() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "sneaky",
        scripted(
            |ctx| ctx.wake_self(ScheduleWhen::Now, Phase::Commit, 0),
            |_, ctx| {
                // Earlier phase in the current tick: rejected, and the error is ignored.
                let _ = ctx.wake_self(ScheduleWhen::Now, Phase::Request, 1);
                // The context stays poisoned for the rest of the handler.
                let again =
                    ctx.wake_self(ScheduleWhen::After(Duration::from_ns(1)), Phase::Request, 2);
                assert!(matches!(again, Err(SimError::PhaseViolation { .. })));
                Ok(())
            },
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    let err = rt.step().unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::Faulted(SimError::PhaseViolation {
            requested: Phase::Request,
            ..
        })
    ));
    assert_eq!(rt.lifecycle(), Lifecycle::Faulted);
}

#[test]
fn faulted_is_terminal() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "bad",
        scripted(
            |ctx| {
                ctx.wake_self(ScheduleWhen::Now, Phase::Request, 0)?;
                ctx.wake_self(ScheduleWhen::Now, Phase::Request, 1)
            },
            |_, _| Err(SimError::ComponentFault("boom")),
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    let fault = RuntimeError::Faulted(SimError::ComponentFault("boom"));
    assert_eq!(rt.step(), Err(fault));
    // The second event is still queued, but nothing can dispatch it.
    assert_eq!(rt.pending(), 1);
    assert_eq!(rt.step(), Err(fault));
    assert_eq!(rt.run_until(Tick(u64::MAX)), Err(fault));
    assert_eq!(rt.init(), Err(fault));
    assert_eq!(rt.fault(), Some(SimError::ComponentFault("boom")));
}

#[test]
fn livelock_faults_and_cannot_be_bypassed() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "spin",
        scripted(
            |ctx| ctx.wake_self(ScheduleWhen::Now, Phase::Transfer, 0),
            |_, ctx| ctx.wake_self(ScheduleWhen::Now, Phase::Transfer, 0),
        ),
    );
    let mut rt = t
        .elaborate(SessionConfig {
            scheduler: SchedulerConfig {
                max_events_per_phase: 10,
            },
            ..SessionConfig::default()
        })
        .unwrap();
    rt.init().unwrap();
    let err = rt.run_until(Tick(0)).unwrap_err();
    let expected = SimError::SameTickLivelock {
        tick: Tick(0),
        phase: Phase::Transfer,
        limit: 10,
    };
    assert_eq!(err, RuntimeError::Faulted(expected));
    assert_eq!(rt.step(), Err(RuntimeError::Faulted(expected)));
}

#[test]
fn init_failure_faults_whole_session() {
    let ran = Log::default();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    for (i, fails) in [false, false, true, false].into_iter().enumerate() {
        let r = ran.clone();
        t.add_component(
            format!("c{i}"),
            scripted(
                move |_| {
                    r.borrow_mut().push(format!("c{i}"));
                    if fails {
                        Err(SimError::ComponentFault("init"))
                    } else {
                        Ok(())
                    }
                },
                |_, _| Ok(()),
            ),
        );
    }
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    let fault = RuntimeError::Faulted(SimError::ComponentFault("init"));
    assert_eq!(rt.init(), Err(fault));
    // Components after the failure are not initialized.
    assert_eq!(*ran.borrow(), ["c0", "c1", "c2"]);
    assert_eq!(rt.lifecycle(), Lifecycle::Faulted);
    assert_eq!(rt.step(), Err(fault));
}

#[test]
fn lifecycle_order_is_enforced() {
    let (mut rt, _) = ping_echo(None);
    assert_eq!(
        rt.step(),
        Err(RuntimeError::InvalidState(Lifecycle::Elaborated))
    );
    assert_eq!(
        rt.run_until(Tick(0)),
        Err(RuntimeError::InvalidState(Lifecycle::Elaborated))
    );
    rt.init().unwrap();
    assert_eq!(rt.init(), Err(RuntimeError::InvalidState(Lifecycle::Ready)));
}

#[test]
fn unknown_port_faults() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    t.add_component(
        "c",
        scripted(
            |ctx| ctx.send(PortId(3), read(0), ScheduleWhen::Now, Phase::Request),
            |_, _| Ok(()),
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    assert_eq!(
        rt.init(),
        Err(RuntimeError::Faulted(SimError::UnknownPort(PortId(3))))
    );
}

#[test]
fn unknown_clock_domain_in_schedule_faults() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let when = ScheduleWhen::Cycles {
        domain: ClockDomainId(9),
        k: 1,
    };
    t.add_component(
        "c",
        scripted(
            move |ctx| ctx.wake_self(when, Phase::Request, 0),
            |_, _| Ok(()),
        ),
    );
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    assert_eq!(
        rt.init(),
        Err(RuntimeError::Faulted(SimError::UnknownClockDomain(
            ClockDomainId(9)
        )))
    );
}

fn elaborate_err(build: impl FnOnce(&mut TopologyBuilder)) -> ElaborationError {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    build(&mut t);
    match t.elaborate(SessionConfig::default()) {
        Ok(_) => panic!("elaboration unexpectedly succeeded"),
        Err(e) => e,
    }
}

fn with_ports(ports: Vec<PortSpec>) -> Box<Scripted> {
    let mut c = scripted(|_| Ok(()), |_, _| Ok(()));
    c.ports = ports;
    c
}

#[test]
fn elaboration_rejects_invalid_topologies() {
    let log = Log::default;
    assert_eq!(
        elaborate_err(|t| {
            t.add_component("x", with_ports(vec![]));
            t.add_component("x", with_ports(vec![]));
        }),
        ElaborationError::DuplicatePath("x".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            t.add_component("x", with_ports(vec![MEM_INIT, MEM_TARGET]));
        }),
        ElaborationError::DuplicatePortName("x.mem".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            t.add_component("p", Box::new(Pinger { log: log() }));
        }),
        ElaborationError::UnconnectedPort("p.mem".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            let b = t.add_component("b", Box::new(Pinger { log: log() }));
            t.connect((a, "mem"), (b, "mem"), None);
        }),
        ElaborationError::RoleMismatch("a.mem".into(), "b.mem".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            let b = t.add_component("b", Box::new(Echo { log: log() }));
            let c = t.add_component("c", Box::new(Pinger { log: log() }));
            t.connect((a, "mem"), (b, "mem"), None);
            t.connect((c, "mem"), (b, "mem"), None);
        }),
        ElaborationError::PortAlreadyLinked("b.mem".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            let b = t.add_component("b", Box::new(Echo { log: log() }));
            t.connect((a, "bus"), (b, "mem"), None);
        }),
        ElaborationError::UnknownPort("a.bus".into())
    );
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            t.connect((a, "mem"), (ComponentId(5), "mem"), None);
        }),
        ElaborationError::UnknownComponent(ComponentId(5))
    );
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            let b = t.add_component("b", Box::new(Echo { log: log() }));
            let latency = LinkLatency::Cycles {
                domain: ClockDomainId(0),
                k: 1,
            };
            t.connect((a, "mem"), (b, "mem"), Some(latency));
        }),
        ElaborationError::UnknownClockDomain(ClockDomainId(0))
    );
    let other = PortSpec {
        protocol: systemscope_contracts::protocol::ProtocolId {
            name: "mem",
            version: 1,
        },
        ..MEM_TARGET
    };
    assert_eq!(
        elaborate_err(|t| {
            let a = t.add_component("a", Box::new(Pinger { log: log() }));
            let b = t.add_component("b", with_ports(vec![other]));
            t.connect((a, "mem"), (b, "mem"), None);
        }),
        ElaborationError::ProtocolMismatch("a.mem".into(), "b.mem".into())
    );
}

#[test]
fn message_protocol_must_match_sending_port() {
    let v1 = systemscope_contracts::protocol::ProtocolId {
        name: "mem",
        version: 1,
    };
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let mut a = scripted(
        |ctx| ctx.send(PortId(0), read(0), ScheduleWhen::Now, Phase::Request),
        |_, _| Ok(()),
    );
    a.ports = vec![PortSpec {
        protocol: v1,
        ..MEM_INIT
    }];
    let b = with_ports(vec![PortSpec {
        protocol: v1,
        ..MEM_TARGET
    }]);
    let a = t.add_component("a", a);
    let b = t.add_component("b", b);
    t.connect((a, "mem"), (b, "mem"), None);
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    assert_eq!(
        rt.init(),
        Err(RuntimeError::Faulted(SimError::ProtocolMismatch {
            port: PortId(0),
            expected: v1,
            actual: mem::PROTOCOL,
        }))
    );
    assert_eq!(rt.pending(), 0);
}
