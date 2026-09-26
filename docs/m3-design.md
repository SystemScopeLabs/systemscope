# M3 Design: Modeled OS Backend

> Status: Design frozen (M3.0), with M3.2 clarifications to §5.1, §5.3, §5.5, §15.1, §15.3, and §17 from [m3-2-spike-appendix.md](m3-2-spike-appendix.md) · Parent: [plan.md](../plan.md) · Builds on: [m2-design.md](m2-design.md), [m1-design.md](m1-design.md), [m0-design.md](m0-design.md)

This document is the architecture contract for M3. It fixes the decisions the M3 implementation steps (§17) depend on. §19.1 records the design choices accepted at the freeze and the alternatives considered. Nothing in it is implemented yet: M3.0 changes documentation only. The M2 reference platform is frozen, and nothing here changes it.

---

## 1. Goal and Topology

**M3 runs user programs loaded from simulated storage as processes. They live in their own Sv32 address spaces, whose page tables are in simulated RAM, and they reach a modeled kernel only through `ecall` and architectural traps. Their output leaves through a syscall and the UART.**

plan.md §11 names the M3 exit criterion: "the full path from executable to output runs through models, with page tables living in simulated RAM". M3 gives that path one general execution model, the **first execution slice**:

```text
Executable ─▶ Storage ─▶ RAM ─▶ Process ─▶ CPU ─▶ Memory ─▶ Syscall ─▶ Kernel ─▶ Output
 ELF file     disk       frames  PCB +     U-mode  Sv32 walk  ecall →     dispatch  write(1) →
 on disk      (block.v0, + page  address   fetch/  in RAM     S-mode      + process UART TX
              DMA)       tables  space     execute            trap        transition
```

The canonical M3 topology, `m3-reference` (§11), is `m2-reference` plus one component:

```text
      ┌──────────────────────── irq.v0 (MEIP, unused by the kernel) ─────┐
      ▼                                                                  │
soc.cpu0  Rv32iCpu (M3 profile: M/S/U, Sv32)                             │
      │ mem.v1 (master 0)                                                │
      ▼                                                                  │
soc.bus   MultiMasterBus ◀── mem.v1 (master 1: dma0) ─────────────┐      │
  │                      ◀── mem.v1 (master 2: kernel0) ──────┐   │      │
  ├─ ram   ─▶ soc.ram     Ram  (firmware, trampoline, frames, │   │      │
  │                             page tables, user pages)      │   │      │
  ├─ uart  ─▶ soc.uart    SimpleUart                          │   │      │
  ├─ irqc  ─▶ soc.irqc    SimpleIrqController ────────────────┼───┼──────┘
  ├─ blk   ─▶ soc.blk     DmaBlockController ─────────────────┼───┘
  │                          │ block.v0                       │
  │                          ▼                                │
  │                       soc.disk  SimpleBlockMedia (ExecTable + ELF files)
  └─ kgate ─▶ soc.kernel  ModeledKernel ──────────────────────┘
```

In words:

- The CPU gains an **M3 profile**. It is the M2 CPU extended with supervisor and user modes, delegated synchronous exceptions, `SRET`, and Sv32 translation. Page walks read PTEs from RAM over the CPU's `mem.v1` port (§4, §5).
- **`ModeledKernel`** (§6) is the Modeled OS Backend of plan.md §6. It is a Rust state machine holding the process table, frame allocator, loader, and syscall dispatcher. It has two `mem.v1` ports:
  - `gate`, a small MMIO target window, `kgate`;
  - `mem`, a bus master (`kernel0`), through which it reads and writes RAM (page tables, trap frames, user memory), programs the `DmaBlockController`, and writes the UART.
- **Guest-side glue** is ordinary RV32 code in RAM: a machine-mode boot stub and a supervisor-mode trap trampoline (§7). The trampoline saves the user context into a trap frame in RAM and enters the kernel with one store to `kgate.ENTER`. The kernel holds that store's response until it has finished the trap, then the trampoline restores the (possibly different) context and executes `SRET`.
- Storage, DMA, the bus, the RAM, the UART, and the IRQ controller are the M2 components, **unchanged**.

**The CPU never knows the kernel is a model.** Everything the kernel does reaches the hart through architectural state: CSRs and registers that the trampoline loads from the trap frame, page tables in RAM, and `SRET`. That is the property plan.md §6 requires for M6: a native guest kernel replaces the boot stub, the trampoline, `kgate`, and `ModeledKernel`, and needs no change to CPU code.

M3 is done when the M3 reference workload (§12) runs to a clean shutdown on `m3-reference` with the expected UART output, exit statuses, and syscall trace, including a process killed by a page fault, and every M0, M1, and M2 acceptance test still passes unchanged (§18).

---

## 2. Scope and Non-goals

### 2.1 In Scope

- **Privilege:** machine, supervisor, and user modes; `medeleg` delegation of synchronous exceptions to S-mode; `SRET`; the supervisor CSRs of §4.3; `MRET` returning to any mode.
- **Virtual memory:** `satp` and Sv32. There are two-level page walks in simulated RAM, PTE permission checks (`R`/`W`/`X`/`U`, `SUM`, `MXR`), 4 MiB megapages, and instruction, load, and store page faults. `SFENCE.VMA` is included.
- **Process model:** PID, state, saved register context, address space (root page table and mapped regions), a FIFO run queue, creation, exit, fault kill, and context switch.
- **Kernel model:** trap entry through the trampoline and `kgate`, trap classification, syscall dispatch with a Linux-style RV32 ABI, and process transitions.
- **Loader:** the executable table on disk (§8.1), DMA of an ELF file into RAM through the M2 block controller, user ELF validation, segment mapping with permissions, stack setup, and entry.
- **Output:** `write` on fd 1 and 2 to the `SimpleUart`.
- **Verification:** the CPU M3 profile against Spike and pure oracles, and the kernel against a pure kernel oracle. The reference workload, snapshot/restore from every event, observation invariance, and `m3-reference` golden files are included too (§15).

### 2.2 Non-goals

M3 does **not** include, and nothing in this document may be read as promising:

- **Scheduling:**
  - no preemption, timer interrupt, CLINT, priorities, or time slices;
  - the only scheduling decision is the FIFO order of §6.6, with `sched_yield` as the only voluntary switch.
- **Storage software:** no filesystem, directories, file descriptors other than 1 and 2, `open`/`read`, or writing to the disk from user space. The executable table (§8.1) is a flat boot manifest, not a filesystem.
- **Process features:**
  - no `fork`, `exec` from user space, `wait`, signals, threads, shared memory, or `brk`/`mmap`;
  - no demand paging, swapping, or copy-on-write;
  - processes exist only as the executable table creates them at boot.
- **Hardware:** no SMP or multiple harts, TLB, caches, PMP, `Svadu` (hardware A/D updates), `Sv39`, ASIDs, the H extension, or S-mode interrupts (`mideleg` is read-only zero).
- **Kernels and interfaces:** no real Linux kernel or other real kernel (that is M6's Native Guest OS Backend), no SBI beyond the one shutdown call of §7.4, and no networking or GUI.
- **Conformance claims:** M3 implements a documented subset of the privileged architecture (§4.1). No document may claim Sm, Ss, or Sv32 certification or compliance, and ACT4 `include_priv_tests` stays `False` (§15.4).
- **Performance meaning:** as in M1 and M2, cycle counts are deterministic and pinned by golden digests, but they are not a performance model (§10).

---

## 3. M3.0 Design Decisions

Decisions marked **(R)** were settled in the M3.0 review and accepted at the freeze; §19.1 records the alternatives considered for each. The rest follow from plan.md and the M2 freeze.

| Topic | Decision | Section |
|---|---|---|
| M2 base | `m2-reference`, its components, the `M1` and `M2` CPU profiles, and every M0/M1/M2 golden file are frozen. M3 adds a profile, a component, and a platform. It never edits frozen behavior | §4.2, §15.5 |
| Kernel realization **(R)** | A Rust `ModeledKernel` component, entered through a guest trampoline and a held MMIO store (`kgate.ENTER`); it has no CPU-side hook | §6, §7 |
| Kernel privilege **(R)** | Kernel boundary in S-mode (trampoline); a minimal M-mode boot stub delegates exceptions and enters S-mode with `MRET` | §7 |
| M-mode synchronous traps **(R)** | Still halt the CPU with no CSR written (M2 §5.6); only delegated exceptions are delivered, to `stvec` | §5.3 |
| End of run **(R)** | The trampoline's SBI `SRST` call (`ecall` from S-mode, not delegated) halts the CPU; `a1` carries the reason | §7.4 |
| CPU profile | New `M3` profile, snapshot schema 3 = schema 2 plus the mode, the new CSRs, and walk state; same CSR file, extended | §4.2, §9.1 |
| Translation | Sv32 only; `satp.ASID` reads 0; M-mode is always bare (`MPRV` read-only 0) | §5.1 |
| A/D bits **(R)** | Svade behavior: a clear `A`, or a clear `D` on a store, raises a page fault; the CPU never writes a PTE | §5.2 |
| TLB **(R)** | None at F2: every translated access walks; `SFENCE.VMA` is a legal no-op in S and M | §5.4 |
| Walk transport | Each PTE read is a `mem.v1` `ReadReq { len: 4 }` on the CPU's `mem` port, one outstanding, scheduled like a data access | §5.4 |
| Exception priority | M1's order is kept: alignment is checked before translation, translation before the access; confirmed against the specification text and Spike at M3.3 before it is relied on | §5.3 |
| M6 CPU contract | The `M3` profile defines the architectural surface intended for the M6 native backend scenario (plan.md §11); nothing a native kernel needs for the M3 scenario may be deferred to a later CPU change. M3 does not prove M6 correctness (§16 risk 4) | §5, §16 |
| Storage | The M2 `SimpleBlockMedia` and `DmaBlockController`, unchanged; the kernel programs them over MMIO as bus master 2 | §8 |
| DMA wait **(R)** | The kernel polls `STATUS` and keeps `IRQ_ENABLE = 0`; device interrupts are not used by the modeled kernel | §8.2 |
| Disk layout **(R)** | Executable table at LBA 0 (`SSX0`), up to 8 entries, each a contiguous ELF file | §8.1 |
| Syscall ABI **(R)** | Linux RV32 asm-generic numbers: `a7` = number, `a0`–`a5` = arguments, `a0` = result, `-errno` on error. Set: `write` 64, `exit` 93, `exit_group` 94, `sched_yield` 124, `getpid` 172 | §6.5 |
| Scheduling **(R)** | FIFO run-to-completion plus `sched_yield`; no timer or preemption in M3 | §6.6 |
| Fault policy | A delegated exception other than a user `ecall` kills the process (`os.process.fault`); the kernel continues with the next process | §6.7 |
| Frames | Lowest-free-first bitmap allocator over a fixed pool; each frame is zeroed on allocation and freed on process exit | §6.4 |
| Contracts | No new protocol; `mem.v0`, `mem.v1`, `irq.v0`, `block.v0`, and `COMPATIBILITY_ID` `"0.0.0"` unchanged; `contracts` pin stays `90900a1` | §14 |
| ACT4 | `include_priv_tests: False` stays | §15.4 |

---

## 4. The M2 Base and What M3 Adds

### 4.1 What M2 Provides

M3 is built only from what M2 already implemented and verified (`docs/releases/m2-exit-audit.md`):

| M2 capability | Where | M3 use |
|---|---|---|
| RV32I CPU, F2, precise traps, one outstanding memory op, commit-only architectural state | m1-design §5–§6 | Unchanged execution core of the M3 profile |
| Machine CSR file (8 CSRs), Zicsr, `MRET`, MEI at the retirement boundary | m2-design §4–§6 | Extended in place (m2-design §16 risk 4) |
| Checked `TxnId` allocation | m2-design §6.2 | Kept for the M3 profile and `ModeledKernel` |
| `Ram`, sparse and canonical | m1-design §7.2 | Holds firmware, trampoline, trap frame, staging, page tables, and user frames |
| `SimpleUart` | m1-design §7.3 | Target of `write` |
| `MultiMasterBus`, `(master, txn)` identity, round-robin | m2-design §10 | Gains a third master (`kernel0`) and a region (`kgate`); no code change (§11) |
| `SimpleBlockMedia`, `block.v0`, `DmaBlockController` | m2-design §8–§9 | Store and load executables, unchanged |
| `SimpleIrqController`, `irq.v0` | m2-design §7 | Present and wired as in M2, but unused by the modeled kernel (§8.2) |
| Snapshot/restore from every event, component-owned state only | m0/m1/m2 §13 | Extended to the new state (§9) |
| Deterministic runtime, five phases, observation invariance, trace and digests | m0-design | Unchanged |
| Host-side ELF loader `systemscope-elf` | m1-design §8 | Unchanged; loads the M3 firmware image into RAM (§7.1) |

### 4.2 What Does Not Change

- **Runtime and contracts:** the scheduler, phases, time, snapshot format, trace format, and every protocol. M3 adds trace kinds and component schemas, which is not a format change (m2-design §14).
- **The `M1` and `M2` CPU profiles:** their decode, execution, CSR rules, snapshot schemas 1 and 2, and traces are byte-for-byte unchanged. The M3 behavior is reachable only in the `M3` profile.
- **Every M2 platform component:** `Ram`, `SimpleUart`, `AddressBus`, `MultiMasterBus`, `SimpleIrqController`, `SimpleBlockMedia`, and `DmaBlockController`. Their code, schemas, and traces stay as they are. `m3-reference` only configures them differently: more bus masters and regions, and a larger disk.
- **`m1-reference` and `m2-reference`:** topology, fixtures, and golden files. **Never re-blessed.**
- **The host-side ELF loader's M1 rules** (m1-design §8): the user-image validation of §8.3 is a new function next to them, not a change to them.

---

## 5. CPU: The `M3` Profile

### 5.1 Privilege Modes and CSRs

The hart has a current privilege mode `priv ∈ {M, S, U}`, reset to M. It is architectural state (snapshot, inspect), but no CSR exposes it. These three are the only modes the `M3` profile supports. Encoding 2 is reserved: no instruction, trap entry, or return produces it, and restore rejects it (§5.5).

**Canonical privilege fields.** Every privilege-valued field of the CPU state holds only a supported mode:

- `priv` and `MPP` hold one of {U, S, M};
- `SPP` holds one of {U, S}.

An implementation stores them as types with exactly those values, so a reserved encoding cannot be represented. Each field can come from four places, and none of them produces a reserved encoding:

- **Reset:** gives supported values only.
- **A CSR write:** is legalized (`MPP = 0b10` becomes U).
- **Trap entry and return:** copy only values that are already supported.
- **Restore:** rejects a reserved encoding before any state is built (§5.5).

**Reset (M3 profile only).** `priv` = M. `mstatus` = `0x0000_0000`, so `MPP` = U and every other field is 0. Every CSR added in this section is 0. The M2 CSRs reset as in M2, apart from `mstatus`. This matches the pinned Spike (m3-2-spike-appendix B.1). The `M2` profile keeps its own reset `mstatus` of `0x0000_1800` with `MPP` hardwired to `0b11` (m2-design §4.3). This rule applies to the `M3` profile and schema 3 only.

**CSR access rule.** Bits [9:8] of a CSR number give the lowest privilege that may access it, and bits [11:10] = `0b11` mark it read-only. Accessing a CSR from a lower mode raises `IllegalInstruction`, and so does writing a read-only one; in both cases `tval` is the instruction. In U-mode every CSR is therefore illegal.

**The M3 whitelist** is the eight M2 CSRs plus the ones below. Every other CSR number still raises `IllegalInstruction` (m2-design §4.5).

| CSR | Rule in the `M3` profile |
|---|---|
| `mstatus` | Writable: `SIE`(1), `MIE`(3), `SPIE`(5), `MPIE`(7), `SPP`(8), `MPP`(12:11), `SUM`(18), `MXR`(19). `MPP` is WARL over {U=0, S=1, M=3}. A write of the reserved encoding `0b10` stores U (`0b00`), as the pinned Spike does (m3-2-spike-appendix B.4 P1); the other written fields are stored as usual. `MPP` therefore never holds `0b10`. `MPRV`, `TVM`, `TW`, `TSR`, the endianness bits, and `FS`/`VS`/`XS`/`SD` read 0 |
| `sstatus` | The S view of `mstatus`: `SIE`, `SPIE`, `SPP`, `SUM`, `MXR`; everything else reads 0, and writes to it are ignored |
| `medeleg` | WARL; writable mask `0xB1FF`, which covers causes 0–8, 12, 13, and 15. Bit 9 (`ecall` from S) and bit 11 (`ecall` from M) read 0, so an S-mode `ecall` always goes to M (§7.4) |
| `mideleg` | Read-only 0: no interrupt is delegated |
| `sie`, `sip` | Read-only 0 (there are no S-level interrupts) |
| `stvec` | Direct mode only; a write stores `value & !0b11`, like `mtvec` |
| `sscratch` | Any value |
| `sepc` | Bits [1:0] read 0 (IALIGN = 32), like `mepc` |
| `scause`, `stval` | As `mcause` and `mtval` (m2-design §4.6) |
| `satp` | `MODE` (bit 31: 0 Bare, 1 Sv32), `ASID` (30:22) reads 0, `PPN` (21:0). WARL, see the `satp` rule below. A write takes effect from the next instruction, because CSR writes land in `Commit` (m2-design §6.3) |

`misa`, `mhartid`, the counters, `mcounteren`, `scounteren`, and `menvcfg` remain unsupported (§19.2).

**`satp` rule.** `MODE` is one bit on RV32, and each of its two values is either a supported mode or an unsupported one:

| Step | Supported | Unsupported |
|---|---|---|
| M3.2 | Bare (0) | Sv32 (1) |
| M3.3 onward | Bare (0), Sv32 (1) | none |

- **A write with a supported `MODE`** stores `MODE` and `PPN` as written and stores `ASID` as 0. With Bare, `PPN` is stored and read back, but it is not used.
- **A write with an unsupported `MODE`** leaves every field of `satp` at its previous value. This is the privileged specification's rule for an unsupported mode. The CSR instruction still retires normally: `rd` receives the old value, and `pc` advances.
- **Determinism:** the new value depends only on the old `satp` and the written value, never on other state or timing. The snapshot stores the 32-bit value, and restore rejects a value that no write could produce (§5.5).

**`WFI`** is illegal in every mode in the `M3` profile, as in M2 (m2-design §5.7), and raises `IllegalInstruction` with `tval` = the instruction. The pinned Spike retires `WFI` in M, and in S with `TW` = 0, then waits for an interrupt. The Spike differential therefore compares `WFI` only in U, where both raise `IllegalInstruction`; m3-2-spike-appendix B.6 records the exclusion (D9). A SystemScope-only unit test covers M and S.

**Changed M2 rules (M3 profile only):**

- **`MRET`**: `priv ← MPP`, `MIE ← MPIE`, `MPIE ← 1`, `MPP ← U`. In the M2 profile `MPP` stays hardwired to `0b11`.
- **`SRET`**, new: legal in S and M, illegal in U. It sets `priv ← SPP`, `SIE ← SPIE`, `SPIE ← 1`, `SPP ← U`, and `pc ← sepc`, and it retires.
- **`SFENCE.VMA`**, new: legal in S and M, illegal in U, and it retires as a no-op (§5.4). `MRET` from S or U is illegal. `WFI` stays illegal in every mode (see the `WFI` rule above).
- **MEI** (m2-design §5): it is still sampled only after a retirement. It is eligible when `MEIP & MEIE & (priv < M || MIE)`. On entry `MPP ← priv` and `priv ← M`; every other entry rule is unchanged. The reference workload never sets `MEIE` (§8.2), but the rule is specified and tested (§15.1).

  **Ownership:** this rule belongs to the `M3` profile only. The `M2` profile keeps m2-design §5 exactly: MEI is eligible when `MEIP & MEIE & MIE`, and `MPP` stays `0b11`. The M2 `take_mei` oracle and its tests are unchanged. The M3 rule is a separate oracle that also takes `priv`, and only the `M3` profile uses it. `mideleg` is 0, so MEI is always taken in M, from any mode.

### 5.2 Sv32 Translation

A fetch, load, or store is translated when `priv ∈ {S, U}` and `satp.MODE = Sv32`. M-mode is always bare. The walk is the privileged specification's Sv32 algorithm, with these fixed choices:

- **PTE format:** `V`(0) `R`(1) `W`(2) `X`(3) `U`(4) `G`(5) `A`(6) `D`(7), RSW (9:8, ignored), and `PPN[0]` (19:10), `PPN[1]` (31:20). The PTE address is `(a.PPN × 4096) + VPN[i] × 4`, where `a` starts at `satp.PPN` and level `i` goes from 1 down to 0.
- **Invalid:** `V = 0`, or `R = 0` with `W = 1`. Either raises a page fault.
- **A pointer at level 0** (`R = W = X = 0`) raises a page fault.
- **A misaligned megapage** (a level-1 leaf with `PPN[0] ≠ 0`) raises a page fault.
- **Permissions:**
  - Fetch needs `X`.
  - Load needs `R`, or `X` when `MXR = 1`.
  - Store needs `W`.
  - U-mode needs `U = 1`.
  - S-mode on a `U = 1` page faults on fetch always, and on load or store unless `SUM = 1`.
- **A/D (Svade):** `A = 0`, or `D = 0` on a store, raises a page fault. The CPU never writes a PTE, so every walk is read-only.
- **Physical address:** 34 bits (`PPN × 4096 + offset`), sent on `mem.v1` as a `u64`. An address outside every bus region is an ordinary bus `AccessFault` (§5.3).

A pure function `sv32_translate(satp, priv, sum, mxr, access, va, read_pte) -> Result<PhysAddr, Fault>` in `components/rv32i/src/sv32.rs` is the reference for the walk. The CPU drives the same step logic one PTE read at a time (§5.4), and property tests compare the two (§15.1).

### 5.3 Exceptions and Delivery

**Cause codes** in the `M3` profile:

| Code | Cause | New in M3 |
|---|---|---|
| 0–7 | as M1 (`InstructionAddressMisaligned` … `StoreAccessFault`) | |
| 8 | `EnvironmentCallFromU` | yes |
| 9 | `EnvironmentCallFromS` | yes |
| 11 | `EnvironmentCallFromM` (M1/M2 `EnvironmentCall`) | renamed in M3 traces only |
| 12 | `InstructionPageFault` (`tval` = VA) | yes |
| 13 | `LoadPageFault` (`tval` = VA) | yes |
| 15 | `StorePageFault` (`tval` = VA) | yes |

These are the architectural `mcause`/`scause` values. The snapshot uses its own numbering for causes (§5.5), which is not the same as these values.

**Which step raises which cause:**

- **Defined from M3.2:** every cause in this table. That means its name, its `medeleg` bit inside `0xB1FF`, and its schema 3 code.
- **Raised from M3.2:** causes 0–9 and 11. These are M1's causes, plus the `ecall` causes that now depend on the mode.
- **Raised from M3.3:** causes 12, 13, and 15, which come only from the Sv32 walk.

In M3.2, no instruction raises a page fault. Restore rejects their snapshot codes until M3.3 (§5.5).

**Order within one access**, keeping M1's order. This design's reading is that the privileged specification lets address-misaligned exceptions take either priority relative to page and access faults. M3.3 confirms that reading against the specification text and the pinned Spike before implementing it, and records the result in the Spike appendix (§15.3):

1. alignment;
2. translation (page fault);
3. a PTE read that faults on the bus raises the **access** fault of the original access type, with `tval` = VA;
4. the access itself (access fault on `Fault`).

A misaligned access never walks.

**Delivery.** A synchronous exception in mode `p` with cause `c`:

- **Delegated** (`p ≠ M` and `medeleg[c] = 1`), it is delivered to S:
  - `sepc ← pc`, `scause ← c`, `stval ← tval`;
  - `SPP ← p` (U=0, S=1), `SPIE ← SIE`, `SIE ← 0`;
  - `priv ← S`, `pc ← stvec.BASE`.

  Like MEI entry, it is not a retirement: the instruction does not retire, and `instret` is unchanged. The CPU goes to `FetchIssue` at `stvec` and emits `rv32.exception` (§5.6). Nothing is left to snapshot beyond the updated CSRs.
- **Not delegated**, or `p = M`: the M2 rule applies unchanged. The CPU records `Halted(Trap(RvTrap))`, writes no CSR, and emits `rv32.trap` (m2-design §5.6). This is the documented execution-environment boundary, and §7.4 uses it to end a run.

### 5.4 Walk Execution, States, and Timing

- **No TLB.** Every translated fetch and data access walks. Caching translations is microarchitecture and belongs to a later F3 backend (§16 risk 3). Without a cache, `SFENCE.VMA` has nothing to invalidate, and the architecture permits that.
- **New CPU states** (M3 profile only):
  - `WalkIssue { purpose, level, table }`
  - `WalkWait { txn, purpose, level, table }`

  `purpose` is `Fetch`, or `Data { insn }` for a load or store whose plan is already prepared. The raw instruction bits are stored, never the decoded plan (m1-design §5.6).
- **The translated address is CPU state.** The last PTE read determines the physical address, and the CPU does not read it again. In the `M3` profile, therefore:
  - `FetchIssue`, `FetchWait`, `MemIssue`, and `MemWait` carry `pa: Option<u64>`. `None` means untranslated (the access goes to the address itself); `Some` holds the result of a completed walk.
  - After a walk completes, the CPU is in `FetchIssue { pa: Some }` or `MemIssue { insn, pa: Some }`, and the access is issued at the next cycle's `Request`.
  - A page fault found by the walk goes straight to `CommitPending` with the pending trap, like any other trap.
- **Timing** follows the M0 phase rules like any memory access. A PTE `ReadReq` is sent in `Request` of the next CPU cycle, and its response arrives in `Complete`. The next level, or the translated access itself, is issued in `Request` of the following cycle. A 4 KiB translation therefore adds two PTE round trips per access, and a megapage adds one.
- **Satp sampling:** `satp`, `priv`, `SUM`, and `MXR` are read when the walk starts. They can change only in `Commit`, and a walk never spans a `Commit` of its own instruction, so they cannot change mid-walk.

### 5.5 Snapshot (Schema 3)

Schema 3 is written and restored only by the `M3` profile. It is schema 2 (m2-design §6.4), then the M3 block below. Schema 3 appears in no file and no digest that exists today, so schemas 1 and 2 stay byte-for-byte unchanged, as §4.2 and §14 require.

**M3 block**, after schema 2's `irq` input level:

| # | Field | Encoding | Restore check |
|---|---|---|---|
| 1 | `priv` | `u8`: 0 U, 1 S, 3 M | 0, 1, or 3 |
| 2 | `mstatus.SIE` | `u8` 0/1 | 0 or 1 |
| 3 | `mstatus.SPIE` | `u8` 0/1 | 0 or 1 |
| 4 | `mstatus.SPP` | `u8`: 0 U, 1 S | 0 or 1 |
| 5 | `mstatus.MPP` | `u8`: 0 U, 1 S, 3 M | 0, 1, or 3; never `0b10` (§5.1) |
| 6 | `mstatus.SUM` | `u8` 0/1 | 0 or 1 |
| 7 | `mstatus.MXR` | `u8` 0/1 | 0 or 1 |
| 8 | `medeleg` | `u32` | no bit outside `0xB1FF` |
| 9 | `stvec` | `u32` | bits [1:0] = 0 |
| 10 | `sscratch` | `u32` | — |
| 11 | `sepc` | `u32` | bits [1:0] = 0 |
| 12 | `scause` | `u32` | — |
| 13 | `stval` | `u32` | — |
| 14 | `satp` | `u32` | `ASID` = 0; `MODE` supported at this step (§5.1) |

- **Fields 2–7** are the architectural `mstatus` fields that schema 2 does not hold. `MIE` and `MPIE` are schema 2's first two fields and are not repeated. Every other `mstatus` bit reads 0 in the `M3` profile (§5.1), so it is not stored.
- **Not stored:** `sstatus` is a view of `mstatus` fields 2–7 and is derived from them. `mideleg`, `sie`, and `sip` are constant 0 in the `M3` profile. Restore derives or re-creates all four and never reads a value for them, so the snapshot has one copy of each fact.

**Cause codes and outcome tags.** Both extend only in schema 3. Schemas 1 and 2 keep their value sets and still reject everything added here.

- **Trap cause codes** 0–8 keep m1-design §6's order in every schema. Code 4 is `EnvironmentCall`, which M3 traces name `EnvironmentCallFromM` (§5.3). Schema 3 appends:
  - 9 `EnvironmentCallFromU`;
  - 10 `EnvironmentCallFromS`;
  - 11 `InstructionPageFault`;
  - 12 `LoadPageFault`;
  - 13 `StorePageFault`.

  Codes 11–13 are rejected on restore until M3.3, because no walk exists before it.
- **`CommitPending` outcome tags:**
  - 0 (retirement) and 1 (trap) exist in every schema;
  - 2 (CSR operation) and 3 (`MRET`) exist in schemas 2 and 3;
  - schema 3 appends 4 (`SRET`), with no payload. Like `MRET`, it reads `sepc`, `SPP`, and `SPIE` at `Commit`.

  A pending `SFENCE.VMA` is tag 0: a retirement with no register write. A delegated exception adds no tag. It is a pending trap (tag 1), and `Commit` decides from `priv` and `medeleg` whether it is delivered or halts.

**M3.3 additions.** Schema 3 gains the walk encodings at M3.3:

- `WalkIssue` and `WalkWait` state records: `purpose`, the raw instruction for `Data`, `level`, `table`, and `txn` for `WalkWait`;
- the `pa` option in `FetchIssue`, `FetchWait`, `MemIssue`, and `MemWait` (§5.4).

Schema 3's byte layout is final when M3.3 merges. Before that, no schema 3 snapshot is committed or published: the first is `m3-reference.mid.snap` at M3.7. The M3.3 additions therefore invalidate no stored file.

**Restore order (schema 3).** Restore runs in three steps, strictly in order:

1. **Decode all fields.** Read the whole snapshot to its end: configuration, `pc`, registers, counters, the state record, the schema 2 CSR block, and the M3 block. Only encoding is checked here: known tags, `u8` flags of 0 or 1, and the lengths of fields.
2. **Validate invariants.** Check every invariant on the decoded values together:
   - the schema 1 and 2 checks (configuration, `pc` alignment, `TxnId`, `instret` against the limit, `mtvec`/`mepc` alignment, the trap `pc`);
   - the M3 block checks in the table above;
   - a cause code allowed at this step;
   - the pending outcome, recomputed from the instruction word, the registers, `priv`, and the CSRs, must match the stored one. This covers the CSR access rule, the `ecall` cause, and whether `MRET`/`SRET` is legal;
   - from M3.3, the walk and `pa` checks below.
3. **Construct state.** Build the CPU state from the validated values, and replace the current state in one step. A restore that fails at any step changes nothing.

Partial decode followed by validation is forbidden. The state record comes before `priv` and the CSRs in the byte stream, but its checks depend on them, so no invariant may be checked while a field it could depend on is still unread. Schemas 1 and 2 keep their current restore unchanged (m1-design §5.6, m2-design §6.4): the inputs they accept and reject stay the same.

**Walk checks (M3.3).** Restore also rejects:

- a walk state that the recorded `satp`, `priv`, registers, and instruction cannot reach: a walk while translation is off, a `level` above 1, or a `table` that is not `satp.PPN` at level 1;
- a `pa` of `Some` while translation is off, a `pa` of `None` while it is on, and a `pa` whose low 12 bits differ from the virtual address's. Restore cannot check the rest of `pa` without reading RAM, and it never reads RAM.

### 5.6 Inspect and Trace

`inspect()` adds `priv` and every new CSR, and for a walk state `walk_purpose`, `walk_level`, and `walk_table`.

| Kind | When | Fields, in order |
|---|---|---|
| `rv32.commit` | as M1; in the M3 profile it adds `priv` U64 after `next_pc`, and for loads and stores `paddr` U64 after `addr` | |
| `rv32.exception` | a delegated exception is delivered, in `Commit` | `pc` U64 · `insn` U64 · `cause` Str · `tval` U64 · `from` Str · `to` Str |
| `rv32.interrupt` | as M2, plus `from` Str | |
| `rv32.trap` | a halt trap, as M1, with the M3 cause names | |

There are no walk records: the runtime's `runtime.dispatch` records already show every PTE read, as they show fetches (m1-design §5.7). In M1 and M2 traces, `EnvironmentCall` keeps its spelling.

---

## 6. `ModeledKernel`

### 6.1 Role and Boundary

`ModeledKernel` (crate `systemscope-os`, `components/os`, created at M3.4a) is the Modeled OS Backend. It is an **architectural state machine**. It holds OS state and changes it only in response to architectural events that reach it through memory. It never reads or writes CPU state directly.

- **What it can see:** the trap frame in RAM (written by the trampoline), page tables and user memory in RAM, the block controller's registers, and the disk contents that DMA delivers into RAM.
- **What it can change:** RAM (page tables, user frames, the trap frame), the block controller's registers, and the UART's output, all through its bus master port. It also controls when the held `ENTER` store completes.
- **What it cannot do:** touch registers, CSRs, `pc`, or `priv`. Every context change reaches the hart when the trampoline loads the trap frame and executes `SRET` (§7.3).

### 6.2 Ports and Configuration

| Port | Protocol | Role |
|---|---|---|
| `gate` | `mem.v1` target | The `kgate` window: `ENTER` at offset `0x0`, 4 bytes, write-only. Every other access gets `Fault { AccessFault }` |
| `mem` | `mem.v1` initiator | Bus master `kernel0`. At most one request outstanding; checked `TxnId` counter that never wraps |

The configuration is fixed at construction and is part of the snapshot (m0-design §6):

- the clock domain;
- the physical layout of §11.2: the trap frame, staging, frame pool, UART TX, and block controller base addresses;
- the user layout: `USER_BASE`, `STACK_TOP`, and `STACK_PAGES`;
- the limits: `MAX_PROCESSES` = 8, `MAX_PHDRS` = 16, and `WRITE_MAX` = 4096;
- the disk capacity.

The builder checks that the frame pool, the staging area, and the trap frame lie inside the RAM region and are disjoint, and that the firmware image lies outside the frame pool.

### 6.3 Kernel Execution Model

- **Entry.** The `ENTER` write is accepted when it is dispatched, as at every target. The kernel records the held request's `TxnId` and starts an **operation**. The recorded `TxnId` is the downstream one the bus assigned on its `kgate` port (m2-design §10.2), not the CPU's; the bus maps the response back to the CPU. An operation is one of: `Boot` on the first entry, `Trap` on every later one. The write's value must equal the configured trap frame address. Anything else is a guest bug: the kernel shuts down with the failure reason (§7.4).
- **Work.** An operation is a sequence of single bus accesses on `mem`, one outstanding, each sent in `Request` of a kernel clock cycle, like the DMA engine (m2-design §9.6). The accesses are:
  - reads and writes of up to 16 bytes that do not cross a 4 KiB page;
  - 1-byte UART writes;
  - word MMIO accesses to the block controller.

  Kernel computation between accesses takes no simulated time. Its cost is the bus traffic it generates (§10).
- **Exit.** When the operation finishes, the kernel sends `WriteResp(Done)` for the held `TxnId` in `Complete`. The CPU's store then commits, and the trampoline continues.
- **Kernel states.** Like the CPU (m1-design §5.3) and the DMA engine, every kernel state but `AwaitBoot` and `Idle` waits for exactly one runtime-owned event:
  - `Issue { op, step }` waits for the kernel's own wake, scheduled for the next `Request`;
  - `Wait { op, step, txn }` waits for the response to its one outstanding `mem` request.

  A wake or response that does not match the state faults the session. Neither the wake nor the request is ever a copy in the kernel's snapshot. **Restore resumes by waiting, never by reissuing.** A kernel write in flight at a checkpoint (a PTE, a trap-frame word, a UART byte, a `COMMAND`) therefore reaches its target exactly once, as a CPU store does (m1-design §5.6).
- **Access whitelist.** Before sending, the kernel checks every physical address against the ranges its configuration grants: the trap frame, staging, the frame pool, the block controller window, and UART TX. The firmware range and `kgate` are not granted. An address outside them is a kernel bug and faults the session before anything is sent, so the kernel can never deadlock on its own held gate (§16 risk 2).
  - User-supplied addresses never reach this check unvalidated. They are translated through the caller's page table first (§6.5), and a PTE can only point into the frame pool or the two megapages, because the kernel wrote every PTE.
  - A user buffer that resolves into a megapage is refused with `-EFAULT` by the `U = 1` rule before any physical access.
- **Pure core.** The kernel's decision logic is a pure step function, `kernel_step(state, completion) -> (state, next access | respond)`, over an abstract memory interface. The component only moves messages. The pure core is tested against an in-memory model with no runtime (§15.1), as `take_mei` was in M2.
- **Session faults** (`ComponentFault`) are the model bugs of m1-design §5.3 and m2-design §9.8:
  - a response with the wrong `TxnId`, or while nothing is outstanding;
  - a second `ENTER` while one is held (unreachable, since the CPU is stalled on the first);
  - a zero-length request;
  - a `Fault` on the kernel's own access to a region the configuration says exists.

  Guest-caused errors, such as a bad user pointer, a malformed ELF, or an invalid executable table, are never session faults. They are syscall errors, process kills, or a failure shutdown.

### 6.4 Processes, Address Spaces, and Frames

**Process control block**, one per PID (`1..=MAX_PROCESSES`):

| Field | Content |
|---|---|
| `pid` | `u32`, the executable table index + 1 |
| `state` | `Ready`, `Running`, `Exited { status: i32 }`, `Faulted { cause, epc, tval }` |
| `context` | `x1`–`x31`, `pc` (the saved `sepc`), `sstatus`. It is valid only when the process is not `Running`; while it runs, its context is in the hart and, on a trap, in the trap frame |
| `root` | PPN of its level-1 page table |
| `regions` | The mapped regions in ascending VA order: `(va, pages, perms, frames)`, the loaded segments and the stack. This is the kernel's bookkeeping for freeing frames. The page tables in RAM are authoritative for translation |

**Address space.** Every page table maps:

- **User segments and stack:** 4 KiB leaves with `U = 1`, `A = 1`, `D = W`, and the segment's `R`/`W`/`X` (§8.3). User VAs lie in `[USER_BASE, STACK_TOP)`. The stack is `STACK_PAGES` read-write pages ending at `STACK_TOP`, and the page above `STACK_TOP` stays unmapped.
- **The kernel megapage:** a 4 MiB identity leaf at the RAM base with `R W X`, `U = 0`, `G`, `A`, and `D`. It holds the firmware, the trampoline, the trap frame, and the staging area, so the trampoline runs unchanged under any `satp`.
- **The MMIO megapage:** a 4 MiB identity leaf at `0x1000_0000` with `R W`, `U = 0`, `G`, `A`, and `D`, covering the UART, IRQ controller, block controller, and `kgate` windows.

User code can reach neither megapage, because `U = 0` faults in U-mode.

**Frames.** The pool is a fixed physical range (§11.2) outside the kernel megapage. Allocation is lowest-free-first over a bitmap, so it is deterministic, and every allocated frame is zeroed with kernel writes before use. Page-table frames, segment frames, and stack frames all come from the pool. They are freed when the process exits or is killed. If the pool is exhausted during creation, the process is not created (`os.process.create` with an error), and boot continues with the next entry.

### 6.5 Syscall ABI and Dispatch

A trap whose `scause` is 8 (`ecall` from U) is a syscall.

- **Registers:** `a7` holds the number and `a0`–`a5` the arguments. The result goes in `a0`, and every other register is preserved.
- **Return:** the kernel sets `sepc ← sepc + 4` in the trap frame before returning.

| Nr | Name | Behavior | Result |
|---|---|---|---|
| 64 | `write(fd, buf, count)` | `fd` must be 1 or 2; otherwise `-EBADF` (9). `n = min(count, WRITE_MAX)`. Every page of `[buf, buf + n)` must be mapped `U = 1` with `R` in the caller's page table, or the result is `-EFAULT` (14), checked **before any byte is output**. Then the `n` bytes go to UART TX in order | `n` |
| 93 | `exit(status)` | The process becomes `Exited { status }`, its frames are freed, and the next process is dispatched (§6.6) | does not return |
| 94 | `exit_group(status)` | Same as `exit` (one thread per process) | does not return |
| 124 | `sched_yield()` | The caller goes to the tail of the run queue (§6.6) | 0 |
| 172 | `getpid()` | | `pid` |
| any other | | | `-ENOSYS` (38) |

The kernel reads user memory with its own Sv32 walk over the caller's page table in RAM. That walk is written independently of the CPU's `sv32.rs`, and tests compare the two (§15.1).

### 6.6 Scheduling and Process Transitions

- **Boot** creates one process per executable-table entry, in table order, and appends each to the run queue as `Ready`. It then dispatches the head.
- **Dispatch** of process `p`:
  - `state ← Running`;
  - the trap frame's registers, `sepc`, and `sstatus` come from `p.context`. For a new process that is all zeros except `sp = STACK_TOP` and `sepc = entry`, with `sstatus` having `SPP = U` and `SPIE = SUM = MXR = 0`;
  - the frame's `satp` is `Sv32 | p.root`;
  - the frame's action is `Resume`.
- **Switching away** from `p` (a yield) copies the trap frame into `p.context` and appends `p` to the run queue.
- **Exit or kill** is terminal: the process's frames are freed and it never runs again.
- **Next process:** after a yield, exit, or kill, the head of the run queue is dispatched. If the queue is empty, the kernel shuts down (§7.4) with reason 0 when every process exited with status 0, and with reason 1 otherwise.
- **Preemption:** none. A process that never traps runs until `max_instructions` (m1-design §5.5). That is a halt, not a kernel decision.

Every transition is traced (§6.8), so the full process history can be reconstructed from the trace.

### 6.7 Non-syscall Traps

The kernel is entered for every delegated exception (§7.1 sets `medeleg = 0xB1FF`).

- **From U-mode (`SPP = U`), any cause but 8:** the process becomes `Faulted { cause: scause, epc: sepc, tval: stval }` and is killed (§6.6). This covers the page faults 12, 13, and 15, illegal instructions, misalignment, access faults, and breakpoints. The kernel never resumes a faulting instruction: M3 has no demand paging.
- **From S-mode (`SPP = S`):** the kernel cannot be relied on to see it. `medeleg` delegates traps from S as well as from U, so a fault inside the trampoline re-enters the trampoline. By then `sscratch` may already hold the user's `t6` instead of the frame address, and the frame cannot be trusted.
  - The trampoline therefore **must never fault**. It runs from the identity-mapped kernel megapage, touches only the frame, `kgate`, and CSRs its mode may access, and is fixed, manifest-pinned code.
  - This is a verified property, not a runtime check. Every M3 test asserts that no `rv32.exception` has `from = S`.
  - If the kernel does see `SPP = S`, it shuts down with reason 1 and traces `os.shutdown` with the cause. That is a best-effort diagnostic, not a recovery path.

### 6.8 Snapshot, Inspect, Trace

**Snapshot (schema 1)** holds only kernel-owned state:

- the configuration;
- the kernel state (§6.3): `AwaitBoot`, `Idle`, `Issue`, or `Wait`, with the operation, its step and cursor, and the operation's working data;
- the held `ENTER` `TxnId`, if any;
- the outstanding `mem` `TxnId` and the next one;
- the PCBs in PID order, the run queue, and the running PID. The `Running` process's `context` is encoded as absent, because the hart and the trap frame own it; restore rejects a `Running` PCB that carries one;
- the frame bitmap.

**Working data is not a cache.** Some bytes the kernel read from RAM are part of its snapshot while an operation runs: trap-frame words, the executable table during `Boot`, ELF and program headers during a load. That is kernel-owned state, like the DMA controller's buffered beat (m2-design §9.9). The rules are:

- it exists only for the operation that read it, and is dropped when that operation ends;
- a later operation always reads RAM again;
- in particular the executable table is not kept after `Boot`.

Across operations the kernel keeps only what RAM does not hold: PCBs, the run queue, and the frame bitmap.

It never holds page tables, user memory, the trap frame, or disk contents: those belong to the RAM and the media, whose snapshots hold them. It never holds a message in flight either: those live in the runtime's queue.

**The held entry spans three snapshots.** Each component owns one part and restores it independently, as in M2:

- the CPU holds `MemWait { txn, insn: the ENTER store }`;
- the bus holds the active `kgate` transaction and its `(master, txn)` mapping;
- the kernel holds the downstream `TxnId` and its operation.

None of them copies another's part. A checkpoint taken between the `ENTER` request's dispatch and its acceptance is ordinary: the request is an event in the runtime's queue, and the kernel is still `Idle`.

**Restore rejects:**

- a different configuration;
- a held `ENTER` with an idle phase, or an operation without a held `ENTER`;
- a `Wait` whose `txn` is not the latest issued (next `TxnId` − 1), or an `Issue` or `Idle` state that still has an outstanding request;
- working data that does not belong to the recorded operation and step;
- a frame that is marked allocated but owned by no live process, or the reverse;
- two `Running` processes, or `Idle` after boot with no `Running` process (between operations the running process's context is in the hart, so `Idle` with exactly one `Running` process is the normal state);
- a run queue entry that is not `Ready`, or a `Ready` process missing from the queue.

**Inspect** shows the phase, the running PID, and each process's PID, state, and root PPN, plus the number of free frames. The CPU's `inspect` shows registers and `priv`. The address space is visible in RAM, and M4 renders it (plan.md §2 State Inspector).

**Trace kinds:**

| Kind | Fields |
|---|---|
| `os.boot` | `entries` U64 |
| `os.process.create` | `pid` U64 · `entry` U64 · `root` U64 · `error` Str (empty on success) |
| `os.load.segment` | `pid` · `va` · `memsz` · `perms` Str |
| `os.syscall.enter` | `pid` · `nr` · `a0` · `a1` · `a2` |
| `os.syscall.exit` | `pid` · `nr` · `ret` (U64, the `a0` bit pattern) |
| `os.process.switch` | `from` · `to` |
| `os.process.exit` | `pid` · `status` |
| `os.process.fault` | `pid` · `cause` Str · `epc` · `tval` |
| `os.shutdown` | `reason` U64 · `detail` Str |

These are plan.md §2's `PROCESS_CREATE`, `SYSCALL_ENTER`, and `PAGE_FAULT` events, in SystemScope's dotted spelling. The CPU's `rv32.exception` is the architectural page fault; `os.process.fault` is the kernel's reaction to it.

---

## 7. Guest Glue: Firmware Stub and Trampoline

### 7.1 Boot Stub (M-mode)

The firmware image `m3-firmware.elf` is placed in RAM by the existing host-side loader before the run (m1-design §8), which is the equivalent of a boot ROM. The CPU resets at its entry in M-mode. The stub:

1. sets `medeleg = 0xB1FF`;
2. sets `stvec` to the trampoline and `sscratch` to the trap frame address;
3. sets `mstatus.MPP = S` and `mepc` to `s_boot`;
4. executes `MRET`.

`s_boot` (S-mode, `satp` still bare) stores the trap frame address to `kgate.ENTER`. The kernel runs `Boot` (§8.2), fills the trap frame for the first process, and completes the store. `s_boot` then falls through into the trampoline's restore path (§7.3).

### 7.2 Trap Frame ABI

The trap frame is at a fixed physical address in the kernel megapage (§11.2). All fields are little-endian `u32`.

| Offset | Field | Written by |
|---|---|---|
| `0x00`–`0x78` | `x1`–`x31` | trampoline on entry; kernel for the context to resume |
| `0x7C` | `sepc` | both |
| `0x80` | `sstatus` | both |
| `0x84` | `scause` | trampoline |
| `0x88` | `stval` | trampoline |
| `0x8C` | `satp` | kernel |
| `0x90` | `action`: 0 `Resume`, 1 `Shutdown` | kernel |
| `0x94` | `reason` (for `Shutdown`) | kernel |

This layout is the whole contract between the modeled kernel and the guest glue, and it is versioned with them. The M6 native backend replaces both sides, so it needs no compatibility with it.

### 7.3 Trampoline (S-mode)

Entry, with `stvec` pointing here:

1. `csrrw t6, sscratch, t6`;
2. store `x1`–`x30` to the frame, then the user `t6` from `sscratch`;
3. store `sepc`, `sstatus`, `scause`, and `stval`;
4. store the frame address to `kgate.ENTER`. The store is held until the kernel finishes.

Restore, which `s_boot` shares:

5. if `action = Shutdown`, go to §7.4;
6. `csrw satp` from the frame, then `sfence.vma`;
7. `csrw sepc` and `csrw sstatus` from the frame; `csrw sscratch` with the frame address;
8. load `x1`–`x31`, with `t6` last;
9. `SRET`.

The trampoline is about 90 instructions of RV32I + Zicsr assembly. It is built with the pinned toolchain and pinned by a manifest, like `block_irq.elf` (m2-design §12). It executes the same way for every process and every trap. It holds no knowledge of processes or syscalls, which is what keeps the demo from being hard-coded.

### 7.4 Shutdown

For `action = Shutdown` the trampoline executes the SBI `SRST` call:

- `a7 = 0x5352_5354` (the `SRST` extension), `a6 = 0` (`system_reset`);
- `a0 = 0` (shutdown), `a1 = reason` (0 = no reason, 1 = system failure);
- then `ecall`.

`medeleg[9]` reads 0, so this `ecall` from S goes to M, where synchronous traps halt (§5.3). The run ends with `Halted(Trap { cause: EnvironmentCallFromS })`, and `a0`, `a1`, `a6`, and `a7` hold the request. This is M1's "ECALL ends the program" boundary, moved to where an S-mode kernel meets its execution environment. A native kernel at M6 reaches the same halt through the same SBI call.

---

## 8. Storage and Loading

### 8.1 Executable Table (Disk Layout)

LBA 0 holds the executable table, little-endian:

| Offset | Field |
|---|---|
| `0x00` | magic `0x3058_5353` (`"SSX0"`) |
| `0x04` | version, 0 |
| `0x08` | `count`, 1 to 8 |
| `0x0C` | reserved, 0 |
| `0x10 + 16·i` | entry `i`: `start_lba`, `byte_len`, `flags` (0), reserved (0) |

The table is valid when:

- every entry has `1 ≤ start_lba`, `byte_len ≥ 1`, and `ceil(byte_len / 512)` blocks that end within the disk capacity;
- entries do not overlap each other or LBA 0;
- `byte_len` ≤ the staging size;
- unused entry slots and every reserved byte are zero.

An invalid table shuts the kernel down with reason 1 and an `os.shutdown` detail that names the check. The table is the entire "storage software" of M3. It has no names, directories, or writes.

### 8.2 Reading an Executable

For entry `i`, the kernel runs the M2 controller exactly as a driver would (m2-design §9):

1. write `LBA`, `MEM_ADDR` (the staging base, 16-byte aligned), and `BLOCK_COUNT`, then `COMMAND = READ`;
2. read `STATUS` once per kernel cycle until `DONE`;
3. check `ERROR = 0`, then write `IRQ_STATUS`/`ACK`.

`IRQ_ENABLE` stays 0, so the controller never raises its line, and the CPU (with `MEIE = 0`) takes no interrupt. A controller error fails that entry's creation with an `os.process.create` error, and boot continues with the next entry. The DMA engine and the kernel are both bus masters on the RAM region, and round-robin arbitration orders them (m2-design §10.4).

### 8.3 User ELF Validation and Mapping

The kernel reads the ELF header and program-header table from staging, and validates them with a new pure function in `systemscope-elf`, `parse_user_elf32(headers, file_len, user_range)`. The M1 host-side rules (m1-design §8) are unchanged. Differences from them:

| Rule | M1 host loader | M3 user image |
|---|---|---|
| Class, data, version, machine, `ET_EXEC` | required | same |
| Address used | `p_vaddr = p_paddr` | `p_vaddr` (virtual); `p_paddr` ignored |
| Placement | inside the RAM region | inside `[USER_BASE, STACK_TOP − STACK_PAGES·4096)` |
| Permissions | ignored | `p_flags` R/W/X become PTE bits. `W` without `R`, or no permission at all, is rejected |
| Overlap | segments must not overlap | segments must not share a **page** (no permission merging) |
| Entry | aligned, inside a segment | aligned, inside a segment with `X` |
| Header count | any | `e_phnum ≤ MAX_PHDRS` |

**Mapping.** For each `PT_LOAD`, the kernel:

1. allocates and zeroes one frame per page;
2. copies `p_filesz` bytes from staging, at `p_offset`, to the frames at the right page offsets (the rest stays zero, which is `.bss`);
3. writes the PTEs, allocating a level-0 table on first use.

It then maps the stack and the two megapages. Frames are written before the PTEs that point at them. That is unobservable in M3 because the CPU is stalled on `ENTER`, but a native kernel must keep the same order. The next ELF file overwrites the staging area.

---

## 9. Snapshot

### 9.1 New State

| Component | Schema | Adds |
|---|---|---|
| `Rv32iCpu`, `M3` profile | 3 | `priv`, new CSRs, walk states (§5.5) |
| `ModeledKernel` | 1 | process table, run queue, frames, operation, held `ENTER` (§6.8) |
| `Ram` | 1 (unchanged) | nothing: page tables, trap frames, and user pages are ordinary RAM contents |
| `SimpleBlockMedia`, `DmaBlockController`, `MultiMasterBus`, `SimpleUart`, `SimpleIrqController` | unchanged | nothing |

The snapshot contract is unchanged:

- components store only their own state;
- restore takes no context and never sends;
- restore rejects unreachable state;
- encoding is canonical.

The process table and page tables add no new mechanism. Page tables are RAM pages, so the M1 rule "an omitted page means all zero now" (m1-design §7.2) covers them, including freed and re-zeroed frames.

### 9.2 Stress Points

Resume from **every** event of the reference workload (§12), and name these explicitly:

1. mid-walk: a level-1 or level-0 PTE read in flight, for a fetch and for a load and a store;
2. a delegated exception entry just taken (`FetchIssue` at `stvec`);
3. `ENTER` held with a kernel access in flight, at boot and during a syscall;
4. a DMA beat in flight during an executable load, and a kernel `STATUS` poll behind it;
5. a partial frame zeroing or segment copy;
6. a `write` with some bytes already at the UART;
7. between a context switch's trap-frame write and the trampoline's `SRET`;
8. after `SRET`, before the first U-mode fetch of a new process.

A portable snapshot, `tests/golden/m3-reference.mid.snap`, is taken at stress point 3 during a syscall. It must restore on Linux and Windows.

---

## 10. Fidelity

| Model | Level | Meaning in M3 |
|---|---|---|
| CPU | **F2**, unchanged | Architectural ISA and privileged state. Timing is the M0 phase rules plus bus round trips; there is no pipeline or cache |
| Memory translation | **Sv32 architectural** | The specification's walk, permissions, and faults, walking real PTEs in simulated RAM. No TLB, so translations are exact but not timed like hardware |
| OS | **Architectural state machine** | Processes, syscalls, and address spaces are OS semantics (plan.md principle 5: software semantics may be abstracted). The kernel's boundary is the architectural one: `ecall`, `scause`/`sepc`/`stval`, `satp`, PTEs, and the register ABI, all spec-accurate. Kernel time is only the bus traffic it generates |
| Storage | **M2 block model**, reused | `SimpleBlockMedia` and `DmaBlockController` unchanged |
| Interconnect, RAM, UART | unchanged from M2 | |

As in M1 and M2, cycle counts are deterministic and golden-pinned, but they are **not a performance model**. The kernel's zero-time computation is a modeling choice, not a claim about kernel cost.

---

## 11. Reference Platform `m3-reference`

### 11.1 Topology

```text
soc.cpu0    Rv32iCpu, profile M3 (clock "cpu", 100 MHz), entry = firmware entry
soc.bus     MultiMasterBus, masters [cpu, dma0, kernel0], clock cpu
  ├─ ram    ─▶ soc.ram     Ram                  base 0x8000_0000, size 16 MiB
  ├─ uart   ─▶ soc.uart    SimpleUart           base 0x1000_0000, size 0x8
  ├─ irqc   ─▶ soc.irqc    SimpleIrqController  base 0x1000_1000, size 0x8, 1 source
  ├─ blk    ─▶ soc.blk     DmaBlockController   base 0x1000_2000, size 0x20
  └─ kgate  ─▶ soc.kernel  ModeledKernel (gate) base 0x1000_3000, size 0x8
soc.disk    SimpleBlockMedia  capacity 256 blocks, latency Cycles { cpu, 16 }, image = the M3 disk fixture
soc.kernel  ModeledKernel, mem ─▶ bus kernel0
```

- **Components**, in declaration order: `soc.cpu0`, `soc.bus`, `soc.ram`, `soc.uart`, `soc.irqc`, `soc.blk`, `soc.disk`, `soc.kernel`.
- **Links:** the M2 links in M2 order, then bus `kgate` ↔ `soc.kernel` `gate`, then `soc.kernel` `mem` ↔ bus `kernel0`. Every link has latency `Cycles { cpu, 1 }`, and targets respond `Cycles { cpu, 0 }` after acceptance, except the held `ENTER` (§6.3).
- **DMA aperture:** the RAM, as in M2. It excludes `kgate`, so no DMA can reach the gate.
- **Seed** 0; **`max_instructions`** 10,000,000.

### 11.2 Physical Layout

| Range | Use |
|---|---|
| `0x8000_0000`–`0x8000_FFFF` | `m3-firmware.elf`: boot stub, `s_boot`, trampoline |
| `0x8001_0000`–`0x8001_0FFF` | trap frame (one hart) |
| `0x8010_0000`–`0x801F_FFFF` | staging (1 MiB), the DMA target for executables |
| `0x8000_0000`–`0x803F_FFFF` | = the kernel megapage (identity, `U = 0`) |
| `0x8040_0000`–`0x80FF_FFFF` | frame pool (3072 frames): page tables, user pages, stacks |

User layout: `USER_BASE = 0x0001_0000`, `STACK_TOP = 0x7FFF_F000`, `STACK_PAGES = 4`.

The M2 memory map is unchanged, and `kgate` sits above the block controller.

---

## 12. The M3 Reference Workload

### 12.1 The M3 Scenario

The **M3 scenario** is the disk image plus its expected results. It is independent of the backend: M4 visualizes it, and M6's native kernel must reproduce its UART output and exit statuses on the same disk image (plan.md §11). The scenario's user programs are ordinary RV32I ELF executables, linked at `USER_BASE`, that use only the syscall ABI of §6.5. They are built with the pinned toolchain and pinned by manifests, and nothing in them knows which backend runs them.

### 12.2 Programs

| Entry | Program | Exercises |
|---|---|---|
| 0 | `hello` | `write(1, "hello from pid N\n")` with `N` from `getpid`, then `exit(0)`. Text, rodata, data, and bss segments |
| 1 | `ping` | Three rounds of `write` then `sched_yield`, then `exit(0)` |
| 2 | `pong` | Interleaves with `ping` through `sched_yield`, then `exit(0)` |
| 3 | `fault` | `write`, then a store to its own read-only text → `StorePageFault`, `tval` = the VA; killed |
| 4 | `badptr` | `write(1, kernel_address, 4)` → `-EFAULT`, `write(7, …)` → `-EBADF`, syscall 999 → `-ENOSYS`, checks each result, then `exit(0)` |

### 12.3 Acceptance

A run passes only if:

- it halts with `EnvironmentCallFromS`, `a7 = SRST`, `a0 = 0`, and `a1 = 1`. The failure reason is expected, because `fault` was killed. A variant disk without `fault` must end with `a1 = 0`;
- the UART output equals the committed expected bytes exactly;
- the `os.*` trace records, with `pid`, `nr`, `ret`, `status`, `cause`, and `tval`, equal the committed expected sequence;
- `rv32.exception` shows exactly one `StorePageFault` (from U, at `fault`'s store) plus one `EnvironmentCallFromU` per syscall;
- after shutdown, every frame is free and every process is `Exited` or `Faulted`.

---

## 13. Determinism

- **Sources of order:** the M0 phase rules, `(tick, phase, sequence)` ordering, round-robin arbitration among three masters, and deterministic kernel choices (FIFO queue, lowest-free frame, fixed table order). There is no randomness. As in M1 and M2, the seed reaches only the session information.
- **One outstanding access per agent:** the CPU (fetch, data, or PTE), the DMA engine, and the kernel each have at most one request in flight, so no agent's behavior depends on the order of same-phase responses.
- **Held store:** the CPU is in `MemWait` for the whole kernel operation. No instruction retires, so no MEI is sampled, and nothing guest-visible can interleave with the kernel.
- **Observation invariance** O0–O5 apply unchanged. `inspect` of the kernel reads only component state.

---

## 14. Versioning and Contracts

- **No `contracts` change.** M3 uses `mem.v1` for walks, the gate, and kernel accesses, and `block.v0` and `irq.v0` unchanged. The `contracts` pin stays at `90900a128d51f6b42e86dc789447882c90c52ef4`. If implementation finds a need for a protocol change, this document is updated first and the change follows the M2.1 pattern (contracts, then a pin bump).
- **`COMPATIBILITY_ID` stays `"0.0.0"`.** Every existing snapshot and trace decodes and restores as before.
- **Component schemas:** CPU schema 3 (M3 profile) and `ModeledKernel` schema 1 are new. Every existing schema is unchanged.
- **Trace:** `rv32.exception` and the `os.*` kinds are new, and `rv32.commit` gains fields only in the M3 profile. Adding kinds and fields is not a format change (m2-design §14).
- **Crates and Cargo:** `components/os` (`systemscope-os`) is added at M3.4a. `systemscope-elf` gains `parse_user_elf32` at M3.1, and `systemscope-rv32i` gains `sv32.rs` at M3.3. M3.0 changes no Cargo file.

---

## 15. Validation

### 15.1 Matrix

| Area | Oracle | Tests | Step |
|---|---|---|---|
| Executable table, user ELF rules | this document's §8.1 and §8.3 tables | each rule and boundary, a test-only ELF writer (m1-design §8), property tests, arbitrary bytes | M3.1 |
| Privilege, CSRs, `MRET`/`SRET`, delegation | pinned Spike, directed (§15.3), plus this document for divergences | every mode transition, the CSR access rule, WARL masks, delegated vs halting traps | M3.2 |
| MEI with modes | a pure M3 oracle: M2's `take_mei` plus `priv` (the M2 oracle and its tests unchanged, §5.1) | property tests; entry from U and S | M3.2 |
| Sv32 | pinned Spike, directed; the pure `sv32_translate` | leaf and megapage, every permission and fault case, `SUM`/`MXR`, A/D, PTE access fault, PA beyond the bus; property tests with random page tables, CPU vs oracle | M3.3 |
| M3 CPU on RV32I | M1 oracles | 40 `rv32ui` and 39 ACT4 on the M3 profile (M-mode, bare) | M3.2 |
| Gate and held entry | a scripted gate operation with a known effect on the trap frame and UART | ecall round trips from U, register preservation, every-event resume including held `ENTER`, kernel access to `kgate` refused before sending, three-master bus contention | M3.4a |
| Kernel core | the pure kernel oracle over an in-memory RAM/disk model | boot, create, yield, exit, fault kill, frame accounting, every syscall and error | M3.4b, M3.5 |
| Kernel walk | the CPU's `sv32_translate` | random address spaces, the kernel's user-copy walk vs the oracle | M3.5 |
| End to end | §12.3 | the M3 scenario on `m3-reference` | M3.6 |
| M6 feasibility (optional, non-blocking) | §16 risk 4 | if executed: a small native S-mode kernel (assembly, §5 only) runs `hello` from the M3 disk image to its expected output on the unchanged `M3` profile | M3.6 or later |
| Snapshot/restore | resume equivalence | every event, the §9.2 stress points, the portable snapshot | M3.7 |
| Observation invariance | M0 O0–O5 | on `m3-reference` | M3.7 |
| Golden | `tests/golden/m3-reference.json` | Linux and Windows, cross-OS | M3.7 |
| M0/M1/M2 regressions | §15.5 | every step | all |

**The M6 feasibility check is a non-blocking architectural validation. It is not required to complete the M3 backend acceptance.** It checks one thing: whether the M3 CPU contract (§5) exposes the architectural surface a native backend needs. It is not an M3 acceptance gate. A failure is classified before anything changes:

- **The native kernel is incomplete or wrong:** this is M6 work. M3 is unaffected.
- **The `M3` profile lacks something the M3 scenario needs from a native kernel:** this is an M3 design issue. It is recorded in this document and resolved under §16 risk 4.

### 15.2 Syscall Trace Check

The expected `os.*` sequence of §12.3 is a committed file keyed by the disk image's `image_hash`. It is compared exactly, as a test independent of the golden digests. A digest change therefore always has a readable explanation: which syscall, process, or fault moved.

### 15.3 Spike-Directed Tests (M3.2, M3.3)

- Directed programs run on the pinned Spike (`19609434`) with supervisor and user modes enabled. The exact arguments were measured at M3.2 and are recorded in [m3-2-spike-appendix.md](m3-2-spike-appendix.md) (B.0), like M2.0's Appendix A. They include the device tree: `--disable-dtb` is not used.
- The comparison is per retirement and per delivered exception (`pc`, register writes, whitelisted CSR writes, `priv`) up to the first halting trap. It reuses the M1-A3 judges.
- These runs continue through handlers, so they must not rely on `--instructions` (m2-design §15.2).
- WARL choices were measured first (m3-2-spike-appendix B.2–B.5). §5.1 matches Spike where the appendix says it agrees, for example `MPP = 0b10` stored as U and Svade A/D. Every difference is listed in appendix B.6 (D1–D11), is kept out of the differential, and is covered by SystemScope-only unit tests.

### 15.4 ACT4

`include_priv_tests` stays `False`. The M3 privileged subset is still not full Sm/Ss (no counters, `misa`, `mhartid`, PMP, or M-mode exception delivery), so running a selection would suggest a conformance M3 does not have. The unprivileged corpus runs on all three profiles.

### 15.5 Regressions

Every M3 step must keep all of these passing, unchanged:

- the M0, M1, and M2 golden files byte-identical to `v0.1.0-m0`, `v0.2.0-m1`, and the M2 release. **Never re-blessed;**
- `m1-reference` on `AddressBus` and the `M1` profile; `m2-reference` on the `M2` profile, with unchanged event counts and digests;
- M1-A1 to M1-A8, the M2 acceptance (`block_irq.elf`, every-event snapshot, portable mid-DMA snapshot), AT-1 to AT-3;
- `FIXED_SEEDS_DIGEST` and every fixture manifest.

---

## 16. Architecture Risks

1. **The kernel boundary leaking into the CPU.** A modeled kernel invites shortcuts, such as a CPU hook on `ecall` or a direct register read. Any shortcut breaks plan.md's M6 promise.
   - Mitigation: the `rv32i` crate never depends on `os`, and the kernel's only inputs are memory accesses (§6.1).
   - M3.4a runs the M3 CPU profile against a scripted gate target before any real kernel exists. This shows that the CPU works against any backend, and it keeps doing so from then on.
2. **Held MMIO response.** The CPU stalls for a whole kernel operation, which can be thousands of cycles during boot, and the `kgate` bus region stays busy throughout.
   - The kernel must never address its own gate window: that would deadlock. The access whitelist (§6.3) excludes `kgate` and is checked before every send, and the builder checks that `kgate` lies outside every granted range.
   - No other agent can block on `kgate`. The DMA aperture excludes it (§11.1), user code cannot reach the MMIO megapage (`U = 0`), and the CPU is the only master whose requests reach the gate.
   - M3.4a proves the held entry works, including snapshots taken while an entry is held, before any OS logic depends on it.
   - Any future device that expects the CPU to make progress during a syscall would need a different entry mechanism. That is recorded, not solved.
3. **Walk traffic without a TLB.** Every U-mode fetch costs two extra round trips, roughly tripling events per instruction. That affects trace volume, run time, and snapshot queue size.
   - It is accepted at F2.
   - A TLB is an F3 decision. It must keep `SFENCE.VMA` semantics exact, and it changes digests, so it would be a new profile or backend, never an edit to the `M3` profile.
4. **Partial privileged architecture versus M6.** M-mode synchronous traps halt, and several CSRs are missing. plan.md §11 requires M6's C/assembly tiny kernel to run the M3 scenario "with no changes to CPU code". The `M3` profile therefore defines the architectural surface intended for the M6 native backend scenario. This does not mean that M6 implementation correctness is proven by M3. The optional feasibility check only validates that the declared surface is sufficient for a minimal native kernel.
   - A native kernel for the M3 scenario needs S-mode, `medeleg`, `stvec`/`sepc`/`scause`/`stval`/`sscratch`/`satp`, Sv32, `SRET`, and the SBI shutdown halt (§7.4). All of these are in §5, and none needs `misa`, `mhartid`, counters, or a timer.
   - A kernel that wants more, such as a Linux-class kernel, preemption, or SBI services beyond `SRST`, is outside the M3 scenario. It would be a new CPU profile and a new scenario, never a change to `M3`.
   - This can be checked as an optional validation, at M3.6 or later, to confirm M6 readiness. A small native S-mode kernel written in assembly uses only §5, implements `write`, `getpid`, and `exit`, and runs `hello` (entry 0) from the same disk image on the same CPU profile (§15.1).
   - It is separate from M3 acceptance: M3 completes without it (§18). **M6 feasibility validation does not require implementing the M6 backend. It only verifies that the M3 CPU contract exposes the required architectural surface.**
   - The wording of §2.2 and §15.4 must stay precise.
5. **Spike divergence on WARL and A/D.** Where Spike's choices differ from §5, directed tests could be tempted to match Spike silently. Measure first (§15.3) and record every divergence in this document.
6. **Snapshot growth.** Page-table and stack frames add RAM pages, and the kernel adds a 384-byte frame bitmap. The disk is 256 blocks, but only its non-zero blocks are stored. Sizes are measured at M3.7 and recorded, as in m2-design §13.3.
7. **Contention changes timing.** A third master changes grant sequences relative to M2. This cannot affect `m2-reference`, which has two masters, but `m3-reference` goldens depend on it. Any arbitration change is a platform change and needs a new reference.

---

## 17. Roadmap

| Step | Content | Merge unit | Exit criteria |
|---|---|---|---|
| **M3.0** | This design | docs only | Design reviewed; §19.1 decisions accepted; status set to "Design frozen" |
| **M3.1** | ELF user-image loader and executable table: `parse_user_elf32`, table validation, pure mapping plan (segments → pages + perms) | `elf` | §8.1 and §8.3 rules each tested at their boundaries; property and arbitrary-byte tests; M1 loader tests unchanged; no runtime change |
| **M3.2** | Privilege and trap boundary: `M3` profile, `priv`, the §5.1 CSRs and access rule, `MRET`/`SRET`, `medeleg` delivery, M3 cause names (page faults defined but not raised, §5.3), `SFENCE.VMA` as a privilege-checked no-op, MEI with modes, schema 3 (no walk yet; Bare is the only supported `satp` mode until M3.3, §5.1). Excluded: Sv32 translation, page walks, A/D handling (all M3.3), and any TLB (none in M3, §5.4) | CPU | Spike-directed privilege tests pass; the M3 MEI oracle, with M2's `take_mei` unchanged; 40 `rv32ui` + 39 ACT4 on M3; every-event snapshot of a mode-switching program; M1/M2 profiles and goldens unchanged |
| **M3.3** | Sv32 MMU: `sv32.rs`, `satp` Sv32, walk states, permissions, A/D, page faults, `SFENCE.VMA` checked with translation on (still a no-op, §5.4), walk snapshot | CPU | Spike-directed Sv32 tests; oracle property tests; resume from every event including mid-walk; regressions unchanged |
| **M3.4a** | Kernel gate prototype: `systemscope-os` skeleton, `gate` and `mem` ports, held `ENTER`, the `Issue`/`Wait` state engine with a scripted operation (read the trap frame, write it back with `sepc + 4`, write one UART byte), schema 1 for that state, the access whitelist; the firmware stub and trampoline fixture; a three-master bus configuration | os + tests | On a minimal platform, a bare-metal U-mode loop of `ecall`s round-trips through the trampoline and the scripted gate with registers preserved; resume from every event, including every held-`ENTER` state; a kernel access to `kgate` faults the session before sending; no `rv32.exception` from S; regressions unchanged |
| **M3.4b** | Process model (re-scoped, §17.1): PCBs and lifecycle states, the frame allocator, per-process Sv32 address spaces built from already-staged, validated user images (M3.1), the FIFO run queue, dispatch, an interim cooperative switch on `ecall` from U, the fault kill (§6.7), shutdown reasons, and the process-mode snapshot | os + tests | On a minimal platform, two processes created from staged images switch A→B→A through the trampoline with isolated address spaces; kernel oracle and property tests; frame accounting (all free after shutdown); OOM without leaks; every-event snapshot through creation, switching, and shutdown |
| **M3.5** | Syscall and output: the full §6.5 ABI, user-copy walk, `write` to UART, `sched_yield` context switch (replacing M3.4b's interim switch), `exit`, errors | os | Every syscall and error path by the kernel oracle and in the runtime; kernel walk vs `sv32_translate`; UART output exact |
| **M3.6** | `m3-reference` and the M3 reference workload: boot from the executable table through the block controller (moved from M3.4b, §17.1), the five programs, the disk fixture, manifests, expected output and syscall trace; optionally, the non-blocking native-kernel feasibility check (§15.1, §16 risk 4) | tests | §12.3 acceptance on Linux and Windows; every-event snapshot through boot, including DMA in flight |
| **M3.7** | Snapshot stress (§9.2), observation invariance, `m3-reference.json` and `m3-reference.mid.snap`, cross-OS, CI | tests + CI | Golden blessed once; every stress point; M0/M1/M2 goldens byte-identical |
| **M3.8** | Release documentation and exit audit | docs | §18 checked with evidence; tag and release left to the maintainer |

Each step follows the M1/M2 discipline: a feature branch, the full local gate, one push, a draft PR, CI, then a fast-forward merge.

### 17.1 M3.4b Clarifications

M3.4b delivers the process and address-space model without the storage path. These points refine §6 for it; later steps build on them.

- **Process source.** `ModeledKernel::with_processes` takes a `ProcessPlan`: the user layout and, per boot image, its staging address, its file length, and the `UserImage` that `parse_user_elf32` validated on the host (§8.1, §8.3). The kernel never parses an ELF. Image `i` becomes PID `i + 1`, as if it were executable-table entry `i`. Loading the images from disk (the executable table, the block controller, DMA) moves to M3.6 with the reference disk fixture. `ModeledKernel::new` stays the M3.4a prototype, and its snapshot bytes are unchanged.
- **Interim switch point (replaced in M3.5).** In M3.4b, an `ecall` from U (scause 8) was the cooperative switch: the frame became the caller's context with `sepc + 4`, the caller went to the queue's tail, and the head was dispatched. No register was decoded, and no guest-visible result was written. M3.5 replaced it with the §6.5 dispatch (§17.2): only `sched_yield` takes that transition now, with `a0 = 0`. With no `exit` in M3.4b, no process ended with status 0, so the empty-queue shutdown reason was 1.
- **Context ownership.** There is one trap frame (§7.2), not one per process. A PCB holds a context exactly when it is `Ready`. The dispatched context leaves the PCB and is written to the frame; while the process runs, the hart owns it, and on a trap the frame does. Terminal PCBs keep only their state and root, as history.
- **Frame reservation.** A creation reserves all the frames it needs at once, lowest free first, before its first write: the root, one level-0 table per 4 MiB slot, and one frame per segment and stack page. If too few are free, nothing is reserved, `os.process.create` reports `frame pool exhausted`, and boot continues with the next image. So a creation never has anything to undo. The reserved frames are zeroed, then filled, then mapped, level-0 leaves before the level-1 entries that point to their tables.
- **Megapage slots.** An image with a page in the 4 MiB slot of the kernel or the MMIO megapage cannot be mapped, and its creation fails with `a segment is in a megapage slot`.
- **Snapshot.** In process mode the snapshot adds the plan after the configuration, and after the prototype fields the lifecycle (`AwaitBoot`, `Up`, `Down`), the PCBs, the queue, the running PID, and the frame bitmap. A creation in flight is recorded by its PID and reserved frames, from which restore rebuilds and checks its accesses.
- **Initial `gp`.** §6.6 leaves every register but `sp` zero, `gp` included. The M3.1 `user.elf` fixture was linked with relaxation, so its `la` of a `.bss` symbol is `gp`-relative and faults under that context. M3.4b runs it as-is and checks the resulting fault kill. The M3.6 reference programs must not depend on `gp`: they should be linked without relaxation, or set `gp` themselves.

### 17.2 M3.5 Clarifications

M3.5 delivers the §6.5 syscall ABI on the M3.4b process model. These points refine §6.5, §6.6, and §6.8 for it; none adds a guest-visible behavior.

- **Dispatch.** A trap with `scause` 8 is decoded from the trap frame the kernel read: `a7` is the number and `a0`–`a2` the arguments (no supported syscall reads `a3`–`a5`). Numbers are exactly §6.5's; every other number, `0` and `u32::MAX` included, is `-ENOSYS` and never switches. The kernel never touches the CPU's register file and never passes a syscall to the host.
- **Return.** A returning syscall writes exactly two frame words, `a0` then `sepc + 4`, before the release. `sched_yield` returns through the context switch: the caller's saved context has `a0 = 0` and `pc = sepc + 4`, the rest of the frame as it trapped. A lone process yields to itself and is dispatched again at once. `exit` and `exit_group` write no result.
- **`write` order of checks.** The fd first (1 and 2 both go to UART TX): any other fd is `-EBADF` without looking at the buffer. Then `n = min(count, WRITE_MAX)`; `n = 0` returns 0 without looking at the buffer. A range that passes the top of the address space (`buf + n > 2^32`) is `-EFAULT` without a walk, which is what a walk would find, since the page at the top of the address space is never user-mapped. Otherwise every page of the range (at most two) is walked first; only then are bytes output.
- **User-copy walk.** The kernel's walk (`os::syscall::walk_step`) is the privileged specification's walk for a U-mode load with `SUM = MXR = 0`, one bus read of a PTE per level: `V` and not (`W` without `R`); a pointer at level 1 only, with `D`, `A`, and `U` clear; a leaf with `U`, `R`, and `A`; a megapage leaf with a zero PPN[0]. So a buffer in a kernel or MMIO megapage (`U = 0`) is `-EFAULT` (§6.3). Property tests compare it with the CPU's `sv32_translate` on random PTEs, including the PTE addresses read.
- **Output.** The bytes are read from the translated physical pages in chunks of at most 16 bytes that never cross a page, and each byte is written to UART TX by its own one-byte bus write, in order. A refused write outputs nothing.
- **Bus faults.** A `Fault` on any kernel access during a syscall (the frame read, a PTE read, a buffer read, a UART write, the `a0` or `sepc` write) is a session fault, as §6.3 says for all kernel accesses: never a guest-visible error, never a synthesized CPU page fault.
- **Trace timing.** `os.syscall.enter` is traced when the syscall is decoded; `os.syscall.exit` when its result is decided (a `write` after its last byte or its refusal), before the `a0` and `sepc` writes; `sched_yield` traces its exit (`ret 0`) before `os.process.switch`. `exit` and `exit_group` trace `os.process.exit` instead of `os.syscall.exit`. `os.process.exit`'s `status` is `I64`, the signed 32-bit status.
- **Snapshot.** A syscall in progress is a stage of the process-mode operation: 4 `Walk` (the buffer as `sepc`, `buf`, and `n`; the physical pages found; the table and level of the next PTE read), 5 `Output` (the buffer, the pages, and the bytes output so far), 6 `Return` (`sepc` and the result). The trap frame, the user bytes, and the UART's output are never copied into it: a chunk being output is the operation's working data, like the frame words. Restore rebuilds the running process's mapping from its image and frames and rejects a stage that no `write` of that process could be in: a bad buffer, pages or a table that are not its mapping, or a byte count that is not a chunk start. A restored kernel never reissues a request; a restored `Wait` continues from the response.
- **Test programs.** The runtime tests' user programs are hand-assembled and gp-independent, building every address with `lui`/`addi` (§17.1 initial `gp`). They are test-only; the M3.6 reference programs must meet the same rule.

---

## 18. M3 Exit Criteria

- [ ] The `M3` CPU profile implements §5, checked by the Spike-directed tests, the `sv32_translate` and `take_mei` oracles, and every-event resume; the `M1` and `M2` profiles are unchanged.
- [ ] `ModeledKernel` implements §6 and §8, checked by the pure kernel oracle and in the runtime.
- [ ] The M3 scenario passes on `m3-reference` (§12.3) on Linux and Windows. Its path runs Executable → Storage → RAM → Process → CPU → Memory → Syscall → Kernel → Output entirely through models, with page tables in simulated RAM.
- [ ] Snapshot/restore from every event, the §9.2 stress points, and the portable snapshot pass; observation invariance holds.
- [ ] `tests/golden/m3-reference.json` is blessed once, and every M0, M1, and M2 golden file is byte-identical to its release.
- [ ] No `contracts` change, or, if one proved necessary, it went through contracts first and a pin bump, and this document records it.
- [ ] Every fixture (firmware, user programs, disk image) is pinned by a manifest.
- [ ] Decisions and contract changes discovered during M3 are reflected back into this document.

The M6 feasibility check (§15.1) is not an exit criterion. **The M6 feasibility check, if executed, is a validation of the CPU contract surface and is not a requirement to implement the M6 backend.**

---

## 19. Decisions and Open Questions

### 19.1 Decisions Accepted at the M3.0 Freeze

These are the **(R)** rows of §3, accepted at the M3.0 freeze. Each entry records the chosen option and the alternatives considered. Reopening one is a change to this frozen design: it is made in this document first, before any implementation that depends on it.

1. **Kernel realization.** Chosen: a Rust `ModeledKernel` behind a guest trampoline and a held `ENTER` store. The alternatives:
   - a CPU-level trap protocol to the kernel: simpler, but it puts OS knowledge into the CPU and breaks the M6 no-CPU-change rule;
   - a kernel written as RV32 guest code: that is the M6 native backend, not a modeled one.
2. **Kernel privilege.** Chosen: an S-mode kernel boundary with an M-mode boot stub. The alternative is M + U only, with no S-mode. It is smaller, but a native kernel at M6 would then need S-mode added to the CPU after all.
3. **M-mode synchronous traps keep halting,** and the run ends with an SBI `SRST` `ecall` from S. The alternative is full M-mode delivery to `mtvec`, with an M-mode SBI firmware that then needs its own halt convention.
4. **A/D policy.** Chosen: Svade (page fault). The alternative is hardware A/D update: the CPU would write PTEs, so walks would need writes.
5. **No TLB at F2.**
6. **Disk layout:** the `SSX0` executable table. The alternative is a single executable at a fixed LBA, which is less general and makes multi-process runs impossible.
7. **DMA completion by polling `STATUS`** rather than interrupts. The alternative is to route the controller's line to the kernel, which needs a new link or an IRQ fan-out that M2 does not have.
8. **Syscall ABI:** the Linux RV32 asm-generic subset (write/exit/exit_group/sched_yield/getpid), so M6 user programs stay valid.
9. **No timer or preemption in M3.** m2-design §19 recorded "timer interrupts … needed for preemption in M3". M3 moves them to a later milestone and keeps to cooperative scheduling, superseding that note. Adding them later means a CLINT-like timer, `mtime`/`mtimecmp`, and S-level timer forwarding, in a new profile and scenario (§16 risk 4).

### 19.2 Later

- `misa`, `mhartid`, the counters, a timer, and M-mode exception delivery. The M3 scenario does not need them, so they belong to a later profile and scenario, not to M6's run of the M3 scenario (§16 risk 4).
- A TLB and `SFENCE.VMA` precision (F3).
- `brk`/`mmap`, `exec` from user space, `wait`, and a real filesystem.
- Snapshot compaction for large disks and RAM (m2-design §16 risk 3).
- Perfetto slices pairing `os.syscall.enter`/`exit` (exporter change).
