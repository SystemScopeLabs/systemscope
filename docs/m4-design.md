# M4 Design: Verification & Debugging Engine

> Status: Frozen at M4.0 · Implementation status: Not started (M4.1 is next) · Parent: [plan.md](../plan.md) · Builds on: [m3-design.md](m3-design.md), [m2-design.md](m2-design.md), [m1-design.md](m1-design.md), [m0-design.md](m0-design.md)

This document is the architecture contract for M4, frozen at M4.0. It fixes the decisions the M4 implementation steps (§27) depend on (§3), the controlled divergences and mutation classes (§25), the acceptance workload and exit criteria (§28), and the answers to the M4 design questions (§29). Nothing in it is implemented yet, and M4.1 is the next step. Changing a frozen decision needs an explicit, reviewed revision of this document. M0–M3 are frozen, and nothing here changes their semantics, schemas, contracts, fixtures, or golden files.

---

## 1. Goal and Shape

### 1.1 The Problem

SystemScope already detects failures well:

| Signal | Where it comes from |
|---|---|
| golden mismatch | `m{0,1,2,3}-golden verify`: a field of a committed golden record differs |
| digest mismatch | `StateDigest`, `ExecutionDigest`, `TraceDigest` differ (m0-design §9.2) |
| Spike mismatch | `spike::compare`: the first differing normalized retirement (m1-design §10.3) |
| snapshot mismatch | a restored run does not replay the uninterrupted run (m3-design §9.2) |
| cross-OS mismatch | `m*-golden check`: the other OS's emitted files differ |
| mutation failure | a targeted mutant survives |

Each signal says *that* something differs. None of them, except the Spike stream comparison, says *where*. Even Spike's only covers the one pair it was written for. M4 turns a failure into answers to five questions:

```text
FAIL
  ↓
Where did the two executions first diverge?            first divergent primary-stream boundary (§11)
  ↓
Which state first differed, and how?                   structured state diff (§13)
  ↓
Which recorded predecessor events relate to it?        relevant-predecessor slice (§15)
  ↓
Can each side be rebuilt and replayed portably?        reproducer bundle (§16)
  ↓
Can the reproducer be made smaller?                    minimization (§17)
```

M4 is not a milestone that adds test counts. It is the product capability that plan.md §2 calls *Differential Verification* and *Reproducible Debugging*. The M7 Visualizer will present it.

### 1.2 Goal

**Deterministically locate, characterize, minimize, and reproduce the first divergence between two executions or two implementations of the same contract.** It does this at stable architectural boundaries that do not depend on how either implementation schedules its internal work.

*Characterize* has a bounded meaning. M4 reports:
- which boundary diverged first;
- which fields differ there;
- which state differs;
- the trace windows around it;
- which recorded predecessor events relate to the differing state (§15).

It does not determine a root cause or claim why a bug occurred (D12).

### 1.3 Shape

```text
      ComparisonCase (§6.6): comparison profile · common scenario · expected side · actual side
                           │
            ┌──────────────┴──────────────┐
            ▼                             ▼
      expected side                 actual side
      backend, input variant,       backend, input variant,
      run config, replay origin     run config, replay origin
      capabilities (§7)             capabilities (§7)
            │  observations at            │
            │  comparison boundaries (§8) │
            └──────────────┬──────────────┘
                           ▼
                 Comparator (§10)        explicit equality under a comparison profile
                           │
                           ▼
                 Locator (§11, §12)      lockstep scan · stream compare · checkpoint search
                           │
            ┌──────────────┼──────────────┬───────────────┐
            ▼              ▼              ▼               ▼
      state diff (§13) trace diff (§14) slice (§15)  reproducer (§16)
                                                          │
                                   minimization (§17) ◀───┤
                                   regression corpus (§18)◀┘
```

### 1.4 Completion Definition

M4 is complete when §28's criteria hold. In short, SystemScope must, for every controlled divergence of §25.1 (five injected into the M3 reference system, one Spike-side):
1. find the first divergent boundary of the comparison profile's primary stream automatically, and equal the independent oracle's answer;
2. report the structured state and trace differences at it;
3. write a portable reproducer that replays the same verification result, byte for byte, for the same comparison case and canonical replay configuration (§6.6), on Linux and Windows wherever both run the case's backends (§28.1);
4. leave every M0–M3 behavior, schema, contract, and golden file unchanged.

---

## 2. Scope and Non-goals

### 2.1 In Scope

- Deterministic comparison of two executions at architectural boundaries, under an explicit comparison profile.
- Divergence detection with explicit equality rules and canonical encodings.
- First-divergence localization for backends with and without snapshot capability.
- Checkpoint search over a monotone prefix-agreement predicate (§12), reusing the M3 portable snapshot unchanged.
- Structured state diff at the divergence, from `StateView` and from schema decoders where they exist.
- Trace diff in three classes: canonical, normalized architectural, and diagnostic (§14).
- A relevant-predecessor slice, limited to links the trace records (§15).
- Reproducer bundles that rebuild each side independently: portable, canonical, corruption-detecting, cross-OS reproducible (§16).
- Deterministic minimization of the replay window and the failing prefix (§17). Scenario reduction is deferred beyond M4 (§17.3).
- Backend-neutral adapters: the SystemScope runtime and the existing Spike differential (§9).
- A regression corpus, with a review step and no automatic blessing (§18).
- Deterministic, seeded, serialized campaigns over the existing program generators (`progen`, `privgen`, `vmgen`), feeding the corpus.

### 2.2 Non-goals

- **GUI or TypeScript.** That is M7. M4's data model is UI-independent (§24.3).
- **The RTL implementation, Verilator, or an RTL adapter.** That is M5.
- **The native guest kernel.** That is M6.
- **Formal proof, symbolic execution, or model checking.** M4 compares concrete executions only.
- **Coverage-guided or mutational fuzzing** (decision D10). Seeded campaigns over the existing generators are in scope, and a fuzzer is a later extension that can feed the same corpus.
- **Automatic cause attribution.** M4 reports the first difference and the recorded predecessors of the differing state. It never labels one of them as the cause (§15).
- **Debugging arbitrary distributed or non-deterministic systems.** M4 requires backends that are deterministic for a given scenario, or that are compared through recorded streams.
- **Machine-learning analysis.**
- **Performance optimization of the simulator, or host-parallel PDES.** M4 may run independent replays on several host threads, as `m3-golden every-event` already does, but it never parallelizes one simulation.
- **Any change to M0–M3 architectural semantics**, component schemas, protocols, contracts, fixtures, or golden files. Controlled divergences are injected by test tooling (§25), never by changing a production component.
- **Watchdog or step-limit semantics in the simulator.** Budgets are harness-side safety bounds in each side's `BackendRunConfig` (§6.6), as in M3.7.

---

## 3. Frozen M4 Decisions

Every decision below is accepted at M4.0. The further decisions M4.0 took on the design questions are in §29.

| # | Decision | Section |
|---|---|---|
| D1 | The universal comparison unit is the **architectural boundary**, not the runtime event. Event-level comparison is a same-engine, same-topology special case. | §6 |
| D2 | Architectural boundaries are **ordinal**, a position in a per-backend boundary stream. Two backends align by ordinal within a stream, never by event index, tick, or cycle. Ordinals of different streams are never compared with each other. | §6.4 |
| D3 | The primary locator is a **lockstep or streaming scan with a chained prefix digest**. Checkpoint search accelerates it only where the backends can restore (§7), and it always searches a monotone predicate. | §11, §12 |
| D4 | The M4 checkpoint **wraps the unchanged M0 runtime snapshot**, adding its boundary positions and prefix-digest values. There is no new snapshot format. Checkpoints belong to one side of a comparison. | §12.2 |
| D5 | Observations carry **named fields with stable identities** (`stream / kind / field`) and `ObservedValue`s, either `Absent` or `Present(Value)` with a contracts `Value`, encoded with the M0 §4.5 primitives. `Absent` has its own canonical tag, distinct from every present value (ObservedValue canonical encoding v1: `0x00` `Absent`, `0x01` `Present`). | §8 |
| D6 | Capabilities are an **M4-local declaration**, not a contracts type. A contracts `Capability` stays a potential future contract (§23). | §7 |
| D7 | M4 lives in a **new workspace crate, `verification/` (`systemscope-verify`)**. It is backend-neutral and depends only on `systemscope-contracts`, `systemscope-runtime`, `blake3`, and `serde_json`. It does not depend on `rv32i`, `platform`, `os`, `elf`, or any test crate. The platform-specific projections and the Spike adapter stay with the code they project (`tests/rv32`, `tests/acceptance`). | §22 |
| D8 | **No contracts change** is planned. Every M4 need was checked against the pinned contracts `90900a1`. | §23 |
| D9 | A reproducer is a **canonical bundle**: a canonical manifest, per-side replay material (an optional checkpoint with the unchanged runtime snapshot inside, and optional input-variant bytes), optional canonical observation evidence, and the canonical verification result. In M4 it is persisted as a **canonical directory bundle**, in the style of the golden files. A single-file container is a possible future transport or packaging form, outside the M4 contract. No form may contain host paths or timestamps. | §16 |
| D10 | Fuzzing is **not M4 core**. Seeded campaigns over the existing generators are in scope. | §2 |
| D11 | Minimization in M4 is **prefix and replay-window minimization** only (M4.5). Scenario reduction is deferred beyond M4. | §17 |
| D12 | "Root cause" is not an M4 output. The slice is **recorded relevant predecessors** only. | §15 |
| D13 | Every comparison profile names one **primary stream**. The first divergence of a comparison is the first divergence of its primary stream. Other streams are auxiliary evidence and never decide it. | §6.4, §6.5 |
| D14 | A comparison is a **`ComparisonCase`**: comparison-wide data (profile, common scenario) plus two sides, each with its own backend, input variant, run config, and replay origin. A reproducer must reconstruct each side independently. | §6.6, §16 |
| D15 | **`Scenario` holds semantic inputs only.** Safety budgets and other non-semantic operational controls are per-side `BackendRunConfig`, never part of any identity or comparison semantics. Reproducers persist only a canonical replay configuration, never host-local limits. The stop rule belongs to the comparison profile. | §6.6 |

---

## 4. What M0–M3 Provide

M4 is built almost entirely on surfaces that exist and are frozen. Each row was checked in the repository at `fd9f5f1`.

| Surface | Where | What M4 uses it for |
|---|---|---|
| `Runtime::step`, `run`, `run_until` | `runtime/src/runtime.rs` | advance one event at a time; pause at event boundaries |
| `Observer::on_trace`, `on_after_dispatch`, `WorldView::inspect` | contracts `observe.rs` | read-only observation of records and `StateView`s (m0-design §8.2) |
| `Runtime::snapshot`, `restore`, `state_digest` | `runtime/src/snapshot.rs` | checkpoints at any event boundary; the snapshot holds `execution_digest` (m0-design §7) |
| `Runtime::execution_digest` | `runtime/src/runtime.rs` | chained event digest at every boundary |
| canonical trace, `Trace::digest`, `resume_trace` | `runtime/src/trace.rs` | `TraceDigest`; a trace prefix continues across restore |
| `runtime.dispatch` records | `runtime/src/trace.rs` | every event's `source`, target, `port`, `protocol`, `msg`, `txn`, and the protocol's fields |
| `rv32.commit`, `rv32.exception`, `rv32.interrupt`, `rv32.trap`, `rv32.halt` | `components/rv32i/src/cpu.rs` | architectural boundaries: `pc`, `insn`, `rd`, `rd_value`, `next_pc`, `priv`, `addr`, `paddr`, `width`, `value`, CSR writes; trap `cause`, `tval` |
| `os.*` records | `components/os/src/{kernel,procop}.rs`, m3-design §6.8 | OS-level boundaries: syscall enter/exit, switch, create, exit, fault, shutdown |
| `platform.uart.tx`, `platform.disk.*`, `platform.blk.*` | `components/platform/src` | externally visible effects |
| CPU `inspect` | `components/rv32i/src/cpu.rs` | `pc`, `x1`–`x31`, `instret`, `state`, `priv`, the supported CSRs by name |
| `spike::Retire`, `spike::Event`, `spike::compare`, `spike::replay` | `tests/rv32/src/spike.rs` | the existing normalized retirement stream, first-difference report, and register replay |
| the M3 portable decoder | `tests/acceptance/src/m3/portable.rs` | an independent schema-level reading of every `m3-reference` component's snapshot bytes |
| every-event sweep, cost-balanced shards | `tests/acceptance/src/m3/checkpoint.rs` | parallel independent replays; resume-without-reissue checks |
| golden `Record` | `tests/acceptance/src/m3/golden.rs` | final-outcome fields: halt, shutdown reason, UART, metrics, digests |

**Gaps M4 must fill** (none needs a contracts or component change):
- There is no backend-neutral observation type; each differential has its own (`Retire`, `Event`).
- `StateView` has no version and no stable ordering promise beyond "the order the component chose". The RAM and kernel views are summaries (`size`, `image_hash`, `non_zero_pages`; kernel phase and transaction numbers), not full state.
- There is no checkpoint index that maps architectural boundaries to event boundaries.
- There is no reproducer format, and the regression flow is manual.

---

## 5. Terminology

| Term | Meaning in M4 |
|---|---|
| **Execution** | One run of one side of a comparison case: one backend on the common scenario plus that side's input variant, from its replay origin to the profile's stop rule. |
| **Backend** | An implementation that can produce an execution through an adapter: the SystemScope runtime with a platform, Spike, and later an RTL simulation or a native-kernel platform. |
| **Scenario** | The semantic inputs common to both sides: platform identity and CPU profile, program or disk bytes (by BLAKE3), and seed. It holds no budget, host path, thread count, or wall-clock data (§6.6). |
| **Comparison case** | The complete description of one comparison: the comparison profile, the common scenario, and the expected and actual sides (§6.6). |
| **Side** | One of the two executions of a case: its backend identity, input variant, run configuration, and replay origin. "Expected" and "actual" are semantic roles. |
| **Input variant** | The side-specific change to the common scenario, identified canonically, for example a patched disk image. Empty when both sides run the same inputs. |
| **BackendRunConfig** | Per-side non-semantic operational controls: the safety budget in the backend's own unit, logging and diagnostic options, harness-side limits. It is not comparison semantics (§6.6). |
| **Canonical replay configuration** | The deterministic, host-independent part of a side's run configuration that a reproducer records (§6.6). |
| **Replay origin** | Where a side's replay starts: its initial state, or one of its own checkpoints. |
| **Observation** | A canonical record of what a backend reports at one comparison boundary: a boundary kind and named fields (§8). |
| **Comparison boundary** | A point every compared backend can identify in its own execution, such as an instruction retirement or a trap entry. Its position is an ordinal in a boundary stream (§6.3). |
| **Event boundary** | A SystemScope runtime point between two dispatched events (m0-design §7). It is not a comparison boundary; §12.1 maps between them. |
| **Checkpoint** | A restorable state of one side at an event boundary, plus the comparison-boundary positions and prefix-digest values it corresponds to (§12.2). |
| **Digest** | A BLAKE3 value. The three M0 digests, and M4's chained **prefix digest** over a boundary stream (§11.2). |
| **Divergence** | A comparison boundary at which the two observations are unequal under the comparison profile's rules (§10). |
| **Primary stream** | The one stream a comparison profile names to define the first divergence (§6.4). |
| **Auxiliary stream** | Any other compared stream. Its divergences are reported as supporting evidence only. |
| **First divergence** | The smallest divergent ordinal in the profile's primary stream, where every earlier primary-stream observation compared equal. |
| **Observed value** | A field's value in an observation: `Absent` or `Present(Value)` (§8.1). |
| **Event mismatch** | Two same-engine executions dispatch different events at the same event index. It is meaningful only for the same engine and topology. |
| **Architectural mismatch** | Two executions report unequal observations at the same comparison boundary. It is meaningful across backends. |
| **Comparison profile** | A named, versioned rule set: the primary and auxiliary streams, the boundary kinds and fields compared, excluded, and required (for example `paddr` excluded across different kernels), windows, and the stop rule (§6.5). |
| **Stop rule** | The semantic condition that ends a comparison, such as `write_tohost`, shutdown, or the first non-delegated trap. Owned by the comparison profile. |
| **Oracle** | The execution treated as expected (Spike, Sail-derived ACT results, a reference run, or the Rust model against RTL). M4 treats "expected" and "actual" as roles only. |
| **Reproducer** | A portable bundle that replays a verification result (§16). |
| **Artifact** | Any persisted M4 output: an observation stream, a verification result, a reproducer, a corpus entry. Every artifact is canonical. |
| **Minimization predicate** | A deterministic function of a candidate reproducer that says whether it still shows *the same* divergence (§17.1). |

---

## 6. Comparison Model

### 6.1 Levels

| Level | Compared | Needs | Across backends? |
|---|---|---|---|
| **L0 Outcome** | final result: halt reason and PC, shutdown reason, UART bytes, selected metrics | any backend | yes |
| **L1 Digest** | `StateDigest` and `ExecutionDigest` at an event boundary; `TraceDigest` | same engine and topology | no |
| **L2 Architectural stream** | ordered observations at comparison boundaries (§6.3) | boundary stream capability | **yes: the M4 contract** |
| **L3 Structured state** | named state fields at a boundary (§13) | state capability | partly: architectural fields only |
| **L4 Event / trace** | `runtime.dispatch` and component records | same engine and topology | no |

L2 is the universal comparison level (D1). L0 is the coarse outcome, L1 and L4 are same-engine accelerators and diagnostics, and L3 characterizes a divergence found at L2.

### 6.2 Why Not Raw Events

In the M3 reference run, 166,897 dispatched events carry 2,176 retirements and 23 exceptions (golden `events`, `instret`, `exceptions`), about 76 events per retirement. Those events model bus arbitration, page-walk reads, DMA beats, and kernel accesses. An RTL CPU takes cycles, not events, to do the same work. Spike has no event model at all, and a native kernel executes instructions where the ModeledKernel issues bus accesses. So:

- raw event indices, ticks, and cycles are **never** a cross-backend alignment key;
- **event mismatch** is reported only between two executions of the same engine and topology, where it is a precise diagnostic (§14.1);
- **architectural mismatch** is the divergence M4 locates and reports.

### 6.3 Comparison Boundary Kinds

These are frozen at M4.0, and each is grounded in records that already exist.

| Stream | Boundary kind | SystemScope source | Spike | RTL (M5, expected) | Native OS (M6) |
|---|---|---|---|---|---|
| `arch` | `retire` | `rv32.commit` | commit log | retirement port | same CPU records |
| `arch` | `exception` | `rv32.exception`; the halting `rv32.trap` (§8.2) | `exception` lines (`-l`) | trap signal | same |
| `arch` | `interrupt` | `rv32.interrupt` | — (not compared in M4, §8.2) | trap signal | same |
| `effect` | `uart_tx` | `platform.uart.tx` | — (device map differs) | — | same device |
| `effect` | `disk_write` | `platform.disk.write` | — | — | same device |
| `effect` | `shutdown` | `os.shutdown` / halt | HTIF exit | — | firmware shutdown |
| `os` | `syscall_enter`, `syscall_exit`, `switch`, `create`, `exit`, `fault` | `os.*` | — | — | derived projection (§24.2) |

- A backend reports only the streams its capabilities declare (§7). A comparison profile names the streams it compares, and comparing a stream that one side cannot report is an **incomparable** result, not a divergence (§20).
- The `arch` stream is the usual primary stream for CPU backends (§6.4). `effect` compares externally visible behavior across different internal designs. `os` compares Modeled OS executions, and native ones only through a projection.

### 6.4 Alignment and the Primary Stream

- Within a stream, boundary *i* on one side aligns with boundary *i* on the other (D2). Existing differentials already align this way: `spike::compare` finds the first index at which two `Retire` lists differ, or one ends first.
- **Primary stream (D13).** Every comparison profile names exactly one primary stream `p`. The first divergence of a comparison is defined on it alone:

  ```text
  FirstDivergence(case, profile) = the smallest ordinal d in profile.primary_stream with
      obs_expected[d] ≠ obs_actual[d]   and   obs_expected[j] = obs_actual[j] for every j < d
  ```

- **Auxiliary streams.** Every other compared stream is compared under the same rules, on its own ordinals. Its first divergence is reported separately, as supporting evidence, and never decides the first divergence of the comparison:

  ```text
  Primary divergence:    arch[152]
  Additional evidence:   effect[4]   os[9]
  ```

- **No cross-stream ordering.** Ordinals of different streams are different coordinate systems. M4 never concludes from ordinals that `effect[4]` occurred before `arch[152]`. Claiming such an order needs timeline evidence from the same backend.
- **Same-engine global time.** When both sides are SystemScope runtimes with the same topology, the event index is a shared timeline. The report may then place boundaries of different streams, and the first state divergence (§12.4), on it as diagnostic evidence, for example "primary divergence `arch[152]`; first state divergence at event 8,991". This is never promoted to a rule for heterogeneous backends.
- **Length.** A difference in stream length is a divergence at the first missing ordinal ("one stream ends first"), as in `spike::compare`.

Frozen primary streams:

| Comparison | Primary | Auxiliary |
|---|---|---|
| SystemScope ↔ Spike (existing suites) | `arch` | none |
| SystemScope ↔ SystemScope (`m3-reference`) | `arch` | `effect`, `os` |
| Rust CPU ↔ RTL (M5) | `arch` | none compared; RTL diagnostics attached (§14.3) |
| Modeled OS ↔ Native OS (M6) | deferred to the M6 design; `effect` is future guidance only (§24.2) | user-visible projection, when defined |

### 6.5 Comparison Profile

```text
ComparisonProfile {
    name, version
    primary_stream:     Str                 // exactly one
    auxiliary_streams:  [Str]               // in name order; may be empty
    kinds:              per stream, the boundary kinds compared
    fields:             per kind, the compared fields in order, each `required` or `optional`
    excluded:           per kind, the fields never read
    windows:            per stream, where comparison starts (for example the ELF entry)
    stop_rule:          the semantic end of the comparison
    report_window:      w, the observations retained before and after a divergence
                        for diagnostics (§14); never a termination condition
}
```

- **Identity.** A profile's identity is the BLAKE3 of its canonical encoding (M0 §4.5 primitives), which includes its name and version. It is part of the case identity (§6.6).
- **Stop rule ownership (D15).** The profile owns the stop rule and the windows. The scenario never defines one. Examples:
  - the rv32ui profile starts at the ELF entry and ends at `write_tohost`;
  - the M3 Spike profile ends at the first trap taken in M (m3-2-spike-appendix D11);
  - the `m3-reference` profile ends at shutdown;
  - a profile may end after N primary-stream boundaries, or at a specific architectural condition.
- **Adapters and the stop rule.** Each adapter is told the stop rule. Reaching it is the normal end of an execution. Exhausting the side's safety budget first is `BackendFailed` (§20).
- **Existing rules.** The existing Spike comparisons become profile versions with unchanged rules.
- **Report window.** Every M4 profile version 1 sets `w = 3`: three observations before and after a divergence, the window `spike::compare` already prints.

### 6.6 Comparison Case, Scenario, and BackendRunConfig

Comparison-wide data and side-specific data are separate (D14):

```text
ComparisonCase
├─ profile            ComparisonProfile (§6.5)
├─ scenario           Scenario: the common semantic inputs
├─ expected: Side
│   ├─ backend        BackendId: name, pinned version, build identity
│   ├─ input          InputVariant: canonical identity of side-specific input changes, or none
│   ├─ run_config     BackendRunConfig
│   └─ origin         initial state | one of this side's checkpoints (§12.2)
└─ actual: Side       (same shape)
```

**Scenario** holds semantic inputs only:
- the platform's semantic identity (for example `m3-reference` and its topology hash) and the CPU profile (`M1`, `M2`, `M3`);
- the program or disk identity: the BLAKE3 of the bytes, with a repository-relative fixture path as a locator only;
- the seed.

Its identity is the BLAKE3 of its canonical encoding. It never includes a budget, a host path, a thread count, or wall-clock data.

**Input variant** is a side's canonical change to the common scenario:
- none;
- a patched disk or program image: the patched bytes' BLAKE3, plus the patch (offset, old bytes, new bytes);
- a state patch: the event index and the patched field, old and new value (§25.1).

**Execution identity.** Each side has `ExecutionId = BLAKE3(scenario identity ‖ input variant identity ‖ backend identity)`. The case identity is the profile identity, both execution identities, and the role assignment. A side's run config and replay origin are not part of any identity. They change cost and starting point, never the result, and a checkpoint used as an origin is validated against its side's `ExecutionId` (§12.2).

**BackendRunConfig** holds per-side **non-semantic operational controls** only:
- the safety budget, in the backend's own unit: events for SystemScope, instructions for Spike, cycles for an RTL simulation;
- logging mode and diagnostic verbosity;
- harness-side execution limits;
- adapter implementation controls that cannot change architectural output.

**Semantic options never live here.** If changing an option can change the architectural observation stream under otherwise identical inputs, that option is semantic and must not live in `BackendRunConfig`. It belongs to the backend identity (for example a build or ISA configuration), the input variant, the scenario, or the comparison profile.

Rules:
- A budget is a watchdog, not comparison semantics. The two sides never need equal numeric budgets, since their units differ.
- Exhausting the budget before the stop rule is `BackendFailed` (§20).

**Local vs. canonical configuration.** Two views of a run configuration are kept apart. They are semantic roles, not a frozen Rust API:

| View | Holds | Persisted? |
|---|---|---|
| **Local run configuration** | the operational limits a host actually used for analysis, for example a larger budget for a long search | never; it is not artifact state |
| **Canonical replay configuration** | the safety budget and the required non-semantic adapter controls a reproducer replays with | yes, per side, in the manifest (§16) |

- The canonical replay configuration comes from the comparison profile, which declares a canonical replay budget per backend kind it supports, or from an explicit value in the comparison case. It never comes from the host.
- It holds no host path, temporary path, thread count, wall-clock value, or other machine-specific value.
- **Artifact rule.** A reproducer writer re-verifies the case under the canonical replay configuration before writing. If the canonical budget is exhausted there, the result is `BackendFailed` and no bundle is written. Analysis with a larger local budget therefore never changes artifact bytes.
- **Byte identity.** The same comparison case, with the same comparison profile and the same canonical replay configuration, gives a byte-identical verification result and reproducer bundle on Linux and Windows.
- **Semantic invariance.** A replay with a larger safety budget must give the same semantic verification result: the same outcome, first divergence, and diffs. This is a semantic requirement, not an artifact byte-identity requirement. An artifact rewritten after such a replay is normalized back to the canonical replay configuration.

**Invariant.** A reproducer contains enough information to reconstruct each side independently (§16). Sharing one checkpoint file between sides is only deduplication, allowed when both origins are byte-identical.

### 6.7 Verification Result

The semantic shape of a result. It is not a frozen Rust API. A reproducer's `result.bin` is the canonical persisted form of this result (§16) for the outcomes a reproducer can hold. An `Incomparable` or `EngineError` result may be persisted only as a diagnostic result artifact (§16.1).

```text
VerificationResult
├─ outcome             Agree | Diverged | AuxiliaryDivergence | Incomparable | EngineError (§20)
├─ primary?            PrimaryResult (below)
├─ auxiliary           per auxiliary stream: Agree | Diverged { ordinal, kind, diff }
├─ state_evidence?     §13; for the same engine, also the first state divergence (§12.4)
├─ trace_evidence?     §14 windows
└─ predecessor_slice?  §15

PrimaryResult =
    Agree    { stream, compared, final_prefix }
  | Diverged { stream, ordinal, kind, observation_diff }
```

`compared` is the number of primary observations compared up to the stop rule, and `final_prefix` is the primary prefix digest `P(compared)` (§8.3). An auxiliary stream's `Agree` carries the same two values for its stream.

`?` marks an entry that may be missing, depending on the outcome or the capabilities. It does not fix a Rust `Option` or enum layout. `PrimaryResult` is a tagged variant, not two independent optional fields (a first divergence and an observation diff): independent fields would allow invalid states, such as a diff with no divergence.

**Outcome invariants.** They match §16.1 and §20:

| `outcome` | `primary` | `auxiliary` | manifest `primary_divergence` |
|---|---|---|---|
| `Diverged` | `Diverged { stream, ordinal, kind, observation_diff }` | each stream's result, as evidence | `(stream, ordinal, kind)` |
| `Agree` | `Agree { stream, compared, final_prefix }` | every stream `Agree` | none |
| `AuxiliaryDivergence` | `Agree { stream, compared, final_prefix }` | at least one stream `Diverged` | none |
| `Incomparable` | may be missing: the primary comparison could not start or finish | as available | none |
| `EngineError` | may be missing: no complete result was produced | as available | none |

**Evidence is conditional.**
- `state_evidence` requires the `state` capability on both sides, so there is none for Spike.
- `predecessor_slice` exists only for a primary divergence, so there is none for `Agree`.
- `Incomparable` and `EngineError` may carry no diff evidence at all.

---

## 7. Capability Model

A backend declares a capability set. It is an M4-local type (D6), and plan.md §7's capability idea applied to verification.

| Capability | Meaning | SystemScope runtime | Spike | RTL (M5, expected) | Native OS on SystemScope (M6) |
|---|---|---|---|---|---|
| `outcome` | L0 fields | yes | exit status, log end | yes | yes |
| `arch_stream` | L2 `arch` boundaries | yes | yes, post-run | expected | yes |
| `effect_stream` | L2 `effect` boundaries | yes | no | partial | yes |
| `os_stream` | L2 `os` boundaries | Modeled OS only | no | no | projection only |
| `step_boundary` | advance to the next boundary on demand (lockstep) | yes | no (post-run log) | expected | yes |
| `state` | L3 named fields | `StateView`; decoders per platform | no | selected signals, if exposed | yes |
| `snapshot` / `restore` | checkpoints at event boundaries | yes (M0 §7) | no | open (M5) | yes |
| `event_trace` | L4 | yes | no | no | yes |
| `deterministic_replay` | same scenario and input variant give the same streams | yes (Principle 2) | yes for a pinned build and fixed arguments | required | yes |

The locator chooses its strategy from the intersection of the two sets (§11.4). A backend never has to fake a capability. Each adapter also declares which fields of a profile it can report. §10 checks that declaration before any execution starts.

**Trace-backed backends** (plan.md §7), which replay a recorded trace instead of simulating, support `outcome`, the `arch` observation stream (post-run, through `record`), and stream compare (§11.3). They support no `state`, no `snapshot` or `restore`, no step back, and no same-engine event diff. A profile that requires a field or capability such a backend cannot provide gives `Incomparable` (§20).

---

## 8. Observation Model

### 8.1 Observation

```text
Observation {
    stream:   Str          // "arch" | "effect" | "os"
    ordinal:  u64          // position in its stream, from 0
    kind:     Str          // "retire", "exception", "uart_tx", …
    fields:   [(Str name, ObservedValue)]   // in the profile's field order for this kind
}

ObservedValue = Absent | Present(Value)
```

- **Field identity** is `stream / kind / field`, for example `arch/retire/rd_value`. Names are stable strings in the comparison profile, versioned with it. They are never Rust type names or pointer-derived keys.
- **Values** are `ObservedValue`s. A present value is a contracts `Value` (`U64`, `I64`, `Bool`, `Str`, `Bytes`). There are no floats, the same rule as trace records.
- **Field order** is the profile's declared order for the kind, not an alphabetical or map order, so the encoding is canonical and a diff reads naturally.
- **`Absent` is explicit.** The field is in the profile, but the backend reports no value for it at this boundary, for example `rd_value` for an instruction that writes no register. `Absent`, `Present(U64(0))`, `Present(Str(""))`, and `Present(Bytes([]))` are four distinct values in comparison, encoding, and digest. Excluded fields are not in observations at all. §10 defines how each case compares.
- **No giant struct.** An observation carries only the fields of its kind. Full state is L3, requested only where it is needed.

### 8.2 Frozen `arch` Fields

These are derived from what `rv32.commit` and the Spike parsers already carry.

| Kind | Fields |
|---|---|
| `retire` | `pc`, `insn`, `priv` (M3 profile), `rd` and `rd_value` (`Absent` for no write or `x0`), `mem` (`none`, or load `addr`, or store `addr`, `width`, `value`), `paddr` (M3, per profile, below), `csr` and `csr_value` (whitelisted write, M2+) |
| `exception` | `pc`, `insn`, `cause` (name), `tval`, `to` (target mode) |
| `interrupt` | `pc`, `cause`, `to` |

The Spike profiles keep their current rules. The first comparison is at the entry, `x0` writes do not count, and `mstatush` and `tcontrol` are not compared. The M3 privilege profile includes `priv` and delegated exceptions.

**`paddr` policy.** It is profile-specific, never universal:

| Profile | `paddr` |
|---|---|
| SystemScope ↔ SystemScope, same platform (`m3-reference`) | compared by default: both sides run the same kernel and frame allocator |
| SystemScope ↔ Spike (M3 privilege and Sv32) | not compared, as today: Spike's commit log names the virtual address, so only `addr` is compared. The SystemScope side keeps its local sanity check: `paddr` is a 34-bit address with `addr`'s page offset, and equals `addr` in M (`tests/rv32/src/spike.rs`) |
| Modeled OS ↔ Native OS (M6) | excluded: frame allocation may differ (§24.2) |

**`interrupt` and the Spike profiles.** The existing Spike profile versions do not compare `interrupt` boundaries, and M4 does not add them. Only a new, versioned profile may add them later; an existing version never changes. In `m3-reference` the CPU takes no interrupt (`IRQ_ENABLE` stays 0, m3-design §8.2), so its `arch` stream has no `interrupt` boundary.

**The halting trap.** In the `M3` profile a delegated exception is traced as `rv32.exception`. A non-delegated trap halts the CPU and is traced as `rv32.trap` instead (`components/rv32i/src/cpu.rs`). The `m3-reference` profile projects that `rv32.trap` as the final `exception` observation, with `to = M`, and reaching it is the profile's stop rule.

### 8.3 Encoding and Digest

- **Encoding** uses the M0 §4.5 primitives through contracts' `Encoder`: `stream`, `ordinal`, `kind`, then the field count, then each field as `name` followed by its `ObservedValue`:

  | Tag (`u8`) | ObservedValue | Payload |
  |---|---|---|
  | `0x00` | `Absent` | none |
  | `0x01` | `Present(Value)` | the `Value` encoding already used by traces |

  This is **ObservedValue canonical encoding v1**, frozen at M4.0. A decoder rejects every other tag value, reserved or unknown, as corrupt. There is no migration framework: a later encoding change is a new artifact format version or profile version, never a reinterpretation of v1 bytes.
- **Prefix digest:** `P(0) = [0; 32]`, `P(i+1) = BLAKE3(P(i) ‖ encode(obs_i))`, per stream. This is the same construction as `ExecutionDigest` (m0-design §4.4), but over observations, so it is backend-neutral. Because `Absent` has its own tag, a missing field and a zero field also give different digests. The locator uses the primary stream's `P`. Auxiliary streams' digests are evidence and consistency checks (§12.3).

### 8.4 Projections

A projection maps a backend's native evidence to observations. Projections are pure functions of recorded evidence. They never re-execute and never share decoding code with the other side, the rule m1-design §10.3 set for Spike.

- **SystemScope → `arch` / `effect` / `os`**: from canonical trace records, read by an observer (`on_trace`).
- **Spike → `arch`**: from the existing strict commit-log parsers (`parse_spike_log`, `parse_spike_l_log`, `parse_spike_m3_log`).
- **User-visible projection** (for M6, §24.2): `arch` restricted to U-mode boundaries, without `paddr`, plus the register file at each return to U, computed by replaying register writes as `spike::replay` does.

---

## 9. Differential Backend Interface

### 9.1 Where It Lives

The interface is local to SystemScope and lives in `systemscope-verify` (D7). It is not a contracts trait. Reasons:
- Its only implementors are adapters in this repository (the SystemScope runtime and Spike) and the planned M5 and M6 adapters.
- Its types (`Observation`, profiles, checkpoints) will change during M4. Contracts are frozen until running code exercises them (plan.md Principle 1).
- Nothing in the simulation depends on it. Components never see it.

### 9.2 Sketch

```rust
// systemscope-verify (a sketch; the Rust API is not frozen)
pub trait Backend {
    fn describe(&self) -> BackendId;          // name, pinned version, build identity
    fn capabilities(&self) -> Capabilities;   // §7
    /// Builds this side: the common scenario, its input variant, its safety
    /// bounds, and the profile's stop rule. Starts at the initial state.
    fn start(&mut self, scenario: &Scenario, input: &InputVariant,
             config: &BackendRunConfig, profile: &ComparisonProfile) -> Result<(), EngineError>;
    /// Advances to the next boundary of `stream` and returns its observation,
    /// or `None` at the end of the execution. Requires `step_boundary`.
    fn next(&mut self, stream: Stream) -> Result<Option<Observation>, EngineError>;
    /// The whole stream at once, for post-run backends such as Spike.
    fn record(&mut self, stream: Stream) -> Result<Vec<Observation>, EngineError>;
    fn outcome(&self) -> Result<Outcome, EngineError>;
    fn state(&self, request: &StateRequest) -> Result<StateObservation, EngineError>; // `state`
    fn checkpoint(&self) -> Result<Checkpoint, EngineError>;       // `snapshot`
    fn restore(&mut self, c: &Checkpoint) -> Result<(), EngineError>; // `restore`
}
```

- A method whose capability is not declared returns `EngineError::Unsupported`. The locator never calls it, and a test pins that it never does.
- **The SystemScope runtime adapter** is generic. It holds a `Runtime` built by a platform builder (`m3ref`, `m2ref`, …) and a projection. It steps events until the projection emits the next observation of the requested stream, bounded by the side's `BackendRunConfig` safety budget, and it ends at the profile's stop rule. `checkpoint` and `restore` are the M0 snapshot plus the §12.2 metadata.
- **The Spike adapter** wraps the existing `tests/rv32` runner and parsers, and implements `record` only.
- **M5 and M6 add adapters.** They should not change the locator, the comparator, or the bundle format (§24).

---

## 10. Divergence Detection

- **Explicit equality.** Two observations at the same `(stream, ordinal)` are equal when their `kind`s are equal and, for every compared field, their `ObservedValue`s are equal: both `Absent`, or both `Present` with equal contracts `Value`s (tag and payload). There are no tolerances and no floating point.
- **Four kinds of "no value":**

  | Case | Meaning | Handling |
  |---|---|---|
  | **Excluded** | the profile does not compare the field | never read; not in observations |
  | **Absent** | the field is in the profile, and the backend reports no value for it at this boundary | an ordinary value: `Absent` equals `Absent`; `Absent` vs. `Present(v)` is a divergence |
  | **Optional, unsupported** | one side cannot report an optional field at all | dropped from this case's comparison before execution, and listed in the result as not compared |
  | **Required, unavailable** | one side cannot report a required field, or reports `Absent` where the profile requires a present value | `Incomparable` (§20), naming the side, the field, and the ordinal if any; never equal and never a divergence |

  Field support comes from each adapter's declaration (§7) and is checked against the profile before either side runs.
- **Determinism.** Comparison reads canonical observations only. It never depends on host pointers, map iteration, thread timing, or wall-clock time. Where a backend runs on several host threads, the result is a pure function of the per-boundary observations, as in the every-event sweep.
- **No observer perturbation.** SystemScope-side observation uses `Observer` callbacks and `inspect()` only, which m0-design §8.2 and AT-3 guarantee change nothing. Checkpoints use `snapshot()`, which reads state and is identical with or without tracing (m0-design §7).
- **Host stability.** Every persisted comparison result is canonical bytes. Its equality on Linux and Windows is an exit criterion (§28.2).

---

## 11. First-Divergence Localization

### 11.1 Two Situations

| Situation | Example | Locator |
|---|---|---|
| **Same-engine replay** | reference vs. injected state on `m3-reference`; before/after a change; restored vs. uninterrupted | lockstep scan, with optional checkpoint search |
| **Heterogeneous backends** | SystemScope vs. Spike; later Rust vs. RTL, modeled vs. native | lockstep scan if both can step, else stream compare; checkpoint search only if both can restore |

### 11.2 The Monotone Predicate

The locator searches **prefix agreement** over the profile's primary stream `p`. A and B are the two sides:

```text
Agree(i) ⇔ for every j < i, obs_A,p[j] = obs_B,p[j]  (under the profile)
```

`Agree` is monotone: once false, false for every larger `i`. The first divergence is `max { i : Agree(i) }`. Plain per-boundary equality, `obs_A,p[i] = obs_B,p[i]`, is **not** monotone, because two executions can diverge and later agree again, for example after a differing register is overwritten. Bisection on it could return a later divergence. The locator never bisects plain equality.

The prefix digest `P(i)` (§8.3) makes `Agree(i)` a comparison of two 32-byte values: `P_A(i) = P_B(i)` ⇔ `Agree(i)`, up to BLAKE3 collisions. The digest is a fast path only. The reported first divergence is always confirmed by direct observation comparison at `i` (unequal) and `i − 1` (equal), so the result never rests on a digest alone.

### 11.3 Strategies

1. **Lockstep scan.** Both backends have `step_boundary`. Advance both one primary-stream boundary at a time and compare. The comparison ends at the first of:
   - the first primary-stream divergence `d`;
   - both sides reaching the profile's `stop_rule` with no primary divergence, so the primary stream agrees;
   - an engine or backend failure (§20).

   Auxiliary observations emitted meanwhile are compared in their own streams and recorded as evidence. With no primary divergence, the auxiliary results then decide between `Agree` and `AuxiliaryDivergence` (§20). Cost is proportional to the divergence position, and no storage or snapshot is needed. This is the default, and the exact algorithm for any backend that can step.
2. **Stream compare.** One side is post-run (Spike). Record its stream, then scan the other side in lockstep against it, with the same ending rules: the first primary divergence, the stop rule, or a failure. This is `spike::compare` generalized.
3. **Checkpoint search** (§12). Both sides have `restore`, and one of these holds:
   - the per-boundary observation is expensive (L3 state);
   - the executions come from different places or times, such as a CI failure against a local reference;
   - the run is long and replaying from the start for each question is too slow.

   It binary-searches `Agree` over recorded primary-stream prefix digests, restores each side from its own checkpoint, then scans densely inside the final window.

**Report collection.** The profile's `report_window` is a rendering concern, never a termination condition. After `d` is found, the locator may continue only as far as needed to collect up to `w` observations after `d` (and keeps up to `w` before it):
- it never goes past the stop rule;
- it never ignores a backend failure to fill the window;
- if trailing observations do not exist, it records the available portion;
- collection never changes `d` or the outcome.

### 11.4 Capability-Aware Choice

| A ∩ B capabilities | Strategy | Cost |
|---|---|---|
| `step_boundary` both | lockstep scan | O(d), where d is the divergence ordinal |
| one side post-run | stream compare | O(n) for the recorded side, O(d) for the other |
| `restore` both, and recorded checkpoints | checkpoint search + window scan | O(log k) probes + O(window) |
| no `restore` on a side | never bisect that side: replaying from the start on every probe costs O(n log n), worse than one scan | O(d) |

Snapshot-incapable backends are fully supported by strategies 1 and 2 (§28.2, Capabilities).

---

## 12. Checkpoints and Search

### 12.1 Event Boundaries vs. Comparison Boundaries

- A SystemScope checkpoint is only possible at an **event boundary** (m0-design §7). A comparison boundary is emitted by the record of some event's handler, for example the event whose handler traces `rv32.commit`.
- One event can emit more than one boundary, in the same or different streams. So a checkpoint is placed "after event `e`", and it **covers** every boundary emitted by events up to and including `e`.
- A checkpoint belongs to one side. It records, per stream, the count of boundaries covered, `c_s(e)`, and the prefix digest `P_s(c_s(e))`. Restoring it and stepping continues each stream at ordinal `c_s(e)` with the recorded prefix digest, exactly as `resume_trace` continues a trace from a prefix.
- **Search positions are comparison ordinals. Event indices are only where a checkpoint happens to be stored.** The two are never confused. The M3 every-event infrastructure indexes checkpoints by event, and M4 adds the boundary map.

### 12.2 The M4 Checkpoint

```text
Checkpoint {
    execution:      ExecutionId           // the side it belongs to (§6.6)
    event_index:    u64                   // events dispatched (SystemScope backends)
    streams:        [(Str stream, u64 covered, [u8; 32] prefix_digest)]   // in stream-name order
    state:          Bytes                 // the unchanged M0 runtime snapshot (SSSNAP, format 1)
}
```

- The `state` bytes are exactly `Runtime::snapshot()`. Restoring them is `Runtime::restore` into a freshly built platform, with every existing validation (m0-design §7, the M3 platform checks). M4 adds no snapshot format (D4).
- A checkpoint's own encoding uses the M0 §4.5 primitives. Restoring it into a side whose `ExecutionId` differs is refused.
- A checkpoint is part of the reproducer, one optional checkpoint per side (§16).

### 12.3 Search Semantics

```text
input:  primary stream p; for each side, its own checkpoints and its recorded P_p values
        probe ordinals 0 = c0 < c1 < … < ck: primary ordinals where both sides have a recorded P_p
        Agree(c0) = true;  Agree(end) = false  (a divergence is known to exist)

1. binary search over i in 0..=k for the last ci with P_A,p(ci) = P_B,p(ci)
     (monotone ⇒ correct)                         → good = ci, bad = ci+1 (or end)
2. restore each side at its own latest checkpoint covering ≤ good primary boundaries
     (or its initial state); step each side to primary ordinal good;
     scan good, good+1, … < bad in lockstep, comparing observations directly
                                                  → first divergence d
3. confirm: obs_A,p[d] ≠ obs_B,p[d] and P_A,p(d) = P_B,p(d)
```

- **Checkpoint density** is a performance choice, never a correctness one. Any density, including none at all, gives the same `d`. Recording a side's `P_p` at every primary boundary costs 32 bytes each, so probe ordinals need not coincide with checkpoints.
- **Auxiliary digests.** A checkpoint's auxiliary-stream counts and prefix digests serve diagnostics, replay validation (`BoundaryDesync`, §20), and artifact consistency. They never steer the search.
- **Same engine, same topology.** A and B share the event model, so checkpoints of the two sides at equal event indices can also compare `StateDigest`. That is an L1 accelerator for *finding* the window: equal digests at event `e` mean equal full state there. The reported divergence is still the L2 boundary. This is how an injected divergence that stays latent in state, for example in RAM that is not yet read, is found before it shows architecturally (§12.4).
- **The every-event infrastructure** (cost-balanced shards on host threads) is reused for independent per-checkpoint evaluations, such as computing both sides' prefix digests at many checkpoints in parallel. Results are combined in checkpoint order, so parallelism never changes the result.

### 12.4 Latent State Divergence

A divergence can exist in state before any compared boundary shows it. For example, a corrupted disk byte reaches RAM through DMA long before the CPU fetches it.
- **L2 first divergence** is defined by the primary observation stream alone. It is the M4 contract, and backends without `state` support it.
- **First state divergence** (same engine, `state` both) is the first event boundary, in the dense window before the L2 divergence, where the L3 state differs. It is reported as additional evidence and labeled as such. Its event index is same-engine global time (§6.4), a diagnostic only. It is searched with the monotone predicate "all state digests equal up to event `e`" inside the window, never by bisecting plain state equality.

---

## 13. Structured State Diff

### 13.1 Output

```text
component: soc.cpu0          (topology path)
field:     scause            (stable field name)
expected:  U64(13)
actual:    U64(5)
source:    inspect           (inspect | decoder:<schema> | derived)
```

### 13.2 Sources

1. **`StateView` via `WorldView::inspect`**, for every SystemScope component. Field identity is `component path / field name`, both stable strings. For the M3 CPU this gives `pc`, `x1`–`x31`, `instret`, `state`, `priv`, and the supported CSRs.
2. **Schema decoders**, where a platform has one. For `m3-reference`, the acceptance tooling's independent snapshot decoder (`tests/acceptance/src/m3/portable.rs`) reads every component's documented schema, including the process table, frame bitmap, bus transactions, and RAM pages. Decoded fields get identities `component path / schema field path` (for example `soc.kernel/procs[1].state`), with the index as part of the path.
3. **Derived fields**, such as a RAM range digest or the register file replayed from the `arch` stream. They are labeled `derived`, so they are never mistaken for component-reported state.

### 13.3 Can `StateView` Be Used As Is?

Partly. It works as the generic, UI-neutral source (Q5):
- **Stable names:** yes. They are `&'static str`.
- **Deterministic order:** yes for a given component version. Each component emits in code order, and M4 diffs by identity, not by position.
- **Complete:** no. RAM and kernel views are summaries, and `StateView` has no schema version. So an L3 diff labels every field with its source and never claims completeness. The component schema version from the snapshot container identifies the layout the decoder read.

M4 makes no change to `StateView` or to any `inspect()`. Richer views, if M7 needs them, are a future component-level decision.

### 13.4 Rules

- The diff lists fields in a deterministic order: component id order, then the source's field order.
- A field present on one side only is reported as `missing(expected)` or `missing(actual)`, never as a value difference.
- Persisted diffs are canonical (§8.3 primitives). The text rendering is a view derived from them, like the Perfetto and JSONL trace exports.

---

## 14. Trace Diff

Three classes, never merged into one universal format:

| Class | Compared | Valid for | Use |
|---|---|---|---|
| **14.1 Canonical execution trace** | the canonical record streams (L4), record by record, from the first differing record | same engine and topology only | the precise event-level mismatch: which event, which fields |
| **14.2 Normalized architectural trace** | the L2 observation windows around the divergence | any backends | the cross-backend diff, and the M4 report's core |
| **14.3 Backend diagnostic trace** | none: shown, not compared | any | Spike's log lines, later RTL waveform excerpts or native-kernel logs, attached as opaque evidence |

- The report shows a bounded window around the primary divergence, `w` boundaries before and after. It also shows a window around each auxiliary stream's divergence, each in that stream's own ordinals. The size is the profile's `w`, which every M4 profile version 1 sets to 3, `spike::compare`'s three records (§6.5).
- A raw trace is never translated into another backend's format for comparison. Normalization happens only in projections (§8.4).

---

## 15. Relevant-Predecessor Slice

M4 reports **recorded relevant predecessors** of the differing state. It does not report causes (D12). A slice is built only from links the canonical trace records.

| Link | Evidence today | Status |
|---|---|---|
| request → response on one link | `runtime.dispatch`: `source`, target, `port`, `protocol`, `msg`, `txn` | available |
| master request → bus downstream request → response back | MultiMasterBus renames transaction ids (m2-design §10.2); the mapping is in bus state (snapshot), not a trace record | available by decoding bus state at checkpoints; no trace record in M4 |
| instruction → its memory access | `rv32.commit` `addr`, `paddr`; the preceding `mem.v1` requests from the CPU port | available by ordering (the CPU has one outstanding access) |
| page walk → PTE reads | CPU walk states in `inspect` (`walk_purpose`, `walk_level`, `walk_table`) and `ReadReq`s from the CPU port between commits | available by ordering; no explicit walk-id record |
| trap → syscall | `rv32.exception` (ecall from U) → `os.gate.enter` → `os.syscall.enter` | available (m3-design §6.8 records) |
| DMA command → beats → completion | `platform.blk.command`, `platform.disk.*`, `mem.v1` writes from the DMA port, `platform.blk.done`, `platform.irq.*` | available |
| kgate `ENTER` → kernel access chain | `os.gate.enter`, kernel-port `mem.v1` records, `os.gate.release` | available |
| data dependence (which store produced a loaded value) | memory contents are not traced per byte | not available; derived by replaying stores in the window (labeled derived) |

- **Deterministic construction.** Start from the differing field at the first divergence, and follow only the links above backwards, within the dense replay window. Output the predecessor records in trace order, each tagged with the link that included it.
- **No invented links.** A relationship not in this table does not appear in a slice.
- **No new instrumentation.** M4 has no bus rename trace record. Slices use only the existing trace ordering, bus state decoded at checkpoints, and the known outstanding-transaction relationships above. M4.3 changes no component schema and no trace semantics.

---

## 16. Reproducer Bundle

### 16.1 Logical Contents and Layout (D9)

A reproducer has the following **logical contents**, frozen at M4.0:

| Logical entry | Contents |
|---|---|
| manifest | the canonical description below |
| expected-side replay material | optional checkpoint (§12.2); optional input-variant bytes |
| actual-side replay material | optional checkpoint; optional input-variant bytes |
| expected observation evidence | optional canonical primary-stream observations, expected side (below) |
| actual observation evidence | optional canonical primary-stream observations, actual side |
| verification result (`result.bin`) | the canonical `VerificationResult` (§6.7): `Diverged`, `Agree`, or `AuxiliaryDivergence` |
| predecessor slice | optional (§15) |

**Physical form.** In M4 these contents are persisted as a **canonical directory bundle** (D9). A single-file container is a possible future transport or packaging form, outside the M4 contract. If one is ever added, it must wrap exactly these logical contents and keep the §16.2 requirements: canonical bytes, an integrity hash for every entry, corruption detection, cross-OS reproducibility, and no host-local data.

The directory layout:

```text
<name>.repro/
├─ manifest.json        canonical rendering, LF, fixed key order, no paths, no timestamps
├─ expected/
│   ├─ checkpoint.bin   optional: this side's §12.2 checkpoint (its state is the unchanged SSSNAP bytes)
│   └─ input.bin        optional: this side's input-variant bytes, e.g. a disk patch
├─ actual/
│   ├─ checkpoint.bin   optional
│   └─ input.bin        optional
├─ expected.obs         optional: canonical primary-stream observation evidence, expected side
├─ actual.obs           optional: canonical primary-stream observation evidence, actual side
├─ result.bin           the canonical VerificationResult (§6.7): outcome, primary divergence if any,
│                       field diffs, auxiliary evidence, state evidence
└─ slice.bin            optional: the relevant-predecessor slice (§15)
```

**Outcome-dependent contents.** A reproducer bundle records one of three outcomes: `Diverged`, `Agree`, or `AuxiliaryDivergence`. Normal corpus entries are `Diverged` or `Agree` (§18).
- `Incomparable` and `EngineError` may produce a **diagnostic result artifact**: the canonical `VerificationResult` with whatever evidence exists. It is not a valid replayable reproducer, and it never becomes a corpus entry.
- `BackendFailed` caused by exhausting the canonical replay budget never produces a reproducer (§6.6).

| Outcome | `primary_divergence` | Observation evidence | Auxiliary evidence |
|---|---|---|---|
| `Diverged` | required: `(stream, ordinal, kind)` | mandatory: `expected.obs` and `actual.obs` (below) | in `result.bin`, if any |
| `Agree` | none | none by default | none (all streams agree) |
| `AuxiliaryDivergence` | none: the primary stream agrees to the stop rule | none for the primary stream by default | each divergent auxiliary stream's divergence and window, in `result.bin` |
| `Incomparable`, `EngineError` | diagnostic only; not a reproducer | none | none |

**Observation-evidence policy** (frozen at M4.0; `w` is the profile's report window, §6.5):
- **`Diverged`.** `expected.obs` and `actual.obs` are mandatory. Each holds that side's primary observations from ordinal `max(0, d − w)` through the divergent observation `d`, then up to `w` after `d`, collected under §11.3's rules: never past the stop rule, never across a backend failure, and only the available portion when trailing observations do not exist.
- **`Agree`.** Observation evidence is omitted by default. `result.bin` is authoritative: it holds each stream's compared count and final prefix digest (§6.7) and the stop-rule result. Neither a full stream nor a trailing window is stored.
- **`AuxiliaryDivergence`.** Primary observation evidence is omitted by default. Each divergent auxiliary stream's observations follow the same `w` rule and are held in `result.bin`. Agreeing auxiliary streams store none.

`manifest.json` records:
- the bundle format version;
- the common scenario identity: platform, CPU profile, program or disk BLAKE3s, seed;
- the comparison profile: name, version, identity, primary and auxiliary streams, stop rule;
- for each side:
  - its role;
  - its backend identity;
  - its capability set;
  - its input variant identity;
  - its canonical replay configuration (§6.6), never its local run configuration;
  - its replay origin: `initial` or `checkpoint`, with the checkpoint's covered primary ordinal;
- the outcome, and `primary_divergence`: `(stream, ordinal, kind)` for `Diverged`, absent otherwise;
- the BLAKE3 of every other entry, including `result.bin`.

A side without a checkpoint replays from its initial state. The manifest says so explicitly (`origin: initial`), and the bundle holds no checkpoint entry for that side (in the directory form, no `checkpoint.bin`).

### 16.2 Rules

- **Portable and canonical.** Every binary entry uses the M0 §4.5 primitives, fixed-width and little-endian. The JSON rendering is deterministic, as the golden files are (`the_golden_files_are_host_independent`): fixed key order, LF line endings, one fixed layout, and no host-dependent values. It never depends on JSON object or map iteration order.
- **Byte identity.** The same comparison case, comparison profile, and canonical replay configuration give byte-identical bundles on Linux and Windows (§6.6). A bundle's bytes never depend on the local run configuration used for analysis.
- **No host data.** There are no absolute paths, user names, host names, timestamps, or thread counts. Fixture references are by BLAKE3 and a repository-relative path.
- **Corruption detection.** On load, every entry must match its manifest BLAKE3. Each checkpoint must belong to its side's `ExecutionId`, and its `state` must pass `Runtime::restore`'s full validation, plus the platform checks where a decoder exists. Any failure is `CorruptReproducer` (§20), and nothing runs.
- **Replay.** Each side is rebuilt independently:

  ```text
  load the ComparisonCase from the manifest
        │
        ├─ build the expected backend: scenario + expected input variant + canonical replay config
        └─ build the actual backend:   scenario + actual input variant + canonical replay config

  for each side:
      if the side has a checkpoint:  restore it
      else:                          replay from the initial state

  run both sides under the recorded case, profile, and canonical replay configuration,
  with the §11.3 ending rules; recompute the VerificationResult
  require canonical equality with result.bin
  ```

  **The same verification result** means a byte-equal canonical `result.bin`, which implies:
  - for `Diverged`: the same primary divergence `(stream, ordinal, kind)`, the same structured diff, the same observation evidence, and equal primary prefix digests at each side's origin and at the divergence;
  - for `Agree`: both sides reach the stop rule, every compared primary and auxiliary observation satisfies the profile's equality, and the outcome stays `Agree`;
  - for `AuxiliaryDivergence`: the primary stream agrees to the stop rule, and each auxiliary divergence is the same.

  Recorded observation evidence, where present, must also be byte-equal.
- **Heterogeneous bundles.** A backend without `restore` (Spike) has no checkpoint. That side replays from its initial state, while the other side may still restore its own checkpoint.
- **Deduplication.** When both sides' checkpoints are byte-identical, one entry may serve both, as an optimization only. The manifest still records each side's origin separately.

---

## 17. Minimization

### 17.1 The Predicate

A candidate still fails **the same way** when its primary-stream first divergence has the same kind, and the same differing field identities with the same expected and actual values. The ordinal may change when the candidate's prefix changes. The predicate is deterministic because every input to it is canonical.

### 17.2 M4: Prefix and Window Minimization

- **Prefix:** the minimal failing prefix is the first divergence itself. Every boundary before it agrees, by definition.
- **Replay window:** choose, for each side that can restore, its latest checkpoint covering at most the divergence's primary ordinal, so the bundle replays the fewest events. This uses the checkpoint index only.
- Both are exact and deterministic, and neither changes the scenario.

### 17.3 Deferred Beyond M4: Scenario Reduction

Scenario reduction is not part of M4 (D11). M4.5 implements §17.2 only. Reducing the scenario is much harder. Examples are removing executables from the `SSX0` table, trimming a generated program's groups, or dropping processes. Removing a process changes scheduling and every later boundary.

A later milestone that adds it must keep these constraints:
- the §17.1 predicate, a fixed candidate order (for example, table entries in index order, then halves as in delta debugging, with no random choice), and a serialized seed if any order is seeded;
- no writes into fixture directories: candidates are built in a scratch location, and a reduced scenario becomes a repository artifact only through §18's review;
- generated programs (`progen`) first, because a program is a pure function of its seed and group list.

---

## 18. Regression Corpus

```text
failure
  ↓  cargo xtask verify locate …        (writes a bundle into a scratch directory)
reproducer bundle
  ↓  human review: the divergence is understood and intended as a regression case
corpus entry  (a canonical directory bundle under tests/verification/corpus/, committed by a normal PR)
  ↓
CI: every entry replays; its recomputed result must equal the committed result.bin byte for byte
```

- **Location and form.** The corpus is `tests/verification/corpus/`. Each entry is a canonical directory bundle (§16.1) whose outcome is `Diverged` or `Agree`. `AuxiliaryDivergence` bundles and `Incomparable` or `EngineError` diagnostic results are never entries.
- **No automatic commit or blessing.** A tool never writes into the corpus or any golden directory. The flow is scratch directory, then human review, then a normal PR. Promotion is a reviewed commit, as golden changes already are (CONTRIBUTING).
- **What an entry asserts.** Either:
  - a `Diverged` entry: "this known divergence still reproduces exactly", a regression test for the engine itself, using injected divergences; or
  - an `Agree` entry: "these two backends continue to agree under this profile", after a fix.

  Both are checked by canonical equality of `result.bin`.
- **Seeded campaigns** (D10) run the existing generators over explicit seed lists. Any divergence becomes a scratch bundle for review. The nightly seed stays printed and reproducible, as in M0 and M1.

---

## 19. The Existing Digests in M4

| Digest | Role in M4 |
|---|---|
| `StateDigest` | same-engine fast path: equal at an event boundary ⇒ identical full state there (§12.3); used to find the state window |
| `ExecutionDigest` | same-engine fast path over event history: equal at event `e` ⇒ identical dispatched-event prefix; carried inside every checkpoint's snapshot |
| `TraceDigest` | same-engine fast path over record history, for the canonical trace diff (§14.1) |
| prefix digest `P` (new, §8.3) | backend-neutral fast path over observation history (§11.2) |

A digest says *something differs*. It never says *what*. Every digest mismatch in M4 leads to localization, then to a structured observation diff:

```text
digest mismatch  →  localization (§11, §12)  →  structured observation, state, and trace diff (§13, §14)
```

No M4 verdict rests on a digest alone (§11.2 confirmation).

---

## 20. Failure Model

M4 separates **what it found** from **whether it could look**.

| Result | Meaning |
|---|---|
| `Agree` | every compared boundary of every compared stream is equal, and the profile's stop rule was reached on both sides |
| `Diverged(report)` | the primary stream diverged at a confirmed first boundary; auxiliary divergences are evidence inside the report |
| `AuxiliaryDivergence(report)` | the primary stream agrees to the stop rule, but an auxiliary stream diverged; there is no first divergence of the comparison, and each auxiliary divergence is reported in its own stream |
| `Incomparable(reason)` | the profile asks for something a side cannot report: the primary stream, a required field (§10), or a capability |
| `EngineError::BackendFailed` | a side did not reach the stop rule: a crash, a fault, or its `BackendRunConfig` safety budget exhausted |
| `EngineError::Unsupported` | a capability-gated call was made (a bug in M4; tested never to happen) |
| `EngineError::CorruptReproducer` | a bundle file does not match its manifest, or its snapshot fails restore validation |
| `EngineError::BoundaryDesync` | a checkpoint's recorded boundary counts or prefix digests do not match a replay |
| `EngineError::Nondeterministic` | the same backend, scenario, and input variant gave different streams on two runs |
| `EngineError::Internal` | an invariant of the engine itself failed |

- A SystemScope session that faults (`Lifecycle::Faulted`) is `BackendFailed`, with the fault. Whether a fault on one side is a divergence, because the other side ran on, is decided at the boundary level: the faulting side's stream ends, and that is a length divergence, reported as such. The fault itself is not called architectural.
- An engine failure is never reported as a divergence, and a divergence is never hidden by an engine failure. The result type keeps them apart.

---

## 21. Determinism Requirements

M4 inherits plan.md Principle 2 and m0-design §8.3.
- Observation is read-only (Observer, `inspect`, `snapshot`). It is AT-3's guarantee, and an M4 test repeats it for every adapter used on SystemScope.
- Comparison, search, diff, slicing, and minimization are pure functions of canonical inputs. Iteration is over ordered collections (`Vec`, `BTreeMap`). The workspace clippy rules (no `HashMap`, `HashSet`, `Instant`, `SystemTime`, or `thread::spawn`) apply to the new crate. Parallel work uses scoped threads combined in a fixed order, as the every-event sweep does.
- There is no randomized search. Any seeded order has its seed serialized in the bundle.
- Wall-clock time never enters a result. Timing output, if any, goes to stderr and is excluded from every artifact.
- **Same inputs → same verification result bytes**, on every supported host. "Same inputs" means the same comparison case, comparison profile, and canonical replay configuration (§6.6). Host-local operational limits are not inputs to any artifact.

---

## 22. Crate Layout (Q9)

```text
verification/            systemscope-verify (new, D7)
  observation, profiles, canonical encoding, prefix digest
  comparator, locator (scan, stream, checkpoint search)
  checkpoint index, bundle read/write/validate, diff and slice builders, minimizer
  SystemScope runtime adapter (generic over any Runtime and projection)
tests/rv32/              projections of rv32.* records; Spike adapter (next to the existing parsers)
tests/acceptance/        m3-reference projections for os.* and effect streams; decoder-backed state source
xtask/                   cargo xtask verify locate | diff | replay | minimize | corpus
tests/verification/      corpus/ (canonical directory bundles, §18) and engine self-tests (M4.7)
```

- **Why a new crate, not `tests/acceptance`?** The engine is a product capability that M5, M6, and M7 will use. The acceptance crate is the frozen M0–M3 harness with its golden files. Keeping them apart stops M7 from depending on test harnesses.
- **Dependencies:** exactly `systemscope-contracts`, `systemscope-runtime`, `blake3`, and `serde_json` (D7). It does not depend on `rv32i`, `platform`, `os`, `elf`, or any test crate. Platform knowledge enters only through adapters and projections.
- **`serde_json`** is used only to render and to parse and validate the canonical manifest. The rendering never relies on JSON object or map iteration order: keys are written in a fixed order, with LF line endings, one fixed layout, and no host-dependent values.
- **Mutation:** unlike `tests/acceptance` and `xtask`, the crate is not excluded from cargo-mutants. It is product code, and each M4 step keeps a targeted mutation check as M1–M3 did.
- **New dependencies:** none. `blake3` and `serde_json` are already workspace dependencies of other crates. This document does not change any `Cargo.toml`.

---

## 23. Contracts

- **No contracts change is planned** (D8). Observation values reuse `trace::Value`, encoding reuses `canonical::Encoder` and `Decoder`, state reuses `StateView` and `WorldView`, and checkpoints reuse the runtime snapshot. The pin stays `90900a128d51f6b42e86dc789447882c90c52ef4`.
- **Potential future contracts** (recorded, not part of M4):
  - a `Capability` declaration, once a second language or repository (the M7 TypeScript UI) must read backend capabilities;
  - a language-neutral observation schema (plan.md §9, "Schema / Protobuf"), once a non-Rust consumer reads M4 artifacts;
  - a trace record for bus transaction renaming, if §15 slices need explicit cross-bus links.

  Each would land in contracts first, then a pin bump, per plan.md §10.

---

## 24. Future Integration

### 24.1 M5: SystemVerilog CPU Backend

**M4 provides:**
- the `arch` stream contract and its profiles;
- the observation schema;
- the locator, with lockstep scan for a co-simulated RTL that can step to a retirement;
- replay and reproducer bundles;
- the regression corpus.

**M5 adds:**
- the SystemVerilog CPU;
- a Verilator adapter that reports retirements and traps (a retirement interface on the RTL);
- an RTL checkpoint capability, if feasible;
- the Rust ↔ RTL differential profile.

- The Rust ↔ RTL profile has `primary_stream = arch`. RTL cycle counts, internal pipeline events, and waveform time are never alignment keys. They can be attached only as auxiliary diagnostic evidence (§14.3).
- If the RTL cannot restore, M4's strategies 1 and 2 still locate divergences exactly (§11.4).
- M4 adds no RTL code, and no hardware-description tooling enters the workspace in M4.

### 24.2 M6: Native Guest OS Backend

- The comparison is of **architecturally observable behavior**. ModeledKernel's Rust state has no counterpart in a native kernel, and the internal event sequences will differ: a native kernel executes S-mode instructions where the ModeledKernel issues bus accesses.
- **Comparable:**
  - UART bytes and the shutdown reason (`effect`);
  - U-mode retirements at virtual addresses, including instruction words, register writes, and memory values (`paddr` excluded, since frame allocation may differ);
  - the register file at each return to U, replayed from the `arch` stream;
  - traps delivered from U, with cause and `tval`;
  - the syscall sequence projected from U-mode `ecall` retirements, with `a7` and `a0`–`a2` taken from the replayed registers.
- **Not comparable:** S-mode instruction streams, kernel-internal state, physical frame numbers, and `satp` roots.
- **Primary stream.** Deferred to the M6 design. M4 does not impose one: the primary stream is a profile parameter (§6.5). Candidates, as future guidance:

  | Candidate | For | Against |
  |---|---|---|
  | `effect` (UART bytes, disk writes, shutdown) | needs no process identity; directly user-visible | coarse, so a divergence may show late |
  | user-visible `arch` projection | fine-grained | U-mode retirements from several processes interleave by scheduling, which may legitimately differ |
  | a dedicated projected user stream (per process) | fine-grained and scheduling-independent | needs the process-identity mapping, deferred to M6 |

  `effect` as primary, with the user-visible projection as an auxiliary stream once it is defined, is future guidance only. The M6 design decides.
- **Deferred to M6:** how to identify a process across kernels without a PID in architectural state. It is not an M4 item. One possible approach aligns by the order of U-mode entries per address space.

### 24.3 M7: Visualizer / Interactive Debugger

M7 consumes M4 artifacts:
- the first divergence and its window;
- state diffs with their sources;
- the architectural trace diff;
- checkpoint positions on the timeline;
- relevant-predecessor slices;
- reproducer manifests;
- regression results.

Rules:
- M4's data model is designed for correctness and canonical persistence, not for a UI. Display concerns such as layout, colors, and grouping never enter observations or bundles.
- M4 writes no TypeScript and no GUI code. Step back and seek in M7 are built on the same checkpoint index (§12.2).

---

## 25. Validation Strategy

### 25.1 Controlled Divergences (No Production Change)

The set is frozen at M4.0: six cases, kept minimal, one or two per category. Each is a comparison case with distinct sides (§6.6). Its **expected first divergence is derived independently** of the locator, by a selector that is a pure function of the reference `arch` stream and the reference trace. The test tooling evaluates each selector and records the resulting ordinal at the step that implements the case. The naive oracle and the locator must both equal it. A selector that finds no match fails the test; a case is never silently re-chosen.

Common rules:
- **Workloads.** SP-1 to CB-2 use the `m3-reference` `reference` scenario (§28.1) under the `m3-reference` SystemScope profile, primary stream `arch`. The expected side is the unmodified run. SK-1 uses the existing rv32ui Spike profile.
- **"After boundary b"** means the SystemScope checkpoint after the event that emits primary observation `b`. There the CPU has just retired or trapped and is waiting to fetch (`FetchIssue`, `components/rv32i/src/cpu.rs`), so no in-flight memory plan depends on the patched field.
- **Patch validity.** A patched checkpoint must pass `Runtime::restore`'s full validation, for example schema 3's `stvec` MODE check and its check that `pa` matches `pc`. A patch that fails validation fails the test; it is never skipped.
- **Injective instructions.** `ADD`, `SUB`, `XOR`, `ADDI`, and `XORI` with `rd ≠ x0` and `rs1 ≠ x0`. For these, flipping bit 0 of `x[rs1]` always changes `rd_value`.
- **Input variant.** Each actual side records its patch in its input variant (§6.6).

| ID | Category | Target | Patch | Expected first divergence (independent derivation) |
|---|---|---|---|---|
| SP-1 | state patch: `pc` | checkpoint after boundary 0, the first `retire`: M-mode boot stub, translation off, so there is no `pa` to match | `pc ^= 0x4` | `arch[1]`, `retire`, field `pc` |
| SP-2 | state patch: GPR | `r` = the first `retire` with `priv = U` whose instruction is injective; checkpoint after boundary `r − 1` | `x[rs1] ^= 1` | `arch[r]`, `retire`, field `rd_value` |
| SP-3 | state patch: CSR | `e` = the first `exception` with cause `EnvironmentCallFromU`; checkpoint after boundary `e − 1` | `stvec ^= 0x4` (MODE stays 0) | `arch[e + 1]`, `retire`, field `pc` (the handler address). `arch[e]` agrees, because no `exception` field reads `stvec` |
| CB-1 | code-byte patch: valid word | in `disk.img`, the word at the ELF entry `e_entry` of `hello` (table entry 0), inside its `PT_LOAD` bytes; `x` = the first boundary with `priv = U` and `pc = e_entry` while `hello`'s pid is current (from `os.process.create` and `os.process.switch`) | the word becomes `0x00000013` (`addi x0, x0, 0`), or `0x00100013` if it already is that word | `arch[x]`, `retire`, field `insn` |
| CB-2 | code-byte patch: illegal word | the same word | the word becomes `0x00000000`, an illegal instruction, delegated to S by `medeleg` | `arch[x]`, kind: `retire` expected, `exception` actual |
| SK-1 | Spike-side | `rv32ui-add`; expected side Spike, origin initial; actual side SystemScope; `r` = the first `retire` in the compared window whose instruction is injective; SystemScope checkpoint after boundary `r − 1` | `x[rs1] ^= 1` on the SystemScope side | `arch[r]`, `retire`, field `rd_value` |

Why the targets are safe:
- The kernel never touches registers, CSRs, `pc`, or `priv` (m3-design §6.1), and the trampoline writes only `satp`, `sepc`, `sstatus`, `sscratch`, and the registers from the trap frame (m3-design §7.3). A patched `stvec` therefore survives until the next trap, and a patched GPR until the instruction that reads it.
- The executable table has no content hash, and the kernel validates only the ELF structure (m3-design §8). A patched instruction word therefore loads normally and shows at the first fetch.
- For CB-1 and CB-2, state differs from the initial state, because the block media holds the patched bytes, and RAM first differs at the DMA write that carries them. That is state evidence (§12.4), not an acceptance ordinal.

"Expected" and "actual" are semantic roles, not a ranking of implementations. A profile may assign them the other way around.

- **Naive oracle.** Record both complete primary streams, then return the first index at which they differ. It is the obvious algorithm, used only in tests, and the optimized locator (checkpoint search, lockstep) must equal it for every case.

### 25.2 Mutation Classes

**Engine mutation classes.** These are mutants of `systemscope-verify`, frozen at M4.0 as classes. The engine self-tests and the controlled-divergence suite must kill every mutant of every class. The exact mutants, and their count per class, are recorded at the step that adds the code they mutate, never invented in advance.

| Class | Step |
|---|---|
| first-divergence off-by-one | M4.1 |
| final observation ignored | M4.1 |
| length mismatch ignored | M4.1 |
| observation kind omitted from the digest | M4.1 |
| field payload ignored | M4.1 |
| `Absent` compared equal to zero | M4.1 |
| primary and auxiliary streams confused | M4.1 |
| observer perturbation, where testable | M4.1 |
| wrong prefix-digest continuation after restore | M4.2 |
| wrong checkpoint boundary count | M4.2 |
| nondeterministic report ordering | M4.3 |
| minimizer accepts a different divergence signature | M4.5 |

**Source mutation classes and targets.** These are mutants of production CPU and kernel code, frozen at M4.0 as classes and target functions. Each is run as a comparison case against the `m3-reference` profile: the expected side is the unmodified build, the actual side the mutated build. They are targeted checks, as in M1–M3, separate from the nightly cargo-mutants exploration (`.cargo/mutants.toml`).

| Class | Target functions | Where the mutant should show |
|---|---|---|
| ALU result | `execute_alu` (`components/rv32i/src/execute.rs`) | `retire` `rd_value` |
| control flow | `execute_control` (`components/rv32i/src/execute.rs`) | the next `retire`'s `pc` |
| load and store completion | `complete_memory` (`components/rv32i/src/memory.rs`) | `retire` `rd_value` or `mem` |
| retirement record fields | `Rv32iCpu::commit_fields` (`components/rv32i/src/cpu.rs`) | `retire` fields |
| exception delivery | `M3State::take_exception` (`components/rv32i/src/privilege.rs`) | `exception` fields, or the next `retire`'s `pc` or `priv` |
| privilege return | `M3State::sret` (`components/rv32i/src/privilege.rs`) | the next `retire`'s `pc` or `priv` |
| Sv32 translation | `sv32::step`, `sv32_translate` (`components/rv32i/src/sv32.rs`) | `retire` `paddr`, or a changed `exception` |
| syscall dispatch | `Model::syscall` (`components/os/src/procop.rs`) | a later `retire`'s `rd_value` (the result the trampoline restores), or the `os` stream |
| scheduling | `Processes::schedule_next`, `Processes::yield_current` (`components/os/src/process.rs`) | the next U-mode `retire`'s `pc`, or the `os` stream |

- **What is frozen here:** the classes and the target functions. The mutation operators are cargo-mutants' for those functions, run with `nextest`.
- **What is recorded later:** the exact mutants generated per target, and their count, at M4.7. Nothing here predicts how many cargo-mutants generates.
- **Detection.** Every generated mutant must be detected: `Diverged`, or `EngineError::BackendFailed` where the mutant stops a side before the stop rule, never `Agree`. Where it is `Diverged`, the first divergence equals the naive oracle's.
- **Equivalent mutants.** A mutant that changes no observable behavior of `m3-reference` is listed at M4.7 with the reason it is equivalent. It is never dropped silently, and it never changes a class or a target.
- **Changing a target.** Adding, removing, or renaming a class or target after M4.0 is a reviewed revision of this document. A pure rename of a target function in production code is recorded at the step that sees it.

### 25.3 Engine Self-tests

- Observation invariance (AT-3 style) for every SystemScope adapter.
- Canonical round trips: observation, checkpoint, and bundle encodings.
- Corrupted bundles: each file, each manifest field, and truncation are refused.
- Bisection correctness with forced non-monotone per-boundary equality, a divergence that reconverges, where the result must still be the first divergence.
- Cross-OS: CI emits verification results and bundles on both OSes, for the same comparison cases and canonical replay configurations, and compares them byte for byte, like `m*-golden check`.
- Replay-limit invariance: replaying a case with a larger local budget gives the same semantic result, and a bundle rewritten afterwards is byte-identical to the canonical one.

---

## 26. Architecture Risks

| Risk | Mitigation |
|---|---|
| Raw event equality mistaken for a universal differential | D1/D2: events are same-engine only; the `arch` stream is the contract; §6.2's numbers are in the design |
| Over-reliance on internal state | L2 needs no state; L3 is explanatory and labeled by source; M6 compares only observable behavior |
| The abstraction cannot accept the M5 RTL | the `Backend` interface needs only `arch_stream` + `step_boundary` or `record`; strategies without `restore` are exact; M4.6 proves it on Spike, the non-SystemScope backend available now |
| Bisection impossible without snapshots | it is never required: lockstep and stream compare are exact (§11.4) |
| Host-specific reproducers | canonical encodings, no host data, cross-OS byte equality in CI (§16.2, §28) |
| Overclaiming causality | D12: recorded predecessors only, links limited to §15's table, no cause labels |
| Flaky or non-deterministic minimizer | fixed candidate order, canonical predicate, serialized seeds, no scenario reduction in M4 (§17) |
| M4 grows into a test framework with weak product value | the exit criteria require reports and reproducers a user runs (`cargo xtask verify`), and M7 is their consumer |
| M7 UI needs contaminating the core data model | §24.3: no display fields in artifacts; the UI derives its views, as trace exporters do |
| Bisection on a non-monotone predicate returns a later divergence | §11.2: only prefix agreement is bisected, confirmed directly; a self-test forces reconvergence |
| Checkpoint metadata desynchronizes from the snapshot | recomputed on replay; `BoundaryDesync` is an engine failure, never a silent pass |
| Ordinals of different streams read as a global time order | D13: the first divergence is the primary stream's; auxiliary divergences are separate evidence; cross-stream order only from same-engine timeline evidence (§6.4) |
| A reproducer cannot rebuild a side whose input or state differs | D14: per-side input variant, run config, and replay origin; each side reconstructed independently (§16) |
| Backend budgets leak into identities or comparison semantics | D15: budgets live in per-side `BackendRunConfig`, never in any identity; exhaustion is `BackendFailed` |
| Host-local limits change artifact bytes | bundles record only the canonical replay configuration; the writer re-verifies under it (§6.6, §16) |
| A semantic option hides in `BackendRunConfig` | §6.6 rule: an option that can change the architectural stream belongs to the backend identity, input variant, scenario, or profile |
| A missing field silently compares equal to zero | explicit `ObservedValue::Absent` with its own canonical tag, in comparison and in the prefix digest (§8.3, §10) |

---

## 27. Roadmap

**Status:** M4.0 is complete, and this document is frozen. M4.1 is next. M4 as a whole is in progress, and no implementation has started.

| Step | Status | Scope | Changes |
|---|---|---|---|
| **M4.0** | complete | Design freeze: terminology, comparison model, comparison case and profiles (primary streams, stop rules), capability model, observation fields and ObservedValue canonical encoding v1, artifact logical contents and the directory bundle, evidence policy, corpus location, controlled divergences and mutation classes, acceptance workload and exit criteria | docs |
| **M4.1** | next | Observation and comparison core: `systemscope-verify` crate, observations, canonical encoding, prefix digest, comparator, SystemScope runtime adapter and `rv32.*` projection; controlled divergences on the same backend (state patch), naive oracle | code, tests |
| **M4.2** | planned | Divergence locator: lockstep scan, stream compare, checkpoint index and search, window scan; equals the naive oracle for every injection; capability-driven choice | code, tests |
| **M4.3** | planned | Structured state and trace diff: `StateView` source, m3 decoder source, derived fields; canonical and normalized trace windows; deterministic reports; relevant-predecessor slice for the recorded links | code, tests |
| **M4.4** | planned | Reproducer bundle: format, write, validate, replay; corruption tests; cross-OS byte equality in CI | code, CI |
| **M4.5** | planned | Minimization: prefix and replay window only; scenario reduction is deferred beyond M4 (§17.3) | code, tests |
| **M4.6** | planned | Differential adapter generalization: the existing Spike suites run through the M4 surface with unchanged counts (rv32ui 40, generated 64, misaligned 9, M2 CSR 20, M3 privilege 13, M3 Sv32 11); a Spike-side controlled divergence is located | code, CI |
| **M4.7** | planned | Verification campaign and regression corpus: corpus runner, promotion workflow, seeded generator campaigns, source-mutant and fault-injection validation; record the generated source-mutant list and count for every frozen §25.2 target, and document each equivalent mutant with its rationale | code, CI |
| **M4.8** | planned | Exit audit | docs |

No step implements an RTL backend, a native kernel, or any UI.

---

## 28. M4 Exit Criteria

### 28.1 Acceptance Workload

Every number below is from the repository at `fd9f5f1`.

**SystemScope ↔ SystemScope** (`tests/golden/m3-reference.json`, m3-design §12):

| Item | Value |
|---|---|
| platform, scenario, seed | `m3-reference`, `reference` (`hello`, `ping`, `pong`, `fault`, `badptr`), seed 0 |
| firmware BLAKE3 | `c94800eaf70a4196d837c51181f203b6f171d15817b0cac3a65240263c4fae9e` |
| disk BLAKE3 | `dfc505ed8cfe426b808db9144da28e603efec4030f2c22343b0567067d2f07d7` |
| events | 166,897 |
| `instret` (one `rv32.commit` per retirement) | 2,176 |
| delegated exceptions (`rv32.exception`) | 23 |
| interrupts | 0 |
| halt | `EnvironmentCallFromS` at `0x800001c0`, shutdown reason 1 |
| UART bytes | 53 |
| primary `arch` observations | 2,200: 2,176 `retire` + 23 `exception` + the halting trap (§8.2) |

M4.1's projection must reproduce exactly these per-kind counts. A mismatch is a design error to report, never a number to re-bless.

**SystemScope ↔ Spike** (`cargo xtask spike diff`, the Linux `spike` CI job): 157 comparisons, each `Agree`: rv32ui 40, generated 64, misaligned 9, M2 CSR 20, M3 privilege 13, M3 Sv32 11.

**Controlled divergences:** the six cases of §25.1. **Engine mutation classes:** the twelve of §25.2, with mutant counts recorded at their steps. **Source mutation classes:** the nine classes and their target functions in §25.2, with generated mutant counts recorded at M4.7.

**Cross-OS scope.** SP-1 to CB-2 run on both CI operating systems. SK-1 needs Spike, which CI runs only on `ubuntu-24.04`, so it replays on Linux only.

### 28.2 Criteria

- [ ] **Detection.** Each of the six controlled divergences of §25.1 is detected automatically, with `Diverged`, never `Agree` or an engine failure.
- [ ] **First divergence.** For every case, the reported first divergence (primary stream, ordinal, kind) and its differing field equal the naive oracle's and the selector-derived expectation of §25.1.
- [ ] **Agreement.** The unmodified `reference` scenario compared with itself gives `Agree`, with 2,200 compared primary observations.
- [ ] **Structured diff.** The state and trace diff at the divergence is deterministic: two runs produce byte-identical canonical reports.
- [ ] **Reproducer.** Every case yields a canonical directory bundle that rebuilds each side independently and whose replay, under its canonical replay configuration, produces a byte-identical `result.bin`. Every corrupted variant of it is refused (§25.3).
- [ ] **Minimization.** For SP-1 to CB-2, the minimized bundle's origin on each side is that side's latest checkpoint covering at most the divergence's primary ordinal, and it replays the same result.
- [ ] **Cross-OS.** For SP-1 to CB-2 and the agreement case, verification results and bundles are byte-identical on Linux and Windows in CI (§28.1).
- [ ] **Corpus.** `tests/verification/corpus/` holds at least one `Diverged` entry (SP-2) and one `Agree` entry (the agreement case), promoted by review, and CI replays every entry.
- [ ] **Capabilities.** The locator is exercised on a snapshot-capable pair (SystemScope ↔ SystemScope) and a snapshot-incapable pair (SystemScope ↔ Spike). Both return the oracle's answer.
- [ ] **Spike through M4.** The existing Spike differential suites run through the M4 comparison surface: 157 comparisons, each `Agree`, with the counts of §28.1.
- [ ] **No regression.** The M0–M3 golden files, schemas, fixtures, and contracts pin are byte-identical, and every M0–M3 CI job still passes.
- [ ] **Observation invariance.** M4 observation does not change any SystemScope execution: the digests are identical with and without the M4 adapters.
- [ ] **Mutation.** Every engine mutation class of §25.2 has at least one mutant (observer perturbation where testable), and every mutant is killed. For every source mutation class and target of §25.2, every generated mutant is detected, or listed as equivalent with its reason, as §25.2 defines.

---

## 29. Design Questions and Frozen Answers

Every answer below is frozen at M4.0.

| # | Question | Frozen answer | Status |
|---|---|---|---|
| Q1 | Is the comparison unit a runtime event or an architectural boundary? | an architectural boundary (D1); events are same-engine diagnostics | frozen |
| Q2 | How do backends with different event counts align? | by ordinal within a boundary stream (D2), never by event, tick, or cycle | frozen |
| Q3 | How is a backend without checkpoints bisected? | it is not: lockstep or stream scan is exact and cheaper (§11.4) | frozen |
| Q4 | What is a state diff's stable field identity? | `component path / field name` (inspect), `component path / schema field path` (decoder), labeled by source | frozen |
| Q5 | Can `StateView` be used as is? | yes, as the generic source, labeled incomplete; decoders add depth; no `inspect` change | frozen |
| Q6 | Is a trace normalization layer needed? | yes, as projections to observations (§8.4); raw traces are never a universal format | frozen |
| Q7 | What is the reproducer's persistence form, and does it wrap the snapshot or use its own format? | a canonical logical bundle (§16.1), persisted in M4 as a canonical directory bundle; it wraps the unchanged snapshot in an M4 checkpoint, at most one per side (D4, D9); a single-file container is a future packaging option outside the M4 contract | frozen |
| Q8 | How much minimization is in M4? | prefix and replay window only (M4.5); scenario reduction is deferred beyond M4 (D11, §17.3) | frozen |
| Q9 | Is verification code a production crate or a test/tool crate? | a product tooling crate, `systemscope-verify`, not a simulation component (D7) | frozen |
| Q10 | Is it possible without a contracts change? | yes (D8) | frozen |
| Q11 | Can M5's RTL adapter be added with minimal M4 change? | yes if it implements `Backend` with `arch_stream` and `step_boundary` or `record`; the `arch` profile may gain RTL-specific exclusions | frozen |
| Q12 | Can M6 compare meaningfully although internal state differs? | yes, on the `effect` stream and the user-visible projection (§24.2); the M6 primary stream and process identity across kernels are deferred to M6 | frozen for M4; details deferred to M6 |
| Q13 | How is the first divergence defined when several streams are compared? | by the profile's primary stream; other streams are auxiliary evidence (D13, §6.4) | frozen |
| Q14 | Can each side have a distinct replay origin, input, and checkpoint? | yes; a reproducer must reconstruct each side independently (D14, §6.6, §16) | frozen |
| Q15 | Is the execution budget part of the scenario identity? | no; it is per-side `BackendRunConfig`, a watchdog (D15, §6.6) | frozen |
| Q16 | Can operational replay limits change artifact bytes? | no; reproducers use the canonical replay configuration, and host-local analysis limits are not persisted as artifact state (D15, §6.6) | frozen |
| Q17 | How is an unavailable field encoded? | `ObservedValue::Absent`, distinct from every `Present(Value)`; ObservedValue canonical encoding v1: `0x00` `Absent`, `0x01` `Present`, every other tag rejected (D5, §8.3) | frozen |
| Q18 | What are the acceptance workload, the controlled divergences, and the counts? | §28.1 and §25.1: the `m3-reference` `reference` scenario with the golden's numbers, six controlled divergences, the 157 existing Spike comparisons, twelve engine mutation classes, nine source mutation classes with their target functions (§25.2) | frozen; generated mutant counts recorded at their steps |
| Q19 | Where is the corpus, and what can be an entry? | `tests/verification/corpus/`; canonical directory bundles, `Diverged` or `Agree` only; no tool writes to it; scratch, then review, then a normal PR (§18) | frozen |
| Q20 | Is `paddr` compared? | per profile: compared for SystemScope ↔ SystemScope on the same platform; not compared for SystemScope ↔ Spike, as today; excluded for Modeled OS ↔ Native OS (§8.2) | frozen |
| Q21 | Does `interrupt` join the Spike profiles? | not the existing versions; only a new, versioned profile may add it (§8.2) | frozen |
| Q22 | Does M4 add a bus rename trace record? | no; slices use existing ordering, checkpoint bus-state decoding, and known outstanding-transaction relationships; M4.3 changes no component schema or trace semantics (§15) | frozen |
| Q23 | How far can a trace-backed backend take part? | `outcome`, the `arch` observation stream, and stream compare; no state, snapshot, restore, step back, or same-engine event diff; unmet requirements give `Incomparable` (§7) | frozen |
| Q24 | What observation evidence does each outcome store? | `Diverged`: `w` before, the divergent observation, up to `w` after; `Agree`: none, `result.bin` is authoritative; `AuxiliaryDivergence`: the divergent auxiliary streams only (§16.1) | frozen |

**Deferred beyond M4.0.** These are future design questions. None blocks M4.1.
- Process identity across modeled and native kernels, and the M6 primary stream: deferred to M6 (§24.2).
- A single-file container for reproducers: a future transport or packaging option outside the M4 contract (§16.1).
- Scenario reduction: deferred beyond M4 (§17.3).
- A bus rename trace record: a future component and contracts decision (§15, §23).
- `interrupt` boundaries in a Spike profile: only in a new profile version (§8.2).
- A contracts-level `Capability` and a language-neutral observation schema (§23).
