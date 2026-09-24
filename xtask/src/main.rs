//! Workspace tasks, run as `cargo xtask <task>`.
//!
//! - `bless`: runs `m0-reference` for every fixed seed and rewrites the golden files in
//!   `tests/golden/`, printing what changed. Tests never write golden files; this is the
//!   only way they change, and the commit that changes them must say why in its body.
//! - `rv32-fixtures build`: Linux only, with the pinned toolchain on `PATH`. Rebuilds the
//!   40 `rv32ui` fixtures from the pinned `riscv-tests` with `tests/rv32/build-fixtures.sh`
//!   (the only step that uses the network) and `hello.elf` with
//!   `tests/rv32/hello/build-hello.sh`, then runs the `manifest` step. Tests never rebuild
//!   fixtures; the commit that changes them must say why in its body.
//! - `rv32-fixtures manifest [<cache-dir>]`: the second half of `build`, for a build run
//!   by hand. Checks the upstream checkout in the cache (default `target/rv32-fixtures`),
//!   rewrites `tests/rv32/fixtures/manifest.json` and `tests/rv32/hello/manifest.json`, and
//!   verifies.
//! - `rv32-fixtures verify`: checks the committed fixtures and `hello.elf` against their
//!   manifests, with no network and no compiler.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use systemscope_acceptance::golden::{GOLDEN_PATH, Golden, MID_SNAPSHOT_PATH, describe_changes};
use systemscope_rv32::hello::{self, HELLO_DIR, HELLO_MANIFEST, HELLO_SCRIPT, HelloManifest};
use systemscope_rv32::manifest::{Manifest, verify};
use systemscope_rv32::{BUILD_SCRIPT, FIXTURE_DIR, MANIFEST_PATH, SELECTED, upstream};

const USAGE: &str = "usage: cargo xtask bless\n       \
                     cargo xtask rv32-fixtures build | manifest [<cache-dir>] | verify";
/// Where `rv32-fixtures build` fetches and builds, relative to the workspace root.
const RV32_CACHE: &str = "target/rv32-fixtures";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["bless"] => bless(),
        ["rv32-fixtures", "build"] => rv32_build(),
        ["rv32-fixtures", "manifest"] => rv32_manifest(&root().join(RV32_CACHE)),
        ["rv32-fixtures", "manifest", cache] => rv32_manifest(Path::new(cache)),
        ["rv32-fixtures", "verify"] => rv32_verify(),
        _ => {
            eprintln!("{USAGE}");
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

fn rv32_build() -> ExitCode {
    if !cfg!(target_os = "linux") {
        eprintln!("rv32-fixtures build runs on Linux only (docs/m1-design.md §10.6)");
        return ExitCode::FAILURE;
    }
    let root = root();
    let mut rv32ui = vec![RV32_CACHE, FIXTURE_DIR];
    rv32ui.extend(SELECTED);
    for (script, args) in [
        (BUILD_SCRIPT, rv32ui),
        (HELLO_SCRIPT, vec![RV32_CACHE, HELLO_DIR]),
    ] {
        match Command::new("bash")
            .arg(script)
            .args(&args)
            .current_dir(&root)
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("{script} failed: {s}");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("cannot run {script}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    rv32_manifest(&root.join(RV32_CACHE))
}

fn rv32_manifest(cache: &Path) -> ExitCode {
    let root = root();
    if let Err(errors) = upstream::check(&cache.join("riscv-tests")) {
        eprintln!("the riscv-tests checkout in {} fails:", cache.display());
        for e in errors {
            eprintln!("  {e}");
        }
        return ExitCode::FAILURE;
    }
    let old = Manifest::read(&root).ok();
    let new = match Manifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(MANIFEST_PATH), new.render()) {
        eprintln!("cannot write {MANIFEST_PATH}: {e}");
        return ExitCode::FAILURE;
    }
    let old_hello = HelloManifest::read(&root).ok();
    let hello = match HelloManifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the hello manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(HELLO_MANIFEST), hello.render()) {
        eprintln!("cannot write {HELLO_MANIFEST}: {e}");
        return ExitCode::FAILURE;
    }
    if old_hello.as_ref() == Some(&hello) {
        println!("{HELLO_MANIFEST} is up to date");
    } else {
        println!("wrote {HELLO_MANIFEST}");
    }
    match &old {
        None => println!("wrote a new {MANIFEST_PATH}"),
        Some(old) => {
            let changes = systemscope_rv32::manifest::describe_differences(old, &new);
            if changes.is_empty() {
                println!("{MANIFEST_PATH} is up to date");
            } else {
                println!("{MANIFEST_PATH} changed:");
                for change in changes {
                    println!("  {change}");
                }
            }
        }
    }
    rv32_verify()
}

fn rv32_verify() -> ExitCode {
    let root = root();
    let mut ok = true;
    match verify(&root) {
        Ok(manifest) => println!(
            "{} fixtures match {MANIFEST_PATH} (riscv-tests {}, {} excluded)",
            manifest.selected.len(),
            manifest.commit,
            manifest.excluded.len()
        ),
        Err(errors) => {
            ok = false;
            eprintln!("the fixtures do not match {MANIFEST_PATH}:");
            for e in errors {
                eprintln!("  {e}");
            }
        }
    }
    match hello::verify(&root) {
        Ok(manifest) => println!(
            "{} matches {HELLO_MANIFEST} (BLAKE3 {})",
            manifest.elf,
            systemscope_rv32::hex(&manifest.blake3)
        ),
        Err(errors) => {
            ok = false;
            eprintln!("hello.elf does not match {HELLO_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
