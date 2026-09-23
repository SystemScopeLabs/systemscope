//! First end-to-end runs of real toy components through the runtime.

use std::num::NonZeroU64;

use systemscope_contracts::component::{
    Component, ComponentId, Delivered, InitContext, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{
    ClockDomainId, Duration, Frequency, Rounding, SimulationClock, Tick,
};
use systemscope_contracts::topology::LinkLatency;
use systemscope_runtime::runtime::{Dispatched, Runtime, RuntimeError, SessionConfig};
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_toy::cpu::{COMMIT, ISSUE};
use systemscope_toy::{ToyCpu, ToyCpuConfig, ToyMemory, ToyMemoryConfig};

const CPU: ComponentId = ComponentId(0);
const MEM: ComponentId = ComponentId(1);

/// CPU workload; `build` replaces `clock` with the domain it declares.
fn cpu_config(ops: u64, write_percent: u64) -> ToyCpuConfig {
    ToyCpuConfig {
        clock: ClockDomainId(0),
        ops,
        max_outstanding: 4,
        max_think_cycles: NonZeroU64::new(8).unwrap(),
        access_len: 8,
        slots: 64,
        write_percent,
    }
}

fn build(seed: u64, cpu: ToyCpuConfig, memory: ToyMemoryConfig) -> Runtime {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(3_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let cpu = t.add_component(
        "soc.cpu0",
        Box::new(ToyCpu::new(ToyCpuConfig { clock, ..cpu })),
    );
    let mem = t.add_component("soc.mem", Box::new(ToyMemory::new(memory)));
    assert_eq!((cpu, mem), (CPU, MEM));
    let link = LinkLatency::After(Duration::from_ns(1));
    t.connect((cpu, "mem"), (mem, "mem"), Some(link));
    let config = SessionConfig {
        seed,
        ..SessionConfig::default()
    };
    let mut rt = t.elaborate(config).unwrap();
    rt.init().unwrap();
    rt
}

fn memory() -> ToyMemoryConfig {
    ToyMemoryConfig {
        size: 64 * 8,
        read_latency: Duration::from_ns(50),
        write_latency: Duration::from_ns(30),
    }
}

fn run_all(rt: &mut Runtime) -> Vec<Dispatched> {
    std::iter::from_fn(|| rt.step().unwrap()).collect()
}

#[test]
fn single_read_flows_from_issue_to_commit() {
    let cpu = cpu_config(1, 0);
    let mut rt = build(1, cpu, memory());
    let trace = run_all(&mut rt);

    let shape: Vec<_> = trace
        .iter()
        .map(|d| {
            let what = match &d.delivery {
                Delivered::Wake { token: ISSUE } => "issue",
                Delivered::Wake { token: COMMIT } => "commit",
                Delivered::Message {
                    msg: Message::Mem(MemMsg::ReadReq { .. }),
                    ..
                } => "read_req",
                Delivered::Message {
                    msg: Message::Mem(MemMsg::ReadResp { .. }),
                    ..
                } => "read_resp",
                other => panic!("unexpected delivery {other:?}"),
            };
            (what, d.source, d.target, d.key.phase)
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("issue", CPU, CPU, Phase::Request),
            ("read_req", CPU, MEM, Phase::Request),
            ("read_resp", MEM, CPU, Phase::Complete),
            ("commit", CPU, CPU, Phase::Commit),
        ]
    );

    // Timing: issue lands on a 3 GHz edge chosen by the RNG; +1 ns link; +50 ns memory;
    // +1 ns link; commit in the same tick as the response.
    let t: Vec<u64> = trace.iter().map(|d| d.key.tick.0).collect();
    let edge = |n: u64| n * 1000 / 3;
    assert_eq!(
        edge((t[0] * 3).div_ceil(1000)),
        t[0],
        "issue is on a 3 GHz edge"
    );
    assert_eq!(t[1], t[0] + 1_000);
    assert_eq!(t[2], t[1] + 50_000 + 1_000);
    assert_eq!(t[3], t[2]);
    assert!(rt.peek_key().is_none());
}

#[test]
fn mixed_traffic_runs_to_completion_and_checks_every_read() {
    let mut rt = build(7, cpu_config(2_000, 40), memory());
    let trace = run_all(&mut rt);
    let responses = trace
        .iter()
        .filter(|d| d.target == CPU && matches!(d.delivery, Delivered::Message { .. }))
        .count();
    assert_eq!(responses, 2_000);
    let writes = trace
        .iter()
        .filter(|d| {
            matches!(
                d.delivery,
                Delivered::Message {
                    msg: Message::Mem(MemMsg::WriteReq { .. }),
                    ..
                }
            )
        })
        .count();
    assert!((600..1_000).contains(&writes), "writes = {writes}");
    // Reads and writes have different latencies, so responses overtake each other.
    let order: Vec<u64> = trace
        .iter()
        .filter_map(|d| match &d.delivery {
            Delivered::Message {
                msg: Message::Mem(MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn }),
                ..
            } => Some(txn.0),
            _ => None,
        })
        .collect();
    assert!(
        order.windows(2).any(|w| w[0] > w[1]),
        "no reordering exercised"
    );
}

#[test]
fn same_seed_reproduces_the_trace_and_other_seeds_do_not() {
    let run = |seed| run_all(&mut build(seed, cpu_config(300, 40), memory()));
    assert_eq!(run(3), run(3));
    assert_ne!(run(3), run(4));
}

#[test]
fn out_of_range_access_faults() {
    // A memory smaller than the CPU's address space rejects the access instead of
    // returning garbage.
    let small = ToyMemoryConfig {
        size: 8,
        ..memory()
    };
    let mut rt = build(7, cpu_config(50, 40), small);
    let err = std::iter::from_fn(|| rt.step().transpose())
        .find_map(Result::err)
        .expect("run should fault");
    assert_eq!(
        err,
        RuntimeError::Faulted(SimError::ComponentFault("toy memory: address out of range"))
    );
}

#[test]
fn invalid_cpu_config_faults_at_init() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    let config = ToyCpuConfig {
        clock,
        max_outstanding: 64,
        ..cpu_config(1, 0)
    };
    let cpu = t.add_component("cpu", Box::new(ToyCpu::new(config)));
    let mem = t.add_component("mem", Box::new(ToyMemory::new(memory())));
    t.connect((cpu, "mem"), (mem, "mem"), None);
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    assert_eq!(
        rt.init(),
        Err(RuntimeError::Faulted(SimError::ComponentFault(
            "toy cpu: max_outstanding must be in 1..slots"
        )))
    );
}

/// A broken memory that acknowledges writes but always reads back zeros.
struct StaleMemory;

impl Component for StaleMemory {
    fn type_name(&self) -> &'static str {
        "test.stale_memory"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Target,
        }]
    }
    fn init(&mut self, _: &mut dyn InitContext) -> Result<(), SimError> {
        Ok(())
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        let Delivered::Message {
            port,
            msg: Message::Mem(msg),
        } = ev
        else {
            return Err(SimError::ComponentFault("stale: unexpected delivery"));
        };
        let resp = match msg {
            MemMsg::ReadReq { txn, len, .. } => MemMsg::ReadResp {
                txn: *txn,
                data: vec![0; *len as usize],
            },
            MemMsg::WriteReq { txn, .. } => MemMsg::WriteResp { txn: *txn },
            _ => return Err(SimError::ComponentFault("stale: unexpected message")),
        };
        let after = ScheduleWhen::After(Duration::from_ns(10));
        ctx.send(*port, resp.into(), after, Phase::Complete)
    }

    // Not snapshotted by these tests.
    fn snapshot_schema_version(&self) -> u32 {
        0
    }
    fn snapshot(&self, _: &mut SnapshotWriter) {}
    fn restore(&mut self, _: &mut SnapshotReader<'_>, _: u32) -> Result<(), RestoreError> {
        Ok(())
    }
}

#[test]
fn stale_memory_is_caught_by_the_read_check() {
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let clock = t
        .add_clock(
            Frequency::from_hz(1_000_000_000).unwrap(),
            Tick::ZERO,
            Rounding::Floor,
        )
        .unwrap();
    // Few slots so reads soon hit written addresses.
    let config = ToyCpuConfig {
        clock,
        slots: 8,
        ..cpu_config(500, 50)
    };
    let cpu = t.add_component("cpu", Box::new(ToyCpu::new(config)));
    let mem = t.add_component("mem", Box::new(StaleMemory));
    t.connect((cpu, "mem"), (mem, "mem"), None);
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    let err = std::iter::from_fn(|| rt.step().transpose())
        .find_map(Result::err)
        .expect("stale reads must fault");
    assert_eq!(
        err,
        RuntimeError::Faulted(SimError::ComponentFault("toy cpu: read mismatch"))
    );
}
