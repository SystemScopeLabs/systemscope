# M1 Design: RV32I Architectural CPU

> Status: Draft · Parent: [plan.md](../plan.md) · Builds on: [m0-design.md](m0-design.md)

M1 runs real RISC-V machine code on the M0 runtime:

```text
ELF32 ──host-side loader──▶ initial RAM image + entry point
                                     │
Rv32iCpu ──mem.v1──▶ AddressBus ──mem.v1──▶ Ram
                                └─mem.v1──▶ SimpleUart
```

It is done when a bare-metal RV32I ELF runs to completion in SystemScope, prints through the UART, and its architectural behavior agrees with four independent checks: SystemScope's own unit and property tests, the `riscv-tests` `rv32ui` suite, Spike in lockstep, and the RISC-V Architectural Compliance Tests (ACT4, with Sail as the reference). Everything still runs through the M0 component, event, and state model, and every M0 guarantee (determinism, snapshot/restore, observation invariance) carries over.

---

## 1. Goals and Non-goals

### Goals

- Implement the complete **RV32I Base Integer Instruction Set, Version 2.1** (ratified), as specified in the RISC-V Unprivileged ISA manual: 40 instructions, 32 × 32-bit registers, and a 32-bit `pc`.
- Model the CPU at **F2 (Architectural)** fidelity (plan.md §5): exact ISA semantics, with no pipeline.
- Fetch every instruction through memory, over a protocol, never from a host-side array.
- Add the `mem.v1` protocol, which can report an access fault, and freeze `mem.v0` for M0.
- Provide a platform: an address-decoding bus, byte-addressed RAM, and a minimal UART.
- Load ELF32 images on the host, before the simulation starts.
- Define an execution-environment trap boundary that later privileged backends can connect to.
- Validate against `rv32ui`, Spike, and ACT4, on both CI operating systems.

### Non-goals (M1)

- The M, A, F, D, and C extensions; Zicsr; Zifencei.
- Privileged architecture: CSRs, M/S/U modes, `mtvec`/`mepc`/`mcause`, `mret`, and trap handlers. Interrupts are M2; virtual memory (Sv32) and the OS are M3.
- Pipelines, caches, branch predictors, and any other microarchitecture (F3).
- More than one hart, and more than one outstanding memory operation.
- Misaligned load/store support. Misaligned accesses trap (§6).
- A 16550-compatible UART.
- Performance modeling. M1 cycle counts carry no performance meaning (§5.4).

---

## 2. Decisions Fixed at M1.0

These decisions were settled while reviewing the M1 plan against the upstream `riscv-tests`, ACT4, and Spike, and against the M0 contracts. Each is specified in the section named.

| Topic | Decision | § |
|---|---|---|
| Base ISA | RV32I 2.1, all 40 instructions | 5 |
| CPU fidelity | F2 architectural, single outstanding memory operation | 5 |
| Architectural updates | `pc` and registers change only in the `Commit` phase | 5.3 |
| Instruction fetch | Always a `mem.v1` read through the bus | 5.3 |
| Misaligned load/store | Trap | 6 |
| Access faults | Reported by the new `mem.v1` protocol | 4 |
| `mem.v0` | Frozen for M0. It is never modified. | 4 |
| M0 regression | M0 golden digests stay byte-identical throughout M1 | 4.4, 11 |
| Serialized compatibility id | Decoupled from the crate's SemVer version. The M0 value `"0.0.0"` is kept. | 4.5 |
| `riscv-tests` environment | A SystemScope test environment, with no CSRs | 10.2 |
| `rv32ui` scope | The 42 upstream tests minus `fence_i` and `ma_data`: exactly 40 | 10.2 |
| External tools | Fixtures built and Spike/ACT4 run on Linux; both operating systems run the same committed ELFs | 10.6 |
| Spike differential | Compared from the ELF entry, boot ROM ignored, up to the first trap or the agreed termination | 10.3 |
| RAM storage | Sparse 4 KiB pages | 7.2 |
| Digests | The three M0 digests are reused; no new digest type | 10.1 |
| Milestone tag | `v0.2.0-m1` | 12 |

---

## 3. Repository and Crate Layout

```text
contracts/
└─ crates/systemscope-contracts/
   └─ protocol/
      ├─ mem.rs        mem.v0 (frozen), TxnId
      └─ mem_v1.rs     mem.v1: MemMsg with ReadOutcome/WriteOutcome, MemFault

systemscope/
├─ components/toy/        unchanged; still mem.v0
├─ components/rv32i/      systemscope-rv32i: decode, execute, Rv32iCpu
├─ components/platform/   systemscope-platform: AddressBus, Ram, SimpleUart
├─ elf/                   systemscope-elf: host-side ELF32 loader (not a component)
├─ reference/             adds m1-reference (§9) next to m0-reference
├─ tests/acceptance/      adds the M1 acceptance tests and the m1-run binary
├─ tests/rv32/
│  ├─ env/                the SystemScope riscv-tests environment (§10.2)
│  ├─ fixtures/           committed ELFs and manifest.json (§10.6)
│  └─ progen/             deterministic random-program generator (§10.3)
└─ tests/golden/          adds m1-reference.json and m1-reference.mid.snap
```

- **Components depend only on `systemscope-contracts`,** as in M0. `systemscope-rv32i` knows nothing about the memory map, the RAM, or the UART.
- **`systemscope-elf` depends on nothing in the simulation.** It turns bytes into a checked load image (§8). The reference builder passes that image to `Ram` and the entry point to `Rv32iCpu` as construction parameters.
- `.gitattributes` marks `*.elf` as binary, as it does `*.snap`.

---

## 4. The `mem.v1` Protocol

### 4.1 Why a New Version

`mem.v0` has no way to say "there is nothing at this address". Today an unroutable request faults the whole session (`SimError::ComponentFault`). RV32I needs that case to become an architectural trap: `InstructionAccessFault`, `LoadAccessFault`, or `StoreAccessFault`. The CPU must not know the memory map, so the fault has to come back as a response.

Changing `mem.v0` would change its canonical encoding and therefore every M0 digest (m0-design §4.5). M1 adds `mem.v1` next to it instead.

### 4.2 Messages

```rust
// contracts, protocol/mem_v1.rs
pub const PROTOCOL: ProtocolId = ProtocolId { name: "mem", version: 1 };

pub enum MemMsg {
    ReadReq   { txn: TxnId, addr: u64, len: u32 },
    ReadResp  { txn: TxnId, outcome: ReadOutcome },
    WriteReq  { txn: TxnId, addr: u64, data: Vec<u8> },
    WriteResp { txn: TxnId, outcome: WriteOutcome },
}
pub enum ReadOutcome  { Data { data: Vec<u8> }, Fault { fault: MemFault } }
pub enum WriteOutcome { Done, Fault { fault: MemFault } }
pub enum MemFault     { AccessFault }
```

- `TxnId` is the same type as in `mem.v0`, with the same rules: allocated by each initiator from its own counter, unique per initiator, and part of its snapshot.
- Requests are unchanged from `mem.v0`. Only responses gain an outcome.
- A `Data` response carries exactly `len` bytes. A shorter or longer one faults the session: that is a component bug, not an architectural event.
- A faulted write changes nothing at the target.
- `MemFault` is deliberately generic. The target reports only that the access failed; the CPU decides which trap it is, from the operation that was outstanding (§6).

### 4.3 Encoding

`mem.v1` follows the M0 primitive rules (m0-design §4.5) with no new rules. `ReadOutcome`, `WriteOutcome`, and `MemFault` are ordinary enums: a `u8` tag in declaration order, then the variant's fields. For example, `ReadResp { txn, outcome: Fault { fault: AccessFault } }` encodes as tag `1`, `txn` as `u64`, outcome tag `1`, fault tag `0`.

- `canonical(ev)` already carries the protocol name and version before the payload, so `("mem", 1)` messages can never collide with `("mem", 0)` ones.
- `Message` gains a variant, `Message::MemV1(mem_v1::MemMsg)`. The `Message` variant tag is not part of any encoding, so existing encodings do not change.
- The strict decoder dispatches on `(name, version)`: `("mem", 0)` to `mem::MemMsg`, `("mem", 1)` to `mem_v1::MemMsg`, and anything else is rejected as before.
- Any later change to these messages, including a new `MemFault` variant, is a new protocol version.

**`runtime.dispatch` fields for `mem.v1`** follow the M0 rule: the message's fields in declaration order. A nested outcome is flattened: `outcome` as Str (the variant name, `Data`, `Done`, or `Fault`), then that variant's fields (`data` as Bytes, `fault` as Str). This adds rows for a new protocol; no existing record's encoding changes, so the trace `format_version` stays 2.

**Perfetto:** a `mem.v1` transaction becomes a slice exactly like a `mem.v0` one. A faulted response ends the slice and carries `outcome` and `fault` as args.

### 4.4 M0 Is Frozen

- `mem.v0`, `ToyCpu`, `ToyDma`, `ToyBus`, `ToyMemory`, and `m0-reference` are not modified in M1.
- **The M0 golden digests must stay byte-identical** for the whole of M1. The M0 acceptance tests keep running in CI, and they already fail on any change to `tests/golden/m0-reference.json`.
- **The serialized contracts identifier does not change** (§4.5), so adding `mem.v1` cannot move the M0 `StateDigest` or `TraceDigest`.

### 4.5 Contracts Compatibility Id

In M0, the string written as `contracts_version` into `SessionInfo` (m0-design §7) and the trace header (m0-design §8.1) is the contracts crate's Cargo version (`env!("CARGO_PKG_VERSION")`, currently `"0.0.0"`). It feeds `StateDigest` and `TraceDigest`. Tying it to the package version would mean the crate's SemVer version could never change without moving the M0 golden digests.

M1.0 separates the two:

- **The crate version is ordinary SemVer.** It changes like any other crate version, for example when `mem.v1` is added, and never reaches a digest.
- **The serialized value becomes a fixed compatibility id,** a constant in `contracts`, independent of Cargo. It keeps the M0 value `"0.0.0"`. The field name, position, and encoding in `SessionInfo` and the trace header are unchanged, so every M0 snapshot and trace stays byte-identical.
- **The compatibility id changes only when snapshots or traces from one session can no longer be restored or resumed by the other.** An example is a change to an existing encoding. Adding a protocol, as `mem.v1` does, does not change it: every existing snapshot and trace decodes exactly as before.
- **Changing it is a deliberate re-bless.** The commit changes the constant, re-blesses the golden files, and must show that every event count and `ExecutionDigest` is unchanged. Only `StateDigest` and `TraceDigest` may move.
- The id looks like a version but is compared only for equality. It carries no ordering and no SemVer meaning.

---

## 5. The RV32I CPU

### 5.1 Architectural State and Invariants

```text
pc      u32
x[32]   u32, with x[0] hard-wired to 0
```

| Invariant | Rule |
|---|---|
| XLEN | 32 |
| `x0` | Always reads 0. Writes to it are discarded. It is not stored. |
| `pc` | Always 4-byte aligned. With no C extension, a misaligned target traps on the jump or branch that produces it (§6), so `pc` itself never becomes misaligned. |
| Instruction width | 32 bits, little-endian |
| Arithmetic | Wraps modulo 2^32 |
| Comparisons | Signed ones use `i32` semantics, unsigned ones `u32` |
| Shifts | Use the low 5 bits of the shift amount |

### 5.2 Decode

Decoding is a pure function `decode(u32) -> Result<Instr, Illegal>`, total over all 2^32 words. It never panics.

- **Immediates are extracted in one place:** `imm_i`, `imm_s`, `imm_b`, `imm_u`, `imm_j`. No instruction extracts immediate bits on its own.
- **The 40 instructions:**

  | Group | Instructions |
  |---|---|
  | Upper | LUI, AUIPC |
  | Jump | JAL, JALR |
  | Branch | BEQ, BNE, BLT, BGE, BLTU, BGEU |
  | Load | LB, LH, LW, LBU, LHU |
  | Store | SB, SH, SW |
  | Immediate | ADDI, SLTI, SLTIU, XORI, ORI, ANDI, SLLI, SRLI, SRAI |
  | Register | ADD, SUB, SLL, SLT, SLTU, XOR, SRL, SRA, OR, AND |
  | Memory ordering | FENCE |
  | System | ECALL, EBREAK |

- **`IllegalInstruction`** is returned for:
  - words whose low two bits are not `11` (16-bit encodings; there is no C extension), including the all-zero word;
  - unknown major opcodes, and unknown `funct3`/`funct7` combinations;
  - `SLLI`/`SRLI`/`SRAI` whose `imm[11:5]` is not exactly `0000000` (or `0100000` for `SRAI`), which covers `shamt[5] = 1` on RV32;
  - load `funct3` values `011`, `110`, `111`, and store `funct3` values `011` and above;
  - `MISC-MEM` with `funct3 = 001` (`FENCE.I`, Zifencei) or any other non-zero `funct3`;
  - every `SYSTEM` word other than exactly `ECALL` (`0x00000073`) and `EBREAK` (`0x00100073`). This covers all CSR instructions, `MRET`, `WFI`, and the rest.
- **FENCE:** every valid and forward-compatible `FENCE` encoding (`MISC-MEM`, `funct3 = 000`) is accepted, whatever its `fm`, `pred`, `succ`, `rs1`, and `rd` fields. That includes `FENCE.TSO`, `PAUSE`, and reserved configurations, which the base ISA requires to behave as ordinary fences. In the M1 execution model it retires as an architectural no-op. That is exact, not an approximation: M1 has one hart and at most one outstanding memory operation, and every load or store completes before the next instruction is fetched, so every ordering a fence could require already holds.
- **HINTs** (for example `ADDI x0, x0, k` with `k ≠ 0`, or register-register operations with `rd = x0`) execute normally. Their only effect is a discarded write to `x0`.

### 5.3 Execution Model and Phases

The CPU runs on its own clock domain and has one `mem.v1` initiator port, `mem`. It has at most one transaction outstanding, and fetches and data accesses share that port.

```text
FetchIssue     Request    send ReadReq { pc, 4 }
  ▼
FetchWait      Complete   ReadResp arrives → decode + execute into pending state
  ├─ ALU, branch, jump, FENCE, ECALL, EBREAK, or a trap ───────────────┐
  └─ load or store                                                     │
       ▼                                                               │
     MemIssue  Request (next cycle)   send ReadReq / WriteReq          │
       ▼                                                               │
     MemWait   Complete   response arrives → pending state             │
       ▼                                                               ▼
CommitPending  Commit (same tick)   apply the effect, or record the trap
  ├─ trap or instruction limit ─▶ Halted
  └─ otherwise ─▶ FetchIssue at the next cycle's Request
```

- **Fetch goes through memory.** `FetchIssue` sends `ReadReq { addr: pc, len: 4 }` on `mem`. The instruction word is the little-endian `u32` of the response's four bytes.
- **Decode and execute happen when the fetch response arrives,** in `Complete`. They compute the instruction's effect into CPU-internal pending state and change no architectural state.
- **A load or store** then issues its one data request in `Request` of the next CPU cycle. Rule S2 forbids an earlier phase in the same tick (m0-design §4.3). Its response arrives in `Complete`.
- **Architectural state changes only in `Commit`.** The CPU wakes itself in `Commit` of the same tick (`Cycles { domain: cpu, k: 0 }`) and resolves the pending instruction in one of two ways:
  - **It retires:** `x[rd] ← value` if `rd ≠ 0`, then `pc ← next_pc`, then `instret += 1`, and the CPU emits `rv32.commit`.
  - **It traps:** it does **not** retire. Registers, `pc`, and `instret` are unchanged, and the CPU emits `rv32.trap` and halts (§6). This holds for every trap cause, `ECALL` and `EBREAK` included.
- **Memory and device state follow their target's rules,** not the CPU's `Commit`. A store is visible at the RAM when the RAM accepts it, as in M0 (m0-design §9.1), and a UART byte is output when the UART accepts it. With one outstanding operation, no instruction can observe the difference.
- After a commit, the next `FetchIssue` is a wake at the next CPU cycle's `Request`.

This gives observers a precise meaning: `on_after_dispatch` of the `Complete` event shows the state before the instruction commits, and of the `Commit` event the state after.

**Reset:** at `init` the CPU schedules the first `FetchIssue` at tick 0 (`Cycles { domain: cpu, k: 0 }`, `Request`), with `pc = entry` and every register 0.

### 5.4 Timing

F2 timing is only as detailed as the M0 phase rules require:

- An ALU instruction takes one fetch round trip plus one cycle.
- A load or store adds one data round trip.
- Link and target latencies are the fixed values of `m1-reference` (§9).

These numbers are deterministic and pinned by the golden digests, but they are **not a performance model**. Cycle-level behavior belongs to a later F3 backend.

### 5.5 Halting

The CPU halts for exactly two reasons:

- **`Halted(Trap(RvTrap))`**: an architectural trap (§6). `ECALL` is the normal way a program ends.
- **`Halted(InstructionLimit)`**: right after the instruction that brings `instret` to `max_instructions` retires, in the same `Commit`, the CPU emits `rv32.halt` and halts without scheduling another fetch. `max_instructions` is a construction parameter that catches runaway programs. This halt is not a trap: that last instruction retired normally and has its `rv32.commit` record.

A halted CPU schedules nothing more. With no pending events, `run` returns `Stop::Drained`.

### 5.6 Snapshot

The CPU's snapshot holds:

- its configuration: `entry`, `max_instructions`;
- `pc`, `x[1..32]`, `instret`, and the next `TxnId`;
- the execution state, with its contents:
  - `FetchWait { txn }`, `MemWait { txn, insn, op }`, `CommitPending { insn, effect }`, or `Halted(reason)`;
  - the raw instruction bits, never a decoded form, since decoding is a pure function of them.

Restore rejects:

- a configuration different from the elaborated CPU's;
- an outstanding `txn` at or above the next `TxnId`;
- instruction bits that do not decode to an instruction consistent with the recorded state;
- a misaligned `pc`.

A checkpoint may fall anywhere in an instruction: while a fetch is in flight, between `Complete` and `Commit`, or while a load or store is in flight. AT-2 covers each of these (§10.1).

### 5.7 Inspect and Trace

`inspect()` shows `pc`, `x1`…`x31`, `instret`, the execution state's name, and, when halted, the halt reason with the trap's `cause`, `pc`, and `tval`.

Canonical trace records are kept to what later tools need. The runtime's `runtime.dispatch` records already show every fetch and data transaction.

| Kind | When | Fields, in order |
|---|---|---|
| `rv32.commit` | each retired instruction, in `Commit` | `pc` U64 · `insn` U64 · `rd` U64 (0 when nothing is written) · `rd_value` U64 · `next_pc` U64; then, for loads, `addr` U64; for stores, `addr` U64 · `width` U64 · `value` U64 |
| `rv32.trap` | a trapping instruction, in `Commit` | `pc` U64 · `insn` U64 · `cause` Str · `tval` U64 |
| `rv32.halt` | an instruction-limit halt, in `Commit` | `instret` U64 |

There are no `fetch` or `decode` records. They would roughly quadruple trace volume without carrying information the dispatch records lack.

---

## 6. Traps

M1 defines no privileged architecture, so it has no `mtvec`, `mepc`, or `mcause`. A trap is the **execution-environment boundary**: the CPU stops, and reports what happened.

```rust
pub struct RvTrap { pub cause: TrapCause, pub pc: u32, pub tval: u32 }
pub enum TrapCause {
    InstructionAddressMisaligned, InstructionAccessFault, IllegalInstruction,
    Breakpoint, LoadAddressMisaligned, LoadAccessFault,
    StoreAddressMisaligned, StoreAccessFault, EnvironmentCall,
}
```

- **Traps are precise.** The trapping instruction does not retire: it writes no register, does not change `pc`, does not increment `instret`, and, for stores, writes no memory. `RvTrap.pc` is the address of the trapping instruction.
- The CPU records the trap in `Commit` and enters `Halted(Trap)`. In M2 and M3, a privileged backend connects this same boundary to real machine or supervisor traps.

| Cause | Raised by | `tval` |
|---|---|---|
| `InstructionAddressMisaligned` | A taken branch, `JAL`, or `JALR` whose target is not 4-byte aligned. It is raised on that instruction, which does not write `rd`. A not-taken branch never raises it. | the target |
| `InstructionAccessFault` | A fetch whose response is `Fault` | `pc` |
| `IllegalInstruction` | `decode` returns `Illegal` (§5.2) | the instruction bits |
| `Breakpoint` | `EBREAK` | `pc` |
| `EnvironmentCall` | `ECALL` | 0 |
| `LoadAddressMisaligned` | `LH`/`LHU` with `addr % 2 ≠ 0`, `LW` with `addr % 4 ≠ 0`. Checked before any request is sent. | `addr` |
| `LoadAccessFault` | A load whose response is `Fault` | `addr` |
| `StoreAddressMisaligned` | `SH` with `addr % 2 ≠ 0`, `SW` with `addr % 4 ≠ 0`. Checked before any request is sent. | `addr` |
| `StoreAccessFault` | A store whose response is `Fault` | `addr` |

- `JALR` computes its target as `(x[rs1] + imm) & !1`, using `x[rs1]` from before the instruction, and checks alignment afterwards.
- Alignment is checked before an access is issued, so a misaligned access never reaches the bus.
- The `tval` values follow the RISC-V privileged convention, so they compare directly with Spike and Sail.

---

## 7. Platform Components (`systemscope-platform`)

### 7.1 AddressBus

The only component that knows the memory map.

- **Ports:** `cpu` (`mem.v1` target), then one `mem.v1` initiator port per region, in region order.
- **Regions:** `(name, base, size)`, given at construction. The constructor rejects:
  - a zero size;
  - a region that wraps past `u64::MAX`;
  - overlapping regions.
- **Routing:**
  - A request whose whole range `[addr, addr + len)` lies inside one region is forwarded on that region's port, in `Transfer`, with `addr − base`. Targets therefore see offsets and can be reused at any base.
  - A request that hits no region, or crosses a region boundary, is answered by the bus itself with a `Fault { AccessFault }` response in `Complete`.
- **`TxnId`s:** the bus has one upstream port, so it forwards the CPU's `TxnId` unchanged. It records `txn → (region, read or write)` in an ordered map, to check each response's port and kind. A response for an unknown `txn`, or on the wrong port, faults the session.
- **Responses** are relayed upstream in the phase they arrived in (`Complete`).
- **Snapshot:** configuration and the outstanding map.
- **Trace:** `platform.bus.fault` (`txn`, `addr`, `len`) whenever the bus answers a request itself.

### 7.2 Ram

- **Port:** `mem` (`mem.v1` target).
- **Configuration:** `size` and the initial image from the loader (§8), identified by `image_hash`.
- **Storage is sparse:** a `BTreeMap<u32, Box<[u8; 4096]>>` of pages. Unmapped pages read as zero.
- **Semantics:**
  - A request is accepted when it is dispatched. Writes become visible and reads are sampled at acceptance, as in `ToyMemory`.
  - The response follows after a fixed latency (§9), in `Complete`.
  - A request outside `[0, size)` gets a `Fault` response. The bus should never send one, but the RAM does not rely on that.
- **Snapshot:**
  - Contents: `size`, `image_hash`, then every page holding at least one non-zero byte, in ascending page order.
  - All-zero pages are omitted. The same memory contents therefore always encode to the same bytes, whatever history produced them (m0-design §7).
  - Restore rejects a different `size` or `image_hash`, pages out of order or out of range, and all-zero pages.
  - **Restore replaces the whole memory.** It first clears every page, including those loaded from the initial image at construction, then inserts the snapshot's pages. An omitted page means "all zero now", never "as in the initial image". Otherwise a program that zeroed an image page would see it reappear after a restore.
  - The initial image is used only by a new session. Restore never reads it; only `image_hash` is compared.
- `inspect()` shows `size`, `image_hash`, and the number of non-zero pages, never the contents.

### 7.3 SimpleUart (F1)

A minimal, SystemScope-specific device. It is not a 16550.

| Offset | Access | Behavior |
|---|---|---|
| `0x0` TX | write, 1 byte | Appends the byte to the output buffer |
| `0x4` STATUS | read, 1, 2, or 4 bytes, naturally aligned | Reads as the `u32` `1` (bit 0: TX ready, always set), little-endian |

- **Every other access gets a `Fault` response:** reads of TX, writes of any width other than 1 at TX, writes to STATUS, and any access in offsets `0x1`–`0x3` or `0x5`–`0x7`. Strict decoding keeps device behavior fully specified.
- It responds in `Complete` after a fixed latency (§9).
- **Snapshot:** the output buffer. **Trace:** `platform.uart.tx` (`byte`). `inspect()` shows the output buffer as Bytes.

A bare-metal program prints with `*(volatile unsigned char *)0x10000000 = 'H';`.

---

## 8. ELF Loader (`systemscope-elf`)

The loader is **host-side code, not a component**. It runs before the topology is built, turns an ELF file into a load image, and never touches a running simulation.

```text
ELF bytes ──▶ LoadImage { segments: [(addr, bytes)], entry, image_hash }
                 │                                   │
                 └─▶ Ram::new(size, image)           └─▶ Rv32iCpu::new(entry, max_instructions)
```

- **The loader rejects:**
  - anything other than `ELFCLASS32`, little-endian (`ELFDATA2LSB`), `EM_RISCV`, `ET_EXEC`;
  - malformed headers and program-header tables that fall outside the file;
  - a `PT_LOAD` segment whose file range falls outside the file, or with `p_filesz > p_memsz`;
  - a `PT_LOAD` segment outside the RAM region;
  - overlapping `PT_LOAD` segments;
  - an entry point that is misaligned or not inside a loaded segment.
- **Only `PT_LOAD` segments are loaded.** Bytes from `p_filesz` to `p_memsz` are zero-filled (`.bss`). Section headers and symbols are not needed. A test harness that needs a symbol such as `tohost` reads it separately.
- **Addresses:** the loader converts segment addresses to RAM offsets using the RAM region's base from the platform configuration.
- **`image_hash`** is the BLAKE3 of the ELF file's bytes. It identifies the program:
  - The RAM stores it in its snapshot, so a snapshot can never be restored under a different program (m0-design §6: component parameters live in the component's snapshot, not in `topology_hash`).
  - Golden files are keyed by it.
- The loader is hand-written. It covers only this subset of ELF32, which keeps the dependency set unchanged.

---

## 9. Reference Platform `m1-reference`

```text
soc.cpu0  Rv32iCpu (clock "cpu", 100 MHz)
  │ mem ─▶ cpu
soc.bus   AddressBus
  ├─ ram  ─▶ mem   soc.ram   Ram          base 0x8000_0000, size 16 MiB
  └─ uart ─▶ mem   soc.uart  SimpleUart   base 0x1000_0000, size 8
```

- **Components** are declared in the order `soc.cpu0`, `soc.bus`, `soc.ram`, `soc.uart`, with one clock domain, `cpu`.
- **Links:**
  - declared in the order CPU ↔ bus `cpu`, bus `ram` ↔ RAM, bus `uart` ↔ UART;
  - every link has latency `Cycles { domain: cpu, k: 1 }`.
- **Targets:** the RAM and the UART respond `Cycles { domain: cpu, k: 0 }` after acceptance, in `Complete`.
- **Session seed:** `0`. The CPU and the platform use no randomness, so the seed does not affect an ELF run. AT-1 step 3 (different seeds give different digests) does not apply to M1.
- **`max_instructions`:** 10,000,000.

A 100 MHz clock is 10,000 ticks per cycle at the default 1 ps resolution. It is chosen only to keep tick values readable, and carries no performance meaning (§5.4).

---

## 10. Verification

Four independent layers. The upstream ACT documentation itself says ACT is not a complete verification suite, so no single layer is trusted alone.

### 10.1 M1 Acceptance Tests

| Test | Statement |
|---|---|
| **M1-A1** | Unit and property tests pass for every instruction and every trap (below). |
| **M1-A2** | The 40 selected `rv32ui` tests pass under the SystemScope environment (§10.2). |
| **M1-A3** | Spike lockstep agrees on every compared instruction, for the 40 `rv32ui` ELFs and for deterministic random programs (§10.3). |
| **M1-A4** | The ACT4 RV32I tests pass (§10.4). |
| **M1-A5** | `hello.elf` runs to `ECALL`, and the UART output is exactly `Hello, SystemScope!\n`. |
| **M1-A6** | Snapshot/restore: resuming from any checkpoint gives the same final digests as never stopping, and a committed portable snapshot restores on both operating systems. |
| **M1-A7** | Observation invariance: tracing, breakpoints, stepping, and probes never change the result. |
| **M1-A8** | On `ubuntu-latest` and `windows-latest`, the committed programs produce identical digests, equal to `tests/golden/m1-reference.json`, and the M0 golden digests are unchanged (§4.4). |

**M1-A1 detail:**

- Decode is total: every word either decodes or returns `Illegal`, and never panics (property test over random words, plus exhaustive tests over opcode, `funct3`, and `funct7`).
- Each immediate extractor is checked against hand-computed vectors, including sign extension at every boundary.
- Semantics are checked against a separate, minimal interpreter in the test crate. It is written independently of `systemscope-rv32i`, and property tests compare the two on random register states and instructions.
- Targeted cases: `x0` writes, wrap-around, `SLT` vs `SLTU`, `SRL` vs `SRA`, shift amounts ≥ 32 in registers, `LB` vs `LBU`, `LH` vs `LHU`, branch boundaries, `JALR` bit 0 with `rd = rs1`, and every trap row of §6.

**M1-A6 checkpoints**, for `hello.elf` and for one long `rv32ui` or random program:

- before the first fetch;
- during a fetch;
- between `Complete` and `Commit`;
- during a load, and during a store;
- right after a UART byte;
- just before a trap commits;
- the last event;
- 8 seeded random event indices.

The trace continues through `resume_trace(prefix)` exactly as in M0 AT-2. The portable snapshot `tests/golden/m1-reference.mid.snap` is checked as in M0 AT-2 step 4: bytes, decode, restore, round-trip, and final event count, `StateDigest`, and `ExecutionDigest`.

**M1-A7** reuses the M0 configurations O0–O5. The breakpoint in O2 pauses on every CPU `Commit` event, and the probe in O4 inspects every component.

**Digests:** `StateDigest`, `ExecutionDigest`, and `TraceDigest` are exactly as in M0 (m0-design §9.2). The M1 golden file records, per committed program: `image_hash`, halt reason, `instret`, the UART output's BLAKE3, the event count, and the three digests.

### 10.2 `riscv-tests` Under a SystemScope Environment

**Why a custom environment:** the upstream `p` environment (`env/p/riscv_test.h`) is not CSR-free.

- Its `RVTEST_CODE_BEGIN` writes `mtvec`, `mstatus`, `satp`, PMP, and delegation CSRs, reads `mhartid`, and enters the test with `mret`.
- `RVTEST_PASS`/`RVTEST_FAIL` end with an `ECALL` that an M-mode trap handler services.

The test bodies themselves use no `SYSTEM` instructions, so M1 replaces only the environment.

**`tests/rv32/env/riscv_test.h`:**

- `RVTEST_CODE_BEGIN` sets every register to 0 and falls into the test. It uses no CSRs and no `mret`.
- `RVTEST_PASS` is `fence; li gp, 1; li a7, 93; li a0, 0; <tohost store>; ecall`.
- `RVTEST_FAIL` is the same with `a0 = (TESTNUM << 1) | 1`.
- `RVTEST_DATA_BEGIN` provides the `tohost` and `fromhost` symbols. The `tohost` store is how Spike stops (§10.3).
- A linker script places the image at `0x8000_0000`.
- The upstream `test_macros.h` and test sources are used unmodified.

**Pass rule:** the run halts with `Trap(EnvironmentCall)`, and then `gp == 1` and `a0 == 0`. Any other halt, including `InstructionLimit`, fails the test and reports `a0 >> 1` as the failing test number.

**Selected tests:** the upstream `rv32ui` list has 42 tests. M1 excludes two:

- `fence_i` needs Zifencei.
- `ma_data` requires misaligned loads and stores to return data, which contradicts §6.

The remaining 40:

`simple` `add` `addi` `and` `andi` `auipc` `beq` `bge` `bgeu` `blt` `bltu` `bne` `jal` `jalr` `lb` `lbu` `lh` `lhu` `lw` `ld_st` `lui` `or` `ori` `sb` `sh` `sw` `st_ld` `sll` `slli` `slt` `slti` `sltiu` `sltu` `sra` `srai` `srl` `srli` `sub` `xor` `xori`

The exclusions are recorded in the fixture manifest with their reasons. Adding or removing a test is a manifest change reviewed like a golden change.

### 10.3 Spike Differential

Spike runs each compared program with `--log-commits`, 16 MiB of memory at `0x8000_0000`, and the narrowest ISA string it accepts for RV32I. That string and the Spike commit are pinned in the manifest. The harness compares SystemScope's `rv32.commit` records with Spike's commit log.

- **Start:** the first instruction at the ELF entry. Spike's boot ROM (reset vector `0x1000`), and any CSR or M-mode setup outside the compared program, are ignored.
- **Compared per retired instruction:** `pc`, instruction bits, the architectural register write (`rd`, value; writes to `x0` are not compared), and memory writes (`addr`, width, value).
- **Stop:** the agreed termination, a store to `tohost`, which Spike's HTIF uses to exit. That store is the last compared instruction. SystemScope then continues to the `ECALL` and halts.
- **Traps:** comparison also stops at the first trap. Both sides must report the same trapping `pc`, and records after it are not compared. Spike has privileged machinery and would continue into a handler; SystemScope halts.
- **Misaligned policy:** the Spike configuration must trap on misaligned accesses, as SystemScope does. The harness checks this once with a dedicated misaligned-access program.
- **Programs exercising MMIO** (the UART) are not compared with Spike. Spike's device map differs.

**Random programs** come from `tests/rv32/progen`, a Rust generator that writes RV32I machine code and a minimal ELF directly, with no assembler.

- It is deterministic: the program is a function of its seed, drawn from a xoshiro256\*\* stream derived as in m0-design §5.2 with the context `"SystemScope 2026-09 rv32 progen v1"`.
- Programs are straight-line code with forward branches only, so they always terminate.
- Loads and stores go to aligned addresses inside a data window, so they never trap, apart from dedicated trap programs.
- Every program ends with the termination sequence.
- A fixed set of seeds runs on every CI run. One random seed runs nightly and is printed, as in M0.

### 10.4 ACT4 and Sail

ACT4 builds self-checking ELFs: it runs each test on the Sail reference model, configured like the DUT, and compiles the expected results into the test. For SystemScope the DUT configuration provides:

- `rvmodel_macros.h`:
  - `RVMODEL_HALT_PASS` and `RVMODEL_HALT_FAIL` are expressed through the §10.2 convention (`a0`, then `ECALL`).
  - No trap handler is provided.
- A UDB configuration declaring RV32I only, with misaligned accesses trapping.
- `include_priv_tests: false`, set from the start. ACT4 then leaves out every test that depends on privilege modes.
- A linker script for `0x8000_0000`.

**Open risk, to settle at M1.10:** with privilege tests excluded, whether `rvmodel_macros.h` or any remaining unprivileged test still needs Zicsr or a trap handler. If they do, the affected tests are listed as exclusions with reasons, exactly as in §10.2. SystemScope does not gain CSRs to run a test harness.

### 10.5 CI

- **Blocking, both operating systems:** everything in M0 CI (including the M0 acceptance tests and the golden-unchanged check), then:
  - M1-A1, M1-A2, M1-A4, M1-A5, M1-A6, M1-A7;
  - M1-A8 against the committed fixtures.

  These need no external tools: they run the committed ELFs.
- **Blocking, Linux only:**
  - M1-A3 Spike lockstep, with Spike built from its pinned commit and cached;
  - a check that every committed fixture matches its manifest hash.
- **Nightly:** the random-seed program through Spike on Linux, and the M1 acceptance tests for that program on both operating systems.
- The random-seed program has no golden digests, as in M0.

### 10.6 Fixtures

The toolchain needed to build ELFs (a RISC-V GCC or Clang), Spike, Sail, and ACT4 run only on Linux. So ELFs are built once and committed:

- **Contents:** `tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs, the ACT4 ELFs, `hello.elf`, and the dedicated trap programs.
- **`manifest.json`** records, for each fixture, its name, BLAKE3, and source. It also records the pinned versions of the toolchain, `riscv-tests`, ACT4, Sail, and Spike, plus the exclusions with their reasons.
- **Rebuilding** is a deliberate, Linux-only step, handled like `cargo xtask bless`:
  - a script rebuilds everything from the pinned versions and rewrites the manifest;
  - the commit explains why.
- **Tests never build fixtures.** They only read them, so Windows and Linux run byte-identical programs.
- **Licensing:** upstream license notices are kept next to the fixtures derived from `riscv-tests` and ACT4.

---

## 11. M1 Exit Criteria

- [ ] `mem.v1` is in `contracts`, with rustdoc, golden encoding vectors, and strict decoding. `mem.v0` is unchanged.
- [ ] `systemscope-rv32i` implements all 40 RV32I instructions and every trap of §6.
- [ ] `systemscope-platform` implements `AddressBus`, `Ram` (sparse pages), and `SimpleUart`. `systemscope-elf` loads and validates ELF32 images.
- [ ] **M1-A1 to M1-A8 pass in CI on Linux and Windows,** with M1-A3 on Linux.
- [ ] The M0 golden digests are byte-identical to `v0.1.0-m0`, and the serialized compatibility id is decoupled from the crate version (§4.5).
- [ ] The fixture manifest pins every external tool version and lists every exclusion with its reason.
- [ ] Contract changes discovered during M1 are reflected back into this document.

---

## 12. Implementation Order

1. **M1.0:** this document; the `plan.md` M1 update; in `contracts`, the compatibility id (§4.5) and the `mem.v1` contract, followed by a pin bump in `systemscope`, with the M0 golden digests unchanged.
2. **M1.1:** `decode`, the immediate extractors, and the register file, with `IllegalInstruction` from the start.
3. **M1.2:** the ALU instructions, with the independent test interpreter.
4. **M1.3:** branches and jumps, including misaligned-target traps.
5. **M1.4:** `AddressBus` and `Ram`; the `Rv32iCpu` component fetching through memory; loads, stores, misaligned and access-fault traps.
6. **M1.5:** the ELF loader.
7. **M1.6:** the SystemScope `riscv-tests` environment, the fixture pipeline, and the 40 `rv32ui` tests. This comes early because it is the strongest oracle available.
8. **M1.7:** `SimpleUart` and `hello.elf`.
9. **M1.8:** M1-A6 and M1-A7, the `m1-reference` golden file, and the portable snapshot.
10. **M1.9:** the random-program generator and the Spike differential.
11. **M1.10:** ACT4 and Sail.
12. **M1.11:** CI, then the M1 exit review, then the tag `v0.2.0-m1`.

The tag follows the M0 convention: a SemVer prerelease identifier marking a milestone. `contracts` gets the same milestone tag on the commit `systemscope` pins.

---

## 13. Open Questions

- **ACT4 prerequisites.** With `include_priv_tests: false`, do `rvmodel_macros.h` or any remaining RV32I test still need Zicsr or a trap handler (§10.4)?
- **Spike configuration.** Which ISA string does the pinned Spike accept for RV32I, and does it exit cleanly on the `tohost` store under the §10.2 environment? To be verified at M1.9 and pinned in the manifest.
- **Spike job placement.** M1-A3 is planned as a blocking Linux job. If building Spike makes CI too slow even with caching, it could move to a prebuilt, pinned binary, but not to nightly-only, since M1-A3 is an exit criterion.
- **`hello.elf` source.** C, which needs the pinned toolchain's libc-free build, or assembly?
