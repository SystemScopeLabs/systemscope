//! ToyMemory ordering semantics: memory order is request dispatch order, and response
//! timing never affects visibility.

use std::cell::RefCell;
use std::rc::Rc;

use systemscope_contracts::component::{
    Component, Delivered, InitContext, PortId, PortSpec, Role, SimContext,
};
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{Phase, ScheduleWhen};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::{self, MemMsg, TxnId};
use systemscope_contracts::snapshot::{RestoreError, SnapshotReader, SnapshotWriter};
use systemscope_contracts::time::{Duration, SimulationClock, Tick};
use systemscope_runtime::runtime::SessionConfig;
use systemscope_runtime::topology::TopologyBuilder;
use systemscope_toy::{ToyMemory, ToyMemoryConfig};

type Responses = Rc<RefCell<Vec<(Tick, MemMsg)>>>;

/// Sends a fixed list of requests from one handler at tick 0 and records responses.
struct Driver {
    requests: Vec<MemMsg>,
    responses: Responses,
}

impl Component for Driver {
    fn type_name(&self) -> &'static str {
        "test.driver"
    }
    fn ports(&self) -> Vec<PortSpec> {
        vec![PortSpec {
            name: "mem",
            protocol: mem::PROTOCOL,
            role: Role::Initiator,
        }]
    }
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError> {
        for req in self.requests.drain(..) {
            ctx.send(PortId(0), req.into(), ScheduleWhen::Now, Phase::Request)?;
        }
        Ok(())
    }
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError> {
        if let Delivered::Message {
            msg: Message::Mem(msg),
            ..
        } = ev
        {
            self.responses.borrow_mut().push((ctx.now(), msg.clone()));
        }
        Ok(())
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

const A: u64 = 16;
const DATA: [u8; 4] = [1, 2, 3, 4];

fn write(txn: u64) -> MemMsg {
    MemMsg::WriteReq {
        txn: TxnId(txn),
        addr: A,
        data: DATA.to_vec(),
    }
}

fn read(txn: u64) -> MemMsg {
    MemMsg::ReadReq {
        txn: TxnId(txn),
        addr: A,
        len: 4,
    }
}

/// Runs `requests` against a memory whose writes are slower than its reads.
fn run(requests: Vec<MemMsg>) -> Vec<(Tick, MemMsg)> {
    let responses = Responses::default();
    let mut t = TopologyBuilder::new(SimulationClock::default());
    let driver = Driver {
        requests,
        responses: responses.clone(),
    };
    let d = t.add_component("driver", Box::new(driver));
    let memory = ToyMemory::new(ToyMemoryConfig {
        size: 64,
        read_latency: Duration::from_ns(50),
        write_latency: Duration::from_ns(100),
    });
    let m = t.add_component("mem", Box::new(memory));
    t.connect((d, "mem"), (m, "mem"), None);
    let mut rt = t.elaborate(SessionConfig::default()).unwrap();
    rt.init().unwrap();
    while rt.step().unwrap().is_some() {}
    responses.borrow().clone()
}

#[test]
fn read_accepted_after_write_sees_it_even_if_its_response_arrives_first() {
    let got = run(vec![write(0), read(1)]);
    assert_eq!(
        got,
        [
            // The read's response is delivered first (50 ns < 100 ns)...
            (
                Tick(50_000),
                MemMsg::ReadResp {
                    txn: TxnId(1),
                    // ...yet it carries the write's data: sampled at acceptance.
                    data: DATA.to_vec(),
                }
            ),
            (Tick(100_000), MemMsg::WriteResp { txn: TxnId(0) }),
        ]
    );
}

#[test]
fn read_accepted_before_write_sees_old_value() {
    let got = run(vec![read(0), write(1)]);
    assert_eq!(
        got[0],
        (
            Tick(50_000),
            MemMsg::ReadResp {
                txn: TxnId(0),
                data: vec![0; 4],
            }
        )
    );
}

#[test]
fn same_tick_responses_keep_acceptance_order() {
    let got = run(vec![read(0), read(1), read(2)]);
    let txns: Vec<u64> = got
        .iter()
        .map(|(t, m)| {
            assert_eq!(*t, Tick(50_000));
            match m {
                MemMsg::ReadResp { txn, .. } => txn.0,
                other => panic!("unexpected {other:?}"),
            }
        })
        .collect();
    assert_eq!(txns, [0, 1, 2]);
}
