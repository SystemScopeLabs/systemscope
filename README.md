# SystemScope — Systems Simulation & Observability Platform

SystemScope is a deterministic discrete-event simulation runtime for modeling computer
systems and observing what they do. The same seed and topology always produce the same
events, the same state, and the same trace, on every supported platform.

## Status

**M1: RV32I architectural CPU** (`v0.2.0-m1`). SystemScope runs bare-metal RV32I ELF
programs on the M0 kernel, and prints through a UART. It is still an early milestone,
not a usable simulator of real hardware. M1 provides:

- `Rv32iCpu`: all 40 RV32I instructions at architectural fidelity (no pipeline), every
  fetch and data access through memory, and the execution-environment trap boundary
- `AddressBus`, sparse `Ram`, and a minimal `SimpleUart` (TX only), over the
  fault-capable `mem.v1` protocol
- a host-side ELF32 loader for a checked subset of `ET_EXEC` RISC-V images
- the `m1-reference` platform: RAM at `0x8000_0000` (16 MiB), UART at `0x1000_0000`
- every M0 guarantee on it: deterministic digests, snapshots that restore exactly and
  portably, and observation that does not change results, checked against committed
  golden files

The CPU's behavior is checked against four independent sources: its own unit and
property tests, 40 `riscv-tests` `rv32ui` tests, Spike retirement by retirement (those
40 tests, 64 generated programs, and 9 misaligned-access programs), and 39 ACT4 RV32I
tests with expected values from the Sail reference model. These are external validation,
not a certification.

SystemScope's capability is RV32I only. It does not implement Zicsr, privileged
architecture, interrupts, or the M, A, F, D, and C extensions, and misaligned loads and
stores trap. SystemScope does NOT implement Sm. Sm appears only in the pinned ACT4/UDB
adapter configuration because that schema requires Sm-owned MXLEN. Privileged ACT tests
are disabled.

M0, the deterministic simulation kernel, is tagged `v0.1.0-m0`. The designs and exit
criteria are in [docs/m0-design.md](docs/m0-design.md) and
[docs/m1-design.md](docs/m1-design.md), the M1 release notes in
[docs/releases/v0.2.0-m1.md](docs/releases/v0.2.0-m1.md), and the overall plan in
[plan.md](plan.md).

## Architecture

```text
contracts   (SystemScopeLabs/contracts)   time, events, components, protocols (mem.v0, mem.v1), snapshots, trace, observation
systemscope (this repository)
├─ runtime/              scheduler, topology elaboration, lifecycle, snapshots, trace sinks
├─ components/toy/       M0: ToyCpu, ToyDma, ToyBus, ToyMemory
├─ components/rv32i/     M1: decode, execution, Rv32iCpu
├─ components/platform/  M1: AddressBus, Ram, SimpleUart
├─ elf/                  M1: host-side ELF32 loader
├─ reference/            builds m0-reference
├─ tests/acceptance/     M0 AT-1..AT-3, M1-A6..M1-A8, golden files in tests/golden
├─ tests/rv32/           m1-reference builder, rv32ui fixtures, hello.elf, Spike differential, program generator
├─ tests/act4/           the committed ACT4 RV32I corpus and its manifest
└─ xtask/                bless, m1-golden, rv32-fixtures, spike, act4
```

`systemscope` uses `contracts` from a sibling directory, and CI checks out a pinned commit
of it.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** (`v0.1.0-m0`) | Time, event queue, component contract, deterministic DES, Perfetto trace |
| **M1** (`v0.2.0-m1`) | RV32I CPU and RAM, bare-metal ELF execution, UART output |
| **M2** (next) | Interrupts, DMA, block storage (abstract SSD model) |

Later milestones (a modeled OS, the visualizer, HDL and native OS backends) are
described in [plan.md](plan.md) §11.

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

The M1 checks that need no external tool run on the committed files:

```sh
cargo xtask rv32-fixtures verify
cargo xtask m1-golden verify
cargo xtask act4 verify
cargo xtask act4 run
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the acceptance tests, the golden-file policy,
the fixtures, the Spike differential, and the ACT4 corpus.
