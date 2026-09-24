# M2 Design: Interrupts, DMA, and Block Storage

> Status: Design frozen (M2.0) · Parent: [plan.md](../plan.md) · Builds on: [m1-design.md](m1-design.md), [m0-design.md](m0-design.md)

This document is the architecture contract for M2. It fixes every decision that the M2 implementation steps (§17) depend on. Nothing in it is implemented yet: M2.0 changes documentation only.

---

## 1. Goal and Topology

**M2 turns the M1 single-master machine into a deterministic asynchronous I/O system where a block device can transfer data directly to/from RAM as a bus master and notify the CPU through a machine-external interrupt.**

The canonical M2 topology, `m2-reference` (§11):

```text
      ┌──────────────────────── irq.v0 (MEIP) ───────────────────────┐
      ▼                                                              │
soc.cpu0  Rv32iCpu (M2 profile)                                      │
      │ mem.v1 (master 0)                                            │
      ▼                                                              │
soc.bus   MultiMasterBus ◀────────── mem.v1 (master 1: DMA) ─────┐   │
  ├─ ram  ─▶ soc.ram   Ram                                       │   │
  ├─ uart ─▶ soc.uart  SimpleUart                                │   │
  ├─ irqc ─▶ soc.irqc  SimpleIrqController ──────────────────────┼───┘
  │                        ▲ irq.v0 (source 0)                   │
  └─ blk  ─▶ soc.blk   DmaBlockController (MMIO) ────────────────┘
                           │ block.v0
                           ▼
                     soc.disk  SimpleBlockMedia
```

In words:

- The CPU is bus master 0. The block controller's DMA engine is bus master 1. Both reach every region through one `MultiMasterBus` (§10).
- The bus regions are the RAM, the `SimpleUart`, the `SimpleIrqController` MMIO window, and the `DmaBlockController` MMIO window.
- The `DmaBlockController` (§9) talks `block.v0` (§8) to a `SimpleBlockMedia`, `mem.v1` to the bus for DMA, and `irq.v0` (§7) to the `SimpleIrqController`.
- The `SimpleIrqController` drives the CPU's machine external interrupt line (`mip.MEIP`) over `irq.v0`.

M2 is done when a bare-metal program, `block_irq.elf` (§12), completes block reads and a block write through DMA, is notified of each completion by a machine external interrupt, and the M0 and M1 acceptance tests still pass unchanged (§18).

---

## 2. Scope and Non-goals

### 2.1 In Scope

- A documented subset of machine-level privileged architecture: the eight CSRs of §4.3, the six Zicsr instructions restricted to those CSRs, and `MRET`.
- The machine external interrupt (MEI), taken only at the instruction retirement boundary (§5).
- `irq.v0`, a level-only interrupt protocol, and `SimpleIrqController`, a minimal level-sensitive interrupt aggregator (§7).
- A multi-master bus with explicit round-robin arbitration and `(master, txn)` transaction identity (§10).
- `block.v0`, a single-block storage protocol, and `SimpleBlockMedia`, a sparse, content-hashed block store (§8).
- `DmaBlockController`: MMIO registers, a command lifecycle, a DMA engine between RAM and storage, and a completion interrupt (§9).
- `block_irq.elf`, the M2 end-to-end program (§12).
- Contention between CPU and DMA, snapshot/restore at every event including mid-DMA, observation invariance, and golden digests for `m2-reference` (§13, §15).

### 2.2 Non-goals

M2 does **not** include, and nothing in this document may be read as promising:

- **Privilege:** full Sm, U or S mode, PMP, trap delegation (`medeleg`/`mideleg`), `satp`, Sv32. Synchronous exceptions keep the M1 halt semantics (§5.6).
- **Interrupts:** timer and software interrupts, CLINT, a full PLIC (priorities, thresholds, claim/complete), edge-triggered or message-signaled interrupts, `WFI` (§5.7).
- **Interconnect and CPU microarchitecture:** NVMe, PCIe, AXI or any real bus protocol, caches, coherence, multicore, pipelines.
- **Software stack:** a filesystem, an OS, a driver framework.
- **DMA features:** an IOMMU, scatter/gather, descriptor rings, multiple channels, more than one outstanding DMA beat.
- **SSD modeling:** an FTL, wear leveling, garbage collection, flash timing. `SimpleBlockMedia` is an abstract block store; the "abstract SSD model" of plan.md §11 starts here and grows in later milestones.
- **Performance meaning:** as in M1 (m1-design §5.4), cycle counts are deterministic and pinned by golden digests, but they are not a performance model.

---

## 3. Decisions Fixed at M2.0

| Topic | Decision | Section |
|---|---|---|
| Privilege wording | "SystemScope M2 implements a documented subset of machine-level privileged architecture required for machine-external interrupts. It does not implement full Sm." | §4.1 |
| CSR whitelist | `mstatus`, `mie`, `mtvec`, `mscratch`, `mepc`, `mcause`, `mtval`, `mip`; every other CSR number raises `IllegalInstruction` | §4.3, §4.5 |
| `mtvec` WARL | Direct mode only; a write stores `value & !0b11` (measured Spike rule is `value & !0b10`; the two agree for MODE 0 and 2) | §4.6.4, App. A |
| `mstatus` | MIE, MPIE writable; MPP reads `0b11` always, including after `MRET`; all other bits read 0 | §4.6.1 |
| CSR instruction timing | CSR reads and writes happen in `Commit`, not at execute time | §6.3 |
| Sampling boundary | Only after an instruction retires in `Commit`; never in `FetchWait`, `MemIssue`, `MemWait`, or `CommitPending` | §5.1 |
| Entry | `mepc` = next PC, `mcause` = `0x8000_000B`, `mtval` = 0, `MPIE` ← `MIE`, `MIE` ← 0, `pc` ← `mtvec.BASE`; not a retirement | §5.2 |
| `MRET` | `pc` ← `mepc`, `MIE` ← `MPIE`, `MPIE` ← 1, `MPP` stays `0b11`; retires; immediate re-entry if still eligible | §5.3, §5.4 |
| Synchronous traps | Unchanged from M1: the CPU halts; no CSR is written | §5.6 |
| `WFI` | Not implemented (illegal); programs wait on a RAM flag | §5.7 |
| `irq.v0` | `Level { asserted: bool }` only; idempotent; delivered in `Complete` | §7.1 |
| IRQ controller | ≤ 32 level sources, `PENDING` (RO) and `ENABLE` (RW); no ACK register; `meip = (pending & enable) != 0` | §7.2 |
| `block.v0` | `ReadBlock`, `WriteBlock`, `ReadResult`, `WriteResult`; 512-byte blocks; initiator-owned `txn` | §8.1 |
| Media identity | `image_hash` = BLAKE3 of the raw initial image; part of config and snapshot; mismatch rejects restore | §8.2 |
| Controller busy rule | `COMMAND` is accepted only when idle; otherwise the sticky `REJECTED` bit is set, nothing else changes, no IRQ; `REJECTED` clears only by W1C | §9.3 |
| Validation errors | Checked in a fixed order before any request; no side effects; `DONE` + `ERROR` code; IRQ if enabled | §9.4 |
| DMA aperture | `dma_base`/`dma_size` config; reference aperture = the RAM; checked arithmetic preflight | §9.5 |
| DMA beats | 16-byte beats, `MEM_ADDR` 16-byte aligned, one beat outstanding | §9.5 |
| Partial failure | No rollback: completed beats/blocks stay; nothing after the failure is issued | §9.7 |
| Bus identity | `(master_port, txn)`; bus-owned downstream `TxnId` per request | §10.2 |
| Id counters | In the `M2` CPU profile and every new M2 component, `TxnId` allocation is checked and never wraps; overflow is a session fault. The `M1` profile keeps its M1 counter semantics unchanged | §6.2, §9.5, §10.2 |
| Bus concurrency | At most one downstream transaction per region; per-region, per-master FIFOs | §10.3 |
| Arbitration | Round-robin by stable master index (CPU = 0, DMA0 = 1), per-region cursor starting at 0, advancing only on a grant | §10.4 |
| Grant timing | Enqueue in `Request`; grant in `Transfer`; a region freed in `Complete` is next granted in the `Transfer` of the next bus clock cycle | §10.5 |
| M1 compatibility | `AddressBus` and the M1 CPU profile are unchanged; `m1-reference` and its golden files are never re-blessed | §6.1, §10.1, §15.4 |
| `COMPATIBILITY_ID` | Stays `"0.0.0"` | §14 |
| ACT4 | `include_priv_tests: False` stays; no Sm certification claim | §15.3 |

---

## 4. Privileged Subset

### 4.1 Wording and Capability

The only permitted description of M2's privileged support is:

> SystemScope M2 implements a documented subset of machine-level privileged architecture required for machine-external interrupts. It does not implement full Sm.

Documentation, READMEs, release notes, and commit messages must not describe SystemScope as Sm, privileged-spec, or ACT-privileged "compliant", "certified", or "conformant".

| Capability | M2 |
|---|---|
| Base ISA | RV32I (unchanged from M1) |
| Additional instructions | Zicsr instructions (`CSRRW`, `CSRRS`, `CSRRC`, `CSRRWI`, `CSRRSI`, `CSRRCI`) for the supported CSRs only, and `MRET` |
| Privilege | The documented machine-level subset of this section only; the hart is always in M-mode |

### 4.2 Instructions

- **Zicsr:** the six CSR instructions, encoded in `SYSTEM` with `funct3` ∈ {`001`, `010`, `011`, `101`, `110`, `111`}. Every `rd`, `rs1`/`uimm`, and `csr` field value is decoded; legality depends only on the `csr` field (§4.5).
- **`MRET`:** exactly the word `0x3020_0073`.
- **`ECALL` and `EBREAK`** keep their M1 decoding and raise their synchronous traps (m1-design §6); `ECALL` still ends a program (§5.6).
- **Still illegal:** apart from the existing M1 `ECALL`/`EBREAK` encodings, the six supported Zicsr forms, and `MRET`, all other `SYSTEM` encodings remain illegal, including `WFI` (`0x1050_0073`), `SRET`, `URET`, `SFENCE.VMA`, and `funct3 = 100`.
- **Write suppression:** `CSRRS`/`CSRRC` with `rs1 = x0`, and `CSRRSI`/`CSRRCI` with `uimm = 0`, do not write the CSR. `CSRRW`/`CSRRWI` always write, whatever `rd` is. No supported CSR has read side effects, so `rd = x0` changes nothing but the destination.
- **Read-only CSR numbers** (`csr[11:10] = 0b11`): none is on the whitelist. Any access to one is illegal because it is unsupported (§4.5); the read-only rule itself never needs to be applied.

### 4.3 CSR Whitelist

Reset values: every CSR reads 0 except `mstatus`, which reads `0x0000_1800` (MPP = `0b11`). Spike's reset values agree (Appendix A.2).

| CSR | Address | Implemented bits | Read | Write (value `v`) | Legal values and unimplemented bits | In snapshot |
|---|---|---|---|---|---|---|
| `mstatus` | `0x300` | MIE [3], MPIE [7], MPP [12:11] | `MIE<<3 \| MPIE<<7 \| 0b11<<11` | MIE ← `v[3]`, MPIE ← `v[7]`; everything else ignored | MPP is WARL with the single legal value `0b11`. Every other field (SIE, SPIE, UBE, SPP, VS, FS, XS, MPRV, SUM, MXR, TVM, TW, TSR, SD, and the reserved bits) reads 0 and ignores writes | MIE, MPIE |
| `mie` | `0x304` | MEIE [11] | `MEIE<<11` | MEIE ← `v[11]` | All other bits read 0, writes ignored | MEIE |
| `mtvec` | `0x305` | BASE [31:2]; MODE [1:0] reads 0 | the stored value | stored ← `v & !0b11` | MODE is WARL with the single legal value 0 (Direct). BASE accepts any 4-byte-aligned value (§4.6.4) | BASE |
| `mscratch` | `0x340` | [31:0] | stored | stored ← `v` | none | yes |
| `mepc` | `0x341` | [31:2]; [1:0] read 0 | stored | stored ← `v & !0b11` | IALIGN = 32, so bits [1:0] are always 0 | yes |
| `mcause` | `0x342` | [31:0] | stored | stored ← `v` | WLRL; SystemScope stores whatever is written. Hardware writes only `0x8000_000B` (§5.2) | yes |
| `mtval` | `0x343` | [31:0] | stored | stored ← `v` | none | yes |
| `mip` | `0x344` | MEIP [11], read-only | `MEIP<<11`, the current level of the CPU's `irq` input | ignored; no trap | All other bits read 0. MEIP is driven only by the interrupt controller (§7) | the input level |

### 4.4 Unimplemented Bits

Every bit of a whitelisted CSR that the table does not list as implemented:

- reads as 0;
- ignores writes, whatever the written value;
- is not stored, so it never reaches a snapshot, a digest, or `inspect`.

For the documented M2 subset, unimplemented fields are hardwired to zero and ignore writes.

### 4.5 Unsupported CSRs

Any CSR instruction whose `csr` field is not one of the eight whitelisted addresses is `IllegalInstruction`, whether it would read, write, or both, and even when write suppression (§4.2) applies. `tval` is the instruction word, as for every other illegal instruction (m1-design §6), and the CPU halts (§5.6).

This includes CSRs that real Sm implementations, and Spike, provide: `misa`, `mhartid`, `mvendorid`, `marchid`, `mimpid`, `mconfigptr`, `mstatush`, `medeleg`, `mideleg`, `mcounteren`, `mcycle`, `minstret`, `cycle`, `time`, `instret`, `satp`, every `pmpcfg*`/`pmpaddr*`, and every custom CSR.

**This deliberately diverges from Spike.** The pinned Spike (Appendix A) implements `misa`, `mhartid`, `mvendorid`, `marchid`, `mimpid`, `mconfigptr`, `mstatush`, `mcycle`, `minstret`, and the PMP CSRs even with `--priv=m`, and traps only on `medeleg`, `mideleg`, `mcounteren`, `satp`, the unprivileged counters, custom CSRs, and writes to read-only CSRs. The M2.1a Spike differential (§15.2) therefore compares unsupported-CSR traps only on CSRs that Spike also rejects. The rest are SystemScope-only tests whose expected values come from this section.

### 4.6 Per-CSR Rules

#### 4.6.1 `mstatus`

- MIE and MPIE are the only writable bits. MPP is `0b11` at reset, after every write, after interrupt entry, and after `MRET`.
- Spike with `--priv=m` behaves the same on every write measured (Appendix A.1): all ones reads `0x0000_1888`, zero reads `0x0000_1800`.
- `mstatush` is not supported (§4.5), although Spike implements it as a read-zero register.

#### 4.6.2 `mie`

MEIE is the only implemented bit. Spike also keeps MSIE and MTIE (all ones reads `0x888`); SystemScope reads `0x800`. The differential writes only MEIE, or masks the comparison to bit 11.

#### 4.6.3 `mip`

- MEIP is read-only and reflects the CPU's `irq` input level (§7.1). Software writes, including set and clear forms, are accepted and ignored: they neither trap nor change MEIP.
- Spike with no interrupt source also reads 0 after an all-ones write (Appendix A.1). Spike's other `mip` bits belong to its own interrupt sources, which the M2 differential never programs.

#### 4.6.4 `mtvec`

Measured on the pinned Spike before this rule was chosen (Appendix A.1): Spike stores `value & !0b10`. It supports vectored mode (MODE = 1 is kept), maps reserved MODE 2 to 0 and 3 to 1, and requires only 4-byte BASE alignment (`0x8000_1004` and `0x8000_1005` are kept as written).

**SystemScope's rule: a write stores `value & !0b11`.** MODE always reads 0 (Direct), and BASE keeps bits [31:2] of the written value.

- Direct mode is the only mode M2 implements, so MODE's single legal value is 0. Clearing both MODE bits is a valid WARL mapping.
- Keeping BASE (instead of ignoring writes with MODE ≠ 0) matches Spike for every write with MODE ∈ {0, 2}, which is the widest agreement possible without implementing vectored mode.
- For MODE ∈ {1, 3}, SystemScope and Spike diverge (Spike reads MODE = 1). Those writes are covered by SystemScope-only tests; the Spike differential writes `mtvec` only with MODE ∈ {0, 2}.
- There is no BASE alignment beyond 4 bytes. Interrupt entry jumps to `mtvec & !0b11` = BASE.

#### 4.6.5 `mepc`, `mcause`, `mtval`, `mscratch`

- `mepc` clears bits [1:0] on write (Spike: `0xFFFF_FFFF` → `0xFFFF_FFFC`, `0x8000_0002` → `0x8000_0000`).
- `mcause`, `mtval`, and `mscratch` store all 32 bits as written (Spike agrees).
- `MRET` jumps to `mepc`, which is always 4-byte aligned, so `MRET` can never raise `InstructionAddressMisaligned`.

---

## 5. Machine External Interrupt

### 5.1 Sampling Boundary

The CPU considers an interrupt at exactly one point: **right after an instruction retires, in the `Commit` handler that retired it.**

```text
CommitPending ──Wake(COMMIT) @ Commit──▶ retire: x[rd], CSR effects, pc, instret
    ├─ instret reached max_instructions ─────────────────────▶ Halted(InstructionLimit)
    ├─ eligible = mstatus.MIE && mie.MEIE && mip.MEIP ─ yes ─▶ take MEI (§5.2) ─▶ FetchIssue (next cycle)
    └─ no ───────────────────────────────────────────────────▶ FetchIssue (next cycle)
```

- The check uses the state **after** the retiring instruction's effects, including its own CSR write (§5.5) and its `pc` update.
- An interrupt is never taken in `FetchIssue`, `FetchWait`, `MemIssue`, `MemWait`, or `CommitPending` before the commit, and never while `Halted`. A load or store in flight always completes and retires first.
- A trapping instruction does not retire, so no interrupt is sampled after it: the CPU halts (§5.6).
- The instruction limit takes priority: if the retiring instruction brings `instret` to `max_instructions`, the CPU halts and does not take a pending interrupt.
- There is no sampling before the first instruction. `mstatus.MIE` is 0 at reset, so none could be taken anyway.

**Why this is deterministic.** `irq.v0` levels are delivered only in `Complete` (§7.1), and the sampling happens in `Commit`, which follows `Complete` in every tick. Every level change delivered at or before the commit's tick is therefore visible, and none after it, whatever the global event sequence (m0-design §4). No decision depends on the order of events within one `(tick, phase)`.

### 5.2 Entry

Taking the MEI is not an instruction and does not retire:

| Effect | Value |
|---|---|
| `mepc` | the next PC: the `next_pc` of the instruction that just retired (the address of the first instruction not executed) |
| `mcause` | `0x8000_000B` (interrupt bit set, code 11) |
| `mtval` | 0 |
| `mstatus.MPIE` | the old `mstatus.MIE` (always 1, since eligibility required it) |
| `mstatus.MIE` | 0 |
| `mstatus.MPP` | `0b11` (unchanged) |
| `pc` | `mtvec` BASE (`mtvec & !0b11`; MODE is always 0) |
| `instret` | unchanged |
| registers `x1`–`x31`, `mie`, `mip`, `mscratch` | unchanged |

`next_pc` is always 4-byte aligned, because M1 raises `InstructionAddressMisaligned` before any misaligned target retires. The CPU emits `rv32.interrupt` (§6.5) and schedules the next `FetchIssue` exactly as after a normal retirement (next cycle's `Request`). No extra event is created.

### 5.3 `MRET`

`MRET` executes in M-mode (the only mode) and is always legal. It retires like any other instruction, with these effects applied in `Commit`:

- `pc` ← `mepc`;
- `mstatus.MIE` ← `mstatus.MPIE`;
- `mstatus.MPIE` ← 1;
- `mstatus.MPP` ← `0b11` (the least-privileged supported mode is M, so MPP stays `0b11`);
- `instret` += 1; no register is written.

Spike agrees on all four fields (Appendix A.1): `mstatus` `0x1880` → `0x1888`, `0x1808` → `0x1880`, and `0x1888` → `0x1888`. The M2.1a differential compares them (§15.2).

### 5.4 Re-entry After `MRET`

`MRET` retires, so the §5.1 check runs right after it, with MIE already restored. **If MEIP is still asserted and MEIE is set, the MEI is taken again immediately**: no instruction at `mepc` executes, and the new `mepc` is the same address. This is the architecturally expected result of a level-sensitive line that software did not clear before `MRET`.

- Handlers must clear the source (acknowledge the device, §9.3) before `MRET`. `block_irq.elf` does, and tolerates a spurious entry anyway (§12.3).
- **Required test (M2.1b):** a directed program with MEIP held asserted executes `MRET` and must re-enter the handler with `mepc` unchanged and `instret` incremented by exactly the `MRET`.

### 5.5 Enabling by CSR Write

If a CSR instruction sets `mstatus.MIE` (or `mie.MEIE`) while the other two conditions already hold, the interrupt is taken **at that instruction's own retirement boundary**: `mepc` is the CSR instruction's `next_pc`, and no further instruction retires first.

- **Required test (M2.1b):** with MEIE = 1 and MEIP asserted, `csrsi mstatus, 8` must be followed directly by entry, with `mepc = pc + 4` of the `csrsi` and the next instruction not retired.
- Clearing MIE by a CSR write takes effect at the same boundary: the check sees MIE = 0 and no interrupt is taken.

### 5.6 Synchronous Traps

Synchronous exceptions keep the M1 semantics (m1-design §6): the trapping instruction does not retire, the CPU records `rv32.trap` and enters `Halted(Trap)`. **No CSR is written**: `mepc`, `mcause`, `mtval`, and `mstatus` are not updated, and `mtvec` is not used. `ECALL` remains the way a program ends.

Delivering synchronous exceptions to `mtvec` is left to a later milestone. The CSR state and entry logic of §5.2 are written so that a later exception path only adds another cause, without changing the interrupt path.

### 5.7 No `WFI`

M2 does not implement `WFI` (it stays `IllegalInstruction`). Reasons:

- A `WFI` that sleeps needs a wake-up event chain between `irq.v0` delivery and the CPU's fetch state, which adds a CPU state and snapshot state without adding any capability M2 needs.
- A `WFI` implemented as a no-op would be legal but would make programs that rely on it misleading.
- Programs wait on a RAM flag set by the handler (§12.2). This is also what exercises CPU/DMA contention for the RAM.

### 5.8 Pure Oracle

M2.1b validates interrupt entry against a pure function written from this section, in the test crate, sharing no code with the CPU:

```rust
struct MeiResult {
    taken: bool,
    new_pc: u32,          // mtvec & !3 when taken, else pc
    mepc: Option<u32>,    // Some(pc) when taken; None = unchanged
    mcause: Option<u32>,  // Some(0x8000_000B) when taken; None = unchanged
    mtval: Option<u32>,   // Some(0) when taken; None = unchanged
    new_mstatus: u32,     // MPIE ← MIE, MIE ← 0, MPP = 0b11 when taken, else mstatus
}

fn take_mei(pc: u32, mstatus: u32, mie: u32, mip: u32, mtvec: u32) -> MeiResult;
```

`pc` is the next PC at the boundary, and `mstatus`, `mie`, `mip`, `mtvec` are the CSR values after the retiring instruction. Property tests compare the CPU's boundary step with the oracle for arbitrary CSR values and levels (§15.1).

---

## 6. CPU Changes

### 6.1 Profiles

`Rv32iConfig` gains a `profile`:

- **`M1`**: exactly the M1 CPU. One port (`mem`), the M1 decoder (every CSR instruction and `MRET` are illegal), the M1 events and trace, and snapshot schema 1, byte for byte. `m1-reference`, the `rv32ui` runs, ACT4, the Spike M1-A3 differential, and every M1 golden file use this profile and must not change.
- **`M2`**: the M1 CPU plus §4 and §5. Two ports: `mem` (`mem.v1` initiator) and `irq` (`irq.v0` target). Snapshot schema 2 (§6.4).

The component keeps its type name `rv32i.cpu` in both profiles. RV32I behavior is identical in both: the M2 profile must also pass the 40 `rv32ui` tests and the 39 ACT4 tests (§15.4).

### 6.2 `irq` Input

- The CPU stores one bit, the `irq` input level, which is `mip.MEIP`.
- A `Level { asserted }` delivered on `irq` sets it, in any CPU state, including `Halted`. A repeated level is a no-op (§7.1).
- A delivery outside `Complete`, or a message that is not `irq.v0` on `irq`, faults the session (`ComponentFault`).

**`TxnId` counters.** In the `M2` CPU profile and in every new M2 component (the bus and the block controller), `TxnId` allocation uses checked arithmetic. It never wraps: an increment that would overflow `u64` returns `SimError::ComponentFault` before the message is sent, and the counter keeps its value. What matters is that the canonical state machine has no wrap semantics, not whether 2^64 requests are reachable.

**The `M1` profile keeps the legacy M1 `TxnId` semantics unchanged,** as it keeps everything else (§6.1). M2 does not change them. Any fix to the M1 counter is a separate, compatibility-reviewed change, not part of M2.

### 6.3 CSR Instructions Execute at `Commit`

M1 decodes and executes in `Complete`, when the fetch response arrives, and applies the result in `Commit` (m1-design §5.3). For CSR instructions and `MRET`, M2 splits this differently:

- In `Complete`, decode and execute produce a **pending CSR operation**: the CSR address, the operation (write, set, or clear), whether a write happens (§4.2), the operand (the `rs1` value read before the instruction, or `uimm`), and `rd`. Legality (§4.5) is decided here, so an unsupported CSR becomes `Trap(IllegalInstruction)` as in M1.
- In `Commit`, the CPU reads the CSR's current value, writes `rd` with it, computes and stores the new value (§4.3), then updates `pc` and `instret`. `MRET` reads `mepc` and `mstatus` in `Commit`.

**Why:** `mip` changes when an `irq.v0` level is delivered, in `Complete`. If `csrr x, mip` read `mip` in the `Complete` handler of its fetch response, the result would depend on whether that response or the level was dispatched first within the same `(tick, Complete)`, which is decided by the global sequence counter. Reading in `Commit` sees every level delivered in the tick, so the value is sequence-independent. Every other CSR changes only in the CPU's own `Commit`, so moving the read there changes nothing for them.

### 6.4 Snapshot (Schema 2, M2 Profile Only)

Schema 2 is schema 1 (m1-design §5.6) followed by the CSR state, in this order:

| Field | Encoding | Restore check |
|---|---|---|
| `mstatus.MIE` | `u8` 0/1 | 0 or 1 |
| `mstatus.MPIE` | `u8` 0/1 | 0 or 1 |
| `mie.MEIE` | `u8` 0/1 | 0 or 1 |
| `mtvec` | `u32` | bits [1:0] = 0 |
| `mscratch` | `u32` | — |
| `mepc` | `u32` | bits [1:0] = 0 |
| `mcause` | `u32` | — |
| `mtval` | `u32` | — |
| `irq` input level | `u8` 0/1 | 0 or 1 |

- **The profile is never encoded.** `Rv32iConfig` gains a `profile` field in Rust, but the canonical configuration block stays exactly the schema 1 block (clock, entry, instruction limit) in both schemas. The profile is implied by the schema version: schema 1 means `M1`, schema 2 means `M2`.
- **Schema 1 serialization is exactly unchanged,** byte for byte, so every M1 snapshot, `StateDigest`, and golden file stays identical. An `M1` CPU writes schema 1 and restores only schema 1; an `M2` CPU writes schema 2 and restores only schema 2. A schema that does not match the CPU's configured profile rejects the restore.
- `CommitPending` outcomes gain the pending CSR operation and `MRET`. Their encoding appends new tags; schema 1 never contains them. Restore recomputes the pending operation from the instruction word and registers and rejects a mismatch, as M1 does for every pending outcome.
- There is **no interrupt-related transient state**: entry happens inside the `Commit` handler that retires the instruction, so no snapshot can be taken "between" retirement and entry. The CPU state after entry is ordinary `FetchIssue` with `pc = mtvec` BASE.

### 6.5 Inspect and Trace

- **Inspect (M2 profile):** the M1 fields, then `mstatus`, `mie`, `mip`, `mtvec`, `mscratch`, `mepc`, `mcause`, `mtval` as `U64`, in that order.
- **`rv32.commit`:** unchanged fields. A CSR instruction adds `csr` (the address) and, only when it writes, `csr_value` (the value stored after WARL). `MRET` adds nothing.
- **`rv32.interrupt`** (new kind): `mepc`, `mcause`, `handler` (the new `pc`), all `U64`, emitted once per entry.
- **No `rv32.commit` record producible by the `M1` profile changes.** Only M2-only CSR commits carry the additional fields, and `rv32.interrupt` exists only in the `M2` profile. Adding kinds and fields for new behavior is not a trace format change: the trace `format_version` stays 2.

---

## 7. `irq.v0` and `SimpleIrqController`

### 7.1 `irq.v0`

```rust
pub const PROTOCOL: ProtocolId = ProtocolId { name: "irq", version: 0 };

pub enum IrqMsg {
    /// The sender's output level is now `asserted`.
    Level { asserted: bool },
}
```

- **Roles:** the source port is an `Initiator`, the sink port a `Target`. There is no response.
- **State is the level.** A sink stores the last level received per port. Every line is deasserted at reset, and a source never sends an initial `Level { asserted: false }`.
- **Idempotent:** a `Level` equal to the stored level changes nothing and is not an error. Sources send only on change, but sinks must not depend on that.
- **Phase:** a source sends `Level` with `Phase::Complete`, and it is delivered in `Complete`. A sink that receives `irq.v0` outside `Complete` faults the session. In `m2-reference` every `irq.v0` link has latency `Cycles { cpu, 1 }`; a source that changes level in a `Commit` handler would violate rule S2 on a zero-latency link, and the runtime reports that as `PhaseViolation`.
- **Encoding:** `Level` is tag 0, then `asserted` as a canonical `bool` (m0-design §4.5). The strict decoder rejects any other tag or `bool` byte. `runtime.dispatch` fields: `msg` = `"Level"`, `asserted` as `Bool`.

### 7.2 `SimpleIrqController`

A level-sensitive aggregator. It latches nothing and has no PLIC features: no priorities, thresholds, claim/complete, or edge detection.

- **Config:** `sources: u8` in 1..=32 (N), and the response latency (like the RAM's).
- **`valid_mask`** = `if N == 32 { u32::MAX } else { (1u32 << N) - 1 }`. Bits outside it are never stored in `pending` or `enable`.
- **Ports:** `src0` … `src{N−1}` (`irq.v0` targets), `cpu` (`irq.v0` initiator), `mem` (`mem.v1` target, the MMIO window).
- **State:** `pending: u32` (bit *i* = the current level of source *i*), `enable: u32`, and `out: bool`, the last level sent on `cpu`.
- **Output:** `meip = (pending & enable) != 0`. Whenever a source level or `ENABLE` changes, the controller recomputes `meip` and, if it differs from `out`, sends `Level { asserted: meip }` on `cpu` (`Now`, `Complete`) and updates `out`.
- **Clearing:** `pending` follows the source levels and cannot be cleared by software. The owner of a deassert is the device: software acknowledges the device (for the block controller, `ACK`, §9.3), the device deasserts its line, and the controller recomputes `meip`. There is no ACK register because there is nothing to acknowledge in a level-sensitive controller without a latch.

**MMIO map** (offsets from the controller's base; window size 8):

| Offset | Register | Access | Behavior |
|---|---|---|---|
| `0x0` | `PENDING` | read, 4 bytes | `pending` (bits outside `valid_mask` read 0) |
| `0x4` | `ENABLE` | read/write, 4 bytes | read: `enable`; write: `enable ← v & valid_mask` |

- Every other well-formed request (a write to `PENDING`, any width other than 4, any offset other than `0x0`/`0x4`, a request crossing the window) gets `Fault { AccessFault }` and changes nothing, like the UART (m1-design §7.3). A zero-length request faults the session.
- Ordering: like the RAM, a request takes effect at acceptance, and the response follows after the configured latency in `Complete`.
- **Snapshot:** config (`sources`, latency), then `pending`, `enable`, `out`. Restore rejects any bit outside `valid_mask`, and `out ≠ ((pending & enable) != 0)`, which no valid run can produce: `out` is updated in the same handler that changes its inputs.
- **Trace:** `platform.irq.level` with `source` and `asserted` on every source level change, and `platform.irq.meip` with `asserted` on every output change.

---

## 8. `block.v0` and `SimpleBlockMedia`

### 8.1 `block.v0`

```rust
pub const PROTOCOL: ProtocolId = ProtocolId { name: "block", version: 0 };
pub const BLOCK_SIZE: usize = 512;

pub enum BlockMsg {
    ReadBlock   { txn: TxnId, lba: u64 },
    WriteBlock  { txn: TxnId, lba: u64, data: Vec<u8> },   // exactly 512 bytes
    ReadResult  { txn: TxnId, outcome: BlockReadOutcome },
    WriteResult { txn: TxnId, outcome: BlockWriteOutcome },
}
pub enum BlockReadOutcome  { Data { data: Vec<u8> }, Error { error: MediaError } } // Data: 512 bytes
pub enum BlockWriteOutcome { Done, Error { error: MediaError } }
pub enum MediaError { OutOfRange, BadBlock }
```

- **One block per message.** Multi-block commands are orchestrated by the controller (§9), one block at a time.
- **`txn`** is initiator-owned, like `mem.v1` (`TxnId` is the `mem` type, reused). The initiator never has two requests with one `txn` outstanding; the target echoes it.
- **Timing:** requests are sent in `Request`, results in `Complete`.
- **Encoding:** enums are a `u8` tag in declaration order, then fields, as in `mem.v1` (m1-design §4.3): `ReadBlock` 0, `WriteBlock` 1, `ReadResult` 2, `WriteResult` 3; `Data` 0/`Error` 1; `Done` 0/`Error` 1; `OutOfRange` 0, `BadBlock` 1. `data` uses the canonical length-prefixed bytes encoding.
- **Length is checked by receivers, not the decoder**, exactly as `mem.v1` does for `Data` (m1-design §4.3): the decoder accepts any length, and a receiver that gets `data` of any length other than 512 faults the session. This needs no new `DecodeError` variant.
- **Errors change nothing:** a request answered with `Error` has not read or written anything.
- Any later change, including a new `MediaError`, is a new protocol version.

### 8.2 `SimpleBlockMedia`

- **Config:**
  - `capacity_blocks: u64` (> 0);
  - `latency`: a `LinkLatency`, applied to every result, as the RAM does;
  - `bad_blocks: BTreeSet<u64>` (LBAs that answer `Error { BadBlock }`; empty in `m2-reference`; used by the failure tests of §9.7);
  - the **initial image**: raw bytes, a whole number of blocks, at most `capacity_blocks × 512`. Blocks past its end are zero.
- **`image_hash`** is the BLAKE3 of the raw initial image bytes as given, before any conversion. It is part of the configuration: `inspect` shows it, the snapshot writes it, and restore compares it.
- **Storage:** `BTreeMap<u64, Box<[u8; 512]>>`, sparse and canonical like the RAM's pages (m1-design §7.2): a block that is all zero is never in the map. A write of zeros removes the entry, and the same contents always give the same map and snapshot bytes.
- **Port:** `blk` (`block.v0` target).
- **Semantics:** like the RAM. A request is accepted when it is dispatched; a read is sampled and a write applied at acceptance; the result is sent after `latency`, in `Complete`.
  - `lba ≥ capacity_blocks` → `Error { OutOfRange }`; `lba ∈ bad_blocks` → `Error { BadBlock }`; neither reads nor writes.
  - A `WriteBlock` whose `data` is not 512 bytes, a result arriving on the target port, or a message that is not `block.v0` faults the session.
- **Snapshot:** config (`capacity_blocks`, latency, `bad_blocks` ascending, `image_hash`), then the number of stored blocks and each `(lba, 512 bytes)` in ascending LBA order.
  - Restore rejects a snapshot whose config, **including `image_hash`**, differs from the component's, a non-ascending or out-of-range LBA, and a stored all-zero block.
  - **Restore never re-applies the initial image.** The initial image is applied once, at construction. Restore replaces the whole block map with the snapshot's, so blocks written before the snapshot keep their written contents, and blocks the image had but a write zeroed stay zero.
- **Inspect:** `capacity_blocks`, `image_hash` (as `Bytes`), and the number of stored blocks; never the contents.
- **Trace:** `platform.disk.read` and `platform.disk.write` with `lba` and `outcome`, at acceptance.

---

## 9. `DmaBlockController`

### 9.1 Ports and Configuration

- **Ports:** `mem` (`mem.v1` target: the MMIO window), `dma` (`mem.v1` initiator: bus master 1), `blk` (`block.v0` initiator), `irq` (`irq.v0` initiator).
- **Config:** `clock` (a clock domain), MMIO response `latency`, `capacity_blocks`, `dma_base`, `dma_size` (the DMA aperture, §9.5).
- **`capacity_blocks` is the controller's own validation contract** (§9.4 check 3). The controller cannot see the media's configuration. The `m2-reference` builder enforces `controller.capacity_blocks == media.capacity_blocks`. In a generic topology they may differ; then a command that passes preflight can still get `Error { OutOfRange }` from the media, which ends it with MEDIA_ERROR (§9.7).

### 9.2 Register Map

Offsets from the controller's base. The window is `0x20` bytes. Every register is 32 bits and accepts only aligned 4-byte accesses.

**The controller is a 32-bit device.** `LBA` and `MEM_ADDR` are `u32` registers, zero-extended to `u64` when the controller builds `block.v0` LBAs and `mem.v1` addresses. There are no `LBA_HI` or `MEM_ADDR_HI` registers:

- the controller's `capacity_blocks` must satisfy `1 ≤ capacity_blocks ≤ 2^32` (the constructor rejects anything else). Validation check 3 (`LBA + BLOCK_COUNT ≤ capacity_blocks`, in `u64`) then guarantees that every block a command touches, `LBA + i` for `i < BLOCK_COUNT`, is at most `2^32 − 1`, including multi-block commands that start near `u32::MAX`;
- `SimpleBlockMedia` keeps a `u64` `capacity_blocks` and may be larger than 2^32 blocks; this controller exposes at most its first 2^32 blocks;
- controller-addressable DMA addresses are below `0x1_0000_0000`; the constructor requires `dma_size > 0` and `dma_base + dma_size ≤ 0x1_0000_0000` (checked arithmetic);
- validation (§9.4) computes in `u64` from the zero-extended values, so no check can wrap.

| Offset | Register | Access | Behavior |
|---|---|---|---|
| `0x00` | `COMMAND` | write | `1` = READ (media → RAM), `2` = WRITE (RAM → media); any other value is a validation error (§9.4). Reads fault |
| `0x04` | `STATUS` | read; W1C bit 2 | bit 0 `BUSY`, bit 1 `DONE`, bit 2 `REJECTED`, bits [15:8] `ERROR`; others read 0. A write clears `REJECTED` if bit 2 is 1 and ignores every other bit |
| `0x08` | `LBA` | read/write | first block of the next command (`u32`, zero-extended) |
| `0x0C` | `MEM_ADDR` | read/write | DMA start address of the next command (a bus address, `u32`, zero-extended) |
| `0x10` | `BLOCK_COUNT` | read/write | number of blocks of the next command |
| `0x14` | `IRQ_ENABLE` | read/write | bit 0 enables the completion interrupt; other bits read 0, writes ignored |
| `0x18` | `IRQ_STATUS` / `ACK` | read; W1C bit 0 | read: bit 0 = `DONE`. A write with bit 0 set acknowledges the completion (§9.3); other bits ignored |
| `0x1C` | reserved | — | faults |

- Every other well-formed request (a non-4-byte or misaligned access, a read of `COMMAND`, the reserved word, a request crossing the window) gets `Fault { AccessFault }` and changes nothing. A zero-length request faults the session.
- MMIO requests take effect at acceptance; the response follows after `latency` in `Complete`.
- `LBA`, `MEM_ADDR`, and `BLOCK_COUNT` can be written at any time. **A command latches them when it is accepted**; writes during `BUSY` change the registers but never the active command.

### 9.3 Lifecycle, `BUSY`, and Acknowledgement

```text
         COMMAND (valid)                   last block done / error
IDLE ───────────────────────▶ BUSY ─────────────────────────────▶ DONE (+ERROR)
 ▲    COMMAND (invalid) ─────────────────────────────────────────▶ DONE (+ERROR)
 │                                                                   │
 └──────────────────────────── ACK (IRQ_STATUS W1C bit 0) ───────────┘
```

| State | `BUSY` | `DONE` | `ERROR` |
|---|---|---|---|
| IDLE | 0 | 0 | 0 |
| BUSY | 1 | 0 | 0 |
| DONE | 0 | 1 | the code (0 = success) |

- **`COMMAND` is accepted only in IDLE.** A `COMMAND` write in BUSY **or DONE** is rejected:
  - the active transfer (or the unacknowledged completion) is unchanged;
  - `STATUS.REJECTED` is set and stays set (sticky);
  - no interrupt is raised and the IRQ line does not change;
  - the MMIO write itself completes normally (`WriteResp Done`).

  Rejecting in DONE too means a completion can never be lost by a new command. `REJECTED` is cleared **only** by writing 1 to `STATUS` bit 2. Neither a later accepted command nor `ACK` clears it.
- **Completion** (success or error) sets `DONE`, `ERROR`, and clears `BUSY`, in the handler that finishes the command.
- **`ACK`** in DONE clears `DONE` and `ERROR` and returns to IDLE. In IDLE or BUSY it does nothing.
- **Interrupt line:** `irq_level = DONE && IRQ_ENABLE.bit0`. It is recomputed after every completion, `ACK`, and `IRQ_ENABLE` write, and a `Level` is sent on `irq` (`Now`, `Complete`) only when it changes. Enabling the interrupt while DONE asserts the line; disabling it deasserts it.

### 9.4 Command Validation

A `COMMAND` accepted in IDLE is validated against the latched registers **before any request is issued**, with checked 64-bit arithmetic, in this fixed order; the first failing check sets the code:

| Order | Check | `ERROR` |
|---|---|---|
| 1 | `COMMAND` ∈ {1, 2} | `1` BAD_COMMAND |
| 2 | `BLOCK_COUNT ≠ 0` | `2` BAD_COUNT |
| 3 | `LBA + BLOCK_COUNT ≤ capacity_blocks` | `3` LBA_RANGE |
| 4 | `MEM_ADDR % 16 = 0` | `4` DMA_ALIGN |
| 5 | `total = BLOCK_COUNT × 512` and `end = MEM_ADDR + total` do not overflow, `MEM_ADDR ≥ dma_base`, and `end ≤ dma_base + dma_size` | `5` DMA_RANGE |

A failing command has **no side effects**: no `block.v0` or `mem.v1` request is sent, no RAM or media byte changes. The controller goes directly to DONE (`BUSY = 0`, `DONE = 1`, `ERROR` = the code) and raises the interrupt if enabled.

Codes `6` DMA_FAULT and `7` MEDIA_ERROR come from execution (§9.7). Code 0 is success; codes 8–255 are unused.

### 9.5 DMA Rules

- **Aperture:** DMA may touch only `[dma_base, dma_base + dma_size)`. The `DmaBlockController` constructor checks only the 32-bit bound of §9.2; it knows nothing about the bus map, and **an aperture that spans several regions or unmapped holes is a valid generic configuration.** A beat that hits a hole gets `Fault { AccessFault }` from the bus and ends the command with DMA_FAULT (§9.7).
- **Reference invariant:** the `m2-reference` builder requires the DMA aperture to lie wholly inside the RAM region. This is a reference-topology invariant, not a generic `DmaBlockController` configuration rule. In `m2-reference` the aperture is exactly the RAM (`0x8000_0000`, 16 MiB), so DMA can never reach an MMIO window, including the controller's own.
- **Preflight** (§9.4 checks 4 and 5) guarantees that every beat lies inside the aperture. No request is issued for a command that fails it.
- **Beats:** 16 bytes. `MEM_ADDR` must be 16-byte aligned, so every beat is aligned and a block is exactly 32 beats. Block *i*, beat *j* uses address `MEM_ADDR + i × 512 + j × 16`. A 512-byte transfer is never sent as one request.
- **One outstanding operation:** the controller has at most one request outstanding in total, a `mem.v1` beat or a `block.v0` request, and issues the next only after the previous result. There is no pipelining.
- **Identity:** the controller owns one `TxnId` counter for `dma` and one for `blk`, each consumed only when a send succeeds; the outstanding one is always the latest issued, as in the CPU. Both follow the counter rule of §6.2: checked increment, no wrap, and overflow faults the session before anything is sent.

### 9.6 Engine Timing

The engine mirrors the CPU's state machine:

```text
Idle ──valid COMMAND @ Transfer──▶ Issue (Wake(ISSUE) @ Request, next controller cycle)
Issue ──Wake(ISSUE)──▶ send the next block.v0 request or mem.v1 beat ──▶ WaitMedia { txn } / WaitBeat { txn }
WaitMedia / WaitBeat ──result @ Complete──▶ Issue (next cycle) | Done
```

- MMIO requests are accepted in `Transfer` (the bus forwards in `Transfer`, §10.5). Every request the engine sends is sent from a `Request`-phase wake, `Now`, so it never depends on link latency to satisfy rule S2.
- **READ:** for each block: `ReadBlock(LBA + i)`; on `Data`, keep the 512 bytes in the block buffer and write them as 32 `WriteReq` beats.
- **WRITE:** for each block: 32 `ReadReq` beats into the block buffer; after the 32nd, `WriteBlock(LBA + i, buffer)`.
- After the last block's last result, the controller completes with `ERROR = 0`.

### 9.7 Failure Semantics

There is no rollback in either direction.

**READ (media → RAM):**

- A media `Error` for block *i*: blocks before *i* are fully in RAM; nothing of block *i* is written; nothing after it is requested. `ERROR = 7` MEDIA_ERROR.
- A `Fault` response to beat *j* of block *i*: every earlier beat, including beats 0 … *j*−1 of block *i*, stays in RAM (the target made them visible at acceptance); the faulting beat wrote nothing (`mem.v1` guarantee); no later beat or block is issued. `ERROR = 6` DMA_FAULT.

**WRITE (RAM → media):**

- `WriteBlock` for block *i* is sent only after all 32 beats of block *i* were read successfully.
- A `Fault` on any beat of block *i*: blocks before *i* are committed on the media; block *i* is not written; later blocks are untouched. `ERROR = 6` DMA_FAULT.
- A media `Error` for block *i*: the same, with `ERROR = 7` MEDIA_ERROR.

In every case the controller completes (`DONE`, `ERROR`, IRQ if enabled). Preflight makes DMA_FAULT unreachable in `m2-reference`. The DMA_FAULT tests use a negative test topology whose aperture contains a bus hole, which the generic controller accepts (§9.5), so the fault comes from a real `mem.v1` `AccessFault`. Media errors use `bad_blocks`.

### 9.8 Error Classes

| Condition | Class | Effect |
|---|---|---|
| Protocol violation: response for an unknown, stale, or duplicate `txn`; response of the wrong kind or on the wrong port; message of the wrong protocol; zero-length `mem.v1` request; `block.v0` data not 512 bytes; `irq.v0` outside `Complete`; request outside `Request` at the bus; duplicate `(master, txn)` at the bus; a `TxnId` counter overflow in an M2 component (§6.2) | **Session fault** (`SimError::ComponentFault`) | The session is `Faulted`, not resumable (m0-design §5.1) |
| A CPU access to no region, across a region boundary, or to a bad MMIO register, width, or offset | **`AccessFault`** (`mem.v1` `Fault`) → CPU trap (`InstructionAccessFault`, `LoadAccessFault`, `StoreAccessFault`) | The CPU halts (§5.6) |
| Command validation failure | **Device `ERROR`** 1–5 | DONE, IRQ if enabled; no side effects; the run continues |
| DMA beat answered with `Fault` | **Device `ERROR`** 6 DMA_FAULT | DONE, IRQ; partial effects per §9.7 |
| Media `Error` | **Device `ERROR`** 7 MEDIA_ERROR | DONE, IRQ; partial effects per §9.7 |
| `COMMAND` in BUSY or DONE | **`REJECTED`** (sticky) | Nothing else changes; no IRQ |

A device error is never a session fault and never a CPU trap; a DMA `Fault` never reaches the CPU.

### 9.9 Snapshot, Inspect, Trace

- **Snapshot:** config; the registers (`LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `IRQ_ENABLE`); `BUSY`/`DONE`/`REJECTED`/`ERROR`; the latched command (operation, LBA, address, count) when BUSY; the engine state (`Idle`, `Issue`, `WaitMedia { txn }`, `WaitBeat { txn }`) with its block index *i* and beat index *j*; the block buffer; both `TxnId` counters; the last IRQ level sent.
- **The block buffer is stored exactly when resuming needs it,** as a length and that many bytes, with the length fixed by the engine state:

  | Command | Engine position | Buffer stored |
  |---|---|---|
  | READ | waiting to issue, or waiting for, `ReadBlock` of block *i* | none (length 0) |
  | READ | from the accepted `Data` of block *i* through the `Done` of its beat 31: issuing or waiting for beat *j* | all 512 bytes of block *i* |
  | WRITE | issuing or waiting for beat *j* of block *i* | the first 16 × *j* bytes (the beats already read) |
  | WRITE | issuing or waiting for `WriteBlock` of block *i* | all 512 bytes until the `WriteBlock` is sent; none (length 0) once it is outstanding, since the request carries the data |
  | either | `Idle` | none (length 0) |

  After beat 31 of a READ block completes, the buffer is dropped before the next `ReadBlock` is issued.
- **Restore rejects** states no run can produce: an engine state other than `Idle` without BUSY; BUSY together with DONE; an outstanding `txn` that is not the latest issued; indices past the latched count; a buffer length other than the one the engine position requires; an IRQ level that does not equal `DONE && IRQ_ENABLE.bit0`.
- **Inspect:** the registers, the lifecycle state, and the engine progress.
- **Trace:** `platform.blk.command` (`op`, `lba`, `addr`, `count`, `accepted`), `platform.blk.done` (`error`), `platform.blk.rejected`.

---

## 10. `MultiMasterBus`

### 10.1 Why a New Component

The M1 `AddressBus` forwards requests immediately and relays the CPU's `TxnId` unchanged (m1-design §7.1). Making it arbitrate would add an event per request and change its snapshot, which would change the `m1-reference` event counts, `ExecutionDigest`, and golden files. **`AddressBus` is therefore frozen**, and M2 adds `MultiMasterBus` to `systemscope-platform`. Both share the region decoding rules of m1-design §7.1 (half-open ranges, checked arithmetic, no splitting, `AccessFault` for unrouted requests).

### 10.2 Transaction Identity

- **Masters** are given at construction as an ordered list of port names; the index is the master index. In `m2-reference`: `cpu` = 0, `dma0` = 1. Indices never change during a session.
- A transaction's identity is **`(master_port, txn)`**. Two masters may use the same `TxnId` at the same time. One master reusing a `txn` that is still queued or active faults the session.
- Each forwarded request gets a fresh **downstream `TxnId`** from one bus-owned counter, so every region sees unique, bus-owned ids. The counter follows §6.2: checked increment, no wrap; if it would overflow, the grant faults the session and nothing is sent. The response is matched by `(region, downstream txn)` and relayed to its master with the master's original `txn`.
- Queued requests are `PendingRequest { master, original_txn, target_region, request }`, where `request` already has the region offset as its address.

### 10.3 Structure

```rust
struct RegionState {
    active: Option<ActiveTxn>,                 // at most one downstream transaction per region
    queues: Vec<VecDeque<PendingRequest>>,     // one FIFO per master, indexed by master
    rr_cursor: usize,                          // next master to consider, 0 at reset
}
struct ActiveTxn { master: usize, original_txn: TxnId, downstream_txn: TxnId, write: bool }
```

- At most one transaction is outstanding downstream per region. A region never sees two requests at once, so targets need no reordering.
- Each master has its own FIFO per region, so requests from one master to one region are forwarded in arrival order.

### 10.4 Arbitration

- **Round-robin by stable master index**, per region. Grant = the first master with a non-empty FIFO, scanning circularly from `rr_cursor`: `rr_cursor`, `rr_cursor + 1`, …, mod master count.
- After a grant to master *g*: `rr_cursor ← (g + 1) mod master_count`. The cursor advances **only on a grant**.
- Initial `rr_cursor` = 0 for every region, so the CPU wins the first tie.
- **Arbitration between masters never depends on the global event sequence or on the relative dispatch order of requests from different masters in the same `Request` phase.** It reads only the FIFOs, the cursors, and `active`, which are fully updated before `Transfer` begins (§10.5).
- **Within one master's FIFO, requests preserve that master's arrival order.** Two requests from the same master to the same region in one `Request` phase are queued in the order they were dispatched. The M2 masters (the CPU and the DMA engine) have at most one request outstanding, so this never happens in `m2-reference`.

### 10.5 Timing

The bus has a `clock` (in `m2-reference`, the CPU clock).

| Phase | Bus action |
|---|---|
| `Request` | A request arrives from master *m*. A request outside `Request` faults the session. If it is unrouted, the bus answers `Fault { AccessFault }` to *m* at `Now`, `Complete` (as in M1) and queues nothing. Otherwise it is pushed on `queues[m]` of its region, and the bus wakes itself at `Now`, `Transfer`. |
| `Transfer` | The arbitration wake. For each region in region order: if `active` is `None` and some FIFO is non-empty, grant one request (§10.4), set `active`, and send it on the region's port at `Now`, `Transfer`. At most one grant per region per wake. |
| `Complete` | A response arrives from region *r*. It must match `active` of *r* (downstream `txn`, kind), or the session faults. The bus relays it to `active.master` with `original_txn` at `Now`, `Complete`, and sets `active = None`. If any FIFO of *r* is non-empty, it wakes itself at `Cycles { clock, 1 }`, `Transfer`. |

- **The next grant after a completion is in the next cycle's `Transfer`, never the same tick's.** This is a consequence of phase ordering, not an added delay: `Transfer` (1) runs before `Complete` (2) within a tick, so by the time a region is freed, that tick's `Transfer` is over. The earliest later `Transfer` is the next bus clock edge.
- **Every wake runs the whole arbitration pass,** and a pass is a pure function of the state. Several wakes may land in the same `(tick, Transfer)` (one per request that arrived in that tick's `Request`, plus one from a completion in the previous cycle). The first one grants everything grantable; the others find every region busy or empty and do nothing. Because enqueuing happens only in `Request` and freeing only in `Complete`, no state changes between two wakes of one `Transfer`. The number of wakes is deterministic, so event counts are too.
- **Uncontended latency equals M1's:** a request that arrives in `Request` is granted in the same tick's `Transfer`, exactly when `AddressBus` forwards.

Example, `m2-reference` (all links `Cycles { cpu, 1 }`, RAM responds `Cycles { cpu, 0 }`), cycle *c* = one CPU clock edge:

| Cycle | CPU (master 0) | DMA (master 1) | RAM region |
|---|---|---|---|
| *c* | sends fetch `ReadReq` in `Request` | sends beat `WriteReq` in `Request` | idle |
| *c*+1 | enqueued (`Request`) | enqueued (`Request`) | `Transfer`: cursor 0 → grant CPU, cursor ← 1 |
| *c*+2 | | waits | RAM accepts, responds in `Complete` |
| *c*+3 | | | bus `Complete`: relay to CPU, free, wake at *c*+4 |
| *c*+4 | response arrives, commits | | `Transfer`: grant DMA, cursor ← 0 |

### 10.6 Snapshot and Inspect

- **Snapshot:** config (regions with name/base/size, master names, clock), the downstream `TxnId` counter, then per region in region order: `active` (tag + fields), `rr_cursor`, and each master's FIFO in order (length, then each `PendingRequest` with its canonical `mem.v1` request).
- **Restore rejects:** a different map, masters, or clock; a cursor ≥ master count; a duplicate `(master, txn)` across all FIFOs and actives; a queued request outside its region; a downstream `txn` ≥ the counter; a non-canonical encoding.
- **No duplicate or lost requests:** forwarded requests and relayed responses in flight are runtime events, not bus state. A queued request is only in the FIFO; an active one only in `active` (its request event or response event is in the runtime queue). Restoring never re-sends anything.
- **Arbitration after restore** is identical, because the cursors and FIFOs are restored exactly and arbitration reads nothing else.
- **Inspect:** per region, `active` master and the FIFO lengths; the cursors.
- **Trace:** `platform.bus.fault` (as in M1, plus `master`), and `platform.bus.grant` (`region`, `master`, `txn`, `downstream_txn`) on every grant.

---

## 11. Reference Platform `m2-reference`

```text
soc.cpu0  Rv32iCpu, profile M2 (clock "cpu", 100 MHz)
soc.bus   MultiMasterBus, masters [cpu, dma0], clock cpu
  ├─ ram  ─▶ soc.ram   Ram                  base 0x8000_0000, size 16 MiB
  ├─ uart ─▶ soc.uart  SimpleUart           base 0x1000_0000, size 0x8
  ├─ irqc ─▶ soc.irqc  SimpleIrqController  base 0x1000_1000, size 0x8, 1 source
  └─ blk  ─▶ soc.blk   DmaBlockController   base 0x1000_2000, size 0x20
soc.disk  SimpleBlockMedia  capacity 16 blocks, latency Cycles { cpu, 16 }, image = the committed disk fixture
```

- **Components**, in declaration order: `soc.cpu0`, `soc.bus`, `soc.ram`, `soc.uart`, `soc.irqc`, `soc.blk`, `soc.disk`. One clock domain, `cpu`.
- **Links**, in order: CPU `mem` ↔ bus `cpu`; bus `ram`/`uart`/`irqc`/`blk` ↔ each target's `mem`; `soc.blk` `dma` ↔ bus `dma0`; `soc.blk` `blk` ↔ `soc.disk` `blk`; `soc.blk` `irq` ↔ `soc.irqc` `src0`; `soc.irqc` `cpu` ↔ CPU `irq`. Every link has latency `Cycles { cpu, 1 }`.
- **Targets** (RAM, UART, IRQ controller, block controller MMIO) respond `Cycles { cpu, 0 }` after acceptance, in `Complete`.
- **DMA aperture:** `dma_base = 0x8000_0000`, `dma_size = 16 MiB`.
- **Builder invariants** (checked when the topology is built; §9.1, §9.5): the DMA aperture lies wholly inside the RAM region, and `soc.blk`'s `capacity_blocks` equals `soc.disk`'s (16).
- **Seed** 0; **`max_instructions`** 10,000,000.
- The M1 memory map is unchanged; the new windows sit above the UART in the MMIO space.

---

## 12. `block_irq.elf`

### 12.1 Flow

An RV32I + Zicsr assembly program, built with the pinned toolchain in the style of `hello.elf` (m1-design §10.7). The disk fixture holds a known 512-byte pattern at LBA 0 and zeros elsewhere.

**Setup**

1. Set `mtvec` to `handler` (Direct).
2. Write 1 to the IRQ controller's `ENABLE` (source 0 = the block controller).
3. Write 1 to the block controller's `IRQ_ENABLE`.
4. Set `mie.MEIE` (`csrs mie` with bit 11).
5. Set `mstatus.MIE` (`csrsi mstatus, 8`). Interrupts are now live.

**Transfer 1: READ LBA 0 → buffer A**

6. Clear the RAM `flag` word.
7. Write `LBA = 0`, `MEM_ADDR = A` (16-byte aligned), `BLOCK_COUNT = 1`.
8. Write `COMMAND = 1` (READ).
9. Wait: loop loading `flag` from RAM until it is non-zero (§12.2). The completion interrupt enters the handler (§12.3), which sets `flag` and returns with `MRET`.
10. Check the status word the handler saved: `ERROR = 0`, else fail.
11. Verify buffer A against the expected LBA 0 pattern, else fail.

**Transfer 2: WRITE buffer A → LBA 1**

12. Modify buffer A in place (a fixed, documented transformation of every word).
13. Clear `flag`.
14. Write `LBA = 1`, `MEM_ADDR = A`, `BLOCK_COUNT = 1`.
15. Write `COMMAND = 2` (WRITE).
16. Wait on `flag`; the interrupt, handler, and `MRET` follow as in step 9.
17. Check the saved status: `ERROR = 0`, else fail.

**Transfer 3: READ LBA 1 → buffer B**

18. Clear `flag`.
19. Write `LBA = 1`, `MEM_ADDR = B`, `BLOCK_COUNT = 1`.
20. Write `COMMAND = 1` (READ).
21. Wait on `flag`; interrupt, handler, `MRET`.
22. Check the saved status, then compare buffer B with buffer A word by word.

**Finish**

23. On success, write `M2 PASS\n` to the UART and end with the M1 convention (`gp = 1`, `a0 = 0`, `ECALL`). On any failure, write `M2 FAIL\n` and end with `gp = 1`, `a0 = 1`, `ECALL`.

### 12.2 Waiting on a RAM Flag

The main program waits by loading a RAM word written by the handler. It never uses `WFI` (§5.7) and never polls the device's MMIO registers, so the only way forward is the interrupt. Each loop iteration is a RAM load that competes with the DMA beats for the RAM region, which exercises arbitration (§10).

### 12.3 Handler

1. Read the block controller's `STATUS`.
2. If `DONE` is 0, the entry is spurious: go to step 6.
3. Store `STATUS` to a RAM status word and increment a RAM entry counter.
4. Write 1 to `IRQ_STATUS`/`ACK`. The controller deasserts its line, and the IRQ controller deasserts MEIP.
5. Store 1 to `flag`.
6. `MRET`.

In `m2-reference`, the MEIP deassert reaches the CPU in the same tick as the `ACK` store's response (both take two link hops from the controller), in `Complete` before the store's `Commit`, so no spurious entry occurs: the expected entry count is exactly 3. The handler still tolerates spurious entries (step 2), because that timing is a property of the reference topology, not of the architecture.

### 12.4 Acceptance

A run passes only if it halts with `EnvironmentCall` and `a0 = 0`, the UART output is exactly `M2 PASS\n`, the handler entry counter is 3, the disk's LBA 1 equals the transformed pattern, LBA 0 is unchanged, and `STATUS.REJECTED` was never set.

---

## 13. Snapshots and Determinism

### 13.1 Stress Points

Checkpoint-and-resume must give identical results (final state, output, `StateDigest`, `ExecutionDigest`, `TraceDigest`, remaining events) from **every** event of a `block_irq.elf` run, as in M1-A6. These points are named in the tests and checked explicitly:

1. a DMA beat is pending (request in flight to the RAM);
2. a CPU request is queued behind an active DMA beat, and the reverse;
3. a media result is pending;
4. the IRQ line is asserted at the CPU but not yet taken (CPU mid-instruction);
5. just after entry to the handler (`FetchIssue` at `mtvec`);
6. the device IRQ is cleared (after `ACK`) but `MRET` has not retired;
7. a WRITE block is partially buffered (some of its 32 beats read).

### 13.2 Guarantees

- No DMA beat, block write, IRQ level change, or bus request is duplicated or lost by a restore. Every in-flight message is a runtime event, and every component stores only its own state, never a copy of an event.
- Arbitration after restore is identical (§10.6).
- **Disk identity:** a snapshot carries the media's `image_hash` and full block map. Restoring into a media built from a different image is rejected, and restore never re-applies the initial image (§8.2).
- M2 adds a portable snapshot, `tests/golden/m2-reference.mid.snap`, taken mid-DMA, which must restore on Linux and Windows.

### 13.3 Sizes

- The M1 portable snapshot `tests/golden/m1-reference.mid.snap` is **8,942 bytes** (measured at M2.0; `m0-reference.mid.snap` is 3,979).
- M2.8 records the size of `m2-reference.mid.snap` next to these numbers in this section. Expected new contributors: the media block map (512 bytes per non-zero block), the controller's 512-byte buffer, the bus FIFOs, and the CPU CSRs. No compaction is planned for M2 (§16).

### 13.4 Observation Invariance

O0–O5 (m0-design, m1-design) apply unchanged to `m2-reference`: observers never change output, events, or digests.

---

## 14. Versioning and Contracts

- **`COMPATIBILITY_ID` stays `"0.0.0"`.** Under the M1 policy (m1-design §4.5) it changes only when existing snapshots or traces can no longer be restored or resumed. M2 adds two protocols and new component schemas; every existing encoding, snapshot, and trace decodes exactly as before.
- **Protocol names:** `irq.v0` = `("irq", 0)` and `block.v0` = `("block", 0)`. `Message` gains `Irq(irq_v0::IrqMsg)` and `Block(block_v0::BlockMsg)`; the strict decoder dispatches on `(name, version)` and rejects anything else. The `Message` variant tag is not encoded, so no existing encoding changes (as for `mem.v1`, m1-design §4.3).
- **`mem.v0` and `mem.v1` are frozen.** M2 adds no `mem.v1` variant.
- **Component schemas:** `AddressBus` schema 1, `Ram`, `SimpleUart`, and the CPU's schema 1 (M1 profile) are unchanged. New: CPU schema 2 (M2 profile), `MultiMasterBus`, `SimpleIrqController`, `SimpleBlockMedia`, `DmaBlockController`, each schema 1.
- **M2.0a (contracts):** `irq_v0.rs` and `block_v0.rs` with rustdoc, `Message` variants, canonical encoding, golden encoding vectors, and strict-decoding tests, in `contracts`; then `systemscope` bumps its `contracts` pin. `contracts` is tagged `v0.3.0-m2` at release, on the commit `systemscope` pins.

---

## 15. Validation

### 15.1 Matrix

| Area | Oracle | Tests | Step |
|---|---|---|---|
| `irq.v0`, `block.v0` encoding | hand-written golden bytes | every variant, strict decode, round-trip | M2.0a |
| Zicsr, CSR WARL, `MRET` | pinned Spike (directed), and this document's tables for divergences | §15.2 | M2.1a |
| Unsupported CSRs | Spike for CSRs it also rejects; §4.5 otherwise | trap cause and `tval`, halt | M2.1a |
| MEI entry and boundary | the pure oracle `take_mei` (§5.8) | property tests over CSR values and levels; directed re-entry (§5.4) and CSR-enable (§5.5) programs; no entry in non-boundary states | M2.1b |
| M2 CPU on RV32I | M1 oracles | 40 `rv32ui`, 39 ACT4 on the M2 profile | M2.1a |
| `SimpleIrqController` | the formula `(pending & enable) != 0` | unit tests, property test over level/enable sequences, MMIO map | M2.2 |
| `MultiMasterBus` | an independent arbitration model written from §10 | scripted test masters; contention, fairness, identity, `AccessFault`, session faults; every-event snapshot; permuting same-phase arrival order across different masters does not change the granted master sequence | M2.3 |
| `SimpleBlockMedia` | a `BTreeMap` model | canonical storage, `image_hash` identity, restore rejection, errors | M2.4 |
| Controller registers and lifecycle | §9.2–§9.4 tables | every register and access width, `REJECTED` rules, validation order, IRQ line | M2.5 |
| DMA engine | a reference transfer model | READ/WRITE multi-block, partial READ and WRITE failures (§9.7), contention with a CPU load loop | M2.6 |
| End-to-end | §12.4 | `block_irq.elf` on `m2-reference` | M2.7 |
| Snapshot/restore | resume equivalence | every event, the §13.1 stress points, the portable mid-DMA snapshot | M2.8 |
| Observation invariance | M0 O0–O5 | on `m2-reference` | M2.8 |
| Golden | `tests/golden/m2-reference.json` | Linux and Windows, cross-OS | M2.8 |
| M1 regressions | §15.4 | every step | all |

### 15.2 M2.1a Spike-Directed Tests

Directed programs run on the pinned Spike (`19609434`) with `--isa=rv32i_zicsr --priv=m`, compared per retirement (`pc`, register writes, CSR writes of whitelisted CSRs) up to the first trap, reusing the M1-A3 judges. Each is kept to the behaviors where §4 and Spike agree:

- CSR read/write for each whitelisted CSR, in all six forms, including write suppression (`rs1 = x0`, `uimm = 0`);
- immediate forms with `uimm` 0, 1, and 31;
- read-only and ignored bits: `mip` writes, unimplemented `mstatus` bits, `mepc` bits [1:0];
- `mstatus` MIE/MPIE writes and MPP always `0b11`;
- `mie`: only MEIE written (§4.6.2);
- `mtvec` writes with MODE ∈ {0, 2} (§4.6.4);
- `mepc`, `mcause`, `mtval`, `mscratch` with all-ones and patterns;
- `MRET`: the three `mstatus` cases of Appendix A.1, `pc ← mepc`;
- unsupported-CSR traps on `medeleg`, `mideleg`, `mcounteren`, `satp`, `cycle`, `time`, `instret`, a custom CSR, and writes to `mhartid`: cause `IllegalInstruction`, `tval` = the instruction.

Spike logs `mstatush` and `tcontrol` writes on `MRET`; the comparison ignores CSRs outside the whitelist. Divergent behaviors (§4.5, §4.6) are SystemScope-only unit tests with values from this document.

Two observations from the M2.0 measurements for the harness:

- The pinned Spike accepts Zicsr with `--isa=rv32i` too (it reports `rv32i2p1_zicsr2p0`). M1-A3 is unaffected: its programs contain no CSR instructions.
- With `--instructions=N`, the observed Spike run ended at the first trap taken into a handler; without it, execution continued through the handler. The M2.1a harness stops at the first trap anyway, as M1-A3 does; any test that runs past a trap in Spike must not rely on `--instructions`.

MEI entry itself is not compared with Spike: triggering MEIP in Spike needs its PLIC and an external source, which the differential does not model. M2.1b uses the pure oracle instead.

### 15.3 ACT4

- **`include_priv_tests` stays `False`.** The privileged ACT4 tests assume full Sm (at least `misa`, `mhartid`, full `mstatus`, synchronous exception delivery through `mtvec`, and the counters). M2 implements a subset (§4) and halts on synchronous exceptions (§5.6), so those tests cannot run, and running a selection would suggest a conformance M2 does not have.
- The unprivileged corpus (39 RV32I tests) is unchanged and keeps passing, on both CPU profiles.
- Sm appears only in the ACT4/UDB adapter configuration, as in M1 (m1-design §10.4). No document may claim Sm certification or compliance.

### 15.4 M1 Regressions

Every M2 step must keep all of these passing, unchanged:

- M0 golden files (`m0-reference.json`, `m0-reference.mid.snap`) byte-identical to `v0.1.0-m0`.
- M1 golden files (`m1-reference.json`, `m1-reference.mid.snap`) byte-identical to `v0.2.0-m1`. **Never re-blessed.**
- `m1-reference` built from `AddressBus` and the `M1` CPU profile, with unchanged event counts and digests.
- M1-A1 to M1-A8: unit/property tests, 40 `rv32ui`, the Spike differential (40 `rv32ui`, 64 generated, 9 misaligned, and the nightly seed), 39 ACT4, `hello.elf`, snapshot/restore, observation invariance, cross-OS.
- The generated-program `FIXED_SEEDS_DIGEST` and every fixture manifest unchanged.
- The M0 acceptance tests AT-1 to AT-3.

---

## 16. Architecture Risks

1. **Global sequence versus arbitration.** Events in one `(tick, phase)` run in global-sequence order, which depends on unrelated scheduling history. Any decision that depends on that order would make behavior fragile. Mitigation: requests are enqueued in `Request` into per-master FIFOs and arbitrated in `Transfer`; IRQ levels land in `Complete` and are sampled in `Commit`; CSR reads happen in `Commit` (§6.3). M2.3 tests that permuting same-phase arrival order across different masters does not change the granted master sequence.
2. **Five-phase expressiveness.** `Request`/`Transfer`/`Complete`/`Commit`/`Observe` must carry request, arbitration, target acceptance, responses and level changes, and architectural commit. M2 fits because every hop has at least one cycle of link latency. A future zero-latency chain (a level that must propagate through several components in one tick) would need either same-phase chains (allowed by S2 but order-sensitive) or more phases. Record any such pressure before adding workarounds.
3. **Snapshot scaling.** Media contents are stored whole in every snapshot (§13.3). A realistic disk would dominate snapshot size. M2 keeps capacity small; content-addressed or delta snapshots are a later decision, not an M2 change.
4. **Partial Sm abstraction.** Real firmware may touch CSRs M2 rejects (`misa`, `mhartid`) and will halt with `IllegalInstruction`, and synchronous exceptions halt instead of trapping to `mtvec`. The wording of §4.1 must stay precise, and M3 must extend the same CSR file rather than replacing it.
5. **Protocol fidelity evolution.** `irq.v0` is level-only and `block.v0` is single-block and in-order. Edge or message-signaled interrupts, queued or out-of-order storage commands, and NVMe-like semantics are new protocol versions (`irq.v1`, `block.v1`), never changes to v0.

---

## 17. Roadmap

| Step | Content | Merge unit |
|---|---|---|
| **M2.0** | This design freeze | docs only |
| **M2.0a** | `contracts`: `irq.v0`, `block.v0` (§14); pin bump | contracts + pin |
| **M2.1a** | CPU `M2` profile: Zicsr subset, CSR file, `MRET`; Spike-directed tests (§15.2) | CPU |
| **M2.1b** | MEI: `irq` port, boundary sampling, entry; pure oracle and property tests; re-entry and CSR-enable tests | CPU |
| **M2.2** | `SimpleIrqController` | platform |
| **M2.3** | `MultiMasterBus`: identity, round-robin, timing, snapshot; scripted test masters | platform |
| **M2.4** | `SimpleBlockMedia`: sparse storage, `image_hash`, snapshot | platform |
| **M2.5** | `DmaBlockController` MMIO: registers, lifecycle, `REJECTED`, validation, IRQ line | platform |
| **M2.6** | DMA engine: beats, READ/WRITE, failure semantics, contention | platform |
| **M2.7** | `m2-reference`, `block_irq.elf` and the disk fixture with manifests, end-to-end acceptance | tests |
| **M2.8** | Snapshot stress, observation invariance, `m2-reference.json` and `m2-reference.mid.snap` golden files, cross-OS, CI | tests + CI |
| **M2.9** | Release documentation, exit criteria evidence; tag `v0.3.0-m2` | docs |

Each step follows the M1 discipline: a feature branch, the full local gate, one push, a draft PR, CI, then a fast-forward merge.

---

## 18. M2 Exit Criteria

- [ ] `irq.v0` and `block.v0` are in `contracts`, with rustdoc, golden encoding vectors, and strict decoding; `mem.v0`, `mem.v1`, and `COMPATIBILITY_ID` are unchanged.
- [ ] The `M2` CPU profile implements §4 and §5, checked by the Spike-directed tests, the pure oracle, and the re-entry and CSR-enable tests; the `M1` profile is unchanged.
- [ ] `MultiMasterBus`, `SimpleIrqController`, `SimpleBlockMedia`, and `DmaBlockController` implement §7–§10 with the tests of §15.1.
- [ ] `block_irq.elf` passes on `m2-reference` (§12.4) on Linux and Windows.
- [ ] Snapshot/restore from every event, the §13.1 stress points, and the portable mid-DMA snapshot pass; observation invariance holds.
- [ ] `tests/golden/m2-reference.json` is blessed once, and every M0 and M1 golden file is byte-identical to its release (§15.4).
- [ ] Every fixture (the ELF and the disk image) is pinned by a manifest.
- [ ] Contract changes discovered during M2 are reflected back into this document.

---

## 19. Open Questions

None of the core decisions (§3) is open. These do not block M2 and are recorded for later milestones:

- Perfetto slices for `block.v0` transactions: M2 shows them as instant dispatch events with flattened fields, like any protocol; pairing them into slices is an exporter change for later.
- Synchronous exception delivery through `mtvec`, and `mstatus`/`mepc` semantics for it (M3, with U-mode).
- Timer interrupts and a CLINT-like timer, needed for preemption in M3.
- A PLIC-compatible controller, if a later OS backend needs one.
- Snapshot compaction for large media (§16, risk 3).

---

## Appendix A: Spike CSR Measurements (M2.0)

Measured with the pinned Spike: the local M1-A3 build, whose `spike.stamp` records commit `19609434bb3d83448eec8796e8f0367c868efbda` (the `SPIKE_COMMIT` pin), on 2026-09-24, before the rules of §4 were fixed:

```text
spike --isa=rv32i_zicsr --priv=m --pcs=0:0x80000000 -m0x80000000:0x1000000 \
      --disable-dtb -l --log-commits <program>.elf
```

The programs were RV32I + Zicsr assembly, built with the pinned rv32 toolchain (`-march=rv32i_zicsr -mabi=ilp32`), each probe writing a CSR and reading it back, with `mtvec` pointing at a handler that records `mcause`/`mtval` and skips the trapping instruction. Values are from the commit log. The programs are not committed; M2.1a turns them into directed tests.

### A.1 Writes

| CSR | Written | Read back |
|---|---|---|
| `mtvec` | `0x8000_1000` (MODE 0) | `0x8000_1000` |
| `mtvec` | `0x8000_1001` (MODE 1) | `0x8000_1001` |
| `mtvec` | `0x8000_1002` (MODE 2) | `0x8000_1000` |
| `mtvec` | `0x8000_1003` (MODE 3) | `0x8000_1001` |
| `mtvec` | `0x8000_1004` (BASE not 64-aligned) | `0x8000_1004` |
| `mtvec` | `0x8000_1005` | `0x8000_1005` |
| `mtvec` | `0xFFFF_FFFF` | `0xFFFF_FFFD` |
| `mtvec` | `0x0000_0000` | `0x0000_0000` |
| `mstatus` | `0xFFFF_FFFF` | `0x0000_1888` |
| `mstatus` | `0x0000_0000` | `0x0000_1800` |
| `mstatus` | `0x0000_0008` | `0x0000_1808` |
| `mstatus` | `0x0000_0080` | `0x0000_1880` |
| `mstatus` | `0x0000_0088` | `0x0000_1888` |
| `mie` | `0xFFFF_FFFF` (and `csrrs` all ones) | `0x0000_0888` |
| `mie` | `0x0000_0800` | `0x0000_0800` |
| `mip` | `0xFFFF_FFFF` (and `csrrs`, `csrrw`) | `0x0000_0000`, no trap |
| `mepc` | `0xFFFF_FFFF` | `0xFFFF_FFFC` |
| `mepc` | `0x8000_0002` | `0x8000_0000` |
| `mcause` | `0xFFFF_FFFF` | `0xFFFF_FFFF` |
| `mcause` | `0x8000_000B` | `0x8000_000B` |
| `mtval` | `0xFFFF_FFFF` | `0xFFFF_FFFF` |
| `mscratch` | `0xFFFF_FFFF`, `0x1234_5678` | as written |
| `mscratch` | `csrrwi 31`, `csrrsi 0`, `csrrsi 16`, `csrrci 1`, `csrrs 0xF0`, `csrrc 0xF0` | `0x1F`, no write, `0x1F`, `0x1E`, `0xFE`, `0x0E` (old value to `rd` each time) |
| `mstatush` | `0xFFFF_FFFF` | `0x0000_0000` |

**`MRET`** (with `mepc` set to the next label):

| `mstatus` before | after | `pc` |
|---|---|---|
| `0x1880` (MPIE 1, MIE 0) | `0x1888` | `mepc` |
| `0x1808` (MPIE 0, MIE 1) | `0x1880` | `mepc` |
| `0x1888` | `0x1888` | `mepc` |

### A.2 Reset Values and Unsupported CSRs

- Reset: `mstatus` = `0x0000_1800`; `mie`, `mip`, `mtvec`, `mepc`, `mcause`, `mtval`, `mscratch` = 0.
- **Readable in Spike** (illegal in SystemScope, §4.5): `misa` = `0x4000_0100`, `mhartid` = 0, `mvendorid` = 0, `marchid` = 5, `mimpid` = 0, `mconfigptr` = 0, `mcycle`/`minstret` (running counts), `pmpcfg0` = `0x1F`, `pmpaddr0` = `0xFFFF_FFFF`, `mstatush` = 0. `csrw misa, 0` is accepted and ignored. `csrrs`/`csrrsi` with no write on `mhartid` do not trap.
- **Illegal in Spike too** (`trap_illegal_instruction`, `tval` = the instruction): `medeleg`, `mideleg`, `mcounteren`, `satp`, `cycle`, `time`, `instret`, custom CSR `0x7C0`, `csrw mhartid`, `csrrci mhartid, 1`.
