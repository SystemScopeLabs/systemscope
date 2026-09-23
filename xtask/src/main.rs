//! Workspace tasks, run as `cargo xtask <task>`.
//!
//! - `bless`: runs `m0-reference` for every fixed seed and rewrites the golden files in
//!   `tests/golden/`, printing what changed. Tests never write golden files; this is the
//!   only way they change, and the commit that changes them must say why in its body.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use systemscope_acceptance::golden::{GOLDEN_PATH, Golden, MID_SNAPSHOT_PATH, describe_changes};

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("bless") => bless(),
        _ => {
            eprintln!("usage: cargo xtask bless");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace root")
        .to_path_buf()
}

fn bless() -> ExitCode {
    let root = root();
    let json_path = root.join(GOLDEN_PATH);
    let snap_path = root.join(MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running m0-reference for every fixed seed...");
    let (new, snapshot) = Golden::generate();
    let json = new.render();

    let mut changes = describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {GOLDEN_PATH}: formatting only"));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {MID_SNAPSHOT_PATH}: bytes differ from the recorded hash"
        ));
    }
    if changes.is_empty() {
        println!("golden files are up to date");
        return ExitCode::SUCCESS;
    }

    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {GOLDEN_PATH} and {MID_SNAPSHOT_PATH}");
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}
