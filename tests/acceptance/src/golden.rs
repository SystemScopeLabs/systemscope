//! The golden files under `tests/golden/` (`docs/m0-design.md` §9, AT-1 step 2 and AT-2
//! step 4).
//!
//! Only `cargo xtask bless` writes them, through [`Golden::generate`] and
//! [`Golden::render`]. Tests read the committed copies and never write.

use serde_json::Value;
use systemscope_reference::{FULL_OPS, ReferenceConfig, build, run};

use crate::digests::{Digests, End, ensure_same, full_run};
use crate::{FIXED_SEEDS, hex, unhex32};

/// The golden digests, relative to the workspace root.
pub const GOLDEN_PATH: &str = "tests/golden/m0-reference.json";
/// The portable mid-run snapshot, relative to the workspace root.
pub const MID_SNAPSHOT_PATH: &str = "tests/golden/m0-reference.mid.snap";
/// The seed the mid-run snapshot is taken from.
pub const MID_SEED: u64 = 0xDEAD_BEEF;

/// The portable snapshot's provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mid {
    /// The run's seed.
    pub seed: u64,
    /// Events dispatched before the snapshot.
    pub after_events: u64,
    /// BLAKE3 of the snapshot file. Guards against line-ending conversion.
    pub blake3: [u8; 32],
}

/// The contents of [`GOLDEN_PATH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Golden {
    /// Operations the CPU issued.
    pub cpu_ops: u64,
    /// Writes the DMA issued.
    pub dma_ops: u64,
    /// Traced full-run digests per fixed seed, in [`FIXED_SEEDS`] order.
    pub seeds: Vec<(u64, Digests)>,
    /// The portable snapshot.
    pub mid: Mid,
}

impl Golden {
    /// Runs the reference and produces the golden digests and the mid-run snapshot.
    pub fn generate() -> (Golden, Vec<u8>) {
        let seeds: Vec<(u64, Digests)> = FIXED_SEEDS
            .iter()
            .map(|&seed| (seed, full_run(seed, true).digests()))
            .collect();
        let total = seeds
            .iter()
            .find(|(seed, _)| *seed == MID_SEED)
            .map(|(_, d)| d.events)
            .expect("the mid-run seed is a fixed seed");
        let after_events = total / 2;
        let mut rt = build(ReferenceConfig::full(MID_SEED));
        rt.init().expect("the reference initializes");
        for _ in 0..after_events {
            rt.step()
                .expect("the reference runs")
                .expect("events remain");
        }
        let snapshot = rt.snapshot().expect("between events");
        let golden = Golden {
            cpu_ops: FULL_OPS,
            dma_ops: FULL_OPS,
            seeds,
            mid: Mid {
                seed: MID_SEED,
                after_events,
                blake3: *blake3::hash(&snapshot).as_bytes(),
            },
        };
        (golden, snapshot)
    }

    /// The digests for `seed`, if it has golden digests.
    pub fn digests(&self, seed: u64) -> Option<&Digests> {
        self.seeds.iter().find(|(s, _)| *s == seed).map(|(_, d)| d)
    }

    /// The file contents: stable, pretty-printed JSON with a trailing newline.
    pub fn render(&self) -> String {
        let digest = |d: Option<[u8; 32]>| d.map_or_else(|| "null".to_owned(), |d| q(&hex(&d)));
        let seeds: Vec<String> = self
            .seeds
            .iter()
            .map(|(seed, d)| {
                format!(
                    "    {{\n      \"seed\": {seed},\n      \"events\": {},\n      \
                     \"state\": {},\n      \"execution\": {},\n      \"trace\": {}\n    }}",
                    d.events,
                    q(&hex(&d.state)),
                    q(&hex(&d.execution)),
                    digest(d.trace),
                )
            })
            .collect();
        format!(
            "{{\n  \"scenario\": \"m0-reference\",\n  \"cpu_ops\": {},\n  \"dma_ops\": {},\n  \
             \"seeds\": [\n{}\n  ],\n  \"mid_snapshot\": {{\n    \"file\": \"m0-reference.mid.snap\",\n    \
             \"seed\": {},\n    \"after_events\": {},\n    \"blake3\": {}\n  }}\n}}\n",
            self.cpu_ops,
            self.dma_ops,
            seeds.join(",\n"),
            self.mid.seed,
            self.mid.after_events,
            q(&hex(&self.mid.blake3)),
        )
    }

    /// Reads [`Golden::render`]'s output.
    pub fn parse(json: &str) -> Result<Golden, String> {
        let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if root["scenario"] != "m0-reference" {
            return Err("not the m0-reference golden file".to_owned());
        }
        let seeds = root["seeds"]
            .as_array()
            .ok_or("no seeds")?
            .iter()
            .map(|s| {
                let digests = Digests {
                    events: u(&s["events"])?,
                    state: digest(&s["state"])?,
                    execution: digest(&s["execution"])?,
                    trace: Some(digest(&s["trace"])?),
                };
                Ok((u(&s["seed"])?, digests))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mid = &root["mid_snapshot"];
        Ok(Golden {
            cpu_ops: u(&root["cpu_ops"])?,
            dma_ops: u(&root["dma_ops"])?,
            seeds,
            mid: Mid {
                seed: u(&mid["seed"])?,
                after_events: u(&mid["after_events"])?,
                blake3: digest(&mid["blake3"])?,
            },
        })
    }

    /// Fails unless the file describes today's full workload and every fixed seed.
    pub fn ensure_current(&self) -> Result<(), String> {
        if (self.cpu_ops, self.dma_ops) != (FULL_OPS, FULL_OPS) {
            return Err(format!(
                "golden digests are for {} + {} operations, the workload is {FULL_OPS} + {FULL_OPS}",
                self.cpu_ops, self.dma_ops
            ));
        }
        let seeds: Vec<u64> = self.seeds.iter().map(|(s, _)| *s).collect();
        if seeds != FIXED_SEEDS {
            return Err(format!(
                "golden seeds {seeds:x?} are not the fixed seeds {FIXED_SEEDS:x?}"
            ));
        }
        Ok(())
    }

    /// AT-1 step 2: `actual` equals the golden digests for `seed`.
    pub fn check(&self, seed: u64, actual: &Digests) -> Result<(), String> {
        self.ensure_current()?;
        let expected = self
            .digests(seed)
            .ok_or_else(|| format!("seed {seed:#x} has no golden digests"))?;
        ensure_same(expected, actual).map_err(|m| format!("seed {seed:#x}: {m} from the golden"))
    }

    /// AT-2 step 4: restores the portable snapshot in a fresh runtime, runs it to the
    /// end, and compares with the golden digests. Returns the resumed run's digests.
    pub fn check_portable(&self, snapshot: &[u8]) -> Result<Digests, String> {
        self.ensure_current()?;
        if *blake3::hash(snapshot).as_bytes() != self.mid.blake3 {
            return Err("the portable snapshot's bytes differ from the golden".to_owned());
        }
        let mut rt = build(ReferenceConfig::full(self.mid.seed));
        rt.restore(snapshot)
            .map_err(|e| format!("the portable snapshot does not restore: {e}"))?;
        if rt.snapshot().map_err(|e| e.to_string())? != snapshot {
            return Err("restoring the portable snapshot is not the identity".to_owned());
        }
        let rest = run(&mut rt).map_err(|e| format!("the resumed run failed: {e}"))?;
        let actual = End::finish(&mut rt, self.mid.after_events + rest).digests();
        let expected = self
            .digests(self.mid.seed)
            .ok_or("the mid-run seed has no golden digests")?
            .untraced();
        ensure_same(&expected, &actual)
            .map_err(|m| format!("the resumed portable snapshot {m} from the golden"))?;
        Ok(actual)
    }
}

/// One line per difference between two golden files, for `cargo xtask bless`.
pub fn describe_changes(old: Option<&Golden>, new: &Golden) -> Vec<String> {
    let Some(old) = old else {
        return vec!["+ new golden file".to_owned()];
    };
    let mut out = Vec::new();
    if (old.cpu_ops, old.dma_ops) != (new.cpu_ops, new.dma_ops) {
        out.push(format!(
            "~ workload: {} + {} -> {} + {} operations",
            old.cpu_ops, old.dma_ops, new.cpu_ops, new.dma_ops
        ));
    }
    let short = |d: &[u8; 32]| hex(&d[..8]);
    for (seed, d) in &new.seeds {
        let Some(o) = old.digests(*seed) else {
            out.push(format!("+ seed {seed:#x}"));
            continue;
        };
        if o.events != d.events {
            out.push(format!(
                "~ seed {seed:#x}: events {} -> {}",
                o.events, d.events
            ));
        }
        let fields = [
            ("StateDigest", Some(o.state), Some(d.state)),
            ("ExecutionDigest", Some(o.execution), Some(d.execution)),
            ("TraceDigest", o.trace, d.trace),
        ];
        for (name, before, after) in fields {
            if before != after {
                let show = |x: Option<[u8; 32]>| x.map_or("none".to_owned(), |x| short(&x));
                out.push(format!(
                    "~ seed {seed:#x}: {name} {}.. -> {}..",
                    show(before),
                    show(after)
                ));
            }
        }
    }
    for (seed, _) in &old.seeds {
        if new.digests(*seed).is_none() {
            out.push(format!("- seed {seed:#x}"));
        }
    }
    if old.mid != new.mid {
        out.push(format!(
            "~ mid snapshot: seed {:#x} after {} events ({}..) -> seed {:#x} after {} events ({}..)",
            old.mid.seed,
            old.mid.after_events,
            short(&old.mid.blake3),
            new.mid.seed,
            new.mid.after_events,
            short(&new.mid.blake3)
        ));
    }
    out
}

fn q(s: &str) -> String {
    format!("\"{s}\"")
}

fn u(v: &Value) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("{v} is not an unsigned integer"))
}

fn digest(v: &Value) -> Result<[u8; 32], String> {
    v.as_str()
        .and_then(unhex32)
        .ok_or_else(|| format!("{v} is not a digest"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Golden {
        let d = |n: u8| Digests {
            events: u64::from(n) * 1000,
            state: [n; 32],
            execution: [n + 1; 32],
            trace: Some([n + 2; 32]),
        };
        Golden {
            cpu_ops: FULL_OPS,
            dma_ops: FULL_OPS,
            seeds: FIXED_SEEDS
                .iter()
                .zip(1..)
                .map(|(&s, n)| (s, d(n)))
                .collect(),
            mid: Mid {
                seed: MID_SEED,
                after_events: 1500,
                blake3: [7; 32],
            },
        }
    }

    #[test]
    fn rendering_parses_back() {
        let golden = sample();
        let text = golden.render();
        assert_eq!(Golden::parse(&text), Ok(golden.clone()));
        assert!(text.ends_with("}\n"));
        assert_eq!(golden.ensure_current(), Ok(()));
    }

    #[test]
    fn digests_are_looked_up_by_seed() {
        let golden = sample();
        for (seed, d) in &golden.seeds {
            assert_eq!(golden.digests(*seed), Some(d));
            assert_eq!(golden.check(*seed, d), Ok(()));
        }
        // Each seed's digests are its own: another seed's fail.
        let (_, first) = golden.seeds[0];
        assert!(golden.check(FIXED_SEEDS[1], &first).is_err());
        assert!(golden.check(42, &first).is_err());
    }

    #[test]
    fn a_stale_workload_or_seed_list_is_refused() {
        let mut golden = sample();
        golden.dma_ops -= 1;
        assert!(golden.ensure_current().is_err());
        let (_, d) = golden.seeds[0];
        assert!(golden.check(FIXED_SEEDS[0], &d).is_err());
        let mut golden = sample();
        golden.seeds.pop();
        assert!(golden.ensure_current().is_err());
    }

    #[test]
    fn changes_are_described_field_by_field() {
        let old = sample();
        assert_eq!(describe_changes(Some(&old), &old), Vec::<String>::new());
        assert_eq!(describe_changes(None, &old), ["+ new golden file"]);
        let mut new = old.clone();
        new.seeds[1].1.execution = [0xAB; 32];
        new.mid.after_events += 1;
        let changes = describe_changes(Some(&old), &new);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(changes[0].starts_with("~ seed 0x1: ExecutionDigest "));
        assert!(changes[0].ends_with("-> abababababababab.."));
        assert!(changes[1].starts_with("~ mid snapshot:"));
    }
}
