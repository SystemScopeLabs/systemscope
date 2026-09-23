//! AT-2: checkpoint, drop, and resume (`docs/m0-design.md` §9).
//!
//! A checkpoint is a number of events `k`. The run that took it is gone before the resumed
//! one starts: only the snapshot bytes and the trace prefix cross over.

use systemscope_contracts::event::{EventKey, Phase};
use systemscope_reference::{ReferenceConfig, build, run};
use systemscope_runtime::trace::Trace;

use crate::digests::{End, ensure_same};

/// The uninterrupted, traced reference run, with the key of every event.
pub struct Reference {
    /// How it ended.
    pub end: End,
    /// Every dispatched event's key, in order.
    pub keys: Vec<EventKey>,
}

/// Runs the full reference for `seed` one event at a time.
///
/// # Panics
///
/// If the run faults, which the reference never does.
pub fn reference(seed: u64) -> Reference {
    let mut rt = build(ReferenceConfig::full(seed));
    rt.start_trace().expect("tracing starts before init");
    rt.init().expect("the reference initializes");
    let mut keys = Vec::new();
    while let Some(d) = rt.step().expect("the reference runs") {
        keys.push(d.key);
    }
    let end = End::finish(&mut rt, keys.len() as u64);
    Reference { end, keys }
}

fn same_slot(a: EventKey, b: EventKey) -> bool {
    (a.tick, a.phase) == (b.tick, b.phase)
}

/// The checkpoints of §9 AT-2 step 2, as events dispatched before the snapshot. The
/// structural ones are searched from a quarter of the way in, so the run is busy.
///
/// # Panics
///
/// If the run never reaches one of the structural checkpoints.
pub fn checkpoints(keys: &[EventKey], seed: u64) -> Vec<(&'static str, usize)> {
    let n = keys.len();
    let find = |what: &'static str, holds: &dyn Fn(usize) -> bool| {
        let k = (n / 4..n)
            .find(|&k| k > 0 && holds(k))
            .unwrap_or_else(|| panic!("seed {seed:#x} never reaches: {what}"));
        (what, k)
    };
    let mut out = vec![
        ("right after init", 0),
        ("after the first event", 1),
        find("at a tick boundary", &|k| keys[k - 1].tick < keys[k].tick),
        find("between Complete and Commit of one tick", &|k| {
            keys[k - 1].tick == keys[k].tick
                && keys[k - 1].phase == Phase::Complete
                && keys[k].phase == Phase::Commit
        }),
        find("mid-phase with events left in it", &|k| {
            k + 1 < n && same_slot(keys[k - 1], keys[k]) && same_slot(keys[k], keys[k + 1])
        }),
        ("halfway", n / 2),
        ("before the last event", n - 1),
    ];
    // splitmix64, so the indices depend only on the seed.
    let mut x = seed ^ 0xA7_2C4E_C0DE;
    for _ in 0..8 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push(("seeded random", (z % (n as u64 + 1)) as usize));
    }
    out
}

/// What a stopped run leaves behind.
pub struct Stopped {
    /// The snapshot bytes.
    pub snapshot: Vec<u8>,
    /// The trace so far.
    pub prefix: Trace,
}

/// Runs `seed` traced for `k` events, snapshots, and drops the runtime.
///
/// # Panics
///
/// If the run faults or ends before `k` events.
pub fn stop_after(seed: u64, k: usize) -> Stopped {
    let mut rt = build(ReferenceConfig::full(seed));
    rt.start_trace().expect("tracing starts before init");
    rt.init().expect("the reference initializes");
    for _ in 0..k {
        rt.step()
            .expect("the reference runs")
            .expect("events remain");
    }
    let snapshot = rt.snapshot().expect("between events");
    let prefix = rt.take_trace().expect("traced");
    drop(rt);
    Stopped { snapshot, prefix }
}

/// Elaborates a fresh runtime, restores `stopped`, checks the round-trip law, resumes the
/// trace, and runs to the end.
pub fn resume(seed: u64, k: usize, stopped: Stopped) -> Result<End, String> {
    let mut rt = build(ReferenceConfig::full(seed));
    rt.restore(&stopped.snapshot)
        .map_err(|e| format!("restore failed: {e}"))?;
    let again = rt.snapshot().map_err(|e| e.to_string())?;
    if again != stopped.snapshot {
        return Err("encode(restore(decode(bytes))) != bytes".to_owned());
    }
    rt.resume_trace(stopped.prefix)
        .map_err(|e| format!("resume_trace failed: {e}"))?;
    let rest = run(&mut rt).map_err(|e| format!("the resumed run failed: {e}"))?;
    Ok(End::finish(&mut rt, k as u64 + rest))
}

/// The whole AT-2 flow for one checkpoint. `doctor` sees the snapshot bytes after the old
/// runtime is dropped; the tests use it to prove that the resumed run really starts from
/// those bytes.
pub fn checkpoint_and_resume(
    seed: u64,
    k: usize,
    doctor: impl FnOnce(&mut Vec<u8>),
) -> Result<End, String> {
    let mut stopped = stop_after(seed, k);
    doctor(&mut stopped.snapshot);
    resume(seed, k, stopped)
}

/// AT-2's comparison: every digest, the canonical trace bytes, and the final snapshot.
pub fn ensure_same_end(expected: &End, actual: &End) -> Result<(), String> {
    ensure_same(&expected.digests(), &actual.digests()).map_err(|m| m.to_string())?;
    let bytes = |e: &End| e.trace.as_ref().map(Trace::canonical_bytes);
    if bytes(expected) != bytes(actual) {
        return Err("canonical trace bytes differ".to_owned());
    }
    if expected.snapshot != actual.snapshot {
        return Err("final snapshots differ".to_owned());
    }
    Ok(())
}
