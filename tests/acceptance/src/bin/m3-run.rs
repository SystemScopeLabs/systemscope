//! Runs both M3 disks on `m3-reference` once and prints this process's id on the first
//! line, then the M3 golden file this run produces, exactly as `cargo xtask m3-golden
//! bless` would write it. The M3 golden tests compare these outputs across separate
//! processes.
//!
//! Usage: `m3-run`.

use std::process::ExitCode;

use systemscope_acceptance::m3::golden::Golden;
use systemscope_rv32::workspace_root;

fn main() -> ExitCode {
    match Golden::generate(&workspace_root()) {
        Ok((golden, _)) => {
            println!("{{\"pid\":{}}}", std::process::id());
            print!("{}", golden.render());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
