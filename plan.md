# SystemScope

> Interactive Computer Systems Simulation & Observability Platform

SystemScope is not a program that *explains* computers. It builds a computer system itself as a **modular execution model** and makes that model **observable**.

It connects the whole system, from hardware through OS, runtime, network, and GPU, as components. It then records, replays, and visualizes their state and events deterministically. The focus is on five things:

**execution · state · events · tracing · replaceable implementations**

Because every run is deterministic and every backend sits behind the same contracts, SystemScope can also compare two executions, or two implementations of the same contract, and show where and how they first differ. Verification and debugging are capabilities of the same execution model, not a separate product.

---

## 1. Design Principles

Every design decision is judged against these principles. An implementation that violates one is not accepted.

1. **Contract-first, but validated by execution.**
   Contracts are defined first, then refined against the executable M0 runtime. A contract is not frozen until running code has exercised it.
2. **Time is integer and deterministic; observation never perturbs execution.**
   Global time is an integer tick. The same inputs and seed always produce bit-identical results. Tracing, breakpoints, and stepping never change the outcome.
3. **Same-tick event ordering is explicitly defined.**
   The order of events at the same tick is decided by runtime-defined phases and runtime-assigned sequence numbers. Components cannot choose arbitrary priorities.
4. **Fidelity may differ per component, but components connect only through explicit time/protocol contracts.**
   Components never reference each other directly.
5. **Software semantics may be abstracted; architectural boundaries remain specification-accurate.**
   An implementation may simplify internals. Wherever it touches real hardware, it must follow the specification exactly. That includes the ISA, traps, CSRs, page tables, the register ABI, and bus protocols.
6. **Every backend is replaceable behind the same contract.**

---

## 2. Target Shape

```text
System
├─ Platform   Motherboard · PCIe · Firmware · Power/Clock
├─ Compute    CPU · GPU · Accelerator
├─ Memory     Cache · RAM · VRAM · Virtual Memory
├─ Storage    NVMe SSD · HDD · Partition · Filesystem
├─ I/O        USB · Keyboard · Display · Interrupt/DMA
├─ Network    NIC · Ethernet · IP · TCP/UDP · Socket
└─ Software   Boot · Kernel · Process/Thread · Scheduler · Driver · Runtime · Application
```

Users can drill into any layer, for example `Computer → CPU → Core → Pipeline → ALU`.

### Core Features

| Feature | Description |
|---|---|
| **System Explorer** | Navigate the full system topology hierarchically. |
| **Execution Trace** | Follow every system-wide event caused by running a program: `PROCESS_CREATE`, `PAGE_FAULT`, `NVME_READ`, `TLB_MISS`, `L1_MISS`, `DRAM_READ`, `SYSCALL_ENTER`, `INTERRUPT`, `DMA_COMPLETE`, `PACKET_TX`, `GPU_DISPATCH`, … |
| **State Inspector** | Inspect real state at any point in time. CPU: PC, registers, pipeline, ROB, cache, TLB. Process: PID, threads, address space, page table, FDs. SSD: queues, LBA, namespaces, controller, NAND mapping. |
| **Timeline** | Play · Pause · Step · Step Back · Seek · Breakpoint · Filter. In effect, a debugger for the entire system. |

These four remain the product's user-facing goal. They are built on two engine capabilities that the UI will consume:

| Capability | Description |
|---|---|
| **Differential Verification** | Compare two executions or two backends at stable architectural boundaries, under a comparison profile, and locate the first boundary where they differ. |
| **Reproducible Debugging** | Package a difference as a portable reproducer that rebuilds each compared side and replays the same result, byte for byte, on any supported host. |

---

## 3. Architecture

The core of the project is neither the UI nor the CPU. It is the **Simulation Runtime**, which binds every component into one execution model.

```text
                 Runtime
   (time · event queue · snapshot · trace)
                    │
      ┌─────────────┼─────────────┐
      ▼             ▼             ▼
     CPU  ◀─port─▶ Memory ◀─port─▶ Storage
      │             │             │
      └──── Events / Protocol ────┘
```

The runtime owns:

- global time and clock domains
- event ordering
- component lifecycle
- topology elaboration
- snapshot and restore
- deterministic replay
- breakpoints
- trace recording

### Skeleton (frozen in M0)

```text
Tick → Event → EventQueue → Component → Port/Protocol → State → Trace
```

| Area | Contract elements |
|---|---|
| Time | `Tick`, `Duration`, `SimulationClock`, `ClockDomain` |
| Event | `Event`, `EventKey`, `Phase`, `EventQueue` |
| Structure | `Component`, `Port`, `Protocol`, `Link`, `Topology` |
| State | `Snapshot`, `restore`, `snapshot_schema_version` |
| Observation | `Trace`, `Observer` |

The detailed design is in [docs/m0-design.md](docs/m0-design.md).

---

## 4. Time and Event Model (Summary)

- **Tick is a `u64`.** The resolution (`ticks_per_second`) is fixed when a session starts. The default is 1 tick = 1 ps. The resolution is never hard-coded.
- **No floating point anywhere in time computation.**
- **A ClockDomain expresses frequency as a rational number (`num/den` Hz).** The tick of edge *n* is computed absolutely, never accumulated. This means no drift, even when a period such as 3 GHz's is not a whole number of picoseconds.
- **Components express latency in cycles or in physical `Duration`.** Only the runtime projects those onto ticks.
- **Events are ordered by `EventKey = (tick, phase, sequence)`.** Phases run in a fixed order: `REQUEST → TRANSFER → COMPLETE → COMMIT → OBSERVE`. The runtime assigns sequence numbers in scheduling order.
- **Phases are monotonic within a tick.** Scheduling an event at the current tick into an earlier phase is a runtime error. Such work must move to `tick + 1` or later.
- **`OBSERVE` cannot mutate state.**

Example of two fidelities meeting:

```text
t = 120 ns       CPU (F3, 3 GHz) issues READ         → StorageRequest  @ REQUEST
t = 100 120 ns   SSD (F1, fixed 100 µs latency) done → StorageComplete @ COMPLETE
```

---

## 5. Fidelity

Levels describe **simulation fidelity**, not educational difficulty.

| Level | Name | Models |
|---|---|---|
| F0 | Structural | structure and connectivity only |
| F1 | Functional | correct input/output behavior |
| F2 | Architectural | ISA, memory, and OS semantics |
| F3 | Microarchitectural | pipeline, cache, ROB, scheduler |
| F4 | RTL | SystemVerilog level |
| F5 | Logic | gates, registers, muxes |
| F6 | Physical | standard cells, transistors, timing |

Components do not need to share a level. A system such as `CPU F3 · RAM F2 · SSD F1 · OS F2 · GPU F1 · Network F2` is valid. Different fidelities meet only through Port/Protocol and the time contract (Principle 4).

---

## 6. Backends

Concepts (contracts) are separated from implementations.

```text
CPU Contract                  Memory Contract
├─ Rust RV32I model (F2)      ├─ Simple RAM
├─ Rust pipeline model (F3)   ├─ DDR model
├─ SystemVerilog CPU (F4)     ├─ NUMA model
├─ QEMU adapter               └─ Trace-backed model
└─ Real trace adapter

OS Contract
├─ Modeled OS Backend        process / scheduler / syscall / VM state machines in Rust
├─ Native Guest OS Backend   real C/Assembly kernel running on the simulated CPU
└─ Trace Backend             Linux / QEMU / real-system traces
```

**A modeled OS is not hard-coding.**
- Hard-coding scripts a scenario, for example `printf called → emit SYSCALL event`.
- A model implements general rules, for example `ecall → trap → syscall dispatch → process state change`.

Under Principle 5, the Modeled OS still follows RISC-V conventions everywhere it touches the CPU:
- `satp` and Sv32 PTEs are written into simulated RAM, and the MMU performs real page walks.
- `ecall`, `mcause`/`scause`, `sepc`, and the register ABI follow the specification.

That is why swapping in the native kernel at M6 requires no CPU changes.

---

## 7. Simulation vs. Real Traces

Real-trace backends come later: QEMU, Linux perf, eBPF, ETW, SystemVerilog simulators, PCIe traces, GPU profilers, and network captures. Each is converted into the common contract and shown on the same timeline.

The two worlds **unify at the Event/Trace level only**.
- Real traces are sampled and incomplete, so full `State` and `Snapshot` exist only for simulation backends.
- Each backend declares what it provides as capabilities, such as `events`, `state`, `snapshot`, and `step_back`.
- The UI enables features based on the declared capabilities. The M4 verification engine uses the same idea: it chooses how to compare and search from what each backend declares ([docs/m4-design.md](docs/m4-design.md)).

---

## 8. Verification Strategy

```text
                   RISC-V Specification
                            │
               ┌────────────┴────────────┐
               ▼                         ▼
           Sail model                  Spike
      ACT expected results     normalized retirement stream
               │                         │
               └────────────┬────────────┘
                            ▼      oracles
                  SystemScope model (M1–M3)
             Rust CPU · SoC · Modeled OS backend
                            │
                            ▼
            Verification & Debugging Engine (M4)
   profile-defined comparison · primary architectural stream
      ┌──────────────┬──────────────┬──────────────┐
      ▼              ▼              ▼              ▼
    first         structured     relevant      portable
  divergence      state and      predecessor   reproducer
  (search)        trace diff     slice         and corpus
      └──────────────┴──────┬───────┴──────────────┘
               ┌────────────┴────────────┐
               ▼                         ▼
     SystemVerilog CPU (M5)     Native Guest OS (M6)
     Rust ↔ RTL differential    modeled ↔ native differential
                            │
                            ▼
              Visualizer / Interactive Debugger (M7)
```

- **riscv-tests** serve as processor unit tests.
- **ACT (riscv-arch-test)** is the reference for architectural conformance of the RV32I subset. The Sail model generates its expected results. ACT does not replace full processor verification, and SystemScope makes no privileged-architecture conformance claim.
- **The Spike differential** runs from M1. Spike runs the same committed program with `--log-commits`, and the harness compares SystemScope's canonical `rv32.commit` records with Spike's commit log after both runs. Each retirement is normalized to `pc`, the instruction word, the register write, and the memory access. M2 adds the whitelisted CSR write, and M3 adds the privilege mode and exceptions delivered to S. It is a post-run stream comparison, not a live lockstep, and it reports the first differing retirement ([docs/m1-design.md §10.3](docs/m1-design.md#103-spike-differential)).
- **The M4 Verification & Debugging Engine** turns a detected failure into a characterized, reproducible divergence. Each comparison follows a profile, which names one primary architectural stream. The engine compares executions at stable architectural boundaries, not at raw runtime events, and locates the first difference in that primary stream. Other streams and event-level traces are auxiliary diagnostic evidence. It reports structured state and trace differences and the recorded predecessor events, and it does not claim a root cause. It then packages the failure as a portable reproducer that rebuilds each side and replays identically on Linux and Windows. The current Spike differential becomes one adapter of this engine, and it is designed to accept further comparisons:
  - Rust CPU ↔ Spike;
  - Rust CPU ↔ SystemVerilog CPU (M5);
  - Modeled OS ↔ Native Guest OS (M6);
  - later, simulation ↔ trace-backed implementations.
- **Rust ↔ SystemVerilog differential** runs from M5, via Verilator co-simulation, through the M4 engine. It also demonstrates concretely that backends are replaceable.
- **Determinism acceptance tests** enforce Principle 2 in CI from M0 onward. They are specified in [docs/m0-design.md §9](docs/m0-design.md#9-deterministic-ci-acceptance-tests).

---

## 9. Languages

Each layer uses the language that fits its nature, and **each language is introduced only when needed**.

| Language | Purpose | Introduced |
|---|---|---|
| Rust | runtime, contracts, core models, verification & debugging engine (M4) | M0 |
| (Perfetto UI) | early timeline visualization, no custom code | M0 |
| RISC-V Assembly / C | bare-metal test programs | M1 |
| Python | analysis, verification scripts, tooling | as needed |
| TypeScript | SystemScope Visualizer / Interactive Debugger | M7 |
| SystemVerilog | RTL implementations (via Verilator) | M5 |
| C / Assembly | native guest kernel | M6 |
| Schema / Protobuf | language-neutral contracts and trace format | once two or more languages share contracts |

---

## 10. Repository Layout

Start with **two repositories**. This keeps contracts independent while avoiding coordination overhead while they are still changing rapidly.

```text
SystemScopeLabs (GitHub org)
├─ contracts     Time · Event · Component · Protocol · Trace · Capability
└─ systemscope   this repository
   ├─ runtime/
   ├─ components/   rv32i/ · platform/ · storage/ · os/ …
   ├─ verification/ (M4, planned; designed in docs/m4-design.md)
   ├─ visualizer/   (M7+)
   ├─ tests/        acceptance tests
   └─ docs/
```

- `systemscope` depends on `contracts` as a **git dependency pinned to a revision**. During local development, a Cargo `[patch]` overrides it with a path.
- A contract change lands in `contracts` first. `systemscope` then bumps the pinned revision.
- Once contracts stabilize, components split out into their own repositories: `cpu`, `storage`, `network`, …
- Because the org is already named after SystemScope, repositories do not repeat a `systemscope-` prefix.
- Future repositories: `linux`, `windows`, `riscv`, `x86`, `cuda`, `rocm`.

---

## 11. Roadmap

The final goal is not reduced. Instead, we cut out the **first complete system slice**. No milestone may hard-code a scenario. Everything must run through the component + event + state model.

| Milestone | Status | Scope | Exit criteria |
|---|---|---|---|
| **M0** | complete (`v0.1.0-m0`) | Time, event queue, component contract, deterministic DES, Perfetto trace | All criteria in [m0-design.md §10](docs/m0-design.md#10-m0-exit-criteria), including the three determinism acceptance tests passing in CI on Linux and Windows |
| **M1** | complete (`v0.2.0-m1`) | RV32I CPU + RAM, bare-metal ELF execution, UART output | All criteria in [m1-design.md §11](docs/m1-design.md#11-m1-exit-criteria): the 40 selected `riscv-tests` rv32ui tests (all but `fence_i` and `ma_data`) pass under the SystemScope test environment, Spike commit-log lockstep matches, ACT4 RV32I passes, on Linux and Windows, with the M0 golden digests unchanged |
| **M2** | complete (`v0.3.0-m2`) | Interrupts, DMA, block storage (abstract SSD model) | A bare-metal program completes a block read via a DMA-completion interrupt, and the acceptance tests still pass. Detailed criteria: [m2-design.md §18](docs/m2-design.md#18-m2-exit-criteria) |
| **M3** | complete (`v0.4.0-m3`) | Modeled OS Backend: process, syscalls, Sv32 virtual memory | The full path from executable to output runs through models, with page tables living in simulated RAM. Detailed criteria: [m3-design.md §18](docs/m3-design.md#18-m3-exit-criteria) |
| **M4** | M4.0 design frozen; M4.1 implementation next | Verification & Debugging Engine: deterministically locate, characterize, minimize, and reproduce the first divergence between two executions or implementations; design in [docs/m4-design.md](docs/m4-design.md) | For controlled, deliberately injected architectural divergences in the M3 reference system, SystemScope finds the first divergent boundary of the profile's primary architectural stream, and it reports the structured state and trace differences there. It then writes a portable reproducer, which replays the same verification result byte for byte on Linux and Windows. The M0–M3 behavior and golden files stay unchanged. Detailed criteria and the acceptance workload: [m4-design.md §28](docs/m4-design.md#28-m4-exit-criteria) |
| **M5** | planned | SystemVerilog CPU backend via Verilator | Matches the Rust CPU at every compared architectural boundary through the M4 differential engine, and passes the same suites. Any divergence is reported with the M4 locator and reproducer |
| **M6** | planned | Native Guest OS Backend: C/Assembly tiny kernel | Runs the M3 scenario with no changes to CPU code. Its architecturally observable behavior is compared with the Modeled OS backend through the M4 differential and reproducer infrastructure |
| **M7** | planned | SystemScope Visualizer / Interactive Debugger: topology, timeline, state, step, step back, seek, breakpoint, filter | The M3 scenario can be explored with step and step back in the UI. The UI also shows M4 artifacts: first divergence, state and trace diffs, checkpoint positions, predecessor slices, and reproducers |

The M3 first-slice scenario: `Executable → Storage → RAM → Process → CPU → Memory → Syscall → Kernel → Output`.

The Visualizer was first planned as M4. It moved to M7 so that the UI is built on top of the verification, reproduction, and backend-comparison data it will display. Its scope is unchanged.

After that, GPU and NIC plug into the same runtime.

---

## 12. Prior Art

We do not invent our own rules first. We study projects that have worked on these problems for years.

- **SystemC TLM-2.0**: loosely-timed and approximately-timed models, and interoperability across timing fidelities
- **gem5**: integer ticks (1 ps by default), clock domains, atomic/timing/functional memory access, SE/FS modes
- **SST (Structural Simulation Toolkit)**: `Component ↔ Link ↔ Event` structure
- **Perfetto**: trace format and timeline UI

---

## 13. Open Questions

- Target users and priority: education, systems research, or performance debugging?
- Explicit non-goals.
- Snapshot serialization format, decided in M0 ([m0-design.md §12](docs/m0-design.md#12-open-questions)).
- When to introduce host-parallel simulation (PDES), and how to keep it deterministic.

The M4.0 freeze answered the verification questions: M4 targets backends that report architectural boundaries, not arbitrary systems; event-level comparison is a same-engine diagnostic, and architectural comparison works across backends; a slice follows only the links the trace records; fuzzing is not in M4's core, only deterministic seeded campaigns; and a trace-backed backend takes part through its outcome and architectural stream only. The M4 questions and their frozen answers are in [docs/m4-design.md §29](docs/m4-design.md#29-design-questions-and-frozen-answers).

---

## 14. Name and Identity

- Project name: **SystemScope**, meaning scoping the whole system to see its structure, state, and events.
- Category: **Systems Simulation & Observability Platform**.
- "Digital twin" is reserved for when the platform synchronizes continuously with real hardware.
