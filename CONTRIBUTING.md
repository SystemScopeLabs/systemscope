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

## M2 and M3 Golden Files

`tests/golden/m2-reference.{json,mid.snap}` hold the `block_irq.elf` result on `m2-reference` and its portable snapshot (`docs/m2-design.md` §15). `tests/golden/m3-reference.{json,mid.snap}` hold the results of both M3 disks on `m3-reference` and the portable snapshot (`docs/m3-design.md` §17.4). The same rules as the M1 golden files apply, and tests and CI only read them.

- `cargo xtask m2-golden verify` and `cargo xtask m3-golden verify` rerun the scenarios and check both files; `m3-golden verify` first checks the M3 fixtures against their manifest. `emit <dir>` and `check <dir>` compare two machines, and CI uses them between Linux and Windows. The tests are `m2_snapshot`, `m2_observation`, and `m2_golden`, and `m3_snapshot`, `m3_observation`, and `m3_golden`, in `systemscope-acceptance`.
- `cargo xtask m3-golden every-event` restores every one of the reference scenario's checkpoints, from the initial state to the final one, into a fresh platform and runs it to the end (`docs/m3-design.md` §9.2). A release build is much faster. `every-event shard <i> <n>` runs one of `n` shards of about equal cost, and CI runs eight.
- **Do not run `m2-golden bless` or `m3-golden bless` to make a failing test pass**, and never re-bless the M0, M1, or M2 golden files for an M3 change: they must stay byte-identical to their releases. CI never blesses.

## rv32 Fixtures

`tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs built from the pinned `riscv-tests`, and `manifest.json`, which records their hashes and the pins (`docs/m1-design.md` §10.2, §10.6). Tests and CI only read them, so Linux and Windows run the same bytes.

- `cargo xtask rv32-fixtures verify` checks the fixtures against the manifest, with no network or compiler. CI runs it on both operating systems.
- `cargo xtask rv32-fixtures build` rebuilds them on Linux with the pinned toolchain on `PATH` (the Ubuntu 24.04 packages the manifest names) and rewrites the manifest. Rebuild only on purpose, for a new pin or environment change, and say why in the commit body. A CI job rebuilds on a clean machine and fails on any byte difference.
- Adding or removing a test is a change to the selection in `tests/rv32/src/lib.rs` and the manifest, reviewed like a golden change.
- `build` also rebuilds `hello.elf`, `block_irq.elf` and its disk, and the M3 firmware and five user programs (`tests/rv32/m3/build-m3.sh`), each pinned by its own manifest. The M3 user programs must stay `gp`-independent: the script and `the_user_programs_are_gp_independent_rv32i_executables` check it.
- `cargo xtask m3-reference manifest` rewrites the M3 disks and expected `os.*` traces from the committed ELFs, with no compiler. `cargo xtask m3-reference run` runs both disks and judges them by `docs/m3-design.md` §12.3, and `m3-reference verify` also checks the fixtures and that two runs are identical.
- `elf/tests/fixtures/user/user.elf`, the M3.1 parser fixture, is built by hand with `elf/tests/fixtures/user/build-user.sh`, not by `rv32-fixtures build`. `elf/tests/user_fixture.rs` checks it against its manifest.

## Spike Differential

M1-A3 compares every selected `rv32ui` fixture, the generated programs for 64 fixed seeds, and 9 misaligned-access programs, retirement by retirement, with Spike at the commit pinned in `tests/rv32/build-spike.sh` (`docs/m1-design.md` §10.3). The same task then runs the directed M2 CSR programs with the M2 CPU profile (`docs/m2-design.md` §4), and the directed M3 privilege and Sv32 programs with the M3 CPU profile (`docs/m3-design.md` §5, [m3-2-spike-appendix.md](docs/m3-2-spike-appendix.md), [m3-3-spike-appendix.md](docs/m3-3-spike-appendix.md)). The known divergences from Spike are listed in appendix B.6 and are kept out of the comparison, never special-cased in it. Only these tasks need Spike; the normal test suite runs without it, on committed Spike logs in `tests/rv32/spike/`.

- `cargo xtask spike build [<dir>]` fetches and builds the pinned Spike into `<dir>` (default `target/spike`) on Linux, with git, a C++ compiler, make, and `dtc`, then verifies it.
- `cargo xtask spike verify [<dir>]` checks the build's stamp, its clean checkout at the pin, and its version line, and requires it to write the committed `simple` and seed 0 logs again, and the committed start of the `lw-1` trap log. `cargo xtask spike diff [<dir>]` verifies, then runs all 40 fixtures, 64 generated programs, and 9 misaligned-access programs on both sides, then the M2 CSR, M3 privilege, and M3 Sv32 programs; all must match, and every misaligned access must trap on both. CI runs both in a Linux job.
- `M1_PROGEN_SEED=<seed> cargo xtask spike random [<dir>]` verifies, then runs the generated program for one seed; the nightly job runs it with the night's seed, and `M1_PROGEN_SEED=<seed> cargo nextest run -p systemscope-rv32 --run-ignored only` runs that program on SystemScope alone.
- The generator (`tests/rv32/src/progen.rs`) is pinned by a digest of the fixed seeds' programs. Changing what a seed produces needs a new generator context, and the committed seed 0 log comes again from the pinned Spike, never by hand.
- `SPIKE`, if set, is the command the tasks run instead of `<dir>/bin/spike`: the same pinned build, reached another way.
- A mismatch is a bug in SystemScope or a misread of Spike's log, never a case to special-case or skip. Do not edit the committed Spike logs by hand; they come from the pinned Spike.

## ACT4 Corpus

M1-A4 runs the ACT4 RV32I tests, self-checking ELFs whose expected values come from the Sail reference model (`docs/m1-design.md` §10.4). The ELFs and `tests/act4/manifest.json` are committed; only generating them needs ACT4, Sail, and the RISC-V GCC.

- `cargo xtask act4 verify` checks the ELFs against the manifest, the pins, and the configuration hashes, and `cargo xtask act4 run` runs them all on `m1-reference`, then again with the M2 and M3 CPU profiles. Neither needs a network or an external tool. CI runs both on both operating systems.
- `cargo xtask act4 build [<cache-dir>]` regenerates the corpus on Linux x86_64 with `tests/act4/build-act4.sh`, which fetches and checks the pinned stack. Regenerate only on purpose, for a new pin or configuration, and say why in the commit body. A CI job generates twice on a clean machine and fails on any byte difference from the other generation or from the committed files.
- The ACT4 corpus covers RV32I only. M3 implements the privileged subset documented in `docs/m3-design.md` §5, which is checked against Spike, not against ACT4. Sm appears in the UDB adapter configuration only because the pinned ACT4/UDB schema needs it to express MXLEN=32; it does not describe a CPU capability, and privileged tests stay off. Do not describe SystemScope as conforming to Sm, Ss, or Sv32.
- A failing ACT4 test is a bug to diagnose: keep the ELF, check Sail's signature and results and the disassembly, then Spike if needed; add a regression test, then fix it. Fix configuration bugs in the configuration, never in the CPU. Do not patch ACT4, bypass UDB validation, or add a workaround without discussing it first.
