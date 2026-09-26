//! Workspace tasks, run as `cargo xtask <task>`.
//!
//! - `bless`: runs `m0-reference` for every fixed seed and rewrites the golden files in
//!   `tests/golden/`, printing what changed. Tests never write golden files; this is the
//!   only way they change, and the commit that changes them must say why in its body.
//! - `m1-golden bless`: runs every committed program on `m1-reference` and rewrites
//!   `tests/golden/m1-reference.json` and `tests/golden/m1-reference.mid.snap`, printing
//!   what changed. As with `bless`, this is the only way they change.
//! - `m1-golden verify`: reruns everything and requires the committed M1 golden files back
//!   byte for byte, then checks the portable snapshot. Writes nothing.
//! - `m1-golden emit <dir>`: writes this machine's M1 golden file and portable snapshot
//!   into `<dir>`, for another machine to `check`.
//! - `m1-golden check <dir>`: requires another machine's emitted files to equal the
//!   committed ones and this machine's, and restores and runs its snapshot here.
//! - `m2-golden bless | verify | emit <dir> | check <dir>`: the same for `block_irq.elf`
//!   on `m2-reference`, with `tests/golden/m2-reference.json` and
//!   `tests/golden/m2-reference.mid.snap`.
//! - `rv32-fixtures build`: Linux only, with the pinned toolchain on `PATH`. Rebuilds the
//!   40 `rv32ui` fixtures from the pinned `riscv-tests` with `tests/rv32/build-fixtures.sh`
//!   (the only step that uses the network), `hello.elf` with
//!   `tests/rv32/hello/build-hello.sh`, and `block_irq.elf` with
//!   `tests/rv32/block_irq/build-block-irq.sh`, then runs the `manifest` step. Tests never
//!   rebuild fixtures; the commit that changes them must say why in its body.
//! - `rv32-fixtures manifest [<cache-dir>]`: the second half of `build`, for a build run
//!   by hand. Checks the upstream checkout in the cache (default `target/rv32-fixtures`),
//!   rewrites `tests/rv32/fixtures/manifest.json` and `tests/rv32/hello/manifest.json`,
//!   writes the `block_irq.elf` disk fixture `tests/rv32/block_irq/disk.img` and rewrites
//!   `tests/rv32/block_irq/manifest.json`, and verifies.
//! - `rv32-fixtures verify`: checks the committed fixtures, `hello.elf`, `block_irq.elf`,
//!   and its disk fixture against their manifests, with no network and no compiler.
//! - `spike build [<dir>]`: Linux only, with git, a C++ compiler, make, and dtc. Fetches
//!   and builds the pinned Spike into `<dir>` (default `target/spike`) with
//!   `tests/rv32/build-spike.sh`, then runs `verify`.
//! - `spike verify [<dir>]`: checks the Spike in `<dir>`: its stamp, its source checkout
//!   at the pin, its version line, and a smoke run of `simple` whose log must equal the
//!   committed one. Prints the binary's BLAKE3.
//! - `spike diff [<dir>]`: `verify`, then M1-A3: every selected fixture's retirements on
//!   SystemScope against Spike's, then the generated programs of the fixed seeds, then
//!   the misaligned-access programs, which must all match; then the directed M2 CSR and
//!   `MRET` programs with the M2 CPU profile against Spike with Zicsr, which must match
//!   too. Spike's logs go to `target/spike-logs`, and the generated ELFs to
//!   `target/spike-logs/progen`.
//! - `spike random [<dir>]`: `verify`, then one generated program, for the seed in
//!   `M1_PROGEN_SEED` (decimal or `0x` hex), against Spike. The nightly workflow runs it.
//!
//! - `act4 build [<cache-dir>]`: Linux x86_64 only, with git, curl, tar, xz, make, and a
//!   host C compiler. Generates the ACT4 RV32I corpus with the pinned ACT4, Sail, and GCC
//!   in `/tmp/systemscope-act4` (`tests/act4/build-act4.sh`; the cache defaults to
//!   `target/act4/cache`) into `target/act4/out`, then runs `install target/act4/out`.
//!   Tests never regenerate the corpus; the commit that changes it must say why.
//! - `act4 install <out-dir>`: the second half of `build`, for a generation run by hand.
//!   Checks its record, replaces `tests/act4/fixtures/*.elf` with its ELFs, rewrites
//!   `tests/act4/manifest.json`, and verifies.
//! - `act4 check <out-dir>`: requires a generation to reproduce the committed corpus
//!   byte for byte: the same tests, identical ELFs, and the same manifest. Writes nothing.
//! - `act4 verify`: checks the committed corpus against its manifest, with no network,
//!   ACT4, Sail, or compiler.
//! - `act4 run`: `verify`, then M1-A4: runs every committed ACT4 ELF on `m1-reference`,
//!   all of which must pass; then runs them all again with the M2 CPU profile.
//!
//! The `spike` tasks run `<dir>/bin/spike`, or the command in `SPIKE` if it is set: the
//! same pinned build, reached another way (such as through WSL). Nothing else in the
//! workspace needs Spike.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use systemscope_acceptance::golden::{GOLDEN_PATH, Golden, MID_SNAPSHOT_PATH, describe_changes};
use systemscope_acceptance::m1::golden as m1;
use systemscope_acceptance::m2::golden as m2;
use systemscope_rv32::act4::{self, ACT4_MANIFEST, ACT4_SCRIPT};
use systemscope_rv32::block_irq::{
    self, BLOCK_IRQ_DIR, BLOCK_IRQ_MANIFEST, BLOCK_IRQ_SCRIPT, BlockIrqManifest,
};
use systemscope_rv32::hello::{self, HELLO_DIR, HELLO_MANIFEST, HELLO_SCRIPT, HelloManifest};
use systemscope_rv32::manifest::{Manifest, verify};
use systemscope_rv32::progen::{self, FIXED_SEEDS, MISALIGNED};
use systemscope_rv32::spike::{
    self, DiffReport, SPIKE_COMMIT, SPIKE_DIR, SPIKE_ISA_M2, SPIKE_LOGS, SPIKE_PRIV_M3,
    SPIKE_SCRIPT,
};
use systemscope_rv32::{
    BUILD_SCRIPT, FIXTURE_DIR, MANIFEST_PATH, Rv32iProfile, SELECTED, upstream,
};
use systemscope_rv32::{csrgen, privgen};

const USAGE: &str = "usage: cargo xtask bless\n       \
                     cargo xtask m1-golden bless | verify | emit <dir> | check <dir>\n       \
                     cargo xtask m2-golden bless | verify | emit <dir> | check <dir>\n       \
                     cargo xtask rv32-fixtures build | manifest [<cache-dir>] | verify\n       \
                     cargo xtask spike build | verify | diff | random [<dir>]\n       \
                     cargo xtask act4 build [<cache-dir>] | install <out-dir> | check <out-dir> \
                     | verify | run";
/// The golden file's name in an `m1-golden emit` directory.
const M1_RESULT: &str = "m1-reference.json";
/// The golden file's name in an `m2-golden emit` directory.
const M2_RESULT: &str = "m2-reference.json";
/// Where `rv32-fixtures build` fetches and builds, relative to the workspace root.
const RV32_CACHE: &str = "target/rv32-fixtures";
/// Where `act4 build` keeps the pinned stack, relative to the workspace root.
const ACT4_CACHE: &str = "target/act4/cache";
/// Where `act4 build` generates the corpus, relative to the workspace root.
const ACT4_OUT: &str = "target/act4/out";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["bless"] => bless(),
        ["m1-golden", "bless"] => m1_bless(),
        ["m1-golden", "verify"] => m1_verify(),
        ["m1-golden", "emit", dir] => m1_emit(Path::new(dir)),
        ["m1-golden", "check", dir] => m1_check(Path::new(dir)),
        ["m2-golden", "bless"] => m2_bless(),
        ["m2-golden", "verify"] => m2_verify(),
        ["m2-golden", "emit", dir] => m2_emit(Path::new(dir)),
        ["m2-golden", "check", dir] => m2_check(Path::new(dir)),
        ["rv32-fixtures", "build"] => rv32_build(),
        ["rv32-fixtures", "manifest"] => rv32_manifest(&root().join(RV32_CACHE)),
        ["rv32-fixtures", "manifest", cache] => rv32_manifest(Path::new(cache)),
        ["rv32-fixtures", "verify"] => rv32_verify(),
        ["act4", "build"] => act4_build(&root().join(ACT4_CACHE)),
        ["act4", "build", cache] => act4_build(Path::new(cache)),
        ["act4", "install", out] => act4_install(Path::new(out)),
        ["act4", "check", out] => act4_check(Path::new(out)),
        ["act4", "verify"] => act4_verify(),
        ["act4", "run"] => act4_run(),
        ["spike", task] => spike_task(task, &root().join(SPIKE_DIR)),
        ["spike", task, dir] => spike_task(task, Path::new(dir)),
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

fn m1_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m1::GOLDEN_PATH);
    let snap_path = root.join(m1::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| m1::Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running every committed program on m1-reference...");
    let (new, snapshot) = match m1::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();

    let mut changes = m1::describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m1::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m1::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M1 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M1 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M1 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m1::GOLDEN_PATH, m1::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M1 golden file and portable snapshot.
fn m1_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m1::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m1::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m1::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m1::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

fn report(result: Result<(), Vec<String>>, ok: &str) -> ExitCode {
    match result {
        Ok(()) => {
            println!("{ok}");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for e in errors {
                eprintln!("{e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn m1_verify() -> ExitCode {
    let root = root();
    let result = m1_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m1::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "every committed program matches {}, and {} restores to it",
            m1::GOLDEN_PATH,
            m1::MID_SNAPSHOT_PATH
        ),
    )
}

fn m1_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m1::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M1_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m1::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M1 result: {M1_RESULT} and {} ({} bytes, BLAKE3 {})",
        m1::MID_FILE,
        golden.mid.size,
        systemscope_rv32::hex(&golden.mid.blake3)
    );
    ExitCode::SUCCESS
}

fn m1_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m1_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M1_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m1::MID_FILE)).map_err(read_error)?;
            m1::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M1 result equals the committed golden files and this machine's run, \
         and its snapshot restores here to the golden end",
    )
}

fn m2_bless() -> ExitCode {
    let root = root();
    let json_path = root.join(m2::GOLDEN_PATH);
    let snap_path = root.join(m2::MID_SNAPSHOT_PATH);
    let old_json = fs::read_to_string(&json_path).ok();
    let old = old_json.as_deref().and_then(|s| m2::Golden::parse(s).ok());
    let old_snap = fs::read(&snap_path).ok();

    eprintln!("running block_irq.elf on m2-reference...");
    let (new, snapshot) = match m2::Golden::generate(&root) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("not blessed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let json = new.render();

    let mut changes = m2::describe_changes(old.as_ref(), &new);
    if old.is_some() && changes.is_empty() && old_json.as_deref() != Some(json.as_str()) {
        changes.push(format!("~ {}: formatting only", m2::GOLDEN_PATH));
    }
    if old_snap.as_deref() != Some(snapshot.as_slice()) && old.is_some_and(|o| o.mid == new.mid) {
        changes.push(format!(
            "~ {}: bytes differ from the recorded hash",
            m2::MID_SNAPSHOT_PATH
        ));
    }
    if changes.is_empty() {
        println!("M2 golden files are up to date");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = fs::create_dir_all(json_path.parent().expect("has a parent"))
        .and_then(|()| fs::write(&json_path, json))
        .and_then(|()| fs::write(&snap_path, &snapshot))
    {
        eprintln!("cannot write the M2 golden files: {e}");
        return ExitCode::FAILURE;
    }
    println!("M2 golden files changed:");
    for change in &changes {
        println!("  {change}");
    }
    println!("wrote {} and {}", m2::GOLDEN_PATH, m2::MID_SNAPSHOT_PATH);
    println!(
        "Commit them on their own and explain in the commit body why the digests changed \
         (see CONTRIBUTING.md)."
    );
    ExitCode::SUCCESS
}

/// The committed M2 golden file and portable snapshot.
fn m2_committed(root: &Path) -> Result<(String, Vec<u8>), String> {
    let json = fs::read_to_string(root.join(m2::GOLDEN_PATH))
        .map_err(|e| format!("{}: {e}", m2::GOLDEN_PATH))?;
    let snap = fs::read(root.join(m2::MID_SNAPSHOT_PATH))
        .map_err(|e| format!("{}: {e}", m2::MID_SNAPSHOT_PATH))?;
    Ok((json, snap))
}

fn m2_verify() -> ExitCode {
    let root = root();
    let result = m2_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| m2::Golden::verify(&root, &json, &snap));
    report(
        result,
        &format!(
            "block_irq.elf matches {}, and {} restores to it",
            m2::GOLDEN_PATH,
            m2::MID_SNAPSHOT_PATH
        ),
    )
}

fn m2_emit(dir: &Path) -> ExitCode {
    let (golden, snapshot) = match m2::Golden::generate(&root()) {
        Ok(generated) => generated,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::create_dir_all(dir)
        .and_then(|()| fs::write(dir.join(M2_RESULT), golden.render()))
        .and_then(|()| fs::write(dir.join(m2::MID_FILE), &snapshot))
    {
        eprintln!("cannot write into {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    println!(
        "wrote this machine's M2 result: {M2_RESULT} and {} ({} bytes, BLAKE3 {})",
        m2::MID_FILE,
        golden.mid.size,
        systemscope_rv32::hex(&golden.mid.blake3)
    );
    ExitCode::SUCCESS
}

fn m2_check(dir: &Path) -> ExitCode {
    let root = root();
    let result = m2_committed(&root)
        .map_err(|e| vec![e])
        .and_then(|(json, snap)| {
            let read_error = |e: std::io::Error| vec![format!("{}: {e}", dir.display())];
            let foreign_json = fs::read_to_string(dir.join(M2_RESULT)).map_err(read_error)?;
            let foreign_snap = fs::read(dir.join(m2::MID_FILE)).map_err(read_error)?;
            m2::Golden::check_foreign(&root, (&json, &snap), (&foreign_json, &foreign_snap))
        });
    report(
        result,
        "the foreign M2 result equals the committed golden files and this machine's run, \
         and its snapshot restores here to the golden end",
    )
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
        (BLOCK_IRQ_SCRIPT, vec![RV32_CACHE, BLOCK_IRQ_DIR]),
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
    if let Err(e) = block_irq::write_disk(&root) {
        eprintln!("cannot write the block_irq disk fixture: {e}");
        return ExitCode::FAILURE;
    }
    let old_block_irq = BlockIrqManifest::read(&root).ok();
    let block_irq = match BlockIrqManifest::generate(&root) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot generate the block_irq manifest: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = fs::write(root.join(BLOCK_IRQ_MANIFEST), block_irq.render()) {
        eprintln!("cannot write {BLOCK_IRQ_MANIFEST}: {e}");
        return ExitCode::FAILURE;
    }
    if old_block_irq.as_ref() == Some(&block_irq) {
        println!("{BLOCK_IRQ_MANIFEST} is up to date");
    } else {
        println!("wrote {BLOCK_IRQ_MANIFEST}");
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
    match block_irq::verify(&root) {
        Ok(manifest) => println!(
            "{} and {} match {BLOCK_IRQ_MANIFEST} (BLAKE3 {}, {})",
            manifest.elf,
            manifest.disk,
            systemscope_rv32::hex(&manifest.blake3),
            systemscope_rv32::hex(&manifest.disk_blake3)
        ),
        Err(errors) => {
            ok = false;
            eprintln!("block_irq.elf does not match {BLOCK_IRQ_MANIFEST}:");
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

fn spike_task(task: &str, dir: &Path) -> ExitCode {
    match task {
        "build" => spike_build(dir),
        "verify" => {
            if spike_verify(dir) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        "diff" => spike_diff(dir),
        "random" => spike_random(dir),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// The Spike to run: `SPIKE`, or the one installed in `dir`.
fn spike_binary(dir: &Path) -> PathBuf {
    std::env::var_os("SPIKE").map_or_else(|| dir.join("bin/spike"), PathBuf::from)
}

fn spike_build(dir: &Path) -> ExitCode {
    if !cfg!(target_os = "linux") {
        eprintln!("spike build runs on Linux only (docs/m1-design.md §10.3)");
        return ExitCode::FAILURE;
    }
    match Command::new("bash")
        .arg(SPIKE_SCRIPT)
        .arg(dir)
        .current_dir(root())
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("{SPIKE_SCRIPT} failed: {s}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot run {SPIKE_SCRIPT}: {e}");
            return ExitCode::FAILURE;
        }
    }
    spike_task("verify", dir)
}

fn spike_verify(dir: &Path) -> bool {
    let binary = spike_binary(dir);
    match spike::verify_install(&root(), dir, &binary) {
        Ok(checked) => {
            println!("Spike {SPIKE_COMMIT} in {} verified:", dir.display());
            for line in checked {
                println!("  {line}");
            }
            true
        }
        Err(errors) => {
            eprintln!("Spike in {} is not the pinned build:", dir.display());
            for e in errors {
                eprintln!("  {e}");
            }
            false
        }
    }
}

fn spike_diff(dir: &Path) -> ExitCode {
    // A Spike from a cache or elsewhere is checked before anything is compared with it.
    if !spike_verify(dir) {
        return ExitCode::FAILURE;
    }
    let report = match spike::run_differential(&root(), &spike_binary(dir), SPIKE_LOGS) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot run the differential: {e}");
            return ExitCode::FAILURE;
        }
    };
    let spike = spike_binary(dir);
    let generated = spike::run_generated(&root(), &spike, SPIKE_LOGS, &FIXED_SEEDS);
    let misaligned = spike::run_misaligned(&root(), &spike, SPIKE_LOGS);
    let parts = [
        ("rv32ui", &report, SELECTED.len()),
        ("generated", &generated, FIXED_SEEDS.len()),
        ("misaligned", &misaligned, MISALIGNED.len()),
    ];
    let mut accepted = true;
    for (what, report, expected) in parts {
        accepted &= print_diff_report("M1-A3", what, report, expected);
    }
    if !accepted {
        return ExitCode::FAILURE;
    }
    println!(
        "M1-A3: every selected rv32ui test and the generated program of every fixed seed \
         retire exactly as on Spike {SPIKE_COMMIT}, and both trap alike on every \
         misaligned access"
    );
    let m2 = spike::run_m2(&root(), &spike, SPIKE_LOGS);
    let expected = csrgen::pass_programs().len() + csrgen::ILLEGAL.len();
    if !print_diff_report("M2 CSR", "m2-csr", &m2, expected) {
        return ExitCode::FAILURE;
    }
    println!(
        "M2 CSR: every directed Zicsr, CSR, and MRET program retires exactly as on Spike \
         {SPIKE_COMMIT} with --isa={SPIKE_ISA_M2}, CSR writes included, and both trap \
         alike on every rejected CSR"
    );
    let m3 = spike::run_m3(&root(), &spike, SPIKE_LOGS);
    if !print_diff_report("M3 privilege", "m3-priv", &m3, privgen::programs().len()) {
        return ExitCode::FAILURE;
    }
    println!(
        "M3 privilege: every directed privilege, CSR, MRET/SRET, and delegation program \
         retires exactly as on Spike {SPIKE_COMMIT} with --isa={SPIKE_ISA_M2} \
         --priv={SPIKE_PRIV_M3}, modes, CSR writes, and delegated exceptions included, up \
         to the same exception taken in M"
    );
    ExitCode::SUCCESS
}

/// Prints one part of `part_of`'s differential, then whether it is accepted.
fn print_diff_report(part_of: &str, what: &str, report: &DiffReport, expected: usize) -> bool {
    for result in &report.results {
        println!("{}", result.line());
    }
    println!(
        "{what}: selected {}, run by SystemScope {}, run by Spike {}, matched {}, \
         retirements compared {}",
        report.selected,
        report.systemscope_executed(),
        report.spike_executed(),
        report.passed(),
        report.compared()
    );
    match report.accept(expected) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{part_of} fails ({what}): {e}");
            false
        }
    }
}

fn spike_random(dir: &Path) -> ExitCode {
    let var = progen::SEED_VAR;
    let Some(seed) = std::env::var(var)
        .ok()
        .and_then(|text| systemscope_acceptance::parse_seed(&text))
    else {
        eprintln!("set {var} to the seed to test, e.g. {var}=0x1234");
        return ExitCode::FAILURE;
    };
    // Printed first, so a failing run names its seed.
    println!("{var}={seed:#x}");
    if !spike_verify(dir) {
        return ExitCode::FAILURE;
    }
    let report = spike::run_generated(&root(), &spike_binary(dir), SPIKE_LOGS, &[seed]);
    if print_diff_report("M1-A3", "random", &report, 1) {
        ExitCode::SUCCESS
    } else {
        eprintln!("the generated program for {var}={seed:#x} differs from Spike");
        ExitCode::FAILURE
    }
}

fn act4_build(cache: &Path) -> ExitCode {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        eprintln!("act4 build runs on Linux x86_64 only (docs/m1-design.md §10.4)");
        return ExitCode::FAILURE;
    }
    let root = root();
    let out = root.join(ACT4_OUT);
    match Command::new("bash")
        .arg(ACT4_SCRIPT)
        .arg(cache)
        .arg(&out)
        .current_dir(&root)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("{ACT4_SCRIPT} failed: {s}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot run {ACT4_SCRIPT}: {e}");
            return ExitCode::FAILURE;
        }
    }
    act4_install(&out)
}

fn act4_install(out: &Path) -> ExitCode {
    match act4::install(&root(), out) {
        Ok((new, None)) => println!("wrote a new {ACT4_MANIFEST} with {} tests", new.count),
        Ok((new, Some(old))) => {
            let changes = act4::describe_differences(&old, &new);
            if changes.is_empty() {
                println!("{ACT4_MANIFEST} is up to date");
            } else {
                println!("{ACT4_MANIFEST} changed:");
                for change in changes {
                    println!("  {change}");
                }
            }
        }
        Err(errors) => {
            eprintln!("cannot install the generation in {}:", out.display());
            for e in errors {
                eprintln!("  {e}");
            }
            return ExitCode::FAILURE;
        }
    }
    act4_verify()
}

fn act4_check(out: &Path) -> ExitCode {
    match act4::check_regenerated(&root(), out) {
        Ok(manifest) => {
            println!(
                "the generation in {} reproduces the committed ACT4 corpus: {} ELFs and \
                 {ACT4_MANIFEST}, byte for byte",
                out.display(),
                manifest.count
            );
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!(
                "the generation in {} does not reproduce the committed corpus:",
                out.display()
            );
            for e in errors {
                eprintln!("  {e}");
            }
            ExitCode::FAILURE
        }
    }
}

fn act4_verify() -> ExitCode {
    match act4::verify(&root()) {
        Ok(manifest) => {
            print_act4_summary(&manifest);
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("the ACT4 corpus does not match {ACT4_MANIFEST}:");
            for e in errors {
                eprintln!("  {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// The corpus's pins and instruction audit, as `verify` and `run` print them.
fn print_act4_summary(manifest: &act4::Act4Manifest) {
    println!(
        "{} ACT4 ELFs match {ACT4_MANIFEST} (ACT4 {}, Sail {}, extensions {})",
        manifest.count,
        act4::ACT4_COMMIT,
        act4::SAIL_VERSION,
        act4::EXTENSIONS
    );
    println!(
        "architectural capability {}; ACT4 adapter schema extensions {} (Sm is a schema \
         shim, not a capability); include_priv_tests {}",
        manifest.capability,
        manifest.adapter_extensions.join(" + "),
        manifest.include_priv_tests
    );
    let mut total = std::collections::BTreeMap::<&str, u64>::new();
    for t in &manifest.tests {
        for (m, n) in &t.recorded.instructions {
            *total.entry(m.as_str()).or_default() += n;
        }
    }
    let outside: u64 = total
        .iter()
        .filter(|(m, _)| !act4::ALLOWED_MNEMONICS.contains(m))
        .map(|(_, n)| n)
        .sum();
    println!(
        "instruction audit: {} distinct mnemonics; outside RV32I and ECALL (CSR, MRET, SRET, \
         WFI, M, A, C, Zifencei, ...): {outside}; ecall {}, fence.tso {}",
        total.len(),
        total.get("ecall").copied().unwrap_or(0),
        total.get("fence.tso").copied().unwrap_or(0)
    );
}

fn act4_run() -> ExitCode {
    let (manifest, report) = match act4::run_corpus(&root()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    print_act4_summary(&manifest);
    for result in &report.results {
        println!("{}", result.line());
    }
    println!(
        "manifest {}, fixtures {}, selected {}, executed {}, passed {}, failed {}",
        manifest.count,
        manifest.tests.len(),
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    match report.accept(manifest.count) {
        Ok(()) => {
            println!(
                "M1-A4: all {} ACT4 RV32I tests pass on m1-reference, each with its exact \
                 TEST PASSED summary",
                manifest.count
            );
        }
        Err(e) => {
            eprintln!("M1-A4 fails: {e}");
            return ExitCode::FAILURE;
        }
    }
    // docs/m2-design.md §15.4: the M2 CPU profile passes the same corpus.
    let report = match act4::run_corpus_with(&root(), Rv32iProfile::M2) {
        Ok((_, r)) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    for result in report.results.iter().filter(|r| r.verdict.is_err()) {
        println!("{}", result.line());
    }
    println!(
        "M2 CPU profile: selected {}, executed {}, passed {}, failed {}",
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    if let Err(e) = report.accept(manifest.count) {
        eprintln!("the M2 CPU profile fails the ACT4 corpus: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "all {} ACT4 RV32I tests also pass with the M2 CPU profile",
        manifest.count
    );
    // docs/m3-design.md §15.4: the M3 CPU profile passes it too, in M-mode.
    let report = match act4::run_corpus_with(&root(), Rv32iProfile::M3) {
        Ok((_, r)) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    for result in report.results.iter().filter(|r| r.verdict.is_err()) {
        println!("{}", result.line());
    }
    println!(
        "M3 CPU profile: selected {}, executed {}, passed {}, failed {}",
        report.selected,
        report.executed(),
        report.passed(),
        report.failed()
    );
    match report.accept(manifest.count) {
        Ok(()) => {
            println!(
                "all {} ACT4 RV32I tests also pass with the M3 CPU profile",
                manifest.count
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("the M3 CPU profile fails the ACT4 corpus: {e}");
            ExitCode::FAILURE
        }
    }
}
