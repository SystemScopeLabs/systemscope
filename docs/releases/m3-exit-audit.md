# M3 exit audit (M3.8)

This audit checks M3, `docs/m3-design.md`, against its exit criteria (§18). It covers function,
compatibility, determinism, and validation evidence, and decides whether M3 can close as a
release candidate. It adds nothing to M3:
- no source behavior, schema, contract, fixture, or golden file changed, and nothing was
  re-blessed;
- the only other changes bring current-state wording up to date in `docs/m3-design.md`,
  `README.md`, `CONTRIBUTING.md`, and one line of the `xtask` doc comment (`act4 run`
  already ran the M3 profile).

This audit does not create a tag, a release, a version bump, or a changelog entry
(m3-design §17, M3.8: "tag and release left to the maintainer").

- **Base:** `main` at `146b3d4b877dc8de70e5de896f8d018e1b76d311`
  (`test(m3): verify snapshot and golden reproducibility`, the M3.7 merge).
- **M3 range:** `c100273..146b3d4`, which is PRs #29–#38:

  | Step | PR | Commits |
  |---|---|---|
  | M3.0 design | #29 | `b9a0e8a`, `e1694cc`, `182f523` |
  | M3.1 | #30 | `b8c5730` |
  | M3.2 design | #31 | `daffe7e` |
  | M3.2 | #32 | `ef3b6bf` |
  | M3.3 | #33 | `1abf214`, `b66e1ad` |
  | M3.4a | #34 | `9e90186` |
  | M3.4b | #35 | `b513a1f`, `f4ea2f6` |
  | M3.5 | #36 | `b94d786`, `945e8e6` |
  | M3.6 | #37 | `2d994b2`, `657935a`, `06a1871` |
  | M3.7 | #38 | `002c5c7`, `d83df61`, `146b3d4` |

- **Contracts pin:** `SystemScopeLabs/contracts` at `90900a128d51f6b42e86dc789447882c90c52ef4`,
  with a clean checkout.
- **Toolchain:** `rust-toolchain.toml`.
- **Local runs:** Windows 11 x86-64 with 12 threads, plus WSL Ubuntu 24.04 for the RISC-V
  toolchain and Spike.
- **CI:** main run 36289912251 at `146b3d4`.

## 1. Scope

m3-design §1 defines M3 as the first execution slice:

```text
Executable → Storage → RAM → Process → CPU → Memory → Syscall → Kernel → Output
```

It runs on `m3-reference`: `m2-reference` plus the `ModeledKernel` behind `kgate`, and the
CPU's `M3` profile. The non-goals are in §2.2. §3 lists which clarifications are final.

## 2. Milestone inventory

| Step | Delivered | Implemented | Tested | Documented | Frozen |
|---|---|---|---|---|---|
| M3.1 | `SSX0` executable table (`elf/src/exec_table.rs`), `parse_user_elf32` and the mapping plan (`elf/src/user.rs`) | yes | `elf/tests/{exec_table,user,user_properties,user_fixture}.rs` (55 tests) | §8.1, §8.3 | M1 loader untouched (`load.rs`, `parse.rs`, `error.rs` unchanged since M1) |
| M3.2 | U/S/M, supervisor CSRs, `medeleg` delivery, `MRET`/`SRET`, MEI with modes, schema 3 (`components/rv32i/src/{privilege,cpu}.rs`) | yes | `rv32i/tests/{privilege,cpu_m3}.rs`; Spike `m3-priv` 13/13; rv32ui and ACT4 on `M3` | §5.1, §5.3, §5.5; appendix B | §17.4 schema 3 |
| M3.3 | Sv32, the walk states, `SUM`/`MXR`, Svade, page faults, walk snapshot (`sv32.rs`, `cpu.rs`) | yes | `rv32i/tests/{sv32,cpu_sv32}.rs`; Spike `m3-sv32` 11/11 | §5.2, §5.4; appendix C | §17.4 |
| M3.4a | `kgate`, the held `ENTER`, `ModeledKernel` as bus master (`components/os/src/{core,kernel,config}.rs`) | yes | `os/tests/{core,kernel,config,gate_runtime}.rs` | §6.1–§6.3 | kernel schema 1 |
| M3.4b | PCBs, frame allocator, Sv32 address spaces, FIFO scheduling (`process.rs`, `frames.rs`, `space.rs`, `procop.rs`, `image.rs`) | yes | `os/tests/{process,process_props,process_runtime}.rs` | §6.4, §6.6, §6.7, §17.1 | §17.1 |
| M3.5 | Syscall ABI: `write`, `sched_yield`, `exit`, `exit_group`, `getpid`, errors; UART output (`syscall.rs`, `procop.rs`) | yes | `os/tests/{syscall,syscall_props,syscall_runtime}.rs` | §6.5, §17.2 | §17.2 |
| M3.6 | Boot from `SSX0` through the block controller, firmware and trampoline, five programs, two disks, `m3-reference` (`boot.rs`, `tests/rv32/src/{m3,m3ref}.rs`, `tests/rv32/m3/`) | yes | `os/tests/{disk_boot,disk_boot_props}.rs`, `rv32/tests/m3_reference.rs`, `m3-reference verify` | §7, §8, §11, §12, §17.3 | fixtures in `tests/rv32/m3/manifest.json` |
| M3.7 | Portable snapshot, golden files, every-event and stress resume, cross-OS (`tests/acceptance/src/m3/`) | yes | `acceptance/tests/{m3_snapshot,m3_observation,m3_golden}.rs`, `m3-golden verify`, the every-event shards | §17.4 | `tests/golden/m3-reference.{json,mid.snap}` |

"Frozen" means the section or artifact that pins it. Changing it is a design, schema, or
golden change.

## 3. Non-goals (m3-design §2.2)

Every production crate was searched (`components/{rv32i,os,platform}/src`, `elf/src`,
`runtime/src`):

| Non-goal | Finding |
|---|---|
| Preemption, timer, CLINT, time slices | Not present: no timer, `mtime`, or CLINT. Scheduling is FIFO plus `sched_yield` (§6.6) |
| PLIC | Not present. `irqc.rs` states it has no PLIC features |
| Filesystem, `open`/`read`, fds other than 1 and 2 | Not present. Other fds return `-EBADF` |
| `fork`, `exec`, `wait`, `brk`/`mmap`, threads | Not present. Any unsupported number returns `-ENOSYS` without a switch (`every_other_number_is_enosys_and_the_caller_runs_on`) |
| TLB | Not present. Every translated access walks, and `SFENCE.VMA` retires as a no-op (§5.4) |
| Non-zero ASID | `satp.ASID` always reads 0 (`privilege.rs` `SATP_ASID`), and the kernel writes ASID 0 (`os/src/pte.rs`) |
| PMP, `MPRV` | Not present. `MPRV` is outside the writable `mstatus` set, so it reads 0. Spike's writable `MPRV` is divergence D4 |
| Svadu (hardware A/D) | Not present. The walk is Svade: a clear A or D raises a page fault, and the CPU never writes a PTE |
| Linux or other real kernel | Not present. Only the syscall numbers are Linux's (§19.1 decision 8) |
| Conformance claims | Absent: no document claims Sm, Ss, or Sv32 compliance, and ACT4 `include_priv_tests: False` stays |

Intended unsupported handling is not a feature:
- `-ENOSYS` and `-EBADF` returns;
- the illegal `WFI`;
- `IllegalInstruction` for `misa`, `mhartid`, and the counters;
- the read-only-zero `mideleg`, `sie`, and `sip`.

## 4. Architecture boundaries

| Boundary | Evidence | Result |
|---|---|---|
| The CPU does not know the kernel | `systemscope-rv32i` depends only on `systemscope-contracts`. No `kernel`, `syscall`, `process`, `kgate`, or `systemscope_os` appears in `components/rv32i/src` | holds |
| The kernel does not touch the register file | `systemscope-os` depends only on `systemscope-contracts` and `systemscope-elf`, not on `rv32i`, `platform`, or `runtime`. The context moves only through trap-frame words it reads and writes over the bus (§7.2, §17.2) | holds |
| No RAM internals mutated | The kernel's only memory path is its `mem.v1` master port (`kernel0`), checked against the access whitelist before every send. Page tables, frames, and trap frames are written by bus writes (`creation_accesses_are_whitelisted_ordered_and_zero_every_frame`) | holds |
| Syscalls go through the trap frame | `a7`/`a0`–`a2` are decoded from the frame the kernel read, and exactly `a0` and `sepc + 4` are written back (§17.2) | holds |
| Storage takes no host shortcut | Only `m3-firmware.elf` is placed in RAM by the host. Every executable is DMA'd from `SimpleBlockMedia` by the unchanged M2 controller, polled over MMIO (`every_executable_reaches_ram_through_the_block_controller`, `boot_reads_the_table_and_every_entry_through_the_controller_by_polling`) | holds |
| Output goes to the modeled UART | `write` issues one-byte bus writes to UART TX. No production crate prints to stdout or stderr | holds |
| Snapshot ownership is not duplicated | Each of the eight components stores only its own state (§17.4 inventory). The kernel snapshot holds metadata only (`the_snapshot_holds_kernel_metadata_only`, `a_real_run_leaves_kernel_metadata_only_in_the_kernel_snapshot`). A PCB holds a context only while `Ready` (§17.1), which the validator checks. The M3.7 mutant "kernel stores duplicate RAM bytes" was killed | holds |

## 5. Compatibility

### M1 and M2

- The M0, M1, and M2 golden files are byte-identical to their releases:

  | Golden files | Identical to |
  |---|---|
  | `m0-reference.*` | `v0.1.0-m0` |
  | `m1-reference.*` | `v0.2.0-m1` |
  | `m2-reference.*` | the M2 release candidate `278b452` |

  Each golden file was written in exactly one commit:

  | Golden files | Commit |
  |---|---|
  | `m0-reference.*` | `382bfe6` |
  | `m1-reference.*` | `f3827cf` |
  | `m2-reference.*` | `bc1acbb` |
  | `m3-reference.*` | `d83df61` |

- `m1-golden verify` and `m2-golden verify` pass locally and in both CI OS jobs. These files
  cover the M1/M2 schema bytes (CPU schema 1 and 2, schema 1 of every M2 component), traces,
  and digests.
- `git diff 278b452 146b3d4` is empty for all of these:
  - `runtime/`, `components/platform/`, `components/toy/`, `reference/`;
  - `elf/src/{load,parse,error}.rs`;
  - `tests/rv32/{fixtures,hello,block_irq}/`, `tests/act4/`;
  - `docs/m{0,1,2}-design.md`.

  So the M2 DMA, block, IRQ controller, bus, RAM, and UART semantics are the M2 code, unchanged.
- Changes in the CPU crate are additive and profile-gated:
  - new `TrapCause` variants, `m3_name`, and `code`;
  - `privilege.rs`, `sv32.rs`, and the `M3` branches of `cpu.rs`.

  `csr.rs`, including M2's `take_mei`, is unchanged. Tests check the M1/M2 behavior:
  - `m1_and_m2_keep_sret_sfence_and_supervisor_csrs_illegal`;
  - `each_profile_restores_only_its_own_schema`;
  - the `cpu_m2`/`cpu_mei` suites;
  - rv32ui and ACT4 on the M1 and M2 profiles;
  - Spike M1-A3 and M2 CSR.
- `tests/rv32/src/progen.rs` changed only in visibility (three encoders and two opcodes became
  `pub(crate)`). `FIXED_SEEDS_DIGEST` is unchanged, and its test passes.

### Contracts

- The pin is `90900a1` in every `ci.yml`, `nightly.yml`, and `mutants.yml` checkout. M3 added
  two checkouts, both at the same pin, and changed no other pin.
- `COMPATIBILITY_ID` is `"0.0.0"` (`contracts/crates/systemscope-contracts/src/lib.rs`). The
  golden file records it as `compatibility_id: "0.0.0"`.
- M3 uses `mem.v1` for walks, the gate, and kernel accesses, and `block.v0` and `irq.v0`
  unchanged. There is no local side protocol:
  - the kernel's only ports are `gate` (a `mem.v1` target) and `mem` (a `mem.v1` master);
  - `ENTER` is an ordinary 4-byte `mem.v1` write whose response is held (§6.3);
  - the new trace kinds and fields are additions, not a format change (§14).

## 6. Determinism

- **Enforced:** `clippy.toml` disallows these, and CI runs clippy with `-D warnings` on every
  target:
  - `HashMap` and `HashSet`;
  - `Instant` and `SystemTime` (the types and `now`);
  - `thread::spawn`.
- **Searched in the M3 sources** (`components/os/src`, `components/rv32i/src`, `elf/src`,
  `tests/rv32/src/{m3,m3ref}.rs`, `tests/acceptance/src/m3/`), with none found:
  - wall-clock time, randomness, host PID;
  - `std::fs`, environment reads, temporary or current-directory paths;
  - `to_ne_bytes`/`from_ne_bytes`;
  - stdout/stderr.
- **Encoding:** every length and index in the kernel snapshot is written fixed-width (`w.u32`
  of `usize` values, `w.u16` for segment indices), and every integer is little-endian (§17.4
  portable format).
- **Surfaces:**
  - scheduling is a FIFO queue, PIDs are table order plus 1, and frames are lowest free first
    (§6.4, §6.6);
  - `TxnId`s come from per-issuer counters, and DMA progress, storage order, and syscall output
    order are fixed by one outstanding access per agent (§13);
  - trace, snapshot, and golden bytes are canonical:
    `snapshots_are_canonical_and_deterministic`, `two_runs_are_identical_across_every_component`,
    `fresh_platforms_reproduce_every_field`, `the_run_reproduces_in_separate_processes`;
  - fixture builds compare two builds from different paths (`build-m3.sh`, `build-user.sh`);
  - cross-OS equality is covered in §9.
- **Host independence:** `the_golden_files_are_host_independent` checks that the golden file has
  no CR, path, or timestamp. The M3.7 regeneration from a fresh clone at another path with
  `core.autocrlf=true` and `LC_ALL=C` emitted byte-identical files (PR #38).
- **Harness only:** the event watchdog and the parallel `every-event` sweep (`thread::scope`)
  are acceptance tooling and are not recorded in any golden file. The simulator has no watchdog.

## 7. External validation

### Spike (pinned `19609434bb3d83448eec8796e8f0367c868efbda`)

Main run 36289912251, job `M1-A3 Spike differential (Linux)`, after `spike verify` of the pin:

| Suite | Programs matched | Retirements compared | Covers |
|---|---|---|---|
| rv32ui | 40/40 | 13,268 | M1 |
| generated (fixed seeds) | 64/64 | 22,344 | M1 |
| misaligned | 9/9 | 45 | M1 |
| M2 CSR | 20/20 | 372 | M2 Zicsr, CSRs, `MRET` |
| M3 privilege (`privgen.rs`) | 13/13 | 557 | reset, `mstatus`/`sstatus`, supervisor CSRs, delegation, `MRET`/`SRET` chains, traps from U and S, `ecall` from U, illegal in M |
| M3 Sv32 (`vmgen.rs`) | 11/11 | 2,618 | 4 KiB pages, megapages, invalid PTEs, S permissions with `SUM`/`MXR`, Svade, page and access faults, U mappings, `SFENCE.VMA`, undelegated fetch, load, and store faults |

A local WSL run of the same suites with the pinned Spike, on this audit's tree, matched the same counts.

- **Appendices:** `m3-2-spike-appendix.md` B.0–B.7 and `m3-3-spike-appendix.md` C.0–C.6 record
  the Spike setup and measurements. C.6 names the eleven Sv32 programs that CI runs.
- **Divergences:** every divergence from the design is listed in appendix B.6 (D1–D11, with C.5
  retiring D8 at M3.3). Each keeps the design's rule. Each is outside the differential (the
  programs never reach it, or compare only up to it) and is covered by SystemScope-only tests,
  for example `wfi_is_illegal_in_every_mode`, `unsupported_and_read_only_csrs_are_illegal_in_every_mode`,
  and `a_synchronous_trap_writes_no_csr`.
- **Priority:** the exception order (alignment, then translation, then access) was measured in
  B.5 and C.2–C.4.
- **What this proves:** the directed and generated programs retire identically on both. It does
  not prove full Spike compatibility or conformance.

### rv32ui and ACT4

| Suite | Profiles | Result |
|---|---|---|
| rv32ui (40 selected, `riscv-tests` `793a5ff`) | M1, M2, M3 (M-mode, Bare) | pass (`the_selected_rv32ui_suite_passes*`) |
| ACT4 RV32I (39 ELFs, ACT4 `54cfe21`, Sail 0.14.1) | M1 39/39, M2 39/39, M3 39/39 | locally, in both OS test jobs, and in the ACT4 job |

- ACT4 keeps `include_priv_tests: False` (`tests/act4/config/systemscope-rv32i/`).
- What these suites prove: RV32I user-level behavior is unchanged in every profile. They do not
  exercise S/U mode, Sv32, or any privileged conformance.

### Kernel oracle

- The kernel is checked against pure oracles that never call its state machine:
  - `os/tests/common/procs.rs` and `common/disk.rs`, used by `process_props`, `syscall_props`,
    and `disk_boot_props`;
  - the core oracle (`the_core_matches_the_oracle`, `the_component_matches_the_oracle`).
- The user-copy walk is checked against the CPU's `sv32_translate` (M3.5, §17.2).

### M6 feasibility

The optional native-kernel check (§15.1) was not run (§17.3). It is not an exit criterion
(§18), so this does not affect the decision.

## 8. Snapshot and golden audit

- **Schemas:** the CPU has schemas 1 (M1) and 2 (M2), both unchanged, as the M1/M2 goldens show,
  and schema 3 (M3), final in §17.4. The other seven `m3-reference` components have schema 1.
  The portable snapshot's schemas are `[3,1,1,1,1,1,1,1]`.
- **Format:** the M0 container, format 1, fixed-width little-endian. It is read by an
  independent decoder, then the platform validator, then restore. A failed restore leaves the
  runtime `Faulted` (`doctored_snapshots_are_caught`, `malformed_snapshots_never_panic`; the kernel's own `a_failed_restore_is_atomic`).
- **No reissue:** checked for every issuer (CPU, DMA, kernel, bus downstream, media), with
  `TxnId`s continuing from the snapshot.
- **Every event:** a checkpoint `k` is the state after `k` dispatched events, for `k` in
  `0..=events`:
  - checkpoint 0 is the state after `init`, before the first event;
  - checkpoint `events` is the final state, and resuming it runs no event;
  - the reference run has 166,897 events and therefore 166,898 checkpoints (`xtask`:
    `0..reference.events() + 1`).

  Evidence:

  | Run | Checkpoints | Failed |
  |---|---|---|
  | local exhaustive run at `146b3d4`, repeated for this audit | 166,898 | 0 |
  | main run 36289912251, 8 shards | 10,780 + 11,581 + 12,593 + 13,930 + 15,811 + 18,755 + 24,441 + 59,007 = 166,898 | 0 |

  The chained test (`every_boundary_is_portable_and_resumes_in_place`) restores all 166,898
  checkpoints in place. `every_nofault_boundary_is_portable` covers the 141,129 checkpoints of
  the no-fault disk.
- **Stress points (§9.2):** all fourteen sub-points occur. The first, middle, and last boundary
  of each resume exactly (`stress_points_resume_exactly`).
- **Golden files:**

  | File | Size | sha256 |
  |---|---|---|
  | `m3-reference.json` | 2,826 bytes | `9ada09c382a0ad2f887916478e5c26434c6f2d84e2a5a1c5393b1e99949500d2` |
  | `m3-reference.mid.snap` | 149,887 bytes | `d3fc30ac3afdf9ff2ee14fe333cec07f41509f13d8c6d94e8c0b7c23ffda51cc` |

  The mid snapshot's BLAKE3 is `3f6c5b24…912c`, after 121,286 events. `m3-golden verify` and
  `m3-reference run`/`verify` reproduce the files at `146b3d4`.

  | Field | reference | nofault |
  |---|---|---|
  | halt | `EnvironmentCallFromS` at `0x800001c0` | same |
  | shutdown reason (`a1`) | 1 | 0 |
  | UART | `hello from pid 1\nping\npong\nfault\nping\npong\nping\npong\n` (53 bytes) | the same without `fault\n` (47 bytes) |
  | events / instret | 166,897 / 2,176 | 141,128 / 1,999 |
  | `os.*` records / syscalls / switches | 75 / 22 / 11 | 68 / 21 / 10 |
  | exceptions | 23 (22 `ecall` from U + 1 `StorePageFault`) | 21 |
  | DMA commands | 6 | 5 |
  | StateDigest | `0f200e0a…ba18` | `ea7f23b2…dd37` |
  | ExecutionDigest | `a419f1f0…103a` | `1066404e…9b9e` |
  | TraceDigest | `b9739324…c148` | `ff07c683…abe8` |
  | final snapshot | 148,925 bytes, BLAKE3 = StateDigest | 126,273 bytes |

- **Bless:** only `cargo xtask m3-golden bless` writes the files. No workflow calls it.
- **Drift:** `golden_verification_catches_drift` keeps per-field drift detection, and the M3.7
  mutants "bless inside verify" and "fixture verify skipped" were killed.

### Fixtures

- **Two sets:**
  - `elf/tests/fixtures/user/user.elf` is the M3.1 parser fixture. It was linked with relaxation,
    so it is `gp`-dependent by design (§17.1), and it is used for the M3.1 layout tests and the
    M3.4b fault-kill test.
  - `tests/rv32/m3/` holds the M3.6 scenario fixtures: `m3-firmware.elf`, `hello`, `ping`,
    `pong`, `fault`, `badptr`, both disks, and both expected `os.*` traces. They are built with
    `.option norelax` and `--no-relax`. `build-m3.sh` rejects relaxation relocations,
    `gp`-relative relocations, `__global_pointer$`, and `x3` in user code, and
    `the_user_programs_are_gp_independent_rv32i_executables` checks again.
- **Pins:** both manifests pin the Ubuntu 24.04 packages `gcc-riscv64-unknown-elf 13.2.0-11ubuntu1+12`
  and `binutils-riscv64-unknown-elf 2.42-1ubuntu1+6` by version and `.deb` sha256, and every
  input and output by BLAKE3.
- **Reproducibility:**
  - CI's `rv32ui fixtures rebuild (Linux)` job rebuilds `tests/rv32/`, including M3, and
    requires no diff.
  - For this audit, both sets were rebuilt in WSL with the pinned toolchain. All six M3 ELFs
    and `user.elf` are byte-identical to the committed files.
  - `user.elf` is not in the xtask/CI rebuild: `build-user.sh` is run by hand, and
    `user_fixture.rs` checks its manifest hashes on every test run.
- **Windows:** both OS jobs run `rv32-fixtures verify` and the scenario from the committed bytes.

## 9. Mutation inventory

No new mutations were made for this audit. The tallies come from the milestone PR bodies. The
runs themselves are not stored in the repository.

| Step | PR | Mutants | Killed | Compile-only | Equivalent replaced | Final survivors | Notes |
|---|---|---|---|---|---|---|---|
| M3.1 | #30 | none recorded | — | — | — | — | the PR records 55 tests, no mutation run |
| M3.2 | #32 | 15 | 15 | 0 | not stated | 0 | |
| M3.3 | #33 | 18 | 18 | 0 (all killed at runtime) | not stated | 0 | |
| M3.4a | #34 | 27 | 27 | 0 | not stated | 0 | |
| M3.4b | #35 | 38 | 38 | 0 | not stated | 0 | |
| M3.5 | #36 | 50 | 50 | 0 | not stated | 0 | 4 survived the first pass; each got a new test |
| M3.6 | #37 | 53 | 53 | not stated | 3 | 0 | several survived the first pass; new restore tests kill them |
| M3.7 | #38 | 35 (non-equivalent) | 35 | 0 | not stated | 0 | the harness checks of #30–#32 were corrected and rerun |
| **Total recorded** | | **236** | **236** | | | **0** | |

The scheduled `mutants.yml` workflow is exploratory and never a gate (§10).

## 10. Known issues

### Periodic Mutants: `elf/src/error.rs` `Display` MISSED

- **Observed:**
  - nightly `mutants.yml` runs 36269890646 (at `945e8e6`) and 36189007981 (at `daffe7e`) both
    report `MISSED elf/src/error.rs:69:9: replace <impl fmt::Display for SegmentError>::fmt ->
    fmt::Result with Ok(Default::default())`;
  - `daffe7e` is M3.2's design commit, before M3.6 and M3.7, so the finding predates them.
- **Why the runs fail:**
  - both runs ended "The runner has received a shutdown signal" and `Error: interrupted`, about
    20 minutes in, out of 4,662 and 2,849 mutants;
  - in run 36269890646, the MISSED was at 20:40:54, and the run went on until the shutdown at
    20:52:32;
  - cargo-mutants did not stop at the MISSED, and the runs are incomplete, so `mutants.out` was
    never uploaded.
- **What it is:**
  - `SegmentError` and its `Display` come from the M1 loader (`c45d0d0`), and
    `elf/src/error.rs` is unchanged in the M3 range;
  - no test asserts the text of a `SegmentError`;
  - it is a real but display-only test gap: the message text of a host-side loader error;
  - validation results, simulated state, traces, and digests do not depend on it.
- **Release criteria:** §18 does not require the mutation workflow to pass. `mutants.yml` says
  "never a PR gate", and `ci.yml` has no mutation job.
- **Classification: NON-BLOCKING KNOWN ISSUE.** It is tracked separately and not fixed here.

### Other observations (non-blocking)

- The M3.1 PR said `user.elf` would join the xtask/CI rebuild pipeline in M3.6. It did not, and
  its build script still says CI does not run it. The fixture is pinned by its manifest and was
  rebuilt byte-identically for this audit (§8).
- The nightly random-seed tests are `#[ignore]`d and skipped in the regular suite (4 skipped).

## 11. CI and cross-OS

### Required workflow

`.github/workflows/ci.yml` runs on PRs and pushes to `main`. All 20 jobs succeeded in PR run
36286000569 (PR #38 head `146b3d4`) and in main run 36289912251.

| Job | What it proves |
|---|---|
| `fmt, check, clippy, machete` | formatting, a build of every target, lints including the determinism rules, no unused dependencies |
| `test (ubuntu-latest)`, `test (windows-latest)` | the full test suite on each OS (see the list after this table) |
| `M1 cross-OS` ×2, `M2 cross-OS` ×2, `M3 cross-OS` ×2 | each OS checks the other's emitted golden files against the committed ones and restores its snapshot |
| `M3 every-event resume` ×8 | `m3-reference verify`, then one cost-balanced shard of the exhaustive resume, in release mode |
| `rv32ui fixtures rebuild (Linux)` | rebuilds rv32ui, `hello`, `block_irq`, and the M3 fixtures with the pinned toolchain; no diff |
| `M1-A3 Spike differential (Linux)` | builds and verifies the pinned Spike; the six suites of §7 |
| `ACT4/Sail external validation (Ubuntu)` | generates the ACT4 corpus twice from scratch, byte-identical to each other and to the committed files; 39/39 on every profile |

The `test` jobs run, on each OS:
- unit, integration, and doc tests;
- AT-1 to AT-3;
- rv32ui, `hello`, `block_irq`, `m3_reference`, and `m3-reference verify`;
- ACT4 on the M1, M2, and M3 profiles;
- the M1-A6 to A8, M2.9, and M3.7 acceptance suites;
- `m1-`, `m2-`, and `m3-golden verify`;
- the fixture and golden unchanged checks;
- M1/M2/M3 result emission, with the M3 sha256s printed.

Checks on the workflow and the logs:
- Every integration-test file of `systemscope-acceptance` and `systemscope-rv32` is named by a
  `ci.yml` step, and the other crates run in the workspace step, so none is dead.
- `ci.yml` has no `continue-on-error` and no `|| true`. Its only `if:` conditions are the Spike
  cache check and two `if: failure()` log uploads.
- The main run's logs show the M3 commands ran:
  - "both M3 disks match tests/golden/m3-reference.json";
  - the eight "checkpoints resumed, 0 failed" lines;
  - "the foreign M3 result equals the committed golden files" in both directions;
  - the M3 privilege and Sv32 Spike summaries.

### Cross-OS values

The `m{1,2,3}-result-{ubuntu,windows}-latest` artifacts of main run 36289912251 were downloaded
and compared with `cmp`:

| File | Linux == Windows | == committed |
|---|---|---|
| `m1-reference.json` (46,413 bytes) | identical | identical |
| `m1-reference.mid.snap` (8,942 bytes) | identical | identical |
| `m2-reference.json` (2,514 bytes) | identical | identical |
| `m2-reference.mid.snap` (14,356 bytes) | identical | identical |
| `m3-reference.json` (2,826 bytes) | identical | identical |
| `m3-reference.mid.snap` (149,887 bytes) | identical | identical |

- `m3-reference.json` holds each disk's UART bytes, shutdown reason, metrics, and the state,
  execution, and trace digests. All of them are therefore equal on Linux and Windows.
- Both OS logs print the same sha256 lines, `9ada09c3…00d2` and `d3fc30ac…51cc`, for the
  emitted and the committed files, in the PR run and the main run.

## 12. Final gate

On this audit's tree (`146b3d4` plus the documentation changes of M3.8), locally, in the M3.8 worktree:

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo check --workspace --all-targets --locked` | ok |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | ok |
| `cargo machete` | no unused dependencies |
| `cargo nextest run --workspace --locked --no-fail-fast` | 1,195 passed, 0 failed, 4 skipped (the `#[ignore]` nightly tests) |
| `cargo test --workspace --doc --locked` | ok |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked` | ok |
| `cargo xtask rv32-fixtures verify` | 40 rv32ui, `hello.elf`, `block_irq.elf` + disk, and the M3 fixtures (firmware, 5 programs, 2 disks, 2 traces) match |
| `cargo xtask act4 verify` / `act4 run` | 39 ELFs match; M1, M2, and M3 profiles 39/39 |
| `cargo xtask m1-golden verify` / `m2-golden verify` / `m3-golden verify` | reproduce byte for byte |
| `cargo xtask m3-reference run` / `verify` | both disks PASS by §12.3; two runs identical |
| `cargo xtask m3-golden every-event` (release) | 166,898 checkpoints, 0 failed |
| Spike differential (WSL, pinned) | rv32ui 40/40, generated 64/64, misaligned 9/9, M2 CSR 20/20, M3 privilege 13/13, M3 Sv32 11/11 |
| M3.1 and M3.6 fixture rebuild (WSL, pinned) | byte-identical |
| `git diff --check` | clean |

The snapshot and stress checks run inside nextest: `m3_snapshot`, `m3_observation`, and
`m3_golden`.

## 13. Exit criteria (m3-design §18)

| # | Criterion | Evidence | Result |
|---|---|---|---|
| 1 | The `M3` profile implements §5, checked by the Spike-directed tests, the `sv32_translate` and `take_mei` oracles, and every-event resume; `M1` and `M2` are unchanged | Spike: M3 privilege 13/13 and Sv32 11/11 (main run 36289912251). Oracles: `sv32_translate_matches_the_reference`, `the_cpu_matches_the_reference_translator`, `the_permission_matrix_matches_the_reference`, `m3_mei_matches_the_oracle`, `mei_with_modes`. Resume: `restored_m3_cpus_continue_identically_from_every_event`, `restored_cpus_continue_identically_from_every_event_of_a_translated_run`. Unchanged: M1/M2 goldens byte-identical, rv32ui and ACT4 on M1/M2, Spike M1-A3 and M2 CSR, `csr.rs` unchanged | PASS |
| 2 | `ModeledKernel` implements §6 and §8, checked by the pure kernel oracle and in the runtime | Oracle: `process_props`, `syscall_props`, `disk_boot_props` (e.g. `a_packed_disk_boots_as_the_oracle_says`), `the_component_matches_the_oracle`, `the_core_matches_the_oracle`. Runtime: `gate_runtime`, `process_runtime`, `syscall_runtime`, `m3_reference` | PASS |
| 3 | The M3 scenario passes on `m3-reference` (§12.3) on Linux and Windows, through models, with page tables in simulated RAM | `the_m3_scenario_passes_on_m3_reference`, `the_os_records_are_the_scenario_semantics`, `every_executable_reaches_ram_through_the_block_controller`, `user_code_runs_in_u_and_enters_the_kernel_through_the_trampoline`; `m3-reference verify` in both OS jobs; §4 boundaries | PASS |
| 4 | Every-event resume, the §9.2 stress points, and the portable snapshot pass; observation invariance holds | 166,898/166,898 locally and across 8 CI shards; `every_boundary_is_portable_and_resumes_in_place`, `stress_points_resume_exactly`, `the_portable_snapshot_resumes_exactly`, `the_reference_scenario_is_the_same_under_every_observation` | PASS |
| 5 | `m3-reference.json` is blessed once; every M0, M1, and M2 golden file is byte-identical to its release | M3 files: one commit (`d83df61`). `git diff`: M0 vs `v0.1.0-m0`, M1 vs `v0.2.0-m1`, M2 vs `278b452`, all empty | PASS |
| 6 | No `contracts` change, or one recorded through contracts first and a pin bump | Pin `90900a1` throughout the M3 range; `COMPATIBILITY_ID "0.0.0"`; no new protocol (§5) | PASS |
| 7 | Every fixture (firmware, user programs, disk image) is pinned by a manifest | `tests/rv32/m3/manifest.json` (firmware, 5 programs, 2 disks, 2 traces), checked by `rv32-fixtures verify` and `the_committed_m3_fixtures_match_their_manifest`; `elf/tests/fixtures/user/manifest.json`, checked by `the_fixture_matches_its_manifest`; both rebuilt byte-identically | PASS |
| 8 | Decisions and contract changes discovered during M3 are reflected back into the design | Design clarifications before each implementation step: `daffe7e` (M3.2), `1abf214` (M3.3 appendix), `b513a1f` (§17.1), `b94d786` (§17.2), `2d994b2` (§17.3), `002c5c7` (§17.4). Spike divergences D1–D11 and resolutions in appendix B.6/B.7 and C.5. No contract change occurred | PASS |

**8 of 8 PASS, 0 FAIL.**

## 14. Release-candidate decision

**M3 release candidate: ACCEPT.**

- Every §18 criterion has concrete evidence and passes.
- No blocker was found:
  - no exit-criterion defect;
  - no mismatch between frozen semantics and code;
  - no M1 or M2 regression;
  - no contracts change.
- The one known issue, the periodic Mutants `Display` MISSED, is classified as non-blocking
  (§10).

`146b3d4` plus this audit is the frozen M3 release candidate. A tag, release notes, and a
version are left to a separate, explicit step.
