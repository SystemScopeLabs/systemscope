//! Runs the full `m0-reference` once, traced, and prints one line of JSON: this process's
//! id, the seed, the event count, and the three digests. AT-1 compares these lines across
//! separate processes.
//!
//! Usage: `m0-run <seed>`, with the seed in decimal or `0x` hex.

use std::process::ExitCode;

use systemscope_acceptance::digests::full_run;
use systemscope_acceptance::parse_seed;
use systemscope_acceptance::process::ProcessRun;

fn main() -> ExitCode {
    let Some(seed) = std::env::args().nth(1).as_deref().and_then(parse_seed) else {
        eprintln!("usage: m0-run <seed>");
        return ExitCode::FAILURE;
    };
    let run = ProcessRun {
        pid: std::process::id(),
        seed,
        digests: full_run(seed, true).digests(),
    };
    println!("{}", run.to_line());
    ExitCode::SUCCESS
}
