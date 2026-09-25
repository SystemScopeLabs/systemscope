//! Runs `block_irq.elf` on `m2-reference` once and prints this process's id on the first
//! line, then the M2 golden file this run produces, exactly as `cargo xtask m2-golden
//! bless` would write it. The M2 golden tests compare these outputs across separate
//! processes.
//!
//! Usage: `m2-run`.

use std::process::ExitCode;

use systemscope_acceptance::m2::golden::Golden;
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
