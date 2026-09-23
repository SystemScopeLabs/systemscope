# SystemScope — Systems Simulation & Observability Platform

SystemScope is a deterministic discrete-event simulation runtime for modeling computer
systems and observing what they do. The same seed and topology always produce the same
events, the same state, and the same trace, on every supported platform.

## Status

**M0: deterministic simulation kernel.** This is an early milestone, not a usable
simulator of real hardware. M0 provides:

- a discrete-event scheduler ordered by `(tick, phase, sequence)`
- a component and port contract, with the `mem.v0` message protocol
- snapshots that restore a run exactly, from bytes that are identical across platforms
- digests for execution, state, and trace, checked against committed golden values
- read-only observation (breakpoints, probes, `step`) that does not change results
- a Chrome JSON trace exporter that Perfetto can open
- a toy reference system (`m0-reference`: CPU, DMA, bus, memory) used by the acceptance tests

The design and exit criteria are in [docs/m0-design.md](docs/m0-design.md); the overall
plan is in [plan.md](plan.md).

## Architecture

```text
contracts   (SystemScopeLabs/contracts)   time, events, components, protocols, snapshots, trace, observation
systemscope (this repository)
├─ runtime/         scheduler, topology elaboration, lifecycle, snapshots, trace sinks
├─ components/toy/  ToyCpu, ToyDma, ToyBus, ToyMemory
├─ reference/       builds m0-reference
├─ tests/           acceptance tests (AT-1 reproducibility, AT-2 snapshot/restore, AT-3 observation)
└─ xtask/           cargo xtask bless: regenerates golden files
```

`systemscope` uses `contracts` from a sibling directory, and CI checks out a pinned commit
of it.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** (current) | Time, event queue, component contract, deterministic DES, Perfetto trace |
| **M1** | RV32I CPU and RAM, bare-metal ELF execution, UART output |

Later milestones (interrupts and storage, a modeled OS, the visualizer, HDL and native OS
backends) are described in [plan.md](plan.md) §11.

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for the acceptance tests and the golden-file policy.
