//! AT-1 step 1: runs in separate processes (`docs/m0-design.md` §9).
//!
//! The `m0-run` binary runs the full reference once and prints one line of JSON with its
//! process id, so a test can prove that the runs it compares really came from different
//! processes.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::digests::{Digests, ensure_same};
use crate::{hex, unhex32};

/// What one `m0-run` process reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessRun {
    /// Its process id.
    pub pid: u32,
    /// The seed it ran.
    pub seed: u64,
    /// Its digests.
    pub digests: Digests,
}

impl ProcessRun {
    /// The line `m0-run` prints.
    pub fn to_line(&self) -> String {
        let d = &self.digests;
        format!(
            "{{\"pid\":{},\"seed\":{},\"events\":{},\"state\":\"{}\",\"execution\":\"{}\",\"trace\":\"{}\"}}",
            self.pid,
            self.seed,
            d.events,
            hex(&d.state),
            hex(&d.execution),
            d.trace.map_or_else(String::new, |t| hex(&t)),
        )
    }

    /// Reads a line [`ProcessRun::to_line`] wrote.
    pub fn parse(line: &str) -> Result<ProcessRun, String> {
        let v: Value = serde_json::from_str(line.trim()).map_err(|e| format!("{e}: {line:?}"))?;
        let u = |name: &str| {
            v[name]
                .as_u64()
                .ok_or_else(|| format!("no {name} in {line:?}"))
        };
        let digest = |name: &str| {
            v[name]
                .as_str()
                .and_then(unhex32)
                .ok_or_else(|| format!("no {name} in {line:?}"))
        };
        Ok(ProcessRun {
            pid: u32::try_from(u("pid")?).map_err(|e| e.to_string())?,
            seed: u("seed")?,
            digests: Digests {
                events: u("events")?,
                state: digest("state")?,
                execution: digest("execution")?,
                trace: Some(digest("trace")?),
            },
        })
    }
}

/// Starts `count` `m0-run` processes for `seed` at once and collects their reports.
pub fn run_in_processes(exe: &Path, seed: u64, count: usize) -> Result<Vec<ProcessRun>, String> {
    let children = (0..count)
        .map(|_| {
            Command::new(exe)
                .arg(format!("{seed:#x}"))
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("cannot start {}: {e}", exe.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(format!("m0-run failed: {}", out.status));
            }
            ProcessRun::parse(&String::from_utf8_lossy(&out.stdout))
        })
        .collect()
}

/// AT-1 step 1: at least two runs, each from its own process other than this one, all
/// of `seed`, and all with the same digests.
pub fn ensure_reproduced(seed: u64, runs: &[ProcessRun]) -> Result<(), String> {
    if runs.len() < 2 {
        return Err("reproducibility needs at least two runs".to_owned());
    }
    let me = std::process::id();
    for (i, run) in runs.iter().enumerate() {
        if run.pid == me {
            return Err(format!("run {i} came from the test process itself"));
        }
        if runs[..i].iter().any(|r| r.pid == run.pid) {
            return Err(format!("run {i} shares process {} with another", run.pid));
        }
        if run.seed != seed {
            return Err(format!("run {i} ran seed {:#x}, not {seed:#x}", run.seed));
        }
        ensure_same(&runs[0].digests, &run.digests)
            .map_err(|m| format!("seed {seed:#x}: run {i} {m} from run 0"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pid: u32) -> ProcessRun {
        ProcessRun {
            pid,
            seed: 7,
            digests: Digests {
                events: 5,
                state: [1; 32],
                execution: [2; 32],
                trace: Some([3; 32]),
            },
        }
    }

    #[test]
    fn lines_round_trip() {
        let r = run(12);
        assert_eq!(ProcessRun::parse(&format!("{}\n", r.to_line())), Ok(r));
        assert!(ProcessRun::parse("{}").is_err());
    }

    #[test]
    fn reproduction_needs_distinct_foreign_processes_and_equal_digests() {
        let me = std::process::id();
        let (a, b) = (me.wrapping_add(1), me.wrapping_add(2));
        assert_eq!(ensure_reproduced(7, &[run(a), run(b)]), Ok(()));
        assert!(ensure_reproduced(7, &[run(a)]).is_err());
        assert!(ensure_reproduced(7, &[run(a), run(a)]).is_err());
        assert!(ensure_reproduced(7, &[run(a), run(me)]).is_err());
        assert!(ensure_reproduced(8, &[run(a), run(b)]).is_err());
        let mut other = run(b);
        other.digests.trace = Some([9; 32]);
        assert!(ensure_reproduced(7, &[run(a), other]).is_err());
    }
}
