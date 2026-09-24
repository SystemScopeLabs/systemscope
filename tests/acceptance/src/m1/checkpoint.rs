//! M1-A6: checkpoint, drop, and resume on `m1-reference` (`docs/m1-design.md` §10.1).
//!
//! As in M0 AT-2, a checkpoint is a number of events `k`. The run that took it is gone
//! before the resumed one starts: only the snapshot bytes and the trace prefix cross over.

use std::cell::RefCell;
use std::rc::Rc;

use systemscope_contracts::event::Phase;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_contracts::trace::Value;
use systemscope_runtime::trace::Trace;
use systemscope_rv32::hello;
use systemscope_rv32::runner::{self, CPU, Finished, Start};

use super::golden::mid_after;
use super::{Program, mem, registers};

/// The uninterrupted, traced run of a program, with the CPU's state after every event.
#[derive(Debug)]
pub struct Reference {
    /// How it ended.
    pub finished: Finished,
    /// The CPU's `state` after each event: entry `i` is after `i + 1` events.
    pub cpu_states: Vec<String>,
}

struct CpuStates(Rc<RefCell<Vec<String>>>);

impl Observer for CpuStates {
    fn on_after_dispatch(&mut self, _: &EventView<'_>, world: &WorldView<'_>) -> Control {
        let state = match world.inspect(CPU).as_ref().and_then(|v| v.get("state")) {
            Some(Value::Str(s)) => s.clone(),
            other => format!("{other:?}"),
        };
        self.0.borrow_mut().push(state);
        Control::Continue
    }
}

/// Runs `program` traced from `init` to its end, one event at a time.
pub fn reference(program: &Program) -> Reference {
    let states = Rc::new(RefCell::new(Vec::new()));
    let finished = runner::execute(
        program.platform(),
        Start::Init { traced: true },
        vec![Box::new(CpuStates(Rc::clone(&states)))],
    );
    Reference {
        finished,
        cpu_states: states.take(),
    }
}

/// The checkpoints M1-A6 names, as events dispatched before the snapshot, located in the
/// reference run. The structural ones are searched from a quarter of the way in, as in
/// M0, so the run is busy; "right after a UART byte" applies to programs with the UART.
pub fn checkpoints(reference: &Reference) -> Result<Vec<(&'static str, usize)>, String> {
    let events = &reference.finished.dispatched;
    let states = &reference.cpu_states;
    let n = events.len();
    if n < 2 || states.len() != n {
        return Err(format!("{n} events and {} CPU states", states.len()));
    }
    // The CPU's state after `k` events.
    let state = |k: usize| states[k - 1].as_str();
    // The last request the CPU sent in the first `k` events.
    let last_request = |k: usize| {
        events[..k]
            .iter()
            .rev()
            .filter(|ev| ev.source == CPU)
            .find_map(mem)
            .filter(|m| matches!(m, MemMsg::ReadReq { .. } | MemMsg::WriteReq { .. }))
    };
    let find = |what: &'static str, holds: &dyn Fn(usize) -> bool| {
        (n / 4..n)
            .find(|&k| k > 0 && holds(k))
            .map(|k| (what, k))
            .ok_or_else(|| format!("the run never reaches: {what}"))
    };
    let mut out = vec![
        ("before the first fetch", 0),
        find("during a fetch", &|k| state(k) == "fetch_wait")?,
        find("between Complete and Commit", &|k| {
            let (a, b) = (events[k - 1].key, events[k].key);
            a.tick == b.tick
                && a.phase == Phase::Complete
                && b.phase == Phase::Commit
                && state(k) == "commit_pending"
        })?,
        find("during a load", &|k| {
            state(k) == "mem_wait" && matches!(last_request(k), Some(MemMsg::ReadReq { .. }))
        })?,
        find("during a store", &|k| {
            state(k) == "mem_wait" && matches!(last_request(k), Some(MemMsg::WriteReq { .. }))
        })?,
    ];
    if let Some(k) = mid_after(events) {
        out.push(("right after a UART byte", k));
    }
    // The trap commits in the last event: the CPU is commit_pending before it and halted
    // after it, and nothing follows.
    let last = &events[n - 1];
    if !(last.target == CPU
        && last.key.phase == Phase::Commit
        && state(n - 1) == "commit_pending"
        && state(n) == "halted")
    {
        return Err("the run does not end with the trap's Commit".to_owned());
    }
    out.push(("just before the trap commits", n - 1));
    out.push(("after the last event", n));
    // splitmix64 over the event count, so the indices depend only on the run.
    let mut x = runner::SEED ^ 0xA7_2C4E_C0DE ^ n as u64;
    for _ in 0..8 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(("seeded random", (z % (n as u64 + 1)) as usize));
    }
    Ok(out)
}

/// What a stopped run leaves behind.
#[derive(Debug)]
pub struct Stopped {
    /// The snapshot bytes.
    pub snapshot: Vec<u8>,
    /// The trace so far.
    pub prefix: Trace,
}

/// Runs `program` traced for `k` events, snapshots, and drops the runtime.
pub fn stop_after(program: &Program, k: usize) -> Result<Stopped, String> {
    let mut rt = program.platform();
    rt.start_trace().map_err(|e| e.to_string())?;
    rt.init().map_err(|e| e.to_string())?;
    for i in 0..k {
        rt.step()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the run ended after {i} of {k} events"))?;
    }
    let snapshot = rt.snapshot().map_err(|e| e.to_string())?;
    let prefix = rt.take_trace().ok_or("not traced")?;
    drop(rt);
    Ok(Stopped { snapshot, prefix })
}

/// Restores `stopped` in a fresh platform, checks the round-trip law, resumes the trace,
/// and runs to the end.
pub fn resume(program: &Program, stopped: Stopped) -> Result<Finished, String> {
    let mut rt = program.platform();
    rt.restore(&stopped.snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    if rt.snapshot().map_err(|e| e.to_string())? != stopped.snapshot {
        return Err("encode(restore(decode(bytes))) != bytes".to_owned());
    }
    drop(rt);
    Ok(program.execute(Start::Restore {
        snapshot: stopped.snapshot,
        prefix: Some(stopped.prefix),
    }))
}

/// M1-A6's comparison of a run resumed after `k` events with the reference: event count,
/// `StateDigest`, `ExecutionDigest`, `TraceDigest` and the canonical trace bytes, the
/// replayed events, and, when any event ran after the checkpoint, the final registers and
/// UART output. Equal replayed events mean no UART byte or store is lost, repeated, or
/// reissued, and no response is sent twice.
pub fn ensure_same_end(reference: &Reference, k: usize, resumed: &Finished) -> Result<(), String> {
    let expected = &reference.finished;
    let (e, a) = (&expected.outcome, &resumed.outcome);
    let mut differ = Vec::new();
    if e.events != k as u64 + a.events {
        differ.push("event count");
    }
    if e.state.is_none() || e.state != a.state {
        differ.push("StateDigest");
    }
    if e.execution != a.execution {
        differ.push("ExecutionDigest");
    }
    if e.trace.is_none() || e.trace != a.trace {
        differ.push("TraceDigest");
    }
    let bytes = |f: &Finished| f.trace.as_ref().map(Trace::canonical_bytes);
    if bytes(expected) != bytes(resumed) {
        differ.push("canonical trace bytes");
    }
    if expected.dispatched.get(k..) != Some(&resumed.dispatched[..]) {
        differ.push("replayed events");
    }
    if k < expected.dispatched.len() {
        if registers(expected) != registers(resumed) {
            differ.push("registers");
        }
        if hello::uart_output(&expected.views).ok() != hello::uart_output(&resumed.views).ok() {
            differ.push("UART output");
        }
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(format!("differs in {}", differ.join(", ")))
    }
}

/// The whole M1-A6 flow for one checkpoint. `doctor` sees the snapshot bytes after the
/// old runtime is dropped; the tests use it to prove that the resumed run really starts
/// from those bytes.
pub fn checkpoint_and_resume(
    program: &Program,
    reference: &Reference,
    k: usize,
    doctor: impl FnOnce(&mut Vec<u8>),
) -> Result<(), String> {
    let mut stopped = stop_after(program, k)?;
    doctor(&mut stopped.snapshot);
    let resumed = resume(program, stopped)?;
    ensure_same_end(reference, k, &resumed)
}
