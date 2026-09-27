# SystemScope — Systems Simulation & Observability Platform

SystemScope is a deterministic discrete-event simulation runtime for modeling computer
systems and observing what they do. The same seed and topology always produce the same
events, the same state, and the same trace, on every supported platform.

## Status

**M3: modeled OS backend** is complete and tagged `v0.4.0-m3`; see
[docs/releases/m3-exit-audit.md](docs/releases/m3-exit-audit.md). The next milestone,
**M4: Verification & Debugging Engine**, has a frozen design (M4.0), and its
implementation has not started; M4.1 is next. SystemScope is still an early system, not a usable simulator of real
hardware.

| Milestone | Status | Tag |
|---|---|---|
| M0 | complete | `v0.1.0-m0` |
| M1 | complete | `v0.2.0-m1` |
| M2 | complete | `v0.3.0-m2` |
| M3 | complete | `v0.4.0-m3` |
| M4 | M4.0 design frozen; implementation not started ([docs/m4-design.md](docs/m4-design.md)) | none |

On `m3-reference`, a bare-metal firmware boots a small modeled kernel. The kernel loads
five user programs from a disk image through the DMA block controller. It runs them as
user processes in their own Sv32 address spaces and serves their system calls. Their
output reaches the modeled UART:

```text
Executable → Storage → RAM → Process → CPU → Memory → Syscall → Kernel → Output
```

The whole path runs through models, with page tables in simulated RAM.

What each milestone provides:

- **M1** (`v0.2.0-m1`):
  - `Rv32iCpu`: all 40 RV32I instructions at architectural fidelity (no pipeline), with
    every fetch and data access through memory;
  - `AddressBus`, sparse `Ram`, and a TX-only `SimpleUart`, over the fault-capable
    `mem.v1` protocol;
  - a host-side ELF32 loader for a checked subset of RISC-V executables;
  - the `m1-reference` platform.
- **M2** (`v0.3.0-m2`, [docs/releases/m2-exit-audit.md](docs/releases/m2-exit-audit.md)):
  - Zicsr, the machine CSRs, `MRET`, and machine external interrupts;
  - a level-sensitive `SimpleIrqController`;
  - `DmaBlockController` with DMA between RAM and `SimpleBlockMedia`, an abstract block
    store (`block.v0`, `irq.v0`);
  - the `m2-reference` platform.
- **M3** (`v0.4.0-m3`, [docs/releases/m3-exit-audit.md](docs/releases/m3-exit-audit.md)):
  - a documented U/S/M privilege subset with `medeleg` delegation, `SRET`, and Sv32
    translation (Svade: no hardware A/D updates, no TLB);
  - `ModeledKernel`, a Rust kernel model behind a memory-mapped gate: executable-table
    boot from storage, processes, frame allocation, FIFO scheduling with `sched_yield`,
    and a small Linux-numbered syscall subset (`write`, `exit`, `exit_group`,
    `sched_yield`, `getpid`);
  - the `m3-reference` platform.

  The CPU does not know the kernel: the kernel works only through memory accesses and
  the saved trap frame.

Every milestone keeps the M0 guarantees, checked against committed golden files: the
same digests on Linux and Windows, snapshots that restore exactly and portably at every
event, and observation that does not change results.

The CPU's behavior is checked against independent sources:
- its own unit and property tests;
- 40 `riscv-tests` `rv32ui` tests;
- Spike, retirement by retirement: rv32ui, generated and misaligned-access programs, and
  the directed M2 CSR, M3 privilege, and M3 Sv32 programs;
- 39 ACT4 RV32I tests with expected values from the Sail reference model, on the M1, M2,
  and M3 CPU profiles.

The modeled kernel is checked against pure oracles. All of this is external validation,
not a certification.

What SystemScope does not implement:
- the M, A, F, D, and C extensions, and misaligned loads and stores (they trap);
- timers, CLINT, PLIC, and preemption;
- PMP, `MPRV`, ASIDs, a TLB, and hardware A/D updates;
- a filesystem, `fork`/`exec`, and memory-management syscalls.

The privileged support is the subset documented in [docs/m3-design.md](docs/m3-design.md)
§5. SystemScope does NOT claim conformance to Sm, Ss, or Sv32. Sm appears only in the
pinned ACT4/UDB adapter configuration, because that schema requires Sm-owned MXLEN.
Privileged ACT tests are disabled.

M0, the deterministic simulation kernel, is tagged `v0.1.0-m0`. More documents:
- designs and exit criteria: [docs/m0-design.md](docs/m0-design.md),
  [docs/m1-design.md](docs/m1-design.md), [docs/m2-design.md](docs/m2-design.md), and
  [docs/m3-design.md](docs/m3-design.md);
- the M4 design, frozen at M4.0: [docs/m4-design.md](docs/m4-design.md);
- M1 release notes: [docs/releases/v0.2.0-m1.md](docs/releases/v0.2.0-m1.md);
- the overall plan: [plan.md](plan.md).

## Architecture

```text
contracts   (SystemScopeLabs/contracts)   time, events, components, protocols (mem.v0, mem.v1, irq.v0, block.v0), snapshots, trace, observation
systemscope (this repository)
├─ runtime/              scheduler, topology elaboration, lifecycle, snapshots, trace sinks
├─ components/toy/       M0: ToyCpu, ToyDma, ToyBus, ToyMemory
├─ components/rv32i/     M1–M3: decode, execution, CSRs, privilege, Sv32, Rv32iCpu
├─ components/platform/  M1–M2: AddressBus, Ram, SimpleUart, SimpleIrqController, DmaBlockController, SimpleBlockMedia
├─ components/os/        M3: ModeledKernel (gate, boot, processes, syscalls)
├─ elf/                  M1: host-side ELF32 loader; M3: user-executable parser and executable table
├─ reference/            builds m0-reference
├─ tests/acceptance/     M0 AT-1..AT-3, M1-A6..M1-A8, M2 and M3 snapshot/golden suites, golden files in tests/golden
├─ tests/rv32/           m1/m2/m3-reference builders, rv32ui fixtures, hello.elf, block_irq.elf, M3 firmware and user programs, Spike differential
├─ tests/act4/           the committed ACT4 RV32I corpus and its manifest
├─ xtask/                bless, m1/m2/m3-golden, m3-reference, rv32-fixtures, spike, act4
├─ verification/         (planned, M4) systemscope-verify, the verification & debugging engine
└─ visualizer/           (planned, M7+) the Visualizer / Interactive Debugger
```

Directories marked *planned* do not exist yet.

`systemscope` uses `contracts` from a sibling directory, and CI checks out a pinned commit
of it.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** (`v0.1.0-m0`) | Time, event queue, component contract, deterministic DES, Perfetto trace |
| **M1** (`v0.2.0-m1`) | RV32I CPU and RAM, bare-metal ELF execution, UART output |
| **M2** (`v0.3.0-m2`) | Interrupts, DMA, block storage (abstract SSD model) |
| **M3** (`v0.4.0-m3`) | Modeled OS backend: processes, syscalls, Sv32 virtual memory |
| M4 (M4.0 design frozen) | Verification & Debugging Engine: compare executions at stable architectural boundaries, locate the first divergence, produce structured diagnostic evidence, and build portable reproducers |
| M5 (planned) | SystemVerilog CPU backend via Verilator |
| M6 (planned) | Native guest OS backend: a C/assembly tiny kernel |
| M7 (planned) | SystemScope Visualizer / Interactive Debugger: topology, timeline, state, step, step back, seek, breakpoint, filter, plus views of M4 divergences and reproducers |

M0–M3 are complete. M4–M7 are not implemented. They are described in [plan.md](plan.md)
§11, and M4 in the frozen design [docs/m4-design.md](docs/m4-design.md). The Visualizer was first
planned as M4 and moved to M7, with its scope unchanged.

## Build and test

Clone both repositories side by side:

```sh
git clone https://github.com/SystemScopeLabs/contracts.git
git clone https://github.com/SystemScopeLabs/systemscope.git
cd systemscope
```

The toolchain is pinned in `rust-toolchain.toml`. Then run:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --no-fail-fast
cargo test --workspace --doc --locked
```

The checks that need no external tool run on the committed files:

```sh
cargo xtask rv32-fixtures verify
cargo xtask m1-golden verify
cargo xtask m2-golden verify
cargo xtask m3-golden verify
cargo xtask m3-reference verify
cargo xtask act4 verify
cargo xtask act4 run
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the acceptance tests, the golden-file policy,
the fixtures, the Spike differential, and the ACT4 corpus.
