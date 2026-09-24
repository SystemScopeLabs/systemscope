//! Runs every committed program on `m1-reference` once and prints this process's id on
//! the first line, then the M1 golden file this run produces, exactly as `cargo xtask
//! m1-golden bless` would write it. M1-A8 compares these outputs across separate
//! processes.
//!
//! Usage: `m1-run`.

use std::process::ExitCode;

use systemscope_acceptance::m1::golden::Golden;
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
