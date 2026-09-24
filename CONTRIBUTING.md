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

`tests/golden/` holds the golden digests for the fixed seeds and a portable mid-run snapshot. Tests only read these files.

- Change them only with `cargo xtask bless`. It reruns the scenario, prints each changed digest as old → new, and rewrites the files.
- Never bless to make a failing test pass without knowing why the digests moved. A digest changes only when simulated behavior, an encoding, or the workload changes.
- Commit golden changes on their own, and say in the commit body what changed and why the new digests are right.

## rv32 Fixtures

`tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs built from the pinned `riscv-tests`, and `manifest.json`, which records their hashes and the pins (`docs/m1-design.md` §10.2, §10.6). Tests and CI only read them, so Linux and Windows run the same bytes.

- `cargo xtask rv32-fixtures verify` checks the fixtures against the manifest, with no network or compiler. CI runs it on both operating systems.
- `cargo xtask rv32-fixtures build` rebuilds them on Linux with the pinned toolchain on `PATH` (the Ubuntu 24.04 packages the manifest names) and rewrites the manifest. Rebuild only on purpose, for a new pin or environment change, and say why in the commit body. A CI job rebuilds on a clean machine and fails on any byte difference.
- Adding or removing a test is a change to the selection in `tests/rv32/src/lib.rs` and the manifest, reviewed like a golden change.
