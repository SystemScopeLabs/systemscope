# M0 Design: Deterministic Simulation Kernel

> Status: Draft · Parent: [plan.md](../plan.md)

M0 builds the skeleton that every later milestone plugs into:

```text
Tick → Event → EventQueue → Component → Port/Protocol → State → Trace
```

M0 contains no CPU, no OS, and no UI. It is done when a small multi-clock, multi-fidelity toy system runs deterministically, when snapshot and restore work without changing the result, and when observing a run never changes it. Three acceptance tests enforce all of this in CI (§9).

---

## 1. Goals and Non-goals

### Goals

- Define the contract types for time, events, components, ports/protocols, snapshots, and observation.
- Implement a single-threaded, deterministic discrete-event simulation (DES) runtime on top of those contracts.
- Validate the contracts with toy components that mix clock domains and fidelities.
- Export traces viewable in the Perfetto UI.
- Enforce determinism mechanically in CI: acceptance tests plus lints.

### Non-goals (M0)

- Real ISA models (RV32I is M1).
- Host-parallel simulation.
- Dynamic topology changes after elaboration.
- Snapshot schema migration. A version mismatch is an error.
- A custom visualizer (M4).
- Real-trace ingestion.

---

## 2. Repository and Crate Layout

```text
contracts/                        (repo: SystemScope/contracts)
└─ crates/systemscope-contracts/
   ├─ time.rs        Tick, SimulationClock, Duration, Frequency, ClockDomain
   ├─ event.rs       Phase, EventKey, ScheduleWhen
   ├─ error.rs       SimError
   ├─ component.rs   Component, InitContext, SimContext, PortSpec, ComponentId
   ├─ topology.rs    LinkLatency
   ├─ protocol/      ProtocolId, closed Message enum
   │  └─ mem.rs      mem.v0 messages
   ├─ snapshot.rs    SnapshotWriter/Reader, schema versioning
   └─ trace.rs       TraceRecord, Value, Observer, WorldView

systemscope/                      (repo: SystemScope/systemscope, this repository)
├─ runtime/                       systemscope-runtime: scheduler, TopologyBuilder and elaboration, lifecycle, snapshots, sinks
├─ components/toy/                systemscope-toy: ToyCpu, ToyDma, ToyBus, ToyMemory
├─ tests/acceptance/              AT-1, AT-2, AT-3
├─ tests/golden/                  golden digests and snapshots
├─ xtask/                         bless, perfetto export, CI helpers
├─ clippy.toml                    determinism lints (§8.3)
├─ rust-toolchain.toml            pinned toolchain
└─ .github/workflows/ci.yml
```

**Components depend only on `systemscope-contracts`, never on the runtime.** The runtime reaches components through the `Component` trait. Components reach the runtime only through the `SimContext` trait. This is what makes backends replaceable (Principle 6).

---

## 3. Time Model

### 3.1 Tick and SimulationClock

```rust
pub struct Tick(pub u64);

pub struct SimulationClock {
    ticks_per_second: u64,   // fixed for the lifetime of a session
}
```

- The resolution is chosen at session start and is never hard-coded. The default is `1_000_000_000_000` (1 tick = 1 ps).
- The maximum simulated time is `u64::MAX / ticks_per_second`, about 213 days at 1 ps. Exceeding it raises `TimeError::TimeOverflow`; time never wraps. The event layer wraps `TimeError` in `SimError`.
- Floating point is not used anywhere in time computation.

### 3.2 Duration

```rust
pub struct Duration { femtoseconds: u128 }   // physical time, independent of tick resolution
// constructors: Duration::from_fs(1), from_ps(333), from_ns(50), from_us(100), from_ms(1), from_s(1)
```

Converting a `Duration` to ticks **rounds up**, so a latency is never shortened:

```text
ticks(d) = ceil(d.femtoseconds × ticks_per_second / 10^15)
```

### 3.3 ClockDomain

```rust
pub struct Frequency { num: u64, den: u64 }  // exactly num / den Hz; both non-zero

impl ClockDomain {
    pub fn new(
        clock: &SimulationClock,             // period is derived from the session resolution
        id: ClockDomainId,
        frequency: Frequency,
        offset: Tick,                        // tick of edge 0
        edge_rounding: Rounding,             // Floor (default) | Ceil
    ) -> Result<ClockDomain, TimeError>;
}
```

Edge *n* is always computed **absolutely**, never by accumulating a rounded period:

```text
edge(n) = offset + round( n × ticks_per_second × den / num )
```

- `next_edge_index(t)` is the smallest `n` such that `edge(n) ≥ t`.
- `Cycles { domain, k }` scheduled at `now` resolves to `edge(next_edge_index(now) + k)`. `k = 0` aligns to the next edge.

Worked example, 3 GHz at 1 ps resolution:

```text
edge(n) = floor(n × 1000 / 3)   →  0, 333, 666, 1000, 1333, 1666, 2000, …
edge(3 × 10^9) = 10^12 exactly  →  no drift, however long the run
```

### 3.4 Arithmetic and Limits

- **Period representation.** At construction, the period in ticks, `ticks_per_second × den / num`, is reduced to `P / Q` by their GCD. `Q ≤ num` always fits in `u64`. `P` can need up to 128 bits and is stored as `u128`. No valid clock is rejected because of storage width.
- **Bounded products.** Edges are computed as `n × ⌊P/Q⌋ + round(n × (P mod Q) / Q)`. The second product is below `2^64 × 2^64` and always fits in `u128`. The first may overflow, and then the edge would not fit in a tick anyway.
- **Checked arithmetic.** Every multiplication and addition in the time model is checked. Any overflow yields `TimeError::TimeOverflow`, exactly when the true result does not fit in a `u64` tick. Nothing wraps or truncates.
- **`FrequencyAboveResolution`.** A clock whose adjacent edges cannot be distinguished at the chosen session resolution is rejected. This happens when its period is shorter than one tick (`ticks_per_second × den < num`). It is not a limit on fast clocks as such: choosing a finer resolution admits them.

Components only ever speak in cycles or `Duration`. **Only the runtime projects them onto ticks.**

---

## 4. Event Model

### 4.1 Phase and EventKey

```rust
#[repr(u8)]
pub enum Phase { Request = 0, Transfer = 1, Complete = 2, Commit = 3, Observe = 4 }

pub struct EventKey { tick: Tick, phase: Phase, sequence: u64 }   // ordered lexicographically
```

| Phase | Intended use |
|---|---|
| `Request` | initiators start transactions |
| `Transfer` | links and interconnects move and arbitrate messages |
| `Complete` | targets finish transactions and send responses |
| `Commit` | architecturally visible state updates, such as retirement and register writes |
| `Observe` | runtime only: observers read state; no mutation is possible |

`sequence` comes from a single global `u64` counter. The runtime increments it every time an event is scheduled. **Components never choose it.** The counter is part of the simulation state and is included in snapshots. It never wraps; exhausting it raises `SimError::SequenceOverflow` (rule S6).

### 4.2 Scheduling API

```rust
// Component-facing: never expressed in ticks.
pub enum ScheduleWhen {
    Now,
    After(Duration),
    Cycles { domain: ClockDomainId, k: u64 },
}

// on InitContext and SimContext:
fn send(&mut self, port: PortId, msg: Message, when: ScheduleWhen, phase: Phase) -> Result<(), SimError>;
fn wake_self(&mut self, when: ScheduleWhen, phase: Phase, token: u64) -> Result<(), SimError>;
```

- **Components never handle ticks.** `ScheduleWhen` has no tick variant, and contexts expose no clock or domain objects. A component that wrote a raw tick count would silently change meaning whenever the session resolution changed.
- **Only the runtime resolves `ScheduleWhen` to an absolute `Tick`**, then adds link latency (§6). Absolute ticks exist only inside the runtime and its tests.
- **The runtime stamps every event with its source.** `send()` and `wake_self()` take no source argument; the context attaches the `ComponentId` of the component that is running. Components cannot forge a source.

### 4.3 Scheduling Rules

| Rule | Statement | On violation |
|---|---|---|
| **S1** | The target tick is ≥ `now`. `ScheduleWhen` delays are unsigned, so this holds by construction for them; runtime-internal absolute ticks are checked. | `SimError::PastTick` |
| **S2** | If the target tick equals `now`, the target phase must be ≥ the current phase. Phases are monotonic within a tick. | `SimError::PhaseViolation`, a fatal error with a diagnostic |
| **S3** | Components cannot schedule into `Observe`. | `SimError::PhaseViolation` |
| **S4** | `sequence` is assigned by the runtime in scheduling order. | — |
| **S5** | At most `max_events_per_phase` events (default 1,000,000) run in one `(tick, phase)`. | `SimError::SameTickLivelock` |
| **S6** | `sequence` is checked-incremented and never wraps. | `SimError::SequenceOverflow` |

Work that needs an earlier phase must move to `tick + 1` or later.

**Every scheduler operation is atomic.** A failed `schedule` or `pop` leaves the queue, sequence counter, and S5 counter exactly as they were. A handler is *not* atomic: if it schedules three events and the fourth fails, the first three stay queued. This is acceptable because any error puts the runtime into `Faulted` (§5.1). A faulted session may still be inspected, partial effects included, but it is never treated as resumable simulation state.

### 4.4 Runtime Loop

```text
loop:
    ev = queue.pop_min()                        // by EventKey
    if ev.key.tick > run_until: push back; stop
    last_dispatched = ev.key                    // now and current phase derive from it
    execution_digest = H(execution_digest ‖ canonical(ev))
    target.handle_event(ev, ctx)                // ctx enforces S1–S6
    observers.on_after_dispatch(ev, world)      // read-only; may request Pause
    if an observe point is due and no simulation events remain before Observe at this tick:
        run observers' on_observe (read-only)
```

`H` is BLAKE3. The 32-byte `execution_digest` is simulation state and is included in snapshots. `canonical(ev)` is defined in §4.5.

### 4.5 Canonical Encoding

`ExecutionDigest` and `topology_hash` depend on byte-exact encodings, so the encoding is specified here and does not depend on any serialization library.

**Primitive rules**

| Type | Encoding |
|---|---|
| unsigned/signed integers | fixed width, little-endian (`u8`, `u16`, `u32`, `u64`, `i64`) |
| `bool` | `u8`: `0` or `1` |
| byte strings, `Vec<u8>` | `u32` length, then the bytes |
| strings | UTF-8 bytes, encoded as a byte string |
| enums | `u8` variant tag in declaration order, then the variant's fields |
| structs | fields in declaration order, with no padding and no field names |
| sequences | `u32` element count, then each element |

**`canonical(ev)`** is the concatenation of:

```text
tick            u64
phase           u8
sequence        u64
source          u32   ComponentId of the scheduling component (u32::MAX for the runtime)
target          u32   ComponentId
delivery tag    u8    0 = Message, 1 = Wake
Message:  port u16 · protocol name (string) · protocol version u16 · payload (message enum, primitive rules)
Wake:     token u64
```

For example, `MemMsg::ReadReq { txn, addr, len }` encodes as tag `0`, then `txn` as `u64`, `addr` as `u64`, and `len` as `u32`.

Any change to this encoding, or to a protocol message's field layout, changes every digest. It therefore requires a bump of the protocol version or of `format_version`, and a golden re-bless.

---

## 5. Component Contract

```rust
pub trait Component {
    fn type_name(&self) -> &'static str;
    fn ports(&self) -> Vec<PortSpec>;
    fn init(&mut self, ctx: &mut dyn InitContext) -> Result<(), SimError>;
    fn handle_event(&mut self, ev: &Delivered, ctx: &mut dyn SimContext) -> Result<(), SimError>;

    fn snapshot_schema_version(&self) -> u32;
    fn snapshot(&self, w: &mut SnapshotWriter) -> Result<(), SimError>;
    fn restore(&mut self, r: &mut SnapshotReader, schema_version: u32) -> Result<(), RestoreError>;

    fn inspect(&self) -> StateView;   // read-only view for observers and the State Inspector
}

pub enum Delivered {
    Message { port: PortId, msg: Message },
    Wake { token: u64 },
}
```

Two contexts give a component everything it may touch:

| | `InitContext` (in `init`) | `SimContext` (in `handle_event`) |
|---|---|---|
| `send()`, `wake_self()` | yes, relative to tick 0; any phase except `Observe` | yes, relative to now; subject to S1–S6 |
| `now()`, `phase()` | no: nothing has been dispatched yet | yes, for diagnostics and traces |
| `rng()` | yes | yes |
| `trace(record)` | yes | yes |
| clocks, clock domains, other components | no | no |

**Components never reach each other directly.** The runtime owns every component (`Vec<Box<dyn Component>>`), and no context offers a way to reach another component. A direct call therefore cannot be written, not merely forbidden. All interaction goes through the event queue: `send()` enqueues and returns, the sending handler finishes, and the runtime dispatches the receiver later. This holds even for a zero-latency link, so there is no reentrancy.

**Errors are sticky.** A context remembers the first error it returned. If the component swallows that error and returns `Ok`, the runtime still faults.

- **There is no global `step()`.** A clocked component models a clock step by waking itself on its domain's edges (`wake_self(Cycles { domain, k: 1 }, …)`). Idle components cost nothing.
- **Randomness comes only from `ctx.rng()`** (§5.2). Components hold no RNG of their own.
- **`ComponentId`s are assigned in topology declaration order.** `component_path` is a stable, human-readable name such as `soc.cpu0`. The declaration order itself must be deterministic (§6).

> **Component invariant.** A component cannot see other components and never handles physical ticks. Every interaction happens through events whose time and source the runtime assigns.

### 5.1 Lifecycle

```text
New session:      build topology → elaborate → init (ComponentId order) → Ready → run
Restored session: build topology → elaborate → restore                  → Ready → run
                                                  ↑ init is never called
Any error (elaboration, init, handler, scheduler)                        → Faulted
```

| State | `init` | `run` / `step` | `snapshot` | `inspect` |
|---|---|---|---|---|
| `Elaborated` | once | no | no | yes |
| `Ready` | no | yes | yes | yes |
| `Faulted` | no | no | no | yes |

- **Initialization order is fixed.** Components are initialized in `ComponentId` order, one at a time. Initial events therefore receive deterministic sequence numbers.
- **Restore never calls `init`.** Initial events already live in the restored queue. Calling `init` again would duplicate them and shift every later sequence number, breaking replay.
- **`Faulted` is terminal.** It records the error that caused it. `run` and `step` are refused, and `snapshot` is refused because a snapshot must be a resumable checkpoint. `inspect()` stays available for post-mortem diagnosis: it may show partially applied effects, which is why the state is never resumed.
  - An init failure faults the session even though earlier components already initialized. Handler failures work the same way.
  - A post-mortem `fault_dump()` that is explicitly *not* resumable may be added later. It is out of scope for M0.
- **Reset means a new session.** A handler may have mutated its component before failing, so clearing the scheduler is not enough. Reset discards the runtime and builds a new one from the same topology and seed: `build topology → elaborate → init`.

### 5.2 Randomness

Randomness is simulation infrastructure, not a component feature. The contract fixes the interface and every derived value; the runtime owns one RNG state per component.

```rust
// contracts
pub trait SimRng {
    fn next_u64(&mut self) -> u64;                                   // 64 uniform bits
    fn below(&mut self, n: NonZeroU64) -> u64 { /* fixed algorithm */ } // uniform in 0..n
    fn chance(&mut self, num: u64, den: NonZeroU64) -> bool { self.below(den) < num }
}
// on InitContext and SimContext:
fn rng(&mut self) -> &mut dyn SimRng;
```

| Aspect | Rule |
|---|---|
| Generator | xoshiro256\*\* (Blackman and Vigna), 256-bit state `s[0..4]`, output `rotl(s[1] × 5, 7) × 9` |
| Seed derivation | `blake3::derive_key("SystemScope 2026-09 SimRng v1", session_seed as u64 LE ‖ component_path as u32 length + UTF-8)`; the 32 bytes become `s[0..4]` as little-endian `u64`s. Rust's `Hash`/`DefaultHasher` is never used. |
| All-zero state | Invalid for xoshiro. A derived all-zero key is replaced by `s = [1, 0, 0, 0]`; a snapshot holding an all-zero state is rejected. |
| `below(n)` | Rejection sampling: `t = n.wrapping_neg() % n`; draw `x` until `x ≥ t`; return `x % n`. Exactly as written, so the number of draws consumed is part of the contract. `n = 0` is unrepresentable (`NonZeroU64`); `n = 1` returns 0 and consumes one draw; `n = u64::MAX` rejects only the draw `0`. |
| `chance(num, den)` | `below(den) < num`. Consumes exactly the draws of `below(den)` even when the result is certain (`num = 0` or `num ≥ den`). |
| Ownership | The runtime stores one state per `ComponentId`. Components never store or copy RNG state. |
| Independence | Each component has its own stream. Draws by one component never change another component's values. |
| Availability | `InitContext` and `SimContext` only. Observers get no RNG. |
| Snapshot | The runtime snapshot contains every component's 256-bit state. After restore, the next draw of every component equals the draw the uninterrupted run would have made. |

The context string is part of the contract; it follows BLAKE3's recommended `[application] [date] [purpose]` form. Golden tests pin the context string, the exact seed-material bytes, the derived key, and the first draws. Changing any rule in this table changes every digest and requires a new context string and a golden re-bless.

---

## 6. Ports, Protocols, and Topology

```rust
pub struct PortSpec   { name: &'static str, protocol: ProtocolId, role: Role }   // Role: Initiator | Target
pub struct ProtocolId { name: &'static str, version: u16 }                        // e.g. ("mem", 0)
pub struct Link       { a: PortRef, b: PortRef, latency: Option<LinkLatency> }    // Duration or Cycles
```

A `TopologySpec` is an **ordered** list of component declarations followed by an ordered list of links:

- **Construction must be deterministic.** It must not be built by iterating a `HashMap`/`HashSet`, a directory listing, environment variables, or anything else whose order is not defined by the code or input file itself.
- **Declaration order is part of the topology's identity.** Reordering components or links produces a different `topology_hash`, even if the resulting graph is the same.
- `component_path`s must be unique. A duplicate is an elaboration error.
- `topology_hash` is BLAKE3 over the canonical encoding (§4.5) of the ordered spec: each component's `component_path`, `type_name`, and ports, then each link.

Elaboration runs once before `init` and validates the topology:

- Each link connects an `Initiator` to a `Target` of the **same protocol name and version**.
- Each port has at most one link in M0.
- Every declared port is connected.
- The topology is immutable after elaboration.
- Its canonical hash (`topology_hash`) is stored in the trace header and in every snapshot.

A message sent on a port is delivered to the peer port after the link latency, if there is one. The runtime resolves `ScheduleWhen` from now, then adds the latency from that tick (`Duration` rounded up, or `Cycles` counted from the next edge). It is delivered in the phase the sender requested, subject to rule S2 at the final tick. The message's protocol must match the sending port's protocol, otherwise the send fails with `SimError::ProtocolMismatch`.

### `mem.v0` (the only M0 protocol)

```rust
pub enum MemMsg {
    ReadReq   { txn: TxnId, addr: u64, len: u32 },
    ReadResp  { txn: TxnId, data: Vec<u8> },
    WriteReq  { txn: TxnId, addr: u64, data: Vec<u8> },
    WriteResp { txn: TxnId },
}
```

Each initiator allocates `TxnId`s from its own counter, so they are deterministic and part of its snapshot.

---

## 7. State and Snapshots

```rust
pub struct RuntimeSnapshot {
    format_version: u32,
    session: SessionInfo,          // seed, ticks_per_second, clock domains, topology_hash, contracts version
    last_dispatched: Option<EventKey>,    // now and current phase derive from it
    dispatched_in_phase: u64,             // S5 count within last_dispatched's (tick, phase)
    next_sequence: u64,
    execution_digest: [u8; 32],
    queue: Vec<Event>,             // sorted by EventKey
    rng_states: Vec<[u64; 4]>,            // xoshiro256** state per component, in ComponentId order
    components: Vec<ComponentSnapshot>,   // { id, schema_version, bytes }, in ComponentId order
}
```

- **Snapshots may be taken at any event boundary**, including mid-tick between phases. `last_dispatched` and `dispatched_in_phase` record exactly where the run was, including S5 progress.
- **The queue is exported sorted by key and may be restored in any order.** Keys are unique, so dispatch order never depends on queue internals.
- **Restore rejects scheduler states no valid run could produce:** duplicate sequences, sequences not below `next_sequence`, pending events at or before `last_dispatched`, pending `Observe` events, or a `dispatched_in_phase` inconsistent with `last_dispatched` or the S5 limit.
- **`max_events_per_phase` is session configuration**, not snapshot state.
- **Encoding is canonical and follows the primitive rules of §4.5.** The same logical state always produces the same bytes. That rules out `HashMap` iteration, floats, and pointer-dependent ordering. Maps are `BTreeMap` or sorted `Vec`s.
- **Restore requires the same `topology_hash` and, for every component, the same `snapshot_schema_version`.** Otherwise it fails with `RestoreError::TopologyMismatch` or `RestoreError::SchemaVersion`. Migrations are out of scope for M0.
- **Observer state and pending observe points are not simulation state.** They are never included in snapshots or digests.
- **`StateDigest = BLAKE3(encode(RuntimeSnapshot))`.**
- **Round-trip law:** `encode(restore(decode(encode(s)))) == encode(s)`.

---

## 8. Observation and Determinism

### 8.1 Trace

```rust
pub struct TraceRecord { key: EventKey, component: ComponentId, kind: &'static str, fields: Vec<(&'static str, Value)> }
pub enum Value { U64(u64), I64(i64), Bool(bool), Str(String), Bytes(Vec<u8>) }   // no floats
```

- **Emission is write-only.** `ctx.trace()` returns `()`, and there is no API to ask whether tracing is on. Components therefore cannot branch on it.
- **The trace header carries** the format version, `ticks_per_second`, clock domains, topology, seed, and contracts version.
- **Two sinks:**
  - **Canonical JSONL** holds exact integer ticks. It is used for digests and replay tooling.
  - **Perfetto** uses the Chrome JSON Trace Event format in M0. It has one track per component. Each `mem.v0` transaction is an async slice from request to response. Timestamps are converted from ticks to microseconds with exact decimal formatting. This conversion is for display only; the canonical sink remains the source of truth.

### 8.2 Observer

```rust
pub trait Observer {
    fn on_after_dispatch(&mut self, ev: &EventView, world: &WorldView) -> Control;
    fn on_observe(&mut self, now: Tick, world: &WorldView) -> Control;
    fn on_trace(&mut self, rec: &TraceRecord);
}
pub enum Control { Continue, Pause }
```

- **`on_after_dispatch` runs after `handle_event` returns**, so it sees the state the event produced. M0 has no pre-dispatch hook. If breakpoints that stop *before* an event are needed later, they will be added as a separate `on_before_dispatch`, not by changing this one.
- **`WorldView` exposes only shared references** (`&dyn Component` and `inspect()`). The Rust borrow checker therefore makes state mutation in `Observe` impossible at compile time.
- **Observe points are kept outside the simulation queue.** Observers request them with `runtime.observe_at(tick)`, and they are held in a separate `BTreeSet<Tick>`. They never consume `sequence` numbers and never enter the execution digest.
- **`Pause` returns control to the driver at an event boundary.** Resuming continues exactly as if no pause had happened.

### 8.3 Determinism Rules and Enforcement

| Rule | Enforcement |
|---|---|
| No wall-clock time (`Instant::now`, `SystemTime::now`) in runtime or components | clippy `disallowed-methods` |
| No `HashMap`/`HashSet` in runtime or components | clippy `disallowed-types` |
| No threads in M0 (`std::thread::spawn`) | clippy `disallowed-methods` |
| No floating point in contracts, runtime, or components | `#![deny(clippy::float_arithmetic)]` |
| Randomness only through `ctx.rng()` | review, plus the absence of any `rand` crate in component dependencies (dev-only oracles excepted) |
| Pinned compiler | `rust-toolchain.toml` |

CI runs clippy with `-D warnings`.

---

## 9. Deterministic CI Acceptance Tests

### 9.1 Reference Scenario `m0-reference`

```text
ToyCpu  (clock "cpu", 3 GHz  = 3_000_000_000/1)  ──mem.v0──┐
                                                           ├─▶ ToyBus (clock "bus", 1 GHz) ──mem.v0──▶ ToyMemory (F1)
ToyDma  (clock "io",  1.5 GHz = 3_000_000_000/2) ──mem.v0──┘
```

- **ToyCpu** issues a seeded stream of reads and writes, with at most 4 outstanding and random think-time in cycles. It checks read data against a shadow copy and folds the results into a checksum register in `Commit`.
- **ToyDma** issues seeded write bursts.
- **ToyBus** arbitrates its two initiator ports in `Transfer`. It is designed to hit same-tick collisions, so it exercises the phase and sequence ordering rules.
- **ToyMemory** has fixed latencies, `After(50 ns)` for reads and `After(30 ns)` for writes, and responds in `Complete`.

The clock mix is chosen on purpose. 3 GHz has a period that is not a whole number of ticks. 1.5 GHz exercises `freq_den ≠ 1`. `Duration`-based latency exercises cross-fidelity conversion.

- **Seeds:**
  - **Fixed seeds** `{0, 1, 0xDEADBEEF}` run on every CI run. Only these have golden digests.
  - **One random seed** runs nightly. It is printed so failures can be reproduced. It has no golden digest and is **never compared against committed golden files**. It is only checked for reproducibility (AT-1 step 1), for snapshot/restore equivalence (AT-2 steps 1–3), and for observation invariance (AT-3).
- **Run length:** until both initiators finish 100,000 operations, or `T_end = 10 ms` of simulated time, whichever comes first.

### 9.2 Digests

| Digest | Definition |
|---|---|
| `StateDigest` | BLAKE3 of the encoded final `RuntimeSnapshot` |
| `ExecutionDigest` | chained BLAKE3 over every dispatched event (§4.4) |
| `TraceDigest` | BLAKE3 of the canonical JSONL trace, when tracing is on |

### AT-1: Reproducibility

*The same inputs always produce the same result, across runs, processes, and operating systems.*

1. For each seed, run the scenario twice in **separate processes**. All three digests must be equal.
2. For the **fixed seeds only**, the CI matrix covers **`ubuntu-latest` and `windows-latest`**. Their digests must be identical to each other and to `tests/golden/m0-reference.json`. The nightly random seed skips this step and is checked only with step 1.
3. Different seeds must produce different `ExecutionDigest`s. This checks that the seed is actually used.

Golden files change only through `cargo xtask bless`. The commit that does so must explain why in its body.

### AT-2: Snapshot/Restore Equivalence

*Checkpointing a run and resuming it produces exactly the same result as never stopping.*

1. **Reference run.** Run uninterrupted to the end and record the digests as `D`.
2. **Checkpoint runs.** The checkpoints are:
   - the first event
   - a tick boundary
   - **mid-tick between `Complete` and `Commit`**
   - mid-phase, with several events left at the same `(tick, phase)`
   - 50% of events
   - the last event before the end
   - 8 seeded random event indices

   At each checkpoint:
   1. Snapshot the run.
   2. Encode the snapshot to bytes.
   3. **Drop the runtime entirely.**
   4. Elaborate a fresh runtime from the same topology.
   5. Decode and restore the snapshot.
   6. Run to the end.

   `StateDigest` and `ExecutionDigest` must equal `D`. The trace prefix plus the resumed suffix must equal the reference `TraceDigest`.
3. **Round-trip law.** At every checkpoint, check that `encode(restore(decode(bytes))) == bytes`.
4. **Portability.** `tests/golden/m0-reference.mid.snap` is a committed snapshot. Restoring it and running to the end must match the golden digests on both CI operating systems.
5. **Negative cases.** A changed `snapshot_schema_version` must fail with `RestoreError::SchemaVersion`. A changed topology must fail with `RestoreError::TopologyMismatch`.

### AT-3: Observation Invariance

*Watching a run never changes it.*

| Config | Observation |
|---|---|
| O0 | no observers, no sinks |
| O1 | canonical JSONL sink and Perfetto sink |
| O2 | breakpoint observer pausing on every `ReadResp`; the driver resumes immediately |
| O3 | single-step, one event per `run` call, until the end |
| O4 | `Observe` probe every 1,000 ticks, calling `inspect()` on every component |
| O5 | O1 + O2 + O3 + O4 |

- Every configuration must produce the same `StateDigest` and `ExecutionDigest`.
- O1 and O5 must produce the same `TraceDigest`.
- Registering observe points must leave `next_sequence` unchanged. This is asserted directly.
- **Compile-fail test** (`trybuild`): code that tries to get `&mut` component state from `WorldView` must not compile.

### Supporting Unit and Property Tests

These are not acceptance gates, but they are required for M0 exit.

- **Clock math:** `edge(3·10^9) == 10^12` at 3 GHz; `edge` is strictly increasing; `next_edge_index` is minimal, including just before, at, and just after edges (property test). Edges, durations, and overflow errors match an exact 256-bit oracle across the full `u64` parameter range; `FrequencyAboveResolution` is returned exactly when `ticks_per_second × den < num`.
- **Duration:** ceiling conversion, zero duration, and overflow into `TimeOverflow`.
- **Scheduling:** an S1 violation raises `PastTick`, an S2 violation raises `PhaseViolation`, scheduling into `Observe` is rejected, the S5 guard allows exactly `max_events_per_phase` events per `(tick, phase)` and then triggers `SameTickLivelock`, and starting the counter at `u64::MAX` triggers `SequenceOverflow`. Every schedule/pop outcome, including errors, matches a naive linear-scan reference model (property test). Same `(tick, phase)` events dispatch in insertion order, and restoring a snapshot from an arbitrarily permuted queue resumes identically.
- **Elaboration:** protocol or version mismatches, `Initiator`↔`Initiator` links, unconnected ports, and duplicate `component_path`s are all rejected. Building the same spec twice yields the same `topology_hash`; reordering two declarations changes it.
- **Canonical encoding:** golden byte vectors for `canonical(ev)` for each `MemMsg` variant and for `Wake`.

---

## 10. M0 Exit Criteria

- [ ] `systemscope-contracts` defines every type in §3–§8, each with rustdoc.
- [ ] The runtime implements scheduling rules S1–S6, elaboration, snapshot/restore, and both trace sinks.
- [ ] `m0-reference` runs, and its Perfetto output opens in ui.perfetto.dev with per-component tracks and transaction slices.
- [ ] **AT-1, AT-2, and AT-3 pass in CI on Linux and Windows.**
- [ ] Determinism lints (§8.3) are enforced in CI.
- [ ] All supporting unit and property tests pass.
- [ ] Contract changes discovered during M0 are reflected back into this document.

---

## 11. Implementation Order

1. Time: `Tick`, `SimulationClock`, `Duration`, `ClockDomain`, with clock-math property tests.
2. `Phase`, `EventKey`, the queue, and scheduling rules S1–S6.
3. `Component`, `SimContext`, `PortSpec`, `Link`, and elaboration.
4. The `mem.v0` protocol, `ToyMemory`, and one `ToyCpu`, reaching the first end-to-end run.
5. Trace sinks: canonical JSONL, then Perfetto.
6. Snapshot/restore, `RuntimeSnapshot`, and the three digests.
7. `ToyDma` and `ToyBus`, completing the full reference scenario.
8. AT-1 to AT-3, the CI workflow, and the determinism lints.
9. Golden files and `cargo xtask bless`.

---

## 12. Open Questions

- **Snapshot encoding.** Should snapshots use the same hand-rolled encoder as §4.5, or `postcard` (format stable since 1.0) configured to match it? Either way, it must satisfy the round-trip law and be identical across platforms. The digest encoding in §4.5 is fixed regardless of this choice.
- **Perfetto format.** M0 uses Chrome JSON. When should we move to native protobuf `TracePacket`s?
- **Same-phase scheduling.** Should the same `(tick, phase)` allow zero-delay chains? S2 currently allows it, guarded by S5. Revisit if hidden ordering dependencies appear.
- **Livelock budget.** Is the default `max_events_per_phase` right?
- **Duration precision.** Femtosecond granularity is assumed to be sufficient.
- **Bootstrapping contracts.** Until the GitHub org exists, `contracts` is developed as a sibling directory referenced by path.
