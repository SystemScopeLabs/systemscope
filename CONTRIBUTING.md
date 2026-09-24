# Contributing

## Checks

CI runs these on every push and pull request (`.github/workflows/ci.yml`):

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo machete
cargo nextest run --workspace --locked --no-fail-fast
cargo test --workspace --doc --locked
```

The acceptance tests (`tests/acceptance`, `docs/m0-design.md` §9) run the full `m0-reference`, so they take about a minute. Run one alone with `cargo nextest run -p systemscope-acceptance --test at1` (or `at2`, `at3`, `compile_fail`).

The nightly random-seed tests are ignored by default. To reproduce a nightly failure, pass its seed:

```sh
M0_SEED=0x1234 cargo nextest run -p systemscope-acceptance --run-ignored only
```

## Golden Files

`tests/golden/` holds the M0 golden digests for the fixed seeds and a portable mid-run snapshot. Tests only read these files.

- Change them only with `cargo xtask bless`. It reruns the scenario, prints each changed digest as old → new, and rewrites the files.
- Never bless to make a failing test pass without knowing why the digests moved. A digest changes only when simulated behavior, an encoding, or the workload changes.
- Commit golden changes on their own, and say in the commit body what changed and why the new digests are right.

## M1 Golden Files

`tests/golden/m1-reference.json` and `tests/golden/m1-reference.mid.snap` hold the M1 results for `hello.elf` and the 40 `rv32ui` tests, and the portable snapshot (`docs/m1-design.md` §10.1). They are separate from the M0 golden files, and tests and CI only read them.

- `cargo xtask m1-golden verify` reruns every program and checks both files. CI runs it on both operating systems. The tests are `cargo nextest run -p systemscope-acceptance --test m1_a6` (or `m1_a7`, `m1_a8`).
- `cargo xtask m1-golden emit <dir>` writes this machine's result and snapshot to `<dir>`, and `cargo xtask m1-golden check <dir>` checks another machine's against the committed files and a local run, and restores its snapshot. CI uses them to compare Linux and Windows.
- **Do not run `cargo xtask m1-golden bless` to make a failing M1 test pass.** It rewrites both files from the current run. A result changes only when the CPU, the platform, an encoding, or a committed ELF changes on purpose; otherwise a failure is a bug to fix. Bless only then, commit the files on their own, and say in the commit body what changed and why the new values are right.

## rv32 Fixtures

`tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs built from the pinned `riscv-tests`, and `manifest.json`, which records their hashes and the pins (`docs/m1-design.md` §10.2, §10.6). Tests and CI only read them, so Linux and Windows run the same bytes.

- `cargo xtask rv32-fixtures verify` checks the fixtures against the manifest, with no network or compiler. CI runs it on both operating systems.
- `cargo xtask rv32-fixtures build` rebuilds them on Linux with the pinned toolchain on `PATH` (the Ubuntu 24.04 packages the manifest names) and rewrites the manifest. Rebuild only on purpose, for a new pin or environment change, and say why in the commit body. A CI job rebuilds on a clean machine and fails on any byte difference.
- Adding or removing a test is a change to the selection in `tests/rv32/src/lib.rs` and the manifest, reviewed like a golden change.

## Spike Differential

M1-A3 compares every selected `rv32ui` fixture, retirement by retirement, with Spike at the commit pinned in `tests/rv32/build-spike.sh` (`docs/m1-design.md` §10.3). Only these tasks need Spike; the normal test suite runs without it, on committed Spike logs in `tests/rv32/spike/`.

- `cargo xtask spike build [<dir>]` fetches and builds the pinned Spike into `<dir>` (default `target/spike`) on Linux, with git, a C++ compiler, make, and `dtc`, then verifies it.
- `cargo xtask spike verify [<dir>]` checks the build's stamp, its clean checkout at the pin, and its version line, and requires it to write the committed `simple` log again. `cargo xtask spike diff [<dir>]` verifies, then runs all 40 fixtures on both sides; all must match. CI runs both in a Linux job.
- `SPIKE`, if set, is the command the tasks run instead of `<dir>/bin/spike`: the same pinned build, reached another way.
- A mismatch is a bug in SystemScope or a misread of Spike's log, never a case to special-case or skip. Do not edit the committed Spike logs by hand; they come from the pinned Spike.
