# M3.2 Spike Appendix: Privilege, CSR, and Delegation Measurements

This appendix records what the pinned Spike does for the M3 privileged subset. It is what m3-design §15.3 asks for: the exact `--isa`/`--priv` arguments, and measured values for the rules of m3-design §5.1 and §5.3, taken before M3.2 implements them. The format follows m2-design Appendix A.

Every value below was read from a Spike commit log. Nothing is inferred from Spike's source. Where the source explains a value, it is named as an explanation only.

## B.0 Setup

**Spike:** the local M1-A3 build, whose `spike.stamp` records commit `19609434bb3d83448eec8796e8f0367c868efbda` (the `SPIKE_COMMIT` pin of `tests/rv32/build-spike.sh`), version `Spike RISC-V ISA Simulator 1.1.1-dev`. Measured on 2026-09-25.

**Command line:**

```text
spike --isa=rv32i_zicsr --priv=msu --pcs=0:0x80000000 -m0x80000000:0x1000000 \
      -l --log-commits --instructions=100000000 <program>.elf
```

- **`--priv=msu`** enables supervisor and user modes. `misa` reads `0x4014_0100`: MXL = 1, with `I`, `S`, and `U`.
- **No `--disable-dtb`.** m2-design Appendix A used `--disable-dtb`, but M3 must not. With `--disable-dtb`, `medeleg` written as `0xB1F7` read back as `0x01F7`: the page-fault bits were not writable. (In Spike's source, `sim.cc` skips the device-tree configuration, so no MMU is configured.) With the device tree, `medeleg` keeps `0xB1F7` and `satp` keeps `MODE = Sv32` (B.2).
- **The device tree needs `dtc`** on `PATH` when Spike starts, not only when Spike is built. It was the Ubuntu 24.04 package `device-tree-compiler` `1.7.0-2build1` (`dtc --version`: `DTC 1.7.0`), the version CI already installs to build Spike (`ci.yml`, `nightly.yml`). Until now nothing ran it, because the M1-A3 differential passes `--disable-dtb`.
- **`--pcs=0:0x80000000`** is honored with the device tree: the first commit is at `0x8000_0000`.
- **`mip.MTIP` is pending.** With the device tree, `mip` (`0x80`, `MTIP`) read 1 at every read, the first one a few instructions after reset. The M3 profile has no timer (m3-design §19.1 item 9), so directed tests must never set `mie.MTIE`.
- **Termination through HTIF.** Each program ends by storing 1 to a `tohost` symbol in its own 4 KiB-aligned section, and then spinning. Spike polls `tohost` every few thousand steps, so up to about 5000 retirements of the spin loop follow in the log. They carry no information.
- **`--instructions`.** With a budget of 100000, the transition program stopped after its 20th trap, at the `ebreak`, having retired far fewer instructions than the budget. (In Spike's source, each trap ends a step chunk of up to 5000, and the whole chunk is subtracted from the budget.) The budget was therefore raised to 10⁸, so every run ends at the HTIF store. This is m3-design §15.3's "must not rely on `--instructions`".

**Programs:** RV32I + Zicsr assembly, built with the pinned rv32 toolchain: `-march=rv32i_zicsr -mabi=ilp32 -static -nostdlib -nostartfiles -Wl,--build-id=none`, linked at `0x8000_0000`. Each program points `mtvec` and `stvec` at a handler. The handler reads `xcause`, `xepc`, `xtval`, and `xstatus` into `t3`–`t6`, advances `xepc` by 4, and returns with `MRET` or `SRET`. As in M2.0, the programs are not committed; M3.2 turns them into directed tests.

**Log conventions:** `p0`/`p1`/`p3` is the privilege of a retirement in the commit log (U/S/M). A trap is logged as `exception <name>, epc <pc>` followed by `tval <v>`, and the handler's first commit shows the mode it runs in.

## B.1 Reset Values (M-mode)

| CSR | Reset value |
|---|---|
| `mstatus`, `sstatus` | `0x0000_0000` (`MPP` = U) |
| `medeleg`, `mideleg` | 0 |
| `satp` | 0 |
| `misa` | `0x4014_0100` |
| `sepc`, `scause`, `stval`, `sscratch`, `sie`, `sip` | 0 |

The reset `mstatus` differs from the M2 profile's `0x1800`: `MPP` resets to U, not M.

## B.2 CSR Writes (M-mode)

| CSR | Written | Read back |
|---|---|---|
| `mstatus` | `0xFFFF_FFFF` | `0x807E_79AA` |
| `mstatus` | `0x0000_0000` | `0x0000_0000` |
| `mstatus` | `0x0000_1000` (`MPP = 0b10`) | `0x0000_0000` (`MPP` = U) |
| `mstatus` | `0x0000_0800` (`MPP` = S) | `0x0000_0800` |
| `mstatus` | `0x0000_1800` (`MPP` = M) | `0x0000_1800` |
| `mstatus` | `0x0006_0000` (`MPRV`, `SUM`) | `0x0006_0000` |
| `sstatus` | `0xFFFF_FFFF` | `sstatus` and `mstatus` both `0x800C_6122` |
| `sstatus` | `0x0000_0000` | `0x0000_0000` |
| `medeleg` | `0xFFFF_FFFF` | `0x0000_B3FF` |
| `medeleg` | `0x0000_B1F7`, `0x0000_B1FF` | as written |
| `mideleg` | `0xFFFF_FFFF` | `0x0000_0222` |
| `sie` | `0xFFFF_FFFF`, with `mideleg` = 0 | `0x0000_0000`; `mie` stays 0 |
| `sip` | `0xFFFF_FFFF`, with `mideleg` = 0 | `0x0000_0000`; `mip` reads `0x0000_0080` |
| `stvec` | `0x8000_1000` / `…1001` / `…1002` / `…1003` / `…1004` | `0x8000_1000` / `…1001` / `…1000` / `…1001` / `…1004` |
| `stvec` | `0xFFFF_FFFF` | `0xFFFF_FFFD` |
| `sepc` | `0xFFFF_FFFF` | `0xFFFF_FFFC` |
| `sepc` | `0x8000_0002` | `0x8000_0000` |
| `scause`, `stval` | `0xFFFF_FFFF` | `0xFFFF_FFFF` |
| `sscratch` | `0x1234_5678` | `0x1234_5678` |
| `satp` | `0xFFFF_FFFF` | `0xFFFF_FFFF` |
| `satp` | `0x7FFF_FFFF` (Bare, `ASID` and `PPN` non-zero) | `0x7FFF_FFFF` |
| `satp` | `0x803F_FFFF` (Sv32, `ASID` 0) | `0x803F_FFFF` |
| `satp` | `0x0000_0000` | `0x0000_0000` |
| `misa` | `0x0000_0000` | `0x4014_0100` (write ignored, no trap) |

Decoded:

- **`mstatus` all ones → `0x807E_79AA`:** `SIE`, `MIE`, `SPIE`, `MPIE`, `SPP`, `MPP` = M, `FS` = 3, `MPRV`, `SUM`, `MXR`, `TVM`, `TW`, `TSR`, and `SD`.
- **`sstatus` all ones → `0x800C_6122`:** `SIE`, `SPIE`, `SPP`, `FS` = 3, `SUM`, `MXR`, and `SD`. Spike's `sstatus` view includes `FS` and `SD` even with no F extension in `--isa`.
- **`medeleg` all ones → `0xB3FF`:** causes 0–9, 12, 13, and 15. Bit 9 (`ecall` from S) is writable; bit 11 (`ecall` from M) is not.
- **`mideleg` all ones → `0x222`:** `SSIP`, `STIP`, and `SEIP` are writable.

**Readable in M-mode without a trap:** `mhartid` = 0, `mcounteren` = 0, `scounteren` = 0, `menvcfg` = 0, `senvcfg` = 0, and `misa`.

## B.3 Privilege Transitions and Delegation

Unless a row says otherwise, `medeleg` = `0xB1F7`: the design's `0xB1FF` without bit 3, so that a breakpoint shows a non-delegated trap from U. `mstatus` is shown before the trap and in the handler.

| # | Mode | Instruction | Result | Handler sees |
|---|---|---|---|---|
| M1 | M | `csrr a0, 0x7C0` (custom CSR), `medeleg[2]` = 1 | `IllegalInstruction`, taken in M (no delegation from M) | `mcause` 2, `mtval` = the instruction `0x7C00_2573`, `mstatus` `0x1800` |
| M2 | M | `ecall` | cause 11, taken in M | `mtval` 0 |
| M3 | M | `MRET` with `MPP` = S, `MPIE` = 1 | enters S at `mepc`; `mstatus` `0x0880` → `0x0088` | — |
| S1 | S | `csrr sstatus` | retires | — |
| S2 | S | `csrr mstatus` | `IllegalInstruction`, delegated to S | `scause` 2, `stval` `0x3000_25F3`, `sstatus` `0x100` (`SPP` = S, `SPIE` = old `SIE` = 0) |
| S3 | S | `csrs sstatus, SIE`, then the illegal `csrr mstatus` | delegated to S | `sstatus` `0x120` (`SPP` = S, `SPIE` = 1, `SIE` = 0) |
| S4 | S | `ecall` (`medeleg[9]` = 0) | cause 9, taken in M | `mstatus` `0x08A2` (`MPP` = S, `MPIE` = old `MIE` = 1, `MIE` = 0) |
| S5 | S | `MRET` | `IllegalInstruction`, delegated | `stval` `0x3020_0073` |
| S6 | S | `sfence.vma` | retires | — |
| S7 | S | `wfi`, `TW` = 0, no interrupt enabled | **retires, then Spike waits for an interrupt: no further commits** | — |
| S8 | S | `wfi`, `TW` = 1 | `IllegalInstruction`, delegated | `stval` `0x1050_0073` |
| S9 | S | `csrr satp`, `TVM` = 0 | retires, 0 | — |
| S10 | S | `csrw satp, 0x0001_2345` | retires; reads back `0x0001_2345` | — |
| S11 | S | `csrr medeleg` | `IllegalInstruction`, delegated | `stval` `0x3020_2573` |
| S12 | S | `SRET` with `SPP` = U, `SPIE` = 1 | enters U at `sepc`; `mstatus` `0xAA` → `0xAA` | — |
| U1 | U | `csrr sstatus` | `IllegalInstruction`, delegated | `stval` `0x1000_2573`, `sstatus` `0x20` (`SPP` = U, `SPIE` = 1) |
| U2 | U | `csrr cycle` (`mcounteren` = 0) | `IllegalInstruction`, delegated | `stval` `0xC000_2573` |
| U3 | U | `ecall` | cause 8, delegated to S | `stval` 0 |
| U4 | U | `SRET` | `IllegalInstruction`, delegated | `stval` `0x1020_0073` |
| U5 | U | `MRET` | `IllegalInstruction`, delegated | `stval` `0x3020_0073` |
| U6 | U | `sfence.vma` | `IllegalInstruction`, delegated | `stval` `0x1200_0073` |
| U7 | U | `wfi` | `IllegalInstruction`, delegated | `stval` `0x1050_0073` |
| U8 | U | `lw` from `0x8000_0001` | `LoadAddressMisaligned` (4), delegated | `stval` `0x8000_0001` |
| U9 | U | `lw` from 0 (no memory) | `LoadAccessFault` (5), delegated | `stval` 0 |
| U10 | U | `sw` to 0 | `StoreAccessFault` (7), delegated | `stval` 0 |
| U11 | U | `lw` from 1 (misaligned, no memory) | `LoadAddressMisaligned` (4): alignment first | `stval` 1 |
| U12 | U | `sw` to 2 (misaligned, no memory) | `StoreAddressMisaligned` (6) | `stval` 2 |
| U13 | U | `ebreak`, `medeleg[3]` = 0 | cause 3, taken in M | `mtval` = its `pc`; `mstatus` `0xAA` → `0xA2` (`MPP` = U, `MPIE` = 1, `MIE` = 0) |
| U14 | U | `jalr` to `0x8000_0002` | `InstructionAddressMisaligned` (0), delegated | `sepc` = the `jalr`, `stval` = the target |
| W1 | M | `wfi` with `mie.MTIE` = 1, `MIE` = 0, `MTIP` pending | retires and continues (no trap) | — |

A delegated trap leaves `MIE`, `MPIE`, and `MPP` unchanged. A trap taken in M leaves `SIE`, `SPIE`, and `SPP` unchanged.

## B.4 `MRET` and `SRET`

| # | Mode | Before (`mstatus`) | Instruction | After (`mstatus`) | New mode |
|---|---|---|---|---|---|
| R1 | M | `0x1880` (`MPP` = M, `MPIE` 1, `MIE` 0) | `MRET` | `0x0088` | M |
| R2 | M | `0x1808` (`MPP` = M, `MPIE` 0, `MIE` 1) | `MRET` | `0x0080` | M |
| R3 | M | `0x1920` (`SPP` = S, `SPIE` 1, `SIE` 0; `MPP` = M) | `SRET` | `0x1822` | S |
| R4 | S | `0x1902` (`SPP` = S, `SPIE` 0, `SIE` 1) | `SRET` | `0x1820` | S |
| R5 | S | `0x1820` | `ecall` → M handler | handler `mstatus` `0x0820` (`MPP` = S, `MPIE` 0); after its `MRET`: `0x00A0` | S |
| R6 | S | `0x01A0` (`SPP` = S, `SPIE` 1) | `SRET` | `0x00A2` | S |
| R7 | S | `0x00A2` (`SPP` = U, `SPIE` 1, `SIE` 1) | `SRET`, then `ecall` | `0x00A2`; the `ecall` is cause 8 to S, with `sstatus` `0x20` | U |
| P1 | M | `MPP` written as `0b10`: reads `0x0000` | `MRET` | `0x0080` | **U** |

- **`MRET`:** `MIE` ← `MPIE`, `MPIE` ← 1, `MPP` ← U, and the new mode is the old `MPP`.
- **`SRET`:** `SIE` ← `SPIE`, `SPIE` ← 1, `SPP` ← U, and the new mode is the old `SPP`. It is legal from M (R3) and leaves `MPP` alone.

## B.5 Page-Fault Preparation (Sv32, measured for M3.3)

`medeleg` = `0xB1FF`. `satp` = Sv32 with a root table in RAM whose level-1 entries were written in M-mode. The accesses run in S-mode. `sepc` is the faulting instruction, except for fetch faults, where the `jalr` retires and `sepc` = `stval` = the target.

| # | Access | Leaf (level 1) | Result (`scause`, `stval`) |
|---|---|---|---|
| P1 | load | `V` = 0 | 13, VA |
| P2 | store | `V` = 0 | 15, VA |
| P3 | fetch | `V` = 0 | 12, VA |
| P4 | load `VA + 1` | `V` = 0 | 4 (`LoadAddressMisaligned`), VA + 1 |
| P5 | store `VA + 2` | `V` = 0 | 6 (`StoreAddressMisaligned`), VA + 2 |
| P6 | load | `A` = 0, `D` = 0 | 13 |
| P7 | store | `A` = 0, `D` = 0 | 15 |
| P8 | load | `A` = 1, `D` = 0 | retires |
| P9 | store | `A` = 1, `D` = 0 | 15 |
| P10 | S load, `SUM` = 0 | `U` = 1 | 13 |
| P11 | S load, `SUM` = 1 | `U` = 1 | retires |
| P12 | S fetch, `SUM` = 0 | `U` = 1 | 12 |
| P13 | load | megapage with `PPN[0]` = 1 | 13 |
| P14 | load | leaf → PA 0 (no memory) | 5 (`LoadAccessFault`), **VA** |
| P15 | store | leaf → PA 0 | 7 (`StoreAccessFault`), VA |
| P16 | load `VA + 1` | leaf → PA 0 | 4, VA + 1 |
| P17 | load | `R` only | retires |
| P18 | store | `R` only | 15 |
| P19 | fetch | `R` only (no `X`) | 12 |
| P20 | load, `MXR` = 1 | `R` only | retires (this page is readable anyway, so it does not test `MXR`) |
| P21 | load | `W` = 1, `R` = 0 | 13 |
| P22 | `sfence.vma` | — | retires |

- **Svade:** no retirement in the log writes a PTE except the eight setup stores from M-mode. Spike never set `A` or `D`; a missing `A`, or a missing `D` on a store, is a page fault.
- **Priority:** alignment comes before translation (P4, P5) and before the access fault (P16, U11, U12). This matches m3-design §5.3's order. M3.3 still confirms the order against the specification text, as §5.3 requires.
- **Not measured here:** a PTE read that faults on the bus, a level-0 walk, a pointer at level 0, an X-only page with `MXR`, S fetch from a `U` page with `SUM` = 1, and U-mode accesses through Sv32. They are M3.3's to measure; [m3-3-spike-appendix.md](m3-3-spike-appendix.md) records them.

## B.6 Divergences from m3-design §5.1

Each row is a place where the pinned Spike and the frozen design disagree. For each one, M3.2 keeps the design's rule. The Spike-directed differential compares only programs that never reach the difference. A SystemScope-only unit test fixes the design's value, as M2 did for its unsupported CSRs (m2-design §4.5).

| # | Area | Spike (measured) | m3-design §5.1 | Directed differential |
|---|---|---|---|---|
| D1 | `medeleg` bit 9 | writable (`0xB3FF`) | reads 0 (mask `0xB1FF`) | writes only masks inside `0xB1FF` |
| D2 | `mideleg` | `0x222` writable | read-only 0 | never writes `mideleg` |
| D3 | `sie`, `sip` | writable through `mideleg` | read-only 0 | reads them only with `mideleg` = 0 (both read 0) |
| D4 | `mstatus` `MPRV`, `TVM`, `TW`, `TSR`, `FS`, `SD` | writable (`FS` sets `SD`) | read 0 | writes only the §5.1 bits |
| D5 | `sstatus` `FS`, `SD` | writable | read 0 | writes only `SIE`, `SPIE`, `SPP`, `SUM`, `MXR` |
| D6 | `stvec` MODE | vectored mode kept (`…01`); `0b10` → `0b00`, `0b11` → `0b01` | direct only (`value & !0b11`) | writes only 4-byte-aligned values |
| D7 | `satp` `ASID` | kept as written | reads 0 | writes `ASID` = 0 only |
| D8 | `satp` write with `MODE` = Sv32, in M3.2 only | Sv32 accepted and stored | Sv32 is an unsupported mode until M3.3: `satp` keeps its previous value (§5.1 `satp` rule) | writes `MODE` = 0 only in M3.2 |
| D9 | `WFI` in M, and in S with `TW` = 0 | legal: retires, then waits for an interrupt | illegal in every mode, `tval` = the instruction (§5.1 `WFI` rule) | **excluded** in M and S; compared only in U, where both raise `IllegalInstruction`. SystemScope-only unit tests cover M and S |
| D10 | `misa`, `mhartid`, `mcounteren`, `scounteren`, `menvcfg`, `senvcfg` | readable in M | unsupported: `IllegalInstruction` | never accesses them |
| D11 | Traps taken in M | delivered to `mtvec` | halt, and no CSR is written (m3-design §5.3) | compares up to the first non-delegated trap; the handler rows above are Spike-only |

**Agreements** recorded for the directed tests:

- the access rule on CSR bits [9:8] (S2, S11, U1, U2) and read-only CSRs;
- `MPP = 0b10` is stored as U (P1), which §5.1 adopts;
- reset `mstatus` = 0 with `MPP` = U (B.1), which §5.1 adopts for the `M3` profile;
- `MRET` and `SRET` effects (B.4);
- `SRET` legal in M and S, illegal in U;
- `MRET` illegal in S and U;
- `SFENCE.VMA` legal in S, illegal in U;
- delegated entry: `SPP`, `SPIE` ← `SIE`, `SIE` ← 0, `sepc`, `scause`, `stval`, `pc` = `stvec`;
- no delegation of a trap taken in M (M1);
- `ecall` causes 8, 9, and 11;
- `sepc` alignment (`value & !0b11`);
- `scause` and `stval` fully writable, as `mcause`/`mtval`;
- alignment before access faults.

## B.7 Resolutions in m3-design

These questions were open after the measurements. m3-design now records each answer, and none of them reopens a frozen decision (§19.1).

1. **`MPP = 0b10`:** stored as U, as Spike does (B.4 P1). `MPP` never holds `0b10`, and restore rejects it (m3-design §5.1, §5.5).
2. **Reset `mstatus` in the `M3` profile:** `0x0000_0000`, with `MPP` = U, as Spike (B.1). It applies only to the `M3` profile and schema 3. The `M2` profile keeps `0x0000_1800` (m3-design §5.1).
3. **`satp`:** each `MODE` value is a supported or an unsupported mode at each step. Bare is the only supported mode until M3.3. A write with an unsupported `MODE` leaves every field at its previous value, and the instruction retires normally. The new value depends only on the old value and the written one (m3-design §5.1). This is D8.
4. **`WFI`:** illegal in every mode. The differential excludes it in M and S (D9), and SystemScope-only unit tests cover those modes (m3-design §5.1).
5. **Schema 3:** the field layout, the derived and constant CSRs that are not stored, the cause codes and outcome tag added only in schema 3, and the decode → validate → construct restore order (m3-design §5.5).
6. **An M3 Spike job** must not pass `--disable-dtb`, and needs `dtc` on `PATH` at run time (B.0). CI already installs the pinned `dtc`.
