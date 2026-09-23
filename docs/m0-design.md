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
contracts/                        (repo: SystemScopeLabs/contracts)
└─ crates/systemscope-contracts/
   ├─ time.rs        Tick, SimulationClock, Duration, Frequency, ClockDomain
   ├─ event.rs       Phase, EventKey, ScheduleWhen
   ├─ error.rs       SimError
   ├─ component.rs   Component, InitContext, SimContext, PortSpec, ComponentId
   ├─ topology.rs    LinkLatency
   ├─ protocol/      ProtocolId, closed Message enum
   │  └─ mem.rs      mem.v0 messages
   ├─ snapshot.rs    SnapshotWriter/Reader, RestoreError
   ├─ canonical.rs   Encoder/Decoder (§4.5 primitive rules), canonical(ev)
   ├─ trace.rs       TraceRecord, Value, TraceHeader, stream encoding
   └─ observe.rs     StateView, EventView, WorldView, Observer, Control

systemscope/                      (repo: SystemScopeLabs/systemscope, this repository)
├─ runtime/                       systemscope-runtime: scheduler, TopologyBuilder and elaboration, lifecycle, snapshots, sinks
├─ components/toy/                systemscope-toy: ToyCpu, ToyDma, ToyBus, ToyMemory
├─ reference/                     systemscope-reference: builds m0-reference (§9.1) for tests and tools
├─ tests/acceptance/              systemscope-acceptance: AT-1, AT-2, AT-3 harness and tests, the m0-run binary
├─ tests/golden/                  golden digests and the portable snapshot (§9)
├─ xtask/                         cargo xtask bless
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
    for each due observe point, in tick order:  // §8.2
        run observers' on_observe (read-only)
```

`H` is BLAKE3. The 32-byte `execution_digest` starts as 32 zero bytes and absorbs each event before its handler runs. It is simulation state and is included in snapshots. `canonical(ev)` is defined in §4.5.

### 4.5 Canonical Encoding

`ExecutionDigest` and `topology_hash` depend on byte-exact encodings, so the encoding is specified here and does not depend on any serialization library.

**Primitive rules**

| Type | Encoding |
|---|---|
| unsigned/signed integers | fixed width, little-endian (`u8`, `u16`, `u32`, `u64`, `u128`, `i64`) |
| `bool` | `u8`: `0` or `1` |
| byte strings, `Vec<u8>` | `u32` length, then the bytes |
| strings | UTF-8 bytes, encoded as a byte string |
| enums | `u8` variant tag in declaration order, then the variant's fields |
| structs | fields in declaration order, with no padding and no field names |
| sequences | `u32` element count, then each element |
| `Option<T>` | `u8` tag: `0` none, or `1` then `T` |
| fixed-size arrays (`[u8; 32]` digests) | the elements, with no count |

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

**Decoding is strict.** A decoder accepts exactly the bytes an encoder can produce: it rejects truncated input, trailing bytes, unknown enum tags and phases, `bool`s other than `0`/`1`, invalid UTF-8, and unknown protocol names or versions. Every encoded value therefore has exactly one decoding.

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
    fn snapshot(&self, w: &mut SnapshotWriter);
    fn restore(&mut self, r: &mut SnapshotReader, schema_version: u32) -> Result<(), RestoreError>;

    fn inspect(&self) -> StateView { StateView::default() }   // read-only view for observers and the State Inspector
}

pub struct StateView { pub fields: Vec<(&'static str, Value)> }   // named values, no floats

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

| State | `init` / `restore` | `run` / `step` | `snapshot` | `inspect` |
|---|---|---|---|---|
| `Elaborated` | once, either one | no | no | yes |
| `Ready` | no | yes | yes | yes |
| `Faulted` | no | no | no | yes |

- **Initialization order is fixed.** Components are initialized in `ComponentId` order, one at a time. Initial events therefore receive deterministic sequence numbers.
- **Restore never calls `init`.** Initial events already live in the restored queue. Calling `init` again would duplicate them and shift every later sequence number, breaking replay.
- **A failed restore faults the session.** Components may already hold part of the snapshot, so the runtime is not reused; build a new one.
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
- `topology_hash` is BLAKE3 over the canonical encoding (§4.5) of the ordered spec: the `components` sequence, then the `links` sequence, byte for byte as they appear in the trace header (§8.1).
- **`topology_hash` identifies structure only:** components, ports, and links. Clock domains, the tick resolution, the seed, and scheduler limits are session settings; restore checks them one by one through `SessionInfo` (§7). Component parameters, such as a CPU's operation count, are in neither; a component that depends on them writes them into its own snapshot and rejects a mismatch.

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

Each initiator allocates `TxnId`s from its own counter, so they are deterministic and part of its snapshot. `TxnId`s are unique per initiator only. An interconnect that merges several initiators onto one port must allocate its own downstream `TxnId`s and map each back to `(upstream port, original TxnId)` to route the response; that map is part of the interconnect's snapshot (ToyBus, §9.1).

---

## 7. State and Snapshots

```rust
pub struct RuntimeSnapshot {
    format_version: u32,
    session: SessionInfo,          // seed, ticks_per_second, max_events_per_phase, contracts version, clock domains, topology_hash
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
- **`max_events_per_phase` is session configuration**, not execution state. It is recorded in `SessionInfo` only so that restore can check the resuming session uses the same limit.
- **Encoding is canonical and follows the primitive rules of §4.5.** The same logical state always produces the same bytes. That rules out `HashMap` iteration, floats, and pointer-dependent ordering. Maps are `BTreeMap` or sorted `Vec`s.

  ```text
  magic                8 bytes  "SSSNAP" followed by two 0x00
  format_version       u32      currently 1
  session              seed u64 · ticks_per_second u64 · max_events_per_phase u64 · contracts_version string
                       · clock domains (as in the trace header) · topology_hash [u8; 32]
  last_dispatched      Option<tick u64 · phase u8 · sequence u64>
  dispatched_in_phase  u64
  next_sequence        u64
  execution_digest     [u8; 32]
  queue                sequence of canonical(ev) (§4.5), sorted by key
  rng_states           sequence of 4 × u64
  components           sequence of (id u32 · schema_version u32 · bytes), in ComponentId order
  ```

- **Restore validates before it resumes**, in this order, and fails with the first error:
  1. The bytes decode strictly (§4.5), with the expected magic and `format_version`.
  2. **Every `SessionInfo` field matches the new session:** seed, `ticks_per_second`, `max_events_per_phase`, contracts version, and every clock domain's id, frequency, offset, and rounding. Otherwise `RestoreError::SessionMismatch(field)`.
  3. `topology_hash` matches. Otherwise `RestoreError::TopologyMismatch`.
  4. Every component's `snapshot_schema_version` matches. Otherwise `RestoreError::SchemaVersion`. Migrations are out of scope for M0.
  5. The state is one a valid run could produce: one RNG state and one component entry per component, in id order, with no all-zero RNG state; queued events name existing components and ports, speak the port's protocol, and wake only their own source; and the scheduler checks above hold. Otherwise `RestoreError::InvalidState`.
  6. Each component restores from its own bytes and must consume all of them.
- **Observer state and pending observe points are not simulation state.** They are never included in snapshots or digests.
- **`StateDigest = BLAKE3(encode(RuntimeSnapshot))`.**
- **Snapshots never contain trace state.** A traced and an untraced run produce identical snapshot bytes at the same event boundary. A trace continues across a restore from outside the snapshot (§8.1).
- **Round-trip law:** `encode(restore(decode(encode(s)))) == encode(s)`.

---

## 8. Observation and Determinism

### 8.1 Trace

```rust
// contracts
pub enum Value { U64(u64), I64(i64), Bool(bool), Str(String), Bytes(Vec<u8>) }   // no floats
pub enum TraceAt { Init, Event(EventKey) }        // what was running when the record was made
pub enum TraceOrigin { Component, Runtime }
pub struct TraceRecord {
    at: TraceAt,
    origin: TraceOrigin,
    component: ComponentId,                        // the emitter; for runtime records, the target
    kind: &'static str,
    fields: Vec<(&'static str, Value)>,
}
// on InitContext and SimContext:
fn trace(&mut self, kind: &'static str, fields: Vec<(&'static str, Value)>);
```

- **Emission is write-only.** `ctx.trace()` returns `()`, and there is no API to ask whether tracing is on. Components therefore cannot branch on it. When tracing is off the record is dropped; nothing else changes.
- **Two origins.** Component records come from `ctx.trace()`. The runtime also emits one `runtime.dispatch` record per dispatched event, before the records its handler emits. Its `component` is the target, and its fields describe the source and the delivery. Perfetto transaction slices are built from these records, so they exist whether or not components trace. Fields, in order:

  | Delivery | Fields |
  |---|---|
  | message | `source` U64 · `port` U64 · `protocol` Str · `version` U64 · `msg` Str (variant name), then the message's fields in declaration order (`txn`, `addr`, `len` as U64; `data` as Bytes) |
  | wake | `source` U64 · `token` U64 |
- **Tracing starts before `init`, or resumes right after `restore`.** Either way the trace holds the header and every record from the start of the session. It cannot be started at any other time in M0.
- **A trace continues across a restore by carrying its prefix outside the snapshot:**

  ```text
  old runtime:  snapshot() → bytes        take_trace() → prefix (header + records, no trailer)
                drop the old runtime
  new runtime:  elaborate → restore(bytes) → resume_trace(prefix) → run → take_trace()
  result:       one header, the prefix records, then the new records, one trailer
  ```

  A BLAKE3 digest cannot be extended from its output, so the prefix is kept as records, not as a digest. It is a `Trace` value, never encoded bytes, so no trailer is ever carried over. `resume_trace` is accepted only on a freshly restored runtime, before its first step, and only if the prefix belongs to the snapshot:
  - its header equals the new session's header;
  - its records are well formed: `Init` records come before the first `runtime.dispatch`, and every other component record carries the key of the dispatch before it;
  - **replaying `canonical(ev)` of its `runtime.dispatch` records from 32 zero bytes reproduces the snapshot's `execution_digest`**, and its last dispatch key equals `last_dispatched`. A snapshot taken before the first event requires a prefix with no dispatch records.

  Two runs can share a header and still differ, for example when a component is configured differently. The digest check binds the prefix to the exact event history the snapshot came from, so M0 needs no separate checkpoint manifest.
- **Trace state is not simulation state.** The recorder lives outside the scheduler, the RNGs, and the components. It is never snapshotted and never read by the simulation.
- **The source of truth is the canonical binary encoding**, not any text format:

```text
TraceHeader + TraceRecords
        │ canonical binary encoding (§4.5)
        ├─ BLAKE3 ─────────▶ TraceDigest
        ├─ JSONL exporter ─▶ human-readable view, replay tooling
        └─ Perfetto exporter ▶ timeline view
```

  **Stream layout**, by the primitive rules of §4.5:

  ```text
  magic           8 bytes  "SSTRACE" followed by 0x00
  format_version  u32      currently 2
  header
  record*         each: u8 0x01 marker, then the record
  end             u8 0x00, then record count as u64
  ```

  **Header:**

  ```text
  ticks_per_second  u64
  seed              u64
  contracts_version string
  topology_hash     [u8; 32] (§6)
  clock domains     sequence of (id u32 · freq num u64 · freq den u64 · offset u64 · rounding u8: 0 Floor, 1 Ceil)
  components        sequence of (path string · type_name string · ports: sequence of (name string · protocol name string · protocol version u16 · role u8: 0 Initiator, 1 Target))
  links             sequence of (a component u32 · a port u16 · b component u32 · b port u16 · latency), in declaration order
  latency           u8 tag: 0 none · 1 After, then femtoseconds u128 · 2 Cycles, then domain u32 · k u64
  ```

  **Record:**

  ```text
  at         u8 tag: 0 Init · 1 Event, then tick u64 · phase u8 · sequence u64
  origin     u8: 0 Component · 1 Runtime
  component  u32
  kind       string
  fields     sequence of (name string · value)
  value      u8 tag, then payload: 0 U64 u64 · 1 I64 i64 · 2 Bool u8 · 3 Str string · 4 Bytes bytes
  ```

  Every element is either fixed-width or length-prefixed, and every record starts with a marker, so the stream decodes in exactly one way. Two different record sequences can never encode to the same bytes, and the trailer's count makes a truncated stream detectable.

  `TraceDigest` is BLAKE3 over the whole stream. JSON escaping, whitespace, field order, and serializer versions therefore never affect a digest. Exporters are views: two exporters may format the same trace differently, and the digest stays the same.

  **Any change to this layout, the header, the record encoding, value tags, or the `runtime.dispatch` fields bumps `format_version`** and requires a golden re-bless. Version 2 added `topology_hash` to the header.
- **Two exporters in M0:**
  - **JSONL:** the first line is the header and each later line is one record. The header line carries every header field, `topology_hash` included, so it identifies the same session and topology as the binary header. Ticks and integers are written as exact JSON integers, and bytes and hashes as lowercase hex strings. It is derived from the records and is not digested. JSON numbers above 2^53 need a 64-bit integer reader.
  - **Perfetto:** Chrome JSON Trace Event format. Each component is its own process and thread, both with `pid = tid = ComponentId + 1` and named by `component_path`; id 0 is avoided because Perfetto treats it specially. Every record becomes an instant event on its component's thread, with its fields as `args`. Each `mem.v0` transaction becomes a process-scoped async slice (`id2.local = "initiator:txn"`) in the initiator's process, from the request's dispatch to the response's dispatch. Global async ids are not used, because Perfetto groups them apart from any component.
  - **Perfetto timestamps** are in µs with exactly nine decimal places: `floor(tick × 10^15 / ticks_per_second)` femtoseconds, then split into µs and the remainder. This is integer arithmetic and exact at the default 1 ps resolution. It is for display only and never feeds back into records or digests. The Perfetto UI itself keeps nanoseconds, so sub-nanosecond detail is only visible in the text.

### 8.2 Observer

```rust
// contracts, observe.rs
pub trait Observer {
    fn on_after_dispatch(&mut self, ev: &EventView<'_>, world: &WorldView<'_>) -> Control { Control::Continue }
    fn on_observe(&mut self, now: Tick, world: &WorldView<'_>) -> Control { Control::Continue }
    fn on_trace(&mut self, rec: &TraceRecord) {}
}
pub enum Control { Continue, Pause }
pub struct EventView<'a> { pub key: EventKey, pub source: ComponentId, pub target: ComponentId, pub delivery: &'a Delivered }
pub struct WorldView<'a> { /* private: now, &'a [Box<dyn Component>] */ }
impl WorldView<'_> {
    pub fn now(&self) -> Tick;
    pub fn component_count(&self) -> usize;
    pub fn type_name(&self, id: ComponentId) -> Option<&'static str>;
    pub fn inspect(&self, id: ComponentId) -> Option<StateView>;
}
// runtime
impl Runtime {
    pub fn add_observer(&mut self, observer: Box<dyn Observer>);
    pub fn observe_at(&mut self, tick: Tick);
    pub fn run(&mut self, until: Tick) -> Result<RunOutcome, RuntimeError>;
    pub fn step(&mut self) -> Result<Option<Dispatched>, RuntimeError>;
}
pub struct RunOutcome { pub events: u64, pub stop: Stop }
pub enum Stop { Paused, Drained, Horizon }
```

- **`on_after_dispatch` runs after `handle_event` returns**, so it sees the state the event produced. M0 has no pre-dispatch hook. If breakpoints that stop *before* an event are needed later, they will be added as a separate `on_before_dispatch`, not by changing this one.
- **`WorldView` is read-only by construction.** It holds only shared references and offers nothing but the time, the component count, each component's `type_name`, and `inspect()`. It never hands out a component, `&` or `&mut`, so an observer cannot call `handle_event`, `restore`, or anything else on one. Observers receive no context, so they cannot reach an RNG or schedule events. A compile-fail test pins this.
- **`on_trace` sees every record the session emits**, in emission order, whether or not a trace recorder is running: `Init` records if the observer was added before `init`, then each dispatch record followed by its handler's records. Records are produced whenever a recorder or an observer exists; components still cannot tell (§8.1).
- **Observe points are kept outside the simulation queue.** `observe_at(tick)` adds the tick to a separate `BTreeSet<Tick>`, so registering a tick twice is the same as once. Points never consume `sequence` numbers, never enter the execution digest, and are never snapshotted.
- **An observe point `T` is due once the simulation has finished tick `T`:** the next queued event is after `T`, or the queue is empty and `T` is at or before the current tick, or, during `run(until)`, the queue holds nothing at or before `until` and `T ≤ until`. Due points fire in tick order at event boundaries, with `now = T`. A point whose tick has already passed fires at the next boundary.
- **`Pause` returns control to the driver at an event boundary.** `run(until)` returns `Stop::Paused` right after the callback that asked for it; the event that triggered an `on_after_dispatch` pause has been fully dispatched, and no further event runs. Remaining due points fire at the start of the next call. Resuming is simply calling `run` again, and it continues exactly as if no pause had happened. `Paused` is a driver outcome, not a lifecycle state and not an error; the session stays `Ready`.
- **`step()` dispatches exactly one event** (with its observer callbacks and the points that become due) and returns it, or `None` when the queue is empty. It ignores pause requests, since it returns anyway. `run_until(until)` is `run(until)` repeated through pauses, returning the total event count.
- **Observation never changes the simulation.** Adding observers, registering points, pausing, resuming, and stepping leave the queue, `next_sequence`, the RNGs, the components, and all three digests exactly as an unobserved run would (AT-3).

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
- **ToyDma** issues seeded write bursts (below).
- **ToyBus** merges the two initiators onto the memory port and arbitrates between them in `Transfer` (below). It is designed to hit same-tick collisions, so it exercises the phase and sequence ordering rules.
- **ToyMemory** has fixed latencies, `After(50 ns)` for reads and `After(30 ns)` for writes, and responds in `Complete`. Memory order is request dispatch order: a request is accepted when it is dispatched, writes become visible and reads are sampled at acceptance, and the response is emitted after the fixed latency. Response timing never affects visibility.
- **ToyCpu's shadow check** updates the shadow copy when a write's response commits. This is exact because the CPU never has two operations to the same slot in flight, and because no other initiator writes the CPU's slots (address map below).

**Topology.** Components are declared as `soc.cpu0`, `soc.dma0`, `soc.bus`, `soc.mem`, in that order, and clock domains as `cpu`, `io`, `bus`. Every link has latency `Cycles { domain: bus, k: 1 }`, one bus cycle counted from the next bus edge. Links are declared in this order: CPU ↔ bus port `cpu`, DMA ↔ bus port `dma`, bus port `mem` ↔ memory.

**Address map.** The CPU and the DMA own disjoint regions, so neither can invalidate the other's view of memory. M0 models no coherence or sharing between initiators.

| Region | Addresses | Owner |
|---|---|---|
| CPU | `0 .. cpu_slots × 8`, 8-byte slots | ToyCpu reads and writes, checked against its shadow copy |
| DMA | the next `dma_slots × 16` bytes, 16-byte slots | ToyDma writes only |

**ToyDma** runs on the `io` clock and issues only `WriteReq`s, at most one per `io` cycle, in `Request`, in bursts:

- A burst starts at a slot drawn uniformly from its region and writes consecutive slots, wrapping at the region's end. Its length is uniform in `1..=max_burst`, capped by the operations left. Data bytes come from `ctx.rng()`.
- After a burst's last write, the DMA idles for a number of `io` cycles uniform in `1..=max_gap_cycles`.
- At most `max_outstanding` writes are in flight. When the limit is reached, the DMA pauses until a response frees a place. A paused burst then resumes one `io` cycle later, and a finished burst starts its gap then.
- `TxnId`s come from the DMA's own counter, starting at 0, independent of the CPU's. The CPU and the DMA therefore routinely have the same `TxnId` in flight at once.
- Its snapshot holds its configuration, its counters, the current burst's next slot and remaining length, whether a wake is pending, the outstanding writes, and a checksum over completed writes. Its RNG state belongs to the runtime (§5.2), never to the DMA.

**ToyBus** runs on the `bus` clock. Its ports are, in order, `cpu` (target), `dma` (target), and `mem` (initiator). The upstream port index is the bus's identity for an initiator: `0` for `cpu`, `1` for `dma`.

- **Requests must arrive in `Request`.** A request is appended to its port's FIFO queue. A request delivered in any later phase faults the session. So every request that arrives at a tick is queued before that tick's arbitration runs, whatever its sequence number.
- **Arbitration** runs as a `Wake` in `Transfer` on bus edges. When a request arrives and no arbitration is pending, the bus schedules one at `Cycles { domain: bus, k: 0 }`, the first bus edge at or after now. Links deliver on bus edges, so that is the same tick. Each arbitration grants **at most one request**. Afterwards, if any queue is non-empty, the next arbitration is scheduled one bus cycle later (`k: 1`).
- **Round-robin.** The bus holds a priority pointer `p ∈ {0, 1}`, initially `0` (CPU first).
  - If exactly one queue is non-empty, its head wins.
  - If both are non-empty, the head of queue `p` wins. This is a *contended* grant.
  - After **every** grant, contended or not, `p` becomes the other port: `p = 1 − winner`.
  - The result depends only on which queues are non-empty and on `p`. It never depends on arrival sequence across ports or on container iteration order.
- **Remapping.** A grant does three things:
  - It allocates the next downstream `TxnId` from the bus's own counter, starting at 0.
  - It records `downstream → (upstream port, upstream TxnId, read or write)` in an ordered map.
  - It forwards the request on `mem` with the downstream `TxnId`, in `Transfer`, with all other fields unchanged.
- **Responses** from memory are not arbitrated. On arrival, the bus:
  - removes the mapping for the response's `TxnId`;
  - checks that the response kind matches the request;
  - sends the response on the recorded upstream port, with the upstream `TxnId`, in the phase it arrived in (`Complete`).

  A response for an unknown or mismatched `TxnId` faults the session.
- **Snapshot:**
  - Contents: configuration, the next downstream `TxnId`, the priority pointer, whether an arbitration is pending, both queues in order, and the map in ascending downstream `TxnId` order.
  - Restore rejects a pointer outside `{0, 1}`, a queued response, map entries out of order or at or above the next downstream `TxnId`, and an unknown port.
- **Trace:**
  - Each grant emits `toy.bus.grant` (`port`, `txn`, `downstream`, `contended`), and each routed response emits `toy.bus.route` (`downstream`, `port`, `txn`).
  - The runtime's dispatch records already show every hop, so Perfetto shows each hop as its own slice: `(cpu, txn)` and `(dma, txn)` in the initiators' processes, and `(bus, downstream)` in the bus's.
  - No trace format change is needed.

The clock mix is chosen on purpose. 3 GHz has a period that is not a whole number of ticks. 1.5 GHz exercises `freq_den ≠ 1`. `Duration`-based latency exercises cross-fidelity conversion.

- **Seeds:**
  - **Fixed seeds** `{0, 1, 0xDEADBEEF}` run on every CI run. Only these have golden digests.
  - **One random seed** runs nightly, passed to the tests as `M0_SEED`. It is printed so failures can be reproduced. It has no golden digest and is **never compared against committed golden files**. It is only checked for reproducibility (AT-1 step 1), for snapshot/restore equivalence (AT-2 steps 1–3), and for observation invariance (AT-3).
- **Run length:** until both initiators finish 100,000 operations, or `T_end = 10 ms` of simulated time, whichever comes first. Development tests use the same topology with fewer operations. The acceptance tests always use the full workload (about 1.3 million events, 2.1 million trace records).
- **Workload parameters:**

  | Component | Parameters |
  |---|---|
  | ToyCpu | 4 outstanding, think time `1..=8` cycles, 8-byte accesses, 64 slots, 40% writes |
  | ToyDma | 8 outstanding, bursts `1..=8`, gaps `1..=128` cycles, 16-byte writes, 64 slots |
  | ToyMemory | `64 × 8 + 64 × 16` bytes, read `50 ns`, write `30 ns` |

### 9.2 Digests

| Digest | Definition |
|---|---|
| `StateDigest` | BLAKE3 of the encoded final `RuntimeSnapshot` |
| `ExecutionDigest` | chained BLAKE3 over every dispatched event (§4.4) |
| `TraceDigest` | BLAKE3 of the canonical binary trace (header, then records in emission order), when tracing is on (§8.1) |

### AT-1: Reproducibility

*The same inputs always produce the same result, across runs, processes, and operating systems.*

1. For each seed, run the scenario twice in **separate processes**. All three digests must be equal. The `m0-run` binary runs the scenario and prints its digests with its process id. The test checks that the two ids differ from each other and from its own.
2. For the **fixed seeds only**, the CI matrix covers **`ubuntu-latest` and `windows-latest`**. Their digests must be identical to each other and to `tests/golden/m0-reference.json`. The nightly random seed skips this step and is checked only with step 1.
3. Different seeds must produce different `ExecutionDigest`s. This checks that the seed is actually used.

### Golden Files

| File | Contents |
|---|---|
| `tests/golden/m0-reference.json` | the workload, then per fixed seed: event count, `StateDigest`, `ExecutionDigest`, `TraceDigest`; the portable snapshot's seed, event index, and BLAKE3 |
| `tests/golden/m0-reference.mid.snap` | the seed `0xDEADBEEF` run, snapshotted halfway (AT-2 step 4) |

- Golden files change only through `cargo xtask bless`. It runs the scenario, prints every changed digest as old → new, and writes the files. Tests only read them.
- CI fails if a test run leaves the working tree changed.
- The commit that changes golden files must explain why in its body.
- `.gitattributes` marks `*.snap` as binary, so checkouts on every OS keep the bytes. The recorded BLAKE3 catches any conversion.

### AT-2: Snapshot/Restore Equivalence

*Checkpointing a run and resuming it produces exactly the same result as never stopping.*

1. **Reference run.** Run uninterrupted to the end and record the digests as `D`.
2. **Checkpoint runs.** The checkpoints are:
   - right after `init`, before the first event
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

   `StateDigest` and `ExecutionDigest` must equal `D`. The trace prefix, taken from the dropped runtime and resumed on the new one (§8.1), together with the resumed suffix must equal the reference `TraceDigest`.
3. **Round-trip law.** At every checkpoint, check that `encode(restore(decode(bytes))) == bytes`.
4. **Portability.** `tests/golden/m0-reference.mid.snap` is a committed snapshot. Restoring it and running to the end must match the golden digests on both CI operating systems.
5. **Negative cases.**
   - A changed `snapshot_schema_version` must fail with `RestoreError::SchemaVersion`.
   - A changed topology must fail with `RestoreError::TopologyMismatch`.
   - Each changed `SessionInfo` field must fail with `RestoreError::SessionMismatch` naming it.
   - Malformed bytes must fail: every truncation, trailing bytes, a wrong magic, an unknown format version, and an invalid tag.
   - An impossible scheduler or RNG state must fail with `RestoreError::InvalidState`: a queued sequence at or above `next_sequence`, a queued event before the last dispatched one, an inconsistent `dispatched_in_phase`, a duplicate sequence, and an all-zero RNG state.
   - `resume_trace` must reject a prefix from a differently configured run, a truncated or extended prefix, and any call after the first step.

### AT-3: Observation Invariance

*Watching a run never changes it.*

| Config | Observation |
|---|---|
| O0 | no observers, no sinks |
| O1 | canonical trace recorder with JSONL and Perfetto exporters |
| O2 | breakpoint observer pausing on every `ReadResp`; the driver resumes immediately |
| O3 | single-step, one event per `step` call, until the end |
| O4 | `Observe` probe every 1,000 ticks, calling `inspect()` on every component |
| O5 | O1 + O2 + O3 + O4 |

- Every configuration must produce the same `StateDigest` and `ExecutionDigest`.
- O1 and O5 must produce the same `TraceDigest`.
- Registering observe points must leave `next_sequence` unchanged. This is asserted directly.
- The RNG states, `next_sequence`, every component's snapshot bytes, and the event count must equal O0's.
- Pause, resume, and step counts are driver state. They differ between configurations and never reach the simulation.
- **Compile-fail test** (`trybuild`): code that tries to get `&mut` component state from `WorldView` must not compile. Neither must an `inspect` that writes to its component.

### CI

- **Blocking, on `ubuntu-latest` and `windows-latest`:** `cargo fmt --check`, `cargo check --locked`, `clippy -D warnings` with the determinism lints, `cargo machete` (these four on Linux only), then nextest with `--no-fail-fast`, doctests, AT-1, AT-2, and AT-3. A last step fails if the tests changed any file.
- **Nightly, not blocking:** AT-1 step 1, AT-2, and AT-3 for one random seed on both operating systems (`M0_SEED`, printed), coverage, and `cargo mutants`.
- `contracts` is checked out next to `systemscope`, matching the path dependency.

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
5. Trace: canonical binary records and TraceDigest, then JSONL and Perfetto exporters.
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
- **Bootstrapping contracts.** `contracts` is still consumed as a sibling directory referenced by path; CI checks out `SystemScopeLabs/contracts` at a pinned commit next to this repository. When should this become the git dependency plan.md §10 describes?
