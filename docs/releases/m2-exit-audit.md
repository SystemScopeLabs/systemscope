# M2 exit audit (M2.10)

This audit shows that M2, `docs/m2-design.md`, is complete, reproducible, and deterministic,
and that it is frozen as a release candidate. It adds nothing to M2. No source, schema,
contract, fixture, or golden file changed, and nothing was re-blessed. This audit does not
create a tag, a release, a version bump, or a changelog entry.

- Base: `main` at `278b452fbfd2b74f52898169168d73805f82c53c`
  (`test(rv32): close M2 reference snapshot determinism`, the M2.9 merge).
- Contracts pin: `SystemScopeLabs/contracts` at
  `90900a128d51f6b42e86dc789447882c90c52ef4` (`feat(contracts): add M2 irq and block
  protocols`). The checkout is clean.
- Toolchain: `rust-toolchain.toml`. Local runs used Windows 11 x86-64 with 12 threads. CI
  runs are listed in §7.

## 1. Frozen artifact inventory

### Contracts and protocol versions

- Between `v0.2.0-m1` (`4d6e912`) and `90900a1`, the contracts diff touches only these
  files:
  - `README.md`;
  - `src/canonical.rs`, `src/protocol/mod.rs`;
  - the new `src/protocol/block_v0.rs` and `src/protocol/irq_v0.rs`;
  - their encoding tests.
- `mem.v0` (`src/protocol/mem.rs`) and `mem.v1` (`src/protocol/mem_v1.rs`) are unchanged
  since M1.
- `COMPATIBILITY_ID` (`src/lib.rs`, `"0.0.0"`) is unchanged since M1.
- `block.v0` and `irq.v0` are unchanged since the M2.1 pin.
- The workspace still pins `90900a1` (`fd36b2f`, M2.1).

### Components

The snapshot schema is each component's `SNAPSHOT_SCHEMA`. The runtime snapshot container
is `SNAPSHOT_FORMAT_VERSION = 1` (`runtime/src/snapshot.rs`).

| Artifact | Source | Owning tests | Snapshot schema | Golden coverage | Mutation coverage |
|---|---|---|---|---|---|
| CPU (RV32I + Zicsr subset, MEI, MRET) | `components/rv32i/src/{cpu,csr}.rs` | `components/rv32i/tests/{cpu_m2,csr,cpu_mei,cpu_mei_runtime,cpu_irqc_runtime,cpu_protocol}.rs`; Spike M2 CSR differential | M1 profile 1, M2 profile `SNAPSHOT_SCHEMA_M2 = 2` | `m2-reference.json` (registers, instret, cause, digests) | M2.2a 5/5, M2.2b 5/5 |
| IRQ controller | `components/platform/src/irqc.rs` | `components/platform/tests/irqc.rs`, `components/rv32i/tests/cpu_irqc_runtime.rs` | 1 | interrupts ×3, `soc.irqc` in the mid snapshot | M2.3 5/5 |
| MultiMasterBus | `components/platform/src/mmbus.rs` | `components/platform/tests/mmbus.rs`; `tests/rv32/tests/block_irq.rs` (contention, renaming) | 1 | `soc.bus` in the mid snapshot; state digest | M2.4 5/5 |
| SimpleBlockMedia | `components/platform/src/media.rs` | `components/platform/tests/media.rs` | 1 | `disk_blake3`, disk blocks, `soc.disk` | M2.5 5/5 |
| DMA controller (registers, command, IRQ) | `components/platform/src/dma.rs` | `components/platform/tests/dma.rs` | 1 | disk ops, `soc.blk` | M2.6 5/5 |
| DMA READ engine | `components/platform/src/dma.rs` | `components/platform/tests/dma_read.rs` | 1 (shared) | read LBA 0, read LBA 1, 64 beats | M2.7a 7/7 |
| DMA WRITE engine | `components/platform/src/dma.rs` | `components/platform/tests/dma_write.rs` | 1 (shared) | write LBA 1, 32 beats, LBA 1 bytes | M2.7b 8/8 |
| DMA fault semantics | `components/platform/src/dma.rs` | `components/platform/tests/dma_faults.rs`, `dma_snapshot.rs` | 1 (shared) | `ERROR 0` checked by `the_golden_is_the_frozen_baseline` | M2.7c 11/11, M2.7d 12/12 (snapshot state space) |
| Snapshot system | `runtime/src/snapshot.rs`, `tests/acceptance/src/m2/checkpoint.rs` | `tests/acceptance/tests/m2_snapshot.rs`, `m2_observation.rs` | container 1 | `m2-reference.mid.snap` | M2.9 11/11 |
| `m2-reference` platform | `tests/rv32/src/m2ref.rs` | `tests/rv32/tests/block_irq.rs` (topology, wiring, builder) | n/a | `image_hash`, platform in the golden | M2.8 5/5 |
| `block_irq.elf`, `disk.img` | `tests/rv32/block_irq/` | `tests/rv32/tests/block_irq.rs`, `cargo xtask rv32-fixtures verify` | n/a | `elf_blake3 39f23b9f…88bc8`, `disk_blake3 ffcfaec0…83b43` | M2.8 5/5 |
| Golden artifacts | `tests/golden/m2-reference.{json,mid.snap}` | `tests/acceptance/tests/m2_golden.rs`, `cargo xtask m2-golden verify` | n/a | itself | M2.9 (drift cases) |

No code changed for this inventory.

## 2. Regression matrix

All of these ran locally at `278b452` in the M2.10 worktree.

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo check --workspace --all-targets --locked` | ok |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ok |
| `cargo nextest run --workspace --locked` | 842 passed, 0 failed, 4 skipped (the `#[ignore]` nightly tests) |
| `cargo test --workspace --doc --locked` | 8 doctest groups ok, 0 failed |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` | ok |
| `cargo machete` | no unused dependencies |
| `git diff --check` | clean |
| `cargo xtask rv32-fixtures verify` | 40 rv32ui fixtures, `hello.elf`, `block_irq.elf` + `disk.img` match their manifests |
| `cargo xtask m1-golden verify` | M1 golden and mid snapshot reproduce |
| `cargo xtask m2-golden verify` | M2 golden and mid snapshot reproduce |
| `cargo xtask act4 verify` | 39 ELFs match (ACT4 `54cfe21`, Sail 0.14.1); 0 instructions outside RV32I and ECALL |
| `cargo xtask act4 run` | M1 profile 39/39, M2 CPU profile 39/39 |
| Spike differential (CI `spike` job, Spike `19609434`) | rv32ui 40/40 (13,268 retirements), generated 64/64 (22,344), misaligned 9/9 (45), M2 CSR 20/20 (372) |
| `git status` after the matrix | clean (no blessing, no generated drift) |

Spike runs only in CI (Linux), in main run 36112983981.

## 3. Reference run

`block_irq.elf` runs on the official `m2-reference` builder. These tests check the run,
and each passed at `278b452`:
- `tests/rv32/tests/block_irq.rs`;
- `m2_golden.rs`: `the_golden_is_the_frozen_baseline`, `fresh_platforms_reproduce_every_field`,
  `the_run_reproduces_in_separate_processes`.

- **Terminal:** `Trap(EnvironmentCall)`, tval 0, `gp = 1`, `a0 = 0`; instret 2401,
  events 22,000.
- **UART:** `"M2 PASS\n"`.
- **DMA:** READ LBA 0 → WRITE LBA 1 → READ LBA 1, three ops, 96 beats. The controller ends
  with `ERROR 0`, `REJECTED` never set, and the IRQ deasserted.
- **Interrupts:** exactly 3, each `mcause 0x8000000B`, with the handler counter at 3.
  - The line levels are exactly `[(0,high),(0,low)] × 3`, and so is MEIP. The CPU receives
    only those six levels.
  - No interrupt is taken between handler entry and MRET, so none re-enters. MRET returns
    to `mepc`.
  - Every completion is causally ordered: command < done < line high < MEIP high <
    interrupt < line low < MEIP low < ACK < flag < MRET < resumed.
- **Media:** LBA 0 is kept (the pattern). LBA 1 is written (the transformed block), and
  both are checked by BLAKE3.
- **Digests:** state `cd04fb9b…5dd3`, execution `f9b6214d…ce0d`, trace `4534ee4c…90c3`.
- **Repetition:**
  - `fresh_platforms_reproduce_every_field` runs three fresh platforms and gets identical
    records.
  - Two golden generations render byte-identical JSON and portable snapshots.
  - `two_runs_are_identical` gets the same outcome, views, snapshot, state, and canonical
    trace bytes. Tracing changes nothing.
  - Two separate `m2-run` processes both print the committed golden byte for byte.

## 4. Snapshot closure

From `every_event_resumes_to_the_same_end` and its sibling tests, run locally at
`278b452`:

| Measure | Value |
|---|---|
| Events in the reference run | 22,000 |
| Checkpoints (events + 1, `0..=22000`) | 22,001 |
| Successful restores | 22,001 |
| Failed restores | 0 |
| Owner classes covered | all 19 (for example CpuRequest 3,232, RamRequest 3,653, DmaRamRequest 192, MediaResult 77, MeiEntry 3, MretNext 3, CpuBehindDma 192, DmaBehindCpu 190) |
| Design stress points (§13.1) | 8 of 8, each with ≥ 3 checkpoints |
| Portable mid snapshot | 14,356 bytes after 13,462 events; BLAKE3 `5233f9e0…0764`; canonical |

Each checkpoint is restored, re-snapshotted, and checked for identical bytes. It is then
resumed to the end and compared with the uninterrupted run on these:
- every digest;
- the snapshot;
- UART bytes, disk ops, interrupts, handler entries;
- the first boundary after the checkpoint.

The snapshot holds CPU, bus, DMA, IRQ controller, media, RAM, and the queued events. So any
lost, duplicated, or reordered event, or any unsaved component state, changes the result.
IRQ-critical checkpoints always see exactly three interrupts in total (taken before plus
rising after). Malformed, mismatched, and failed restores are rejected with exact errors,
and a failed restore faults the session.

## 5. Cross-platform

CI main run 36112983981 has two emitted result artifacts: `m2-result-ubuntu-latest` and
`m2-result-windows-latest`. They were downloaded and compared with `cmp`:

| File | Linux == Windows | == committed |
|---|---|---|
| `m2-reference.json` (2,514 bytes) | identical | identical |
| `m2-reference.mid.snap` (14,356 bytes) | identical | identical |
| `m1-reference.json` | identical | identical |
| `m1-reference.mid.snap` | identical | identical |

- The JSON carries the snapshot hash, the trace digest, the state digest, and the execution
  digest. All four are therefore equal across the two OSes.
- The emitted JSON has no carriage returns, and `the_golden_file_is_host_independent` also
  checks this.
- It has no paths, timestamps, or host metadata.
- The `m2-cross-os` jobs, run in both directions, restore the other OS's snapshot and
  check its result.

## 6. Mutation coverage

No new mutations were made for this audit. This table collects the manual mutation checks
made during M2.2–M2.9. Each mutation was applied, the named tests were run, and the source
was restored. The per-milestone tallies come from the milestone work reports. The mutation
runs themselves are not stored in the repository.

| Milestone | Mutations | Caught | Survivors |
|---|---|---|---|
| M2.2a CSR / MRET | 5 | 5 | 0 |
| M2.2b MEI entry | 5 | 5 | 0 |
| M2.3 IRQ controller | 5 | 5 | 0 |
| M2.4 MultiMasterBus | 5 | 5 | 0 |
| M2.5 SimpleBlockMedia | 5 | 5 | 0 |
| M2.6 DMA controller | 5 | 5 | 0 |
| M2.7a DMA READ | 7 | 7 | 0 |
| M2.7b DMA WRITE engine | 8 | 8 | 0 |
| M2.7c DMA fault semantics | 11 | 11 | 0 |
| M2.7d DMA snapshot state space | 12 | 12 | 0 |
| M2.8 reference platform | 5 | 5 | 0 |
| M2.9 snapshot / observation | 11 | 11 | 0 |
| **Total** | **84** | **84** | **0** |

The scheduled `mutants.yml` workflow runs nightly and is never a gate. The M1 release notes
record 14 missed and 1 timed-out mutants in M0 code. That item is outside M2 and still open.

## 7. CI

The required workflow is `.github/workflows/ci.yml`. It runs on PRs and on pushes to
`main`. In main run 36112983981 at `278b452`, all 10 jobs succeeded:

- `fmt, check, clippy, machete`
- `test (ubuntu-latest)` and `test (windows-latest)`:
  - unit and integration tests, doctests, AT-1 to AT-3;
  - rv32ui, hello, and `block_irq` (M2.8);
  - ACT4 on both CPU profiles;
  - M1-A6 to A8, M1 golden verify;
  - M2.9 snapshot, observation, and golden;
  - M2 golden verify;
  - fixture and golden unchanged checks;
  - result emission.
- `M1 cross-OS` ×2, `M2 cross-OS` ×2
- `rv32ui fixtures rebuild (Linux)`
- `M1-A3 Spike differential (Linux)`, which includes the M2 CSR 20/20
- `ACT4/Sail external validation (Ubuntu)`

`ci.yml` has no `continue-on-error`, no `|| true`, and no `if:` that skips a required step:
- Its three `if:` conditions are a Spike build cache check and two `if: failure()`
  log-upload steps.
- `fail-fast: false` only lets the other matrix legs finish; each failure still fails the
  run.
- The only `continue-on-error` in the repository is the report-only `coverage` job in
  `nightly.yml`, which is not required.

## 8. Repository cleanliness

- The audit branch changes one file: this one. `git diff main --stat` shows only
  `docs/releases/m2-exit-audit.md`.
- Since `v0.2.0-m1`, none of these changed:
  - the M0/M1 design docs and M0/M1 goldens;
  - `tests/rv32/fixtures`, `tests/rv32/hello`, `tests/act4`.
- `runtime/src/trace.rs` changed once, in the M2.1 pin (`fd36b2f`), to flatten irq/block
  dispatch fields, and the M0/M1 goldens stayed unchanged.
- `docs/m2-design.md` has changed twice since its freeze (`71a3553`):
  - roadmap renumbering (`fd36b2f`);
  - the recorded §13.3 mid snapshot size (`bc1acbb`).
- `tests/rv32/block_irq/` is unchanged since M2.8 (`b5139df`).
- The M2 golden files were written once (`bc1acbb`) and never re-blessed.
- No schema constant, compatibility ID, or contracts pin changed.

## 9. Release readiness

- **Architecture:** PASS
  - CPU, IRQ controller, MultiMasterBus, SimpleBlockMedia, and the DMA controller with its
    READ/WRITE engines and faults are implemented as frozen in `docs/m2-design.md`.
  - `m2-reference` is wired as frozen.
- **Compatibility:** PASS
  - Contracts pin `90900a1`, `COMPATIBILITY_ID "0.0.0"`, mem.v0/mem.v1 unchanged.
  - block.v0 and irq.v0 are frozen.
  - M0/M1 goldens, fixtures, and ACT4 are unchanged; ACT4 passes 39/39 on both CPU
    profiles.
- **Reproducibility:** PASS
  - The golden reproduces in-process, across processes, and across Linux and Windows,
    byte for byte.
  - All 22,001 every-event checkpoints resume to the same end.
- **Testing:** PASS
  - 842/842 nextest, doctests, docs, clippy, fmt, machete.
  - Spike: rv32ui, generated, misaligned, M2 CSR.
  - Mutations: 84/84 caught, 0 survivors.
  - All 10 CI jobs succeeded on `main`.

M2 is complete. `278b452` plus this audit is the frozen M2 release candidate.
