//! The three digests of §9.2, and finished runs of the full reference.

use std::fmt;

use systemscope_contracts::event::EventKey;
use systemscope_reference::{ReferenceConfig, build, run};
use systemscope_runtime::runtime::Runtime;
use systemscope_runtime::trace::Trace;

/// What two runs must agree on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digests {
    /// Events dispatched in the whole run.
    pub events: u64,
    /// `StateDigest`: BLAKE3 of the final snapshot.
    pub state: [u8; 32],
    /// `ExecutionDigest`.
    pub execution: [u8; 32],
    /// `TraceDigest`, when the run was traced.
    pub trace: Option<[u8; 32]>,
}

impl Digests {
    /// The same digests without the trace, for comparison with an untraced run.
    pub fn untraced(self) -> Digests {
        Digests {
            trace: None,
            ..self
        }
    }
}

/// The digests that differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch(pub Vec<&'static str>);

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "differs in {}", self.0.join(", "))
    }
}

/// Compares every digest and names each one that differs. A trace digest on one side
/// only is a difference.
pub fn ensure_same(expected: &Digests, actual: &Digests) -> Result<(), Mismatch> {
    let mut differ = Vec::new();
    if expected.events != actual.events {
        differ.push("event count");
    }
    if expected.state != actual.state {
        differ.push("StateDigest");
    }
    if expected.execution != actual.execution {
        differ.push("ExecutionDigest");
    }
    if expected.trace != actual.trace {
        differ.push("TraceDigest");
    }
    if differ.is_empty() {
        Ok(())
    } else {
        Err(Mismatch(differ))
    }
}

/// A finished run.
#[derive(Debug)]
pub struct End {
    /// Events dispatched in the whole run, including before any checkpoint.
    pub events: u64,
    /// The last dispatched event.
    pub last: Option<EventKey>,
    /// The final snapshot.
    pub snapshot: Vec<u8>,
    /// The final `ExecutionDigest`.
    pub execution: [u8; 32],
    /// The recorded trace, when the run was traced.
    pub trace: Option<Trace>,
}

impl End {
    /// Collects a finished runtime's results.
    ///
    /// # Panics
    ///
    /// If the runtime cannot be snapshotted, which only a faulted run causes.
    pub fn finish(rt: &mut Runtime, events: u64) -> End {
        End {
            events,
            last: rt.last_dispatched(),
            snapshot: rt.snapshot().expect("a finished run snapshots"),
            execution: rt.execution_digest(),
            trace: rt.take_trace(),
        }
    }

    /// The run's digests.
    pub fn digests(&self) -> Digests {
        Digests {
            events: self.events,
            state: *blake3::hash(&self.snapshot).as_bytes(),
            execution: self.execution,
            trace: self.trace.as_ref().map(Trace::digest),
        }
    }
}

/// Runs the full reference for `seed` from `init` to the end.
///
/// # Panics
///
/// If the run faults, which the reference never does.
pub fn full_run(seed: u64, traced: bool) -> End {
    let mut rt = build(ReferenceConfig::full(seed));
    if traced {
        rt.start_trace().expect("tracing starts before init");
    }
    rt.init().expect("the reference initializes");
    let events = run(&mut rt).expect("the reference runs");
    assert_eq!(rt.pending(), 0, "the full reference drains before T_END");
    End::finish(&mut rt, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Digests = Digests {
        events: 10,
        state: [1; 32],
        execution: [2; 32],
        trace: Some([3; 32]),
    };

    #[test]
    fn each_digest_is_compared_on_its_own() {
        assert_eq!(ensure_same(&BASE, &BASE), Ok(()));
        let variants: [(&str, Digests); 5] = [
            ("event count", Digests { events: 11, ..BASE }),
            (
                "StateDigest",
                Digests {
                    state: [9; 32],
                    ..BASE
                },
            ),
            (
                "ExecutionDigest",
                Digests {
                    execution: [9; 32],
                    ..BASE
                },
            ),
            (
                "TraceDigest",
                Digests {
                    trace: Some([9; 32]),
                    ..BASE
                },
            ),
            ("TraceDigest", BASE.untraced()),
        ];
        for (name, other) in variants {
            assert_eq!(ensure_same(&BASE, &other), Err(Mismatch(vec![name])));
            assert_eq!(ensure_same(&other, &BASE), Err(Mismatch(vec![name])));
        }
        assert_eq!(
            ensure_same(
                &BASE,
                &Digests {
                    state: [0; 32],
                    execution: [0; 32],
                    ..BASE
                }
            ),
            Err(Mismatch(vec!["StateDigest", "ExecutionDigest"]))
        );
    }
}
