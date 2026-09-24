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
├─ tests/acceptance/      adds the M1 acceptance tests (src/m1/, tests/m1_a*.rs) and the m1-run binary
├─ tests/rv32/
│  ├─ src/runner.rs       builds the m1-reference platform (§9)
│  ├─ src/spike.rs        the Spike differential (§10.3); build-spike.sh builds the pinned Spike
│  ├─ spike/              committed logs from the pinned Spike, for tests without it
│  ├─ env/                the SystemScope riscv-tests environment (§10.2)
│  ├─ fixtures/           committed ELFs and manifest.json (§10.6)
│  └─ progen/             deterministic random-program generator (§10.3, not yet built)
└─ tests/golden/          adds m1-reference.json and m1-reference.mid.snap
```

- **Components depend only on `systemscope-contracts`,** as in M0. `systemscope-rv32i` knows nothing about the memory map, the RAM, or the UART.
- **`systemscope-elf` depends on nothing in the simulation.** It turns bytes into a checked load image (§8). The reference builder passes that image to `Ram` and the entry point to `Rv32iCpu` as construction parameters.
- `.gitattributes` marks `*.elf` as binary, as it does `*.snap`.
- **The `m1-reference` builder lives in the test crate** (`tests/rv32/src/runner.rs`, M1.8), not in `reference/`: it is only ever used to run committed ELFs in tests, and `reference/` keeps only `m0-reference`.

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
- **Requests are never empty:** a `ReadReq` has `len > 0` and a `WriteReq` has non-empty `data`. A zero-length request is not a memory access and never produces `AccessFault`. It is a protocol violation by the initiator. The first component that receives one (interconnect or target) faults the session with `SimError::ComponentFault`, the same way M0 components treat impossible messages. The wire format can represent one, so the decoder accepts it (§4.3).
- **The range is `[addr, addr + len)`, computed with checked arithmetic.** The last byte is `addr + (len - 1)`, so an access may end exactly at `u64::MAX`. A request whose last byte would lie past `u64::MAX` is well formed but touches no mappable range: it is answered with `AccessFault` and never wraps around. This is different from a zero-length request, which is not answered at all.
- `MemMsg::access()` classifies a request once for every receiver: `Access::Bytes { first, last }` (inclusive), `Access::OutOfRange` (answer `AccessFault`), or `Access::Empty` (fault the session). It returns `None` for responses.
- A `Data` response carries exactly `len` bytes. A shorter or longer one faults the session: that is a component bug, not an architectural event.
- A faulted write changes nothing at the target.
- `MemFault` is deliberately generic. The target reports only that the access failed; the CPU decides which trap it is, from the operation that was outstanding (§6).

### 4.3 Encoding

`mem.v1` follows the M0 primitive rules (m0-design §4.5) with no new rules. `ReadOutcome`, `WriteOutcome`, and `MemFault` are ordinary enums: a `u8` tag in declaration order, then the variant's fields. For example, `ReadResp { txn, outcome: Fault { fault: AccessFault } }` encodes as tag `1`, `txn` as `u64`, outcome tag `1`, fault tag `0`.

- Tags: `ReadReq` 0, `ReadResp` 1, `WriteReq` 2, `WriteResp` 3; `ReadOutcome` `Data` 0, `Fault` 1; `WriteOutcome` `Done` 0, `Fault` 1; `MemFault` `AccessFault` 0. The contracts tests pin them with hand-written golden bytes.
- Decoding is byte-level only. It accepts zero-length requests and a `Data` response of any length, including empty, because checking them needs the request or the topology. Receivers reject empty requests (§4.2), and the initiator checks the `Data` length against its outstanding request.
- `canonical(ev)` already carries the protocol name and version before the payload, so `("mem", 1)` messages can never collide with `("mem", 0)` ones.
- `Message` gains a variant, `Message::MemV1(mem_v1::MemMsg)`. The `Message` variant tag is not part of any encoding, so existing encodings do not change.
- The strict decoder dispatches on `(name, version)`: `("mem", 0)` to `mem::MemMsg`, `("mem", 1)` to `mem_v1::MemMsg`, and anything else is rejected as before.
- Any later change to these messages, including a new `MemFault` variant, is a new protocol version.

**`runtime.dispatch` fields for `mem.v1`** follow the M0 rule: the message's fields in declaration order. A nested outcome is flattened: `outcome` as Str (the variant name, `Data`, `Done`, or `Fault`), then that variant's fields (`data` as Bytes, `fault` as Str). This adds rows for a new protocol; no existing record's encoding changes, so the trace `format_version` stays 2.

**Perfetto:** the exporter is unchanged. Every `runtime.dispatch` record, including a `mem.v1` one, becomes an instant event with its flattened fields as args, so a fault response shows `outcome` and `fault` on its own dispatch event. Transaction slices are paired only by the `msg` and `txn` fields, so a `mem.v1` request and its response, faulted or not, are paired the same way as `mem.v0`; the slice itself carries no outcome. M1 adds no fault-specific slice semantics.

### 4.4 M0 Is Frozen

- `mem.v0`, `ToyCpu`, `ToyDma`, `ToyBus`, `ToyMemory`, and `m0-reference` are not modified in M1. The one exception is compile-only: where a toy component matches on `Message`, it gains an arm for `Message::MemV1` that faults the session. The arm is unreachable, because every send is checked against the port's protocol (m0-design §6) and the toy ports are all `mem.v0`.
- **The M0 golden digests must stay byte-identical** for the whole of M1. The M0 acceptance tests keep running in CI, and they already fail on any change to `tests/golden/m0-reference.json`.
- **The serialized contracts identifier does not change** (§4.5), so adding `mem.v1` cannot move the M0 `StateDigest` or `TraceDigest`.

### 4.5 Contracts Compatibility Id

In M0, the string written as `contracts_version` into `SessionInfo` (m0-design §7) and the trace header (m0-design §8.1) is the contracts crate's Cargo version (`env!("CARGO_PKG_VERSION")`, currently `"0.0.0"`). It feeds `StateDigest` and `TraceDigest`. Tying it to the package version would mean the crate's SemVer version could never change without moving the M0 golden digests.

M1.0 separates the two:

- **The crate version is ordinary SemVer.** It changes when a contracts release is cut, and never reaches a digest. Consumers pin contracts by git revision, not by version, so M1.0 adds `mem.v1` without bumping it (it stays `0.0.0`).
- **The serialized value becomes a fixed compatibility id,** the constant `systemscope_contracts::COMPATIBILITY_ID`, independent of Cargo. It keeps the M0 value `"0.0.0"`. `trace::CONTRACTS_VERSION` remains as an alias under the field's name, so existing callers are unchanged, and a contracts test pins both. The field name, position, and encoding in `SessionInfo` and the trace header are unchanged, so every M0 snapshot and trace stays byte-identical.
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
| `x0` | Always reads 0. Writes to it are discarded. It is not stored: `RegisterFile` holds only `x1` to `x31`, in index order. |
| `pc` | Always 4-byte aligned. With no C extension, a misaligned target traps on the jump or branch that produces it (§6), so `pc` itself never becomes misaligned. |
| Instruction width | 32 bits, little-endian |
| Arithmetic | Wraps modulo 2^32 |
| Comparisons | Signed ones use `i32` semantics, unsigned ones `u32` |
| Shifts | Use the low 5 bits of the shift amount |

### 5.2 Decode

Decoding is a pure function `decode(u32) -> Result<Instr, Illegal>`, total over all 2^32 words. It never panics. `Illegal { word }` keeps the raw word for the trap value (§6). Decode, the immediate extractors, and the register file depend on nothing in the simulation.

- **`Instr` has no representation for an illegal encoding.** Variants follow the formats, with an operation enum where a format has several instructions: `Lui`, `Auipc`, `Jal`, `Jalr`, `Branch { op: BranchOp }`, `Load { op: LoadOp }`, `Store { op: StoreOp }`, `OpImm { op: ImmOp }`, `ShiftImm { op: ShiftOp, shamt }`, `Op { op: RegOp }`, `Fence`, `Ecall`, `Ebreak`. Operands are already extracted, and `decode` produces only in-range immediates and shift amounts, so execution never re-examines `funct3` or `funct7`. `Instr` does not keep the raw word; the CPU keeps it separately for traces and traps.
- **Registers are `Reg`,** an index that is always below 32: the decoder builds it from a 5-bit field, and `Reg::new` rejects 32 and above.
- **Immediates are extracted in one place:** `imm_i`, `imm_s`, `imm_b`, `imm_u`, `imm_j`. No instruction extracts immediate bits on its own. The signed ones return `i32`, already sign-extended, with branch and jump offsets as even byte offsets. `imm_u` returns the `u32` with the low 12 bits zero.
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
- **Architectural state changes only in `Commit`.** The CPU wakes itself in `Commit` of the same tick (`ScheduleWhen::Now`, so the commit stays in the response's tick even if that tick is not a clock edge) and resolves the pending instruction in one of two ways:
  - **It retires:** `x[rd] ← value` if `rd ≠ 0`, then `pc ← next_pc`, then `instret += 1`, and the CPU emits `rv32.commit`.
  - **It traps:** it does **not** retire. Registers, `pc`, and `instret` are unchanged, and the CPU emits `rv32.trap` and halts (§6). This holds for every trap cause, `ECALL` and `EBREAK` included.
- **Memory and device state follow their target's rules,** not the CPU's `Commit`. A store is visible at the RAM when the RAM accepts it, as in M0 (m0-design §9.1), and a UART byte is output when the UART accepts it. With one outstanding operation, no instruction can observe the difference.
- After a commit, the next `FetchIssue` is a wake at the next CPU cycle's `Request` (`Cycles { domain: cpu, k: 1 }`). A data request is scheduled the same way.

This gives observers a precise meaning: `on_after_dispatch` of the `Complete` event shows the state before the instruction commits, and of the `Commit` event the state after.

**Pure execution.** Execution is a pure function of the decoded `Instr`, `pc`, and the values of the source registers, which the caller reads before the instruction and passes in. It reads and changes no architectural state: no register, no `pc`, no `instret`, no runtime call, no event. Its result is a `PendingEffect { reg_write: Option<RegWrite { rd, value }>, next_pc }`, which the CPU holds as pending state and applies only if the instruction retires. An instruction that can trap returns an `ExecOutcome`: either `Effect(PendingEffect)` or `Trap(PendingTrap { cause, tval })`, never both. `PendingTrap` has no `pc`: the trapping instruction's address is the `pc` execution was called with, and the CPU adds it to build the `RvTrap` (§6) in `Commit`. Each instruction family has its own function, and each returns a family-specific error (`NotAlu`, `NotControl`, `NotSystem`, `NotMemory`) for instructions outside its family.

- **ALU instructions** (`LUI`, `AUIPC`, `OP-IMM`, `OP`) go through `execute_alu(instr, pc, rs1, rs2)`, which returns `NotAlu` for every other instruction. Source values an instruction does not use are ignored.
- **Every ALU instruction writes `rd` and sets `next_pc = pc + 4`,** with wrap-around.
- **An `x0` destination stays in the effect.** `RegWrite { rd: x0, .. }` is produced like any other write, so traces keep the instruction's `rd` and HINTs need no special case. The register file discards it when the effect is applied.
- Arithmetic wraps modulo 2^32. `SLT`/`SLTI` compare as `i32`; `SLTU`/`SLTIU` compare as `u32`, with the `SLTIU` immediate sign-extended first. Bitwise immediates use the sign-extended 32-bit pattern. Every shift uses the low 5 bits of its amount, in registers and in `shamt` alike.
- **`FENCE`, `ECALL`, and `EBREAK`** go through `execute_system(instr, pc)`, which returns `NotSystem` for every other instruction. `FENCE` retires as a no-op with `next_pc = pc + 4` (§5.2); `ECALL` returns `Trap(EnvironmentCall, tval 0)` and `EBREAK` returns `Trap(Breakpoint, tval pc)` (§6).
- **Branches and jumps** go through `execute_control(instr, pc, rs1, rs2)`, which returns `NotControl` for every other instruction. Address arithmetic wraps modulo 2^32; a target that wraps past 0 is an ordinary address.
- `BEQ`/`BNE` compare for equality, `BLT`/`BGE` as `i32`, `BLTU`/`BGEU` as `u32`. A branch never writes a register.
- **A branch that is not taken** retires with `next_pc = pc + 4`. It never checks its target, so it never traps, even when `pc + offset` is misaligned.
- **A taken branch** targets `pc + offset`, **`JAL`** targets `pc + offset`, and **`JALR`** targets `(rs1 + offset) & !1`. `JALR` clears bit 0 first and checks alignment afterwards, so a sum ending in `01` is aligned and one ending in `10` or `11` is not.
- **The target must be 4-byte aligned** (no C extension, so IALIGN = 32). If it is, the instruction retires with `next_pc = target`, and `JAL`/`JALR` write `rd = pc + 4`, `x0` included. If not, the result is `Trap(PendingTrap { cause: InstructionAddressMisaligned, tval: target })`, and the link write is not made: it exists only in a retiring `Effect`.
- `JALR` with `rd = rs1` needs no special case: the target comes from the `rs1` value passed in, read before the instruction, and `rd` is written only when the effect is applied.
- **An aligned target is never a control-flow trap,** mapped or not. The branch or jump retires; if nothing answers at the target, the next fetch gets `Fault` and raises `InstructionAccessFault` on that fetch (§6). Target computation is not access validation.
- **Loads and stores** are split in two pure halves, because a memory response comes between deciding the access and knowing the result (M1.4b). Neither half touches the register file, `pc`, or the runtime, and neither sends a message: the CPU does that (M1.4c).
  - `prepare_memory(instr, pc, rs1, rs2)` returns `NotMemory` for every other instruction. For a load or store it computes the **effective address `rs1 + offset` modulo 2^32**, with the sign-extended I-immediate for loads and S-immediate for stores. Wrapping is not a fault; whether anything is mapped there is the bus's business.
  - **Alignment is checked before any request exists.** Byte accesses are always aligned, `LH`/`LHU`/`SH` need `addr % 2 = 0`, and `LW`/`SW` need `addr % 4 = 0`. A misaligned access returns `MemoryPrep::Trap` with `LoadAddressMisaligned` or `StoreAddressMisaligned` and `tval = addr`. No request is planned, so a misaligned store can never write memory.
  - An aligned access returns `MemoryPrep::Request(MemoryPlan)`: a `LoadPlan { rd, addr, width, extension, next_pc }` or a `StorePlan { addr, data, next_pc }`, with `next_pc = pc + 4`. A store's `data` is the low 1, 2, or 4 bytes of `rs2`, little-endian; the upper bits are dropped. A load plan holds no register write yet.
  - `complete_load(plan, outcome)` finishes a load. `Data` of exactly 1, 2, or 4 bytes is assembled little-endian, then **extended after the response**: `LB`/`LH` sign-extend, `LBU`/`LHU` zero-extend, and `LW` keeps the 32-bit pattern. The result is `Effect(PendingEffect { reg_write: Some(RegWrite { rd, value }), next_pc })`.
  - `complete_store(plan, outcome)` finishes a store: `Done` gives `Effect(PendingEffect { reg_write: None, next_pc })`. The RAM made the bytes visible when it accepted the request; the instruction itself retires only in `Commit` after the `WriteResp`.
  - **An access fault is converted after the response.** `Fault { AccessFault }` gives `Trap(PendingTrap { cause: LoadAccessFault or StoreAccessFault, tval: addr })`. The `mem.v1` contract guarantees a faulting write changed nothing at the target.
  - **A malformed response is a model bug, not a trap.** `Data` of the wrong length, or a response of the wrong kind (`complete_memory(plan, msg)` pairs a `LoadPlan` with a `ReadResp` and a `StorePlan` with a `WriteResp`), is a `MemoryCompletionError`. It never becomes `LoadAccessFault` or `StoreAccessFault`; the CPU raises it as `ComponentFault` (M1.4c). Matching the response's `TxnId` is also the CPU's job.
  - **A load to `x0` still accesses memory.** It sends its request, can fault, and completes with `RegWrite { rd: x0, .. }`, which the register file discards. Dropping it would lose the access fault.

**Reset:** at `init` the CPU schedules the first `FetchIssue` at tick 0 (`Cycles { domain: cpu, k: 0 }`, `Request`), with `pc = entry` and every register 0. `Rv32iCpu::new` rejects a misaligned `entry`.

**The CPU component (M1.4c).** `Rv32iCpu` lives in `components/rv32i/src/cpu.rs` and depends only on contracts. It implements no instruction semantics: a fetched word goes through `decode`, then through exactly one of `execute_alu`, `execute_control`, `execute_system`, or `prepare_memory`; a data response goes through `complete_memory`.

- **States.** `FetchIssue`, `FetchWait { txn }`, `MemIssue { insn, plan }`, `MemWait { txn, insn, plan }`, `CommitPending { insn, outcome }`, `Halted(reason)`. `insn` in `CommitPending` is absent only after a faulting fetch. Every state but `Halted` waits for exactly one runtime-owned event: a wake (`FETCH`, `MEMORY`, or `COMMIT`) or the response to the one outstanding request. A wake whose token does not match the state faults the session.
- **Correlation.** Every request takes the next value of a CPU-owned `TxnId` counter, which is consumed only when the send succeeds. A response is accepted only in `FetchWait` or `MemWait`, only for the `TxnId` that state holds, and only in `Complete`. Anything else faults the session: a wrong, stale, or duplicate `TxnId`, or a response while nothing is outstanding. A duplicate response therefore never causes a second commit.
- **Fetch responses.** `Data` of exactly 4 bytes is the instruction word. `Fault` becomes `InstructionAccessFault` with `tval = pc`. `Data` of any other length, or a `WriteResp`, faults the session.
- **Data responses.** `complete_memory` pairs the response with the plan, so a `ReadResp` to a store or a `WriteResp` to a load is a `MemoryCompletionError`, as is load data of the wrong length. The CPU raises every `MemoryCompletionError` as `ComponentFault` and never converts it into an access fault.
- **Trap versus session fault.** A trap is an outcome of the instruction: a fetch `Fault`, an illegal word, a misaligned target or access, `ECALL`/`EBREAK`, or a data `Fault`. It is recorded in `Commit`, changes no architectural state, and halts the CPU. A session fault (`SimError::ComponentFault`) is a model bug. It is returned from the handler that sees it, before any state changes, and neither retires nor traps. Besides the cases above, a request arriving on the initiator port and a message that is not `mem.v1` also fault the session.

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

The CPU's snapshot (schema 1) holds only CPU-owned state:

- its configuration: the clock domain, `entry`, and `max_instructions`;
- `pc`, `x[1..32]`, `instret`, and the next `TxnId`;
- the execution state, with its contents:
  - `FetchIssue`, `FetchWait { txn }`, `MemIssue { insn }`, `MemWait { txn, insn }`, `CommitPending { insn?, outcome }`, or `Halted(reason)`, where `outcome` is the pending `PendingEffect` or `PendingTrap` and a trap halt keeps its `cause`, `pc`, and `tval`;
  - the raw instruction bits, never a decoded form or a memory plan. Decoding and `prepare_memory` are pure functions of the bits, `pc`, and the registers, none of which change while an instruction is pending, so restore recomputes the plan.

**The pending event is never in the CPU's snapshot.** The wake the CPU scheduled, the request it sent, and the response on its way back all live in the runtime's queue, whose snapshot holds them. `MemWait` means "the request has been sent": restoring it waits for the response already in the queue. Restore takes no context and cannot send, so **a store in flight at a checkpoint is never reissued**: the RAM receives it exactly once, whether or not the run was checkpointed.

Restore rejects:

- a configuration different from the elaborated CPU's;
- a misaligned `pc`;
- an outstanding `txn` other than the latest one issued (next `TxnId` − 1), since at most one request is ever outstanding;
- a pending memory instruction whose bits are not an aligned load or store at the recorded `pc` and registers;
- a pending outcome the instruction could not produce: for a non-memory instruction, anything but the pure result; for a load, a write to another register, another `next_pc`, or an access fault at another address; for a store, anything but a retirement or the matching access fault; after a faulting fetch, anything but `InstructionAccessFault` at `pc`;
- a trap halt whose `pc` is not the architectural `pc`, and an `instret` inconsistent with the instruction limit.

A checkpoint may fall anywhere in an instruction: before a fetch, while a fetch is in flight, between `Complete` and `Commit`, before a data request is sent, or while a load or store is in flight. The `systemscope-rv32i` tests resume from every event boundary of a program that covers each of these; AT-2 covers them for `m1-reference` (§10.1).

### 5.7 Inspect and Trace

`inspect()` shows `pc`, `x1`…`x31`, `instret`, and `state` (`fetch_issue`, `fetch_wait`, `mem_issue`, `mem_wait`, `commit_pending`, or `halted`). When halted it adds `halt` (`trap` or `instruction_limit`) and, for a trap, `cause`, `trap_pc`, and `tval`.

Canonical trace records are kept to what later tools need. The runtime's `runtime.dispatch` records already show every fetch and data transaction.

| Kind | When | Fields, in order |
|---|---|---|
| `rv32.commit` | each retired instruction, in `Commit` | `pc` U64 · `insn` U64 · `rd` U64 (0 when nothing is written) · `rd_value` U64 · `next_pc` U64; then, for loads, `addr` U64; for stores, `addr` U64 · `width` U64 · `value` U64 |
| `rv32.trap` | a trapping instruction, in `Commit` | `pc` U64 · `insn` U64 · `cause` Str · `tval` U64 |
| `rv32.halt` | an instruction-limit halt, in `Commit` | `instret` U64 |

- A write to `x0` writes nothing, so its record has `rd = 0` and `rd_value = 0`.
- Load and store details are computed from the instruction and the registers before the commit applies, so a load whose `rd` is its base register still reports the address it read.
- A trap after a faulting fetch has no instruction word; its `insn` is 0.

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
- Pure execution reports a trap as `PendingTrap { cause, tval }` (§5.3), and the CPU adds `pc` when it builds the `RvTrap`. Since M1.4c, `TrapCause` in code has all nine causes. `TrapCause::name` gives the `rv32.trap` spelling, which is the name used in this section.
- The CPU records the trap in `Commit` and enters `Halted(Trap)`. In M2 and M3, a privileged backend connects this same boundary to real machine or supervisor traps.

| Cause | Raised by | `tval` |
|---|---|---|
| `InstructionAddressMisaligned` | A taken branch, `JAL`, or `JALR` whose target is not 4-byte aligned. It is raised on that instruction, which does not write `rd`. A not-taken branch never raises it, and an aligned but unmapped target does not either. | the target, with bit 0 already cleared for `JALR` |
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
- **An access fault is architectural; a malformed response is not.** `LoadAccessFault` and `StoreAccessFault` come only from a `Fault { AccessFault }` response to an aligned access. A response of the wrong length or kind is a component bug and faults the session (§5.3); it never becomes a trap.
- **Precise memory traps.** A misaligned access traps before its request is sent; an access fault traps when the `Fault` response arrives. Either way the instruction does not retire: registers, `pc`, and `instret` are unchanged. For stores, memory is unchanged too: a misaligned store sends nothing, and a faulting store is rejected by the bus or target without writing. A store that the target accepts is visible from acceptance, and the instruction retires in `Commit` after its `Done` response.
- The `tval` values follow the RISC-V privileged convention, so they compare directly with Spike and Sail.

---

## 7. Platform Components (`systemscope-platform`)

### 7.1 AddressBus

The only component that knows the memory map. Implemented in M1.4a.

- **Ports:** `cpu` (`mem.v1` target), then one `mem.v1` initiator port per region, named after the region, in region order.
- **Regions:** `(name, base, size)`, given at construction. A region covers the half-open range `[base, base + size)`; its last byte is `base + (size − 1)`, so a region may end exactly at `u64::MAX`. The constructor rejects:
  - a zero size;
  - a region whose last byte would be past `u64::MAX`;
  - overlapping regions (adjacent regions are fine);
  - duplicate names, and the name `cpu`, which would collide with the upstream port.
- **Routing:**
  - A request whose whole range `[addr, addr + len)` lies inside one region is forwarded on that region's port, in `Transfer`, with `addr − base`. Targets therefore see offsets and can be reused at any base. All range arithmetic is checked; `addr = u64::MAX`, `len = 1` is a valid one-byte access.
  - The bus never splits a request. A request that hits no region, runs past the end of its region, crosses into another region, or runs past `u64::MAX` is answered by the bus itself with a `Fault { AccessFault }` response in `Complete` of the same tick. It is not forwarded and leaves nothing outstanding.
- **Session faults (`ComponentFault`).** An `AccessFault` is an architectural outcome the CPU turns into a trap (M1.4b). A protocol violation is a model bug and faults the session instead:
  - a zero-length request (`MemMsg::access()` is `Empty`);
  - a request that arrives in a phase other than `Request`;
  - a request that reuses a `TxnId` that is still outstanding;
  - a response on the `cpu` port, or a request on a region port;
  - a response for an unknown `txn` (including a second response for the same `txn`), on the wrong region's port, or of the wrong kind (a `WriteResp` for a read, or the reverse).
- **`TxnId`s:** the bus has one upstream port, so it forwards the CPU's `TxnId` unchanged. It records `txn → (region, read or write)` in a `BTreeMap`, to check each response's port and kind.
- **Responses** are relayed upstream unchanged, in the phase they arrived in (`Complete` for the platform's targets).
- **Snapshot:** the configuration (every region's name, base, and size, which also fix the port mapping), then the outstanding map in ascending `txn` order. Restore rejects a different memory map and a non-canonical outstanding map. Forwarded requests and relayed responses in flight live in the runtime's event queue, not in the bus.
- **Inspect:** the number of regions and of outstanding transactions.
- **Trace:** `platform.bus.fault` with fields `txn`, `addr`, `len` (all `U64`, in that order; `len` is the payload length for writes) whenever the bus answers a request itself, and never otherwise.

### 7.2 Ram

Implemented in M1.4a.

- **Port:** `mem` (`mem.v1` target). Addresses are offsets into the RAM.
- **Configuration:** `size` (1 byte to 4 GiB), the response latency, and the initial image from the loader (§8), identified by `image_hash`. The image is a list of `(offset, bytes)` segments; the constructor rejects segments past `size` and overlapping segments.
- **Storage is sparse:** a `BTreeMap<u32, Box<[u8; 4096]>>` of pages, indexed by offset / 4096. Unmapped pages read as zero. Accesses may cross page boundaries.
- **Canonical at all times:** the map never holds an all-zero page. A write that leaves a page all zero removes it, and a write of zeros to an absent page allocates nothing. The initial image is stored the same way.
- **Semantics:**
  - A request is accepted when it is dispatched. Writes become visible and reads are sampled at acceptance, as in `ToyMemory`.
  - The response follows after the fixed latency (§9), in `Complete`. The response waits in the runtime's event queue; the RAM keeps no copy of it.
  - A request not wholly inside `[0, size)` gets a `Fault { AccessFault }` response, and a faulting write changes nothing. The bus should never send one, but the RAM does not rely on that. A zero-length request faults the session.
- **Snapshot:**
  - Contents: `size`, `image_hash`, the latency, then every page holding at least one non-zero byte, as `(index, 4096 bytes)` in ascending page order.
  - All-zero pages are omitted. The same memory contents therefore always encode to the same bytes, whatever history produced them (m0-design §7).
  - Restore rejects a different `size`, `image_hash`, or latency; pages out of order, duplicated, or out of range; pages that are not 4096 bytes; all-zero pages; and non-zero bytes past `size` in a partial last page.
  - **Restore replaces the whole memory.** It first clears every page, including those loaded from the initial image at construction, then inserts the snapshot's pages. An omitted page means "all zero now", never "as in the initial image". Otherwise a program that zeroed an image page would see it reappear after a restore.
  - The initial image is used only by a new session. Restore never reads it; only `image_hash` is compared.
- `inspect()` shows `size`, `image_hash`, and the number of non-zero pages, never the contents. The RAM emits no trace records.

### 7.3 SimpleUart (F1)

Implemented in M1.7a (`components/platform/src/uart.rs`), as a standalone component. M1.7b wires it into the CPU platform of §9 and prints `hello.elf` through it (§10.7), with no change to the component.

A minimal, SystemScope-specific device. It is not a 16550: no receive path, FIFO, baud rate, timing model, or interrupts. It has one `mem.v1` target port, `mem`, and a window of 8 bytes. Offsets are UART-relative, as the bus forwards them.

| Offset | Access | Behavior |
|---|---|---|
| `0x0` TX | write, exactly 1 byte | Appends the byte to the output buffer and responds `WriteOutcome::Done` |
| `0x4` STATUS | read, 1, 2, or 4 bytes | Reads as the `u32` `1` (bit 0: TX ready, always set), little-endian: `[01]`, `[01, 00]`, or `[01, 00, 00, 00]` |

- **Every other access gets a `Fault { AccessFault }` response and changes nothing:** reads of TX, writes of any width other than 1 at TX, writes to STATUS, reads of STATUS with any other width, and any request touching offsets `0x1`–`0x3` or `0x5` and above, including one that starts at a register and runs past it, or whose last byte would lie past `u64::MAX`. Strict decoding keeps device behavior fully specified.
- A zero-length request is a protocol violation, not an access: it faults the session with `ComponentFault`, as for the RAM and the bus.
- **Ordering** follows the RAM (§7.2): a request is accepted when its event is dispatched, a TX byte is appended to the output **at acceptance**, and the response follows after a fixed, configured latency, in `Complete` (§9).
- **Output is raw bytes** (`Vec<u8>`): every value `0x00`–`0xff` is kept as written, never decoded as text. The component exposes it read-only.
- **Snapshot:** the latency, then the output buffer. Restore replaces the output with the snapshot's and rejects a different latency. A pending response is an event in the runtime's queue, not UART state, so a restore neither replays a TX write nor repeats a response: output after a restore is exactly the output at the snapshot.
- **Trace:** `platform.uart.tx` (`byte`), once per accepted TX byte. Faulting requests emit nothing.
- `inspect()` shows `tx_len`, the output length, and `tx_tail`, the last 64 output bytes as Bytes, so the view stays bounded however long a program prints.

A bare-metal program prints with `*(volatile unsigned char *)0x10000000 = 'H';`.

---

## 8. ELF Loader (`systemscope-elf`)

Implemented in M1.5.

The loader is **host-side code, not a component**. It runs before the topology is built, turns an ELF file into a load image, and never touches a running simulation. It has no ports, events, snapshot, or simulation context, and depends on no simulation crate.

```text
load_elf32(elf, ram_base, ram_size) ──▶ LoadImage { segments: [(offset, bytes)], entry, image_hash }
                                           │                                     │
                                           └─▶ Ram::new(size, image)             └─▶ Rv32iCpu::new(entry, max_instructions)
```

- **Supported subset:** `ELFCLASS32`, little-endian (`ELFDATA2LSB`), `EV_CURRENT` (in `e_ident` and `e_version`), `EM_RISCV`, `ET_EXEC`, a 52-byte header, and 32-byte program headers. Everything else is rejected, including ELF64, big-endian files, `ET_DYN` (PIE), and `ET_REL`. There is no relocation, dynamic linking, or symbol lookup, and section headers are never read. A test harness that needs a symbol such as `tohost` reads it separately.
- **Only `PT_LOAD` segments are loaded;** other program headers are ignored whatever they contain. Every `PT_LOAD` must have `p_filesz <= p_memsz` and a file range `[p_offset, p_offset + p_filesz)` inside the file. A `PT_LOAD` with `p_memsz == 0` places nothing: after those two checks it is ignored, including its address, as Spike's loader skips it.
- **Load address: `p_vaddr`, which must equal `p_paddr`.**
  - The CPU has no MMU and runs at the addresses the program was linked for, which are `p_vaddr`. Bare-metal loaders such as Spike's (`fesvr/elfloader.cc`) place segments at `p_paddr`.
  - GNU ld and LLD emit `p_paddr == p_vaddr` unless a linker script gives a section a separate load address (`AT>`). Such an image expects something to copy it at run time, which a flat RAM image cannot express.
  - Requiring both to agree gives one meaning to every accepted file, and SystemScope and Spike place it identically. A loaded segment where they differ is rejected.
- **The segment's memory is `p_memsz` bytes:** the `p_filesz` bytes from the file, then zeros up to `p_memsz` (`.bss`). The zeros are materialized in the load image, so the RAM receives the whole segment.
- **Placement:** each loaded segment must fit in the 32-bit address space (`p_vaddr + p_memsz <= 2^32`) and lie entirely inside the RAM region `[ram_base, ram_base + ram_size)`. Loaded segments must not overlap, in any header order; adjacent segments are fine. The region itself must be non-empty and end at or below `2^32`.
- **Offsets are RAM-relative:** `offset = p_vaddr - ram_base`, the RAM's own addressing (§7.2). Segments come out sorted by offset, non-empty, and non-overlapping, whatever the program-header order. The entry point stays an absolute address, since the CPU fetches through the bus.
- **The entry point** must be 4-byte aligned (no C extension; `Rv32iCpu::new` rejects the same) and inside a loaded segment's memory, `.bss` included. Being inside the RAM is not enough. Segment permissions (`p_flags`) are not checked, as the CPU has no memory protection.
- **`image_hash`** is the BLAKE3 of the original ELF file's bytes, not of the load image. It identifies the program:
  - The RAM stores it in its snapshot, so a snapshot can never be restored under a different program (m0-design §6: component parameters live in the component's snapshot, not in `topology_hash`).
  - Golden files are keyed by it.
  - `LoadImage.image_hash` is the value `RamImage.image_hash` takes; turning a `LoadImage` into a `RamImage` only widens each offset to `u64`.
- **Untrusted input:** every field is read through bounds-checked, explicitly little-endian helpers, and all arithmetic on file values is checked or done in `u64`. Malformed input returns an error, never a panic, and the result does not depend on the host's byte order or word size. Segments are checked against the RAM before any is built, so memory use is bounded by `ram_size`.
- **Errors** (`ElfError`) name the failing check: malformed header, bad magic, unsupported class, byte order, version, type, or machine, malformed program-header table, invalid segment (file size, file range, address mismatch, address overflow), invalid RAM region, segment outside the RAM, overlapping segments, and misaligned or out-of-segment entry. Segment errors carry the program-header index.
- The loader is hand-written. It covers only this subset of ELF32, which keeps the dependency set to `blake3`, already in the workspace.
- **Tests** build ELF files at run time with a test-only writer that shares no code with the parser, and compare against an oracle computed from the writer's own description. They cover each rule and its boundaries, property tests over valid layouts, targeted defects, and arbitrary bytes, and hand the result to the real `Ram` and `Rv32iCpu` in the real runtime. These tests commit no ELF files; the committed `rv32ui` fixtures (§10.6) are loaded by the same loader.

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

**Status (M1.8):** `tests/rv32/src/runner.rs` is the `m1-reference` builder (`runner::platform`, and `runner::platform_with_seed` for the seed checks), with component ids `soc.cpu0` = 0, `soc.bus` = 1, `soc.ram` = 2, and `soc.uart` = 3, and the session seed `runner::SEED` = 0. The `rv32ui` runs leave out the UART region, which none of them touches, and `hello.elf` runs include it. The golden file and the portable snapshot are built on it (§10.1).

The seed reaches only the session information: the seed recorded in the snapshot and the trace header, and the components' RNG streams, which nothing draws from. M1-A8 checks this with M0's other fixed seeds (`1`, `0xDEADBEEF`) on `hello` and `ld_st`: the event count, `ExecutionDigest`, every trace record, and every component's state equal seed 0's, while `StateDigest` and `TraceDigest` change with the recorded seed. No check requires that different seeds give different digests.

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

**Status (M1.8):** M1-A6 to M1-A8 are implemented in `tests/acceptance` (`src/m1/`, `tests/m1_a6.rs`, `tests/m1_a7.rs`, `tests/m1_a8.rs`), on the §9 builder. The canonical workload is `hello.elf`; the committed programs are `hello` and the 40 selected `rv32ui` tests (§10.2), which remain the M1-A2 regression suite.

- **Golden file** `tests/golden/m1-reference.json`, separate from the M0 golden files, which are unchanged. It records the compatibility id and the seed, and for each program, in a fixed order: the ELF's BLAKE3 and the RAM's `image_hash`, the halt cause, trap `pc` and `tval`, `instret`, the event count, `pc` and `x1`–`x31` at the end, the UART bytes (hex) and their BLAKE3 (`hello` only), and the three digests. It is rendered by hand: fixed field order, lowercase hex, LF line ends, no paths or timestamps. `hello` ends in `Trap(EnvironmentCall)` with `gp` = 1, `a0` = 0, `instret` = 87, 728 events, and exactly `Hello, SystemScope!\n`.
- **Portable snapshot** `tests/golden/m1-reference.mid.snap` (8,942 bytes): `hello` after 355 events, right after the UART accepts its tenth byte, with that byte's `WriteResp` the only queued event. This point exercises every component and the reissue hazard at once: a wrong restore would print a byte twice, send the store again, or lose the response. The golden file records its program, checkpoint, event index, seed, compatibility id, `image_hash`, size, and BLAKE3. Its check is M0 AT-2 step 4 plus the UART: bytes, restore, round-trip, then a run with no trace prefix to the golden event count, `StateDigest`, `ExecutionDigest`, registers, the full output, and exactly the ten remaining UART writes.
- **M1-A6 checkpoints** on `hello` (728 events) and `ld_st` (7,718 events, the long `rv32ui` test) are found structurally from each run's CPU states and messages, and checked to be what their names say. `ld_st` has no UART, so it has no UART checkpoint. Each snapshot is dropped and restored into a freshly elaborated platform, which resumes the trace prefix; the resumed run must give the same final digests, trace bytes, registers, and UART output, and replay exactly the uninterrupted run's remaining events. The M1.7b every-event checkpoint test for `hello` (M1-A5) stays.
- **M1-A6 rejections:** another ELF, another topology, another seed or compatibility id, and doctored CPU, RAM, UART, and bus configurations are refused by the existing restore rules; so are truncated, extended, and bad-magic snapshots and impossible CPU states.
- **M1-A7:** O1–O5 end exactly like O0 for `hello` and `ld_st` (M0's `ensure_invariant`: digests, RNG, scheduler, every component), O1 and O5 record the same, golden trace, and O2 pauses once per CPU `Commit` (88 for `hello`, 963 for `ld_st`).
- **M1-A8:** a fresh run of every program equals the golden file field by field and byte for byte, in the test process and in two `m1-run` processes; fresh platforms repeat every field; the seed matrix is §9's.
- **Cross-OS:** each CI test job emits its own result and snapshot (`cargo xtask m1-golden emit`), and the `m1-cross-os` job has Windows check the Linux result and Linux check the Windows one (`cargo xtask m1-golden check`): equal to the committed files and the local run, and the foreign snapshot restores to the golden end.

**Status (M1.9):** M1-A3 is implemented for the 40 `rv32ui` ELFs (§10.3): every selected test retires exactly as on the pinned Spike, 13,268 retirements in all. The deterministic random programs and the misaligned-access program of §10.3 are not built yet, so M1-A3 is not complete; they are known limitations until then.

### 10.2 `riscv-tests` Under a SystemScope Environment

**Why a custom environment:** the upstream `p` environment (`env/p/riscv_test.h`) is not CSR-free.

- Its `RVTEST_CODE_BEGIN` writes `mtvec`, `mstatus`, `satp`, PMP, and delegation CSRs, reads `mhartid`, and enters the test with `mret`.
- `RVTEST_PASS`/`RVTEST_FAIL` end with an `ECALL` that an M-mode trap handler services.

The test bodies themselves use no `SYSTEM` instructions, so M1 replaces only the environment.

**`tests/rv32/env/riscv_test.h`**, derived from `env/p/riscv_test.h` of the pinned `riscv-tests` (§10.6):

- `RVTEST_CODE_BEGIN` sets every register to 0 (upstream `INIT_XREG`), sets `TESTNUM` (`gp`) to 0, and falls into the test. It uses no CSRs, no `mret`, and no trap handler: the upstream trap vector, `mhartid` check, `satp`/PMP/delegation setup, `mtvec`/`mstatus` writes, and `mret` are gone.
- It also drops upstream `CHECK_XLEN`, which jumps to `RVTEST_PASS` when the XLEN check fails. On SystemScope that branch could only turn a broken `slli` or `bltz` into a pass.
- `RVTEST_PASS` is `fence; li gp, 1; li a7, 93; li a0, 0; <tohost store>; ecall`.
- `RVTEST_FAIL` is `fence; gp = (gp << 1) | 1; li a7, 93; a0 = gp; <tohost store>; ecall`, so `a0 = (TESTNUM << 1) | 1`.
- The `<tohost store>` is upstream's `write_tohost`: `sw gp, tohost` and `sw zero, tohost + 4`. On SystemScope it is an ordinary RAM write before the `ecall`. It is there so Spike can run the same ELFs and stop on it (§10.3); whether Spike does is verified at M1.9.
- `RVTEST_CODE_END` is an all-zero word, which RV32I reserves as illegal, instead of upstream's `unimp`: without the C extension the assembler encodes `unimp` as `csrrw x0, cycle, x0`, a Zicsr instruction. Nothing executes it.
- `RVTEST_DATA_BEGIN` and `RVTEST_DATA_END` are upstream's: the `tohost` and `fromhost` symbols in a `.tohost` section, and the signature labels.
- The upstream `test_macros.h` and test sources are used unmodified.

**`tests/rv32/env/linker.ld`**, derived from `env/p/link.ld`: `.text.init` at `0x8000_0000` (the entry, `_start`), then `.tohost`, `.text`, `.rodata`, `.data`, and `.bss` at fixed alignments, with no `AT>`, so every `PT_LOAD` has `p_paddr == p_vaddr` (§8). Every fixture has two `PT_LOAD` segments, code (R E) at `0x8000_0000` and data (RW) at `0x8000_1000`, and ends below `0x8000_3000`.

**The runner** (`tests/rv32`, crate `systemscope-rv32`) runs each fixture alone on `m1-reference` without the UART (§9): the same CPU, bus, RAM, latencies, seed, and `max_instructions`.

**Pass rule:** the run halts with `Trap(EnvironmentCall)`, and then `gp == 1` and `a0 == 0`. Any other halt, including `InstructionLimit`, a runtime fault, or no halt, fails the test. A failure reports the halt, the trap cause, `pc` and `tval`, `gp`, `a0` with `a0 >> 1` as the failing test number, `instret`, and the ELF's BLAKE3.

**M1-A2 acceptance:** selected == executed == passed == 40 and failed == 0, with the selection read from the manifest and checked against the list below, so a test that silently does not run fails M1-A2 as surely as one that fails. Each run's event count and `StateDigest`, `ExecutionDigest`, and `TraceDigest` are reported, not yet compared with golden values (M1.8). Tracing and an extra observer change none of them.

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

**Status (M1.9):** the differential runs for the 40 committed `rv32ui` ELFs, in `tests/rv32/src/spike.rs`, through `cargo xtask spike build | verify | diff`. `hello.elf` is not compared: it writes to the UART (above).

- **Pin:** Spike (`riscv-software-src/riscv-isa-sim`, BSD-3-Clause) at commit `19609434bb3d83448eec8796e8f0367c868efbda`, version line `Spike RISC-V ISA Simulator 1.1.1-dev`. The commit is pinned in `tests/rv32/build-spike.sh` and in the `spike` module, and a test keeps them equal; it is not in the fixture manifest, which only a fixture rebuild rewrites. Spike is fetched and built by the script, never committed, and never built on Windows.
- **Command:** `spike --isa=rv32i --priv=m --pcs=0:<entry> -m0x80000000:0x1000000 --disable-dtb --log-commits --log=<log> --instructions=10000000 <elf>`. `rv32i` is accepted as is. `--pcs` sets hart 0's `pc` to the ELF entry directly, so the boot ROM never runs; a test pins that the first compared record is at the entry. Without it, `--disable-dtb` leaves no boot ROM and Spike never reaches the program. `--disable-dtb` keeps the device tree, and `dtc`, out of the run; Spike's build still needs `dtc`.
- **Records:** both sides normalize to one record per retirement: index, `pc`, instruction bits, register write, and memory access. SystemScope's side is read from its canonical `rv32.commit` trace records, never by re-executing; Spike's from its commit log, with a strict parser of its own that does not use the SystemScope decoder. Every line must be one hart-0, machine-mode retirement laid out exactly as the pinned Spike writes it; anything else fails. A write to `x0` is no write on either side (SystemScope records `rd = 0`, and Spike leaves it out). Memory is compared as recorded: for a load its address, for a store its address, width, and value. `next_pc` is covered by the next record's `pc` and the stream lengths.
- **Stop:** every `rv32ui` log ends with the environment's `write_tohost` (§10.2), two stores, and that is where Spike's HTIF exits. The compared stream is Spike's whole log, `N` records, and it must start at the entry and end with a word store of 1 to the ELF's `tohost` symbol, one instruction, and a word store of 0 to `tohost + 4`, the last compared record. SystemScope's stream must have exactly `N` records; it then halts on the `ECALL` at the next address with the M1-A2 pass rule (`gp` = 1, `a0` = 0), `instret` = `N`, and `x1`–`x31` equal to Spike's register writes replayed from zero. Spike's exit status is not a verdict (it exits 0 on its instruction limit too): a run also needs empty output and the boundary above.
- **Traps:** no `rv32ui` test traps before the boundary, so a trap there fails the differential. Comparing trapping `pc`s is for the trap programs, not yet built.
- **Mismatches** report the test, its ELF BLAKE3, the retirement index, both records, and up to three records before and after; a length difference says which stream ends first. Acceptance is 40 selected, 40 run by each side, and 40 matched.
- **Tests without Spike:** the parser runs on real pinned-Spike lines (`tests/rv32/spike/`), including the whole log of `simple`, which SystemScope's run must equal; negative tests change `pc`, instruction, `rd`, its value, and memory fields, or delete or add a record. `cargo xtask spike verify` checks the build's stamp, clean checkout at the pin, and version line, and requires Spike to write the committed `simple` log again, byte for byte.

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
  - M1-A8 against the committed fixtures;
  - `cargo xtask rv32-fixtures verify`, which checks every committed fixture against its manifest (§10.6), and a check that the fixtures are unchanged after the run;
  - `cargo xtask m1-golden verify`, and the `m1-cross-os` job, which restores and checks each operating system's M1 result on the other (§10.1). CI never blesses.

  These need no external tools: they run the committed ELFs.
- **Blocking, Linux only:**
  - M1-A3 Spike lockstep, with Spike built from its pinned commit and cached: the `spike` job builds on a cache miss, then always runs `cargo xtask spike verify` before `cargo xtask spike diff`, so a cached build is used only once it matches the pin (M1.9, for the `rv32ui` ELFs);
  - the fixture rebuild: `cargo xtask rv32-fixtures build` on `ubuntu-24.04` with the pinned toolchain packages, then no difference from the committed fixtures and manifest. This is the only job that installs a RISC-V toolchain or fetches `riscv-tests`.
- **Nightly:** the random-seed program through Spike on Linux, and the M1 acceptance tests for that program on both operating systems.
- The random-seed program has no golden digests, as in M0.

### 10.6 Fixtures

The toolchain needed to build ELFs (a RISC-V GCC or Clang), Spike, Sail, and ACT4 run only on Linux. So ELFs are built once and committed:

- **Contents:** `tests/rv32/fixtures/` holds the 40 `rv32ui` ELFs (`rv32ui-<test>.elf`) and, from later steps, the ACT4 ELFs and the dedicated trap programs. `hello.elf` lives in `tests/rv32/hello/` with its own manifest (§10.7), since `verify` requires the `rv32ui` directory to hold exactly the selection.
- **`manifest.json`** records, for each fixture, its name, BLAKE3, and source. It also records the pinned versions of the toolchain, `riscv-tests`, ACT4, Sail, and Spike, plus the exclusions with their reasons. It is the acceptance contract: the runner takes the selection from it.
- **Rebuilding** is a deliberate, Linux-only step, handled like `cargo xtask bless`:
  - a script rebuilds everything from the pinned versions and rewrites the manifest;
  - the commit explains why.
- **Tests never build fixtures.** They only read them, so Windows and Linux run byte-identical programs.
- **Licensing:** upstream license notices are kept next to the fixtures derived from `riscv-tests` and ACT4.

**`rv32ui` fixtures (M1.6):**

- **Pins:**
  - `riscv-tests` at `793a5ff2d99a6d9fbd91e84c34b9a0437e313b88` (2026-09-22), the head of `master` when M1.6 started, with its `env` submodule at `6de71edb142be36319e380ce782c3d1830c65d68`. It has the 42-test `rv32ui` list of §10.2, each an `rv32ui/<test>.S` wrapper around `rv64ui/<test>.S`. A newer commit comes in only through a deliberate rebuild.
  - Toolchain: the Ubuntu 24.04 packages `gcc-riscv64-unknown-elf` `13.2.0-11ubuntu1+12` and `binutils-riscv64-unknown-elf` `2.42-1ubuntu1+6`, recorded with their `.deb` SHA-256s.
  - Flags: `-march=rv32i -mabi=ilp32 -static -mcmodel=medany -fvisibility=hidden -nostdlib -nostartfiles -Wl,--build-id=none`. These are upstream's flags for the `p` environment, narrowed to RV32I and without a build-id note.
- **Manifest fields:**
  - schema version;
  - repository and commits;
  - distribution, packages, versions, and SHA-256s;
  - flags and the RAM;
  - the BLAKE3 of each build input: `riscv_test.h`, `linker.ld`, the build script, and both license files;
  - per selected test, in name order: its name, wrapper and body sources, ELF file, BLAKE3, the loader's `image_hash`, and entry;
  - the exclusions with reasons.
- **`cargo xtask rv32-fixtures build`** (Linux) runs `tests/rv32/build-fixtures.sh`, which:
  - checks the tool versions;
  - fetches the pinned commit into `target/rv32-fixtures`, the only network access, and checks the checkout is unmodified;
  - compiles and links each selected test twice, from copies at two different paths, and requires identical bytes;
  - checks each ELF: ELF32 RISC-V `ET_EXEC`, entry and `_start` at `0x8000_0000`, RISC-V attributes `rv32i2p1`, and a no-aliases disassembly that contains only RV32I mnemonics (no CSR, `mret`, or `ebreak`) with `fence` and `ecall`;
  - replaces the ELFs.

  xtask then:
  - checks that the upstream `rv32ui` list is exactly the selection plus the exclusions, and that each wrapper includes the recorded body;
  - rewrites the manifest and verifies it.
- **`cargo xtask rv32-fixtures verify`** needs no network or compiler. It checks that:
  - the manifest parses and has exactly the 40 selected and 2 excluded tests;
  - it equals, byte for byte, the manifest generated from the files on disk: each ELF's BLAKE3, the loader accepting it with the recorded `image_hash` and an entry at the RAM base, and each input's hash;
  - the fixture directory holds nothing else.
- **Reproducibility:**
  - Two builds at different paths, and a fresh fetch into another directory, give identical bytes.
  - One source of nondeterminism was found. In a one-step build, GCC passes the linker a random temporary object name, which the linker records as a `FILE` symbol. The script therefore compiles and links separately.
  - The CI rebuild job repeats the build on a clean machine.
- `.gitattributes` marks `*.elf` as binary; the manifest is LF text.

### 10.7 `hello.elf` (M1-A5, M1.7b)

- **Source:** `tests/rv32/hello/hello.S`, RV32I assembly with no C, libc, or runtime. It:
  - loads the UART base `0x1000_0000`;
  - copies the 20 bytes of `Hello, SystemScope!
` from `.rodata` with `LBU` and `SB`, one store to TX (offset 0) per byte, without polling STATUS and without a terminating NUL;
  - ends with the §10.2 convention: `gp = 1`, `a0 = 0`, then `ECALL`.
- **Build:** `tests/rv32/hello/build-hello.sh`, which `cargo xtask rv32-fixtures build` runs after the `rv32ui` build:
  - the same pinned toolchain and flags as the `rv32ui` fixtures (§10.6), and the same `env/linker.ld`;
  - compiles to a fixed object name and links separately, twice in different directories, and requires identical bytes;
  - checks the ELF: ELF32 RISC-V `ET_EXEC`, entry and `_start` at `0x8000_0000`, attributes `rv32i2p1`, and a code section holding only RV32I, with exactly one `sb`, no `sh` or `sw`, and exactly one `ecall`;
  - installs it with mode `0644`.
- **Manifest:** `tests/rv32/hello/manifest.json` records the toolchain, flags, and RAM, the BLAKE3 of `linker.ld`, the build script, and `hello.S`, and the ELF's BLAKE3, `image_hash`, and entry. `cargo xtask rv32-fixtures verify` checks it next to the `rv32ui` manifest, and that the directory holds nothing else. The CI rebuild job rebuilds `hello.elf` too and requires no difference.
- **Run:** the ELF is loaded by `systemscope-elf` into the §9 topology with the UART. The output is read from the UART's inspect view, not from the trace. A run passes only if:
  - it halts with `EnvironmentCall` and the §10.2 PASS convention (`gp = 1`, `a0 = 0`);
  - the output is exactly `Hello, SystemScope!
`, 20 bytes;
  - the bytes in the UART's `platform.uart.tx` trace records equal the output;
  - on a fresh run, the UART received exactly 20 one-byte `WriteReq`s at offset 0, all from CPU stores to `0x1000_0000`, and answered each.
- **Result:** 87 instructions retire (the trapping `ECALL` does not), in 728 events. Observation does not change the output or the digests.
- **Checkpoints:** resuming from the snapshot after every event but the last, including before the first UART write, with a `WriteResp` pending, after ten bytes, and after the last byte before `ECALL`, gives the same output, trace bytes, final state, and `StateDigest`, `ExecutionDigest`, and `TraceDigest`, and replays exactly the remaining events. The portable snapshot and the full M1-A6 list are M1.8.

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

Status: M1.0 through M1.5 are complete. M1.6, the `rv32ui` fixtures and runner, is implemented, and all 40 selected tests pass. M1.7a, the standalone `SimpleUart`, is complete. M1.7b, `hello.elf` printing through the CPU, bus, and UART, is complete (§10.7). M1.8, M1-A6 to M1-A8 with the `m1-reference` golden file and the portable snapshot, is implemented (§10.1). M1.9, the Spike differential for the 40 `rv32ui` ELFs, is implemented (§10.3); the random-program generator and the misaligned-access program it was planned with are not.

1. **M1.0:** this document; the `plan.md` M1 update; in `contracts`, the compatibility id (§4.5) and the `mem.v1` contract, followed by a pin bump in `systemscope`, with the M0 golden digests unchanged.
2. **M1.1:** `decode`, the immediate extractors, and the register file, with `IllegalInstruction` from the start.
3. **M1.2:** the ALU instructions, with the independent test interpreter.
4. **M1.3:** branches and jumps, including misaligned-target traps.
5. **M1.4:** memory, in three steps:
   - **M1.4a:** `systemscope-platform` with `AddressBus` and `Ram` (§7.1, §7.2), tested on their own with a test-only initiator.
   - **M1.4b:** the pure semantics of loads and stores (§5.3): effective addresses, alignment, extension and byte order, misaligned and access-fault traps, and the split between architectural faults and malformed responses.
   - **M1.4c:** the `Rv32iCpu` component, fetching and accessing data through the bus in a runtime (§5.3, §5.6, §5.7). Its tests run programs on `AddressBus` and `Ram` in the real runtime, since the M0 `ToyBus` and `ToyMemory` speak `mem.v0`. Deferred to later steps: loading programs from ELF (M1.5), `SimpleUart` (M1.7), the `m1-reference` platform with its golden digests and AT-2 checkpoints (M1.8), and the riscv-tests, Spike, and Sail oracles (M1.6, M1.9, M1.10).
6. **M1.5:** the ELF loader (§8): `systemscope-elf` turns an ELF32 RISC-V executable into a checked, RAM-relative load image and an entry point. Its tests hand the image to `Ram` and `Rv32iCpu` in the real runtime; the reference builder that does so for real runs is M1.8.
7. **M1.6:** the SystemScope `riscv-tests` environment, the fixture pipeline, and the 40 `rv32ui` tests. This comes early because it is the strongest oracle available.
8. **M1.7:** the UART, in two steps:
   - **M1.7a:** `SimpleUart` (§7.3) as a standalone component, tested on its own and behind `AddressBus` with a test-only initiator.
   - **M1.7b:** `hello.elf` and the program printing through the UART on a CPU platform.
9. **M1.8:** M1-A6 and M1-A7, the `m1-reference` golden file, and the portable snapshot.
10. **M1.9:** the Spike differential, for the 40 `rv32ui` ELFs. The random-program generator and the misaligned-access program (§10.3) are deferred; M1-A3 needs them before the exit review.
11. **M1.10:** ACT4 and Sail.
12. **M1.11:** CI, then the M1 exit review, then the tag `v0.2.0-m1`.

The tag follows the M0 convention: a SemVer prerelease identifier marking a milestone. `contracts` gets the same milestone tag on the commit `systemscope` pins.

---

## 13. Open Questions

- **ACT4 prerequisites.** With `include_priv_tests: false`, do `rvmodel_macros.h` or any remaining RV32I test still need Zicsr or a trap handler (§10.4)?
- **Spike configuration.** Resolved at M1.9 (§10.3): the pinned Spike accepts `rv32i`, and exits cleanly on the `write_tohost` stores under the §10.2 environment. The pin lives in the build script and the `spike` module rather than the fixture manifest.
- **Spike job placement.** Resolved at M1.9: M1-A3 is a blocking Linux job that builds Spike from source on a cache miss.
