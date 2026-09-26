# M3.3 Spike Appendix: Sv32 Walk Measurements

This appendix records what the pinned Spike does for the Sv32 rules of m3-design §5.2 and the exception order of §5.3, measured before M3.3 implements them. It completes m3-2-spike-appendix B.5, whose last bullet lists the cases left to M3.3: a PTE read that faults on the bus, a level-0 walk, a pointer at level 0, an X-only page with `MXR`, S fetch from a `U` page with `SUM` = 1, and U-mode accesses through Sv32.

As in the M3.2 appendix, every value was read from a Spike commit log. Nothing is inferred from Spike's source.

## C.0 Setup

**Spike and command line:** unchanged from m3-2-spike-appendix B.0: commit `19609434bb3d83448eec8796e8f0367c868efbda`, `--isa=rv32i_zicsr --priv=msu --pcs=0:0x80000000 -m0x80000000:0x1000000 -l --log-commits --instructions=100000000`, with the device tree (no `--disable-dtb`). Measured on 2026-09-26.

**Program:** one RV32I + Zicsr assembly program, built with the pinned rv32 toolchain as in B.0, and not committed. M3.3 turns its cases into the directed programs of `tests/rv32/src/vmgen.rs`.

- In M it sets `mtvec` and `stvec`, `medeleg` = `0xB0FF` (every M3 exception but an `ECALL` from U is delegated), and writes the page tables with ordinary stores: a root table, one level-0 table `l0`, a data page `pg` whose first word is `0x1111_1111`, and a 4 KiB-aligned code page `ftarget` (`addi a3, x0, 0x77`; `jalr x0, 0(ra)`).
- It sets `satp` = Sv32 with `ASID` 0 and the root's PPN, and enters S with `MRET`. S code runs through an identity megapage (`V R W X A D`, `U` = 0), and U code through an alias megapage with `U` = 1.
- The S handler reads `scause`, `sepc`, `stval`, and `sstatus` into `t3`–`t6`. It returns to `ra` after a fetch fault (causes 1 and 12), where the `jalr` has retired, and to `sepc` + 4 otherwise.
- The U part ends with an `ECALL` to M, whose handler re-reads three PTEs and exits through HTIF.

The PTEs below give their flag bits; `→ pg` means a PPN of `pg`, and "no memory" means PA `0x4000_0000`, outside Spike's RAM.

## C.1 Mappings

| VA | Level-1 PTE | Level-0 PTE |
|---|---|---|
| `0x4000_0000` | pointer (`V`) to PA `0x4000_0000` (no memory) | the PTE read fails |
| `0x4040_0000` + `n`×`0x1000` | pointer (`V`) to `l0` | `l0[n]`, below |
| `0x4080_0000` | pointer to `l0` with `U`, `A`, `D` set | — |
| `0x40C0_0000` | `V W`, `R` = 0, PPN of `l0` | — |
| `0x4100_0000` | megapage `V X A D` (X-only), identity | — |
| `0x4140_0000` | megapage `V R W X A D`, `PPN[0]` = 1 | — |
| `0x4180_0000` | megapage `V R W A D`, identity | — |
| `0x41C0_0000` | 0 (`V` = 0) | — |
| `0x4240_0000` | megapage `V R W X`, `A` = 0 | — |
| `0x4280_0000` | megapage `V R W X A`, `D` = 0 | — |

| `n` | `l0[n]` |
|---|---|
| 0 | → `pg`, `V R W A D` |
| 1 | → `ftarget`, `V R X A D` |
| 2 | → `pg`, `V` only (a pointer at level 0) |
| 3 | 0 |
| 4 | → `pg`, `V W A D`, `R` = 0 |
| 5 | → `pg`, `V R W X`, `A` = 0 |
| 6 | → `pg`, `V R W X A`, `D` = 0 |
| 7 | → no memory, `V R W A D` |
| 8 | → `pg`, `V R W U A D` |
| 9 | → `ftarget`, `V R X U A D` |
| 10 | → `pg`, `V X A D` (X-only) |
| 11 | → `pg`, `V R W G A D`, RSW = `0b11` |
| 12 | → `ftarget`, `V R X`, `A` = 0 |
| 13 | → `ftarget`, `V R X A`, `D` = 0 |

## C.2 S-Mode Results

A fetch is a `jalr ra` to the VA. "Retires" means the access retired with the value shown; for a fetch, `ftarget`'s two instructions retired at the VA in S.

| # | Access | VA | Result (`scause`, `stval`) |
|---|---|---|---|
| Q1 | load | `0x4000_0000` | 5 (`LoadAccessFault`), VA |
| Q2 | store | `0x4000_0000` | 7 (`StoreAccessFault`), VA |
| Q3 | fetch | `0x4000_0000` | 1 (`InstructionAccessFault`), VA; `sepc` = VA |
| Q4 | load | `0x4000_0001` | 4 (`LoadAddressMisaligned`), VA |
| Q5 | fetch | `0x4000_0002` | 0 (`InstructionAddressMisaligned`) at the `jalr`, `stval` = target |
| Q6 | load | `l0[0]` | retires, `0x1111_1111` |
| Q7, Q8 | store, then load | `l0[0]` + 4 | retire; the load reads the stored `0x5a5a_5a5a` |
| Q9 | fetch | `l0[1]` | retires |
| Q10 | load | `l0[2]` | 13, VA |
| Q11 | load | `l0[3]` | 13, VA |
| Q12 | load | `l0[4]` | 13, VA |
| Q13 | load | `l0[5]` | 13, VA |
| Q14 | store | `l0[5]` | 15, VA |
| Q15 | load | `l0[6]` | retires |
| Q16 | store | `l0[6]` | 15, VA |
| Q17 | load | `l0[7]` | 5, VA |
| Q18 | store | `l0[7]` | 7, VA |
| Q19 | store | `l0[7]` + 2 | 6 (`StoreAddressMisaligned`), VA |
| Q20 | load | `l0[11]` | retires, `0x1111_1111` |
| Q21 | fetch | `l0[12]` | 12, VA |
| Q22 | fetch | `l0[13]` | retires |
| Q23 | load | `0x4080_0000` | 13, VA |
| Q24 | load | `0x40C0_0000` | 13, VA |
| Q25 | load, `MXR` = 0 | `l0[10]` | 13, VA |
| Q26 | load, `MXR` = 1 | `l0[10]` | retires, `0x1111_1111` |
| Q27 | store, `MXR` = 1 | `l0[10]` | 15, VA |
| Q28 | load, `MXR` = 0 | `0x4100_0000` | 13, VA |
| Q29 | load, `MXR` = 1 | `0x4100_0000` | retires |
| Q30 | fetch | `0x4100_0000` + `ftarget` offset | retires |
| Q31 | load, `SUM` = 0 | `l0[8]` | 13, VA |
| Q32 | store, `SUM` = 0 | `l0[8]` | 15, VA |
| Q33 | fetch, `SUM` = 0 | `l0[9]` | 12, VA |
| Q34 | load, `SUM` = 1 | `l0[8]` | retires |
| Q35 | store, `SUM` = 1 | `l0[8]` | retires |
| Q36 | fetch, `SUM` = 1 | `l0[9]` | 12, VA |
| Q37 | load, `SUM` = 1, `MXR` = 1 | `l0[10]` | retires |
| Q38 | load | `0x4140_0000` | 13, VA |
| Q39 | store | `0x4140_0000` | 15, VA |
| Q40 | fetch | `0x4140_0000` | 12, VA |
| Q41 | load | `0x41BF_FFFC` (last word of a megapage) | retires, PA `0x803F_FFFC` |
| Q42 | load | `0x41C0_0000` (the next megapage) | 13, VA |
| Q43 | load | `0x4180_0000` | retires, PA `0x8000_0000` |
| Q44 | fetch | `0x4240_0000` + offset | 12, VA |
| Q45 | fetch | `0x4280_0000` + offset | retires |
| Q46 | store | `0x4280_0000` + offset | 15, VA |
| Q47 | load | `0x41C0_0002` | 4, VA |
| Q48 | fetch | `0x41C0_0002` | 0 at the `jalr`, `stval` = target |
| Q49 | store | `l0[5]` + 1 | 6, VA |
| Q50 | `sfence.vma` | — | retires |

## C.3 U-Mode Results

U code runs through the alias megapage with `U` = 1, entered with `SRET` (`SPP` = U). Every fault reaches the S handler with `SPP` = U (`sstatus` = `0x20`).

| # | Access | VA | Result |
|---|---|---|---|
| U1 | load | `l0[8]` (`U` = 1) | retires |
| U2 | store | `l0[8]` | retires |
| U3 | load | `l0[0]` (`U` = 0) | 13, VA |
| U4 | store | `l0[0]` | 15, VA |
| U5 | fetch | `l0[9]` (`U` = 1, `X`) | retires in U |
| U6 | fetch | `l0[1]` (`U` = 0) | 12, VA |
| U7 | load | `0x8000_0000` (identity megapage, `U` = 0) | 13, VA |
| U8 | load | `0x4000_0000` | 5, VA |
| U9 | load | `l0[7]` (`U` = 0, no memory) | 13, VA |

The `ECALL` that follows goes to M (cause 8 is not delegated here). The M handler then read `l0[5]` = `0x2000_140F`, `l0[6]` = `0x2000_144F`, and the root entry of `0x4240_0000` = `0x2000_000F`: exactly the values M wrote.

## C.4 Readings

Each reading below is checked against the privileged specification, `riscv/riscv-isa-manual` `src/priv/supervisor.adoc` and `machine.adoc` at commit `95b6c3e21a56b0e941164ab9ded01b576131217c`, "Virtual Address Translation Process" and the synchronous exception priority table.

1. **PTE bus fault** (Q1–Q3, U8): the access fault of the original access type, with `tval` = VA, and `sepc` = VA for a fetch. This is §5.3 step 3 and the specification's step 2. A level-1 PTE read that fails cannot be measured without a root outside memory, which would also fault the S code's own fetches; SystemScope-only tests cover it.
2. **Level-0 walk** (Q6–Q9): the two-level walk succeeds for load, store, and fetch, and the PA is `l0[n].PPN` × 4096 + the page offset.
3. **Invalid PTEs** (Q10–Q12, Q24, Q42): `V` = 0, and `R` = 0 with `W` = 1, are page faults at either level; a pointer at level 0 is a page fault. This is §5.2.
4. **Reserved bits in a pointer** (Q23): a level-1 pointer with `U`, `A`, and `D` set raises a page fault. The specification's step 3 raises a page fault "if any bits or encodings that are reserved for future standard use are set within pte", and for non-leaf PTEs "the D, A, and U bits are reserved for future standard use". §5.2 adopts the specification's algorithm and lists its fixed choices; this rule is part of that algorithm, so M3.3 implements it: a pointer (`R` = `W` = `X` = 0) with any of `D`, `A`, `U` set is a page fault. `G` and RSW are not reserved in a leaf and are ignored (Q20); RSW is ignored in a pointer too.
5. **Permissions:** X-only pages fault on load with `MXR` = 0 and load with `MXR` = 1 (Q25–Q29), never allow a store (Q27), and fetch (Q30). An S fetch from a `U` page faults with `SUM` = 0 and with `SUM` = 1 (Q33, Q36); an S load or store of a `U` page needs `SUM` = 1 (Q31, Q32, Q34, Q35). `MXR` applies to a `U` = 0 page with `SUM` = 1 as usual (Q37). U-mode needs `U` = 1 for load, store, and fetch (U1–U7). This is §5.2.
6. **Superpages:** a level-1 leaf with `PPN[0]` ≠ 0 faults for load, store, and fetch (Q38–Q40). A valid megapage maps all 4 MiB (Q41, Q43), and the next megapage is a separate translation (Q42).
7. **Svade** (Q13–Q16, Q21, Q22, Q44–Q46): `A` = 0 is a page fault for every access, `D` = 0 only for a store, at both levels. Spike wrote no PTE: the three entries re-read in M are unchanged. This is §5.2.
8. **Translation fault versus the final access** (Q17, Q18, U9): a leaf that allows the access but points at no memory raises the access fault, with `tval` = VA; a leaf that denies the access raises the page fault even when it points at no memory (U9). Permission is checked before the physical access.
9. **Alignment first** (Q4, Q5, Q19, Q47–Q49): a misaligned load or store raises its misaligned exception before the walk, whatever the walk would raise: a PTE bus fault (Q4), a final access fault (Q19), an invalid PTE (Q47), or `A` = 0 (Q49). A misaligned jump target raises `InstructionAddressMisaligned` at the jump (Q5, Q48), as in M1. The specification says that "load/store/AMO address-misaligned exceptions may have either higher or lower priority than load/store/AMO page-fault and access-fault exceptions", and that instruction address-misaligned exceptions are raised by the control-flow instruction. §5.3's order (alignment, translation, PTE bus fault, final access) is therefore allowed by the specification and matches the pinned Spike.
10. **`SFENCE.VMA`** in S with Sv32 on retires (Q50), as §5.4 requires.

## C.5 Divergences

No measurement differs from m3-design §5.2–§5.4. The M3.2 rows change as follows:

- **D8** (`satp` with `MODE` = Sv32) no longer applies: from M3.3, Sv32 is a supported mode, stored as Spike stores it. The directed M3.3 programs write `satp` = Sv32.
- **D7** (`satp.ASID`) stays: SystemScope reads `ASID` as 0, so every directed program writes `ASID` = 0.
- The other rows of m3-2-spike-appendix B.6 are unchanged, and the M3.3 programs stay clear of them as the M3.2 programs do.

## C.6 Differential

`tests/rv32/src/vmgen.rs` turns C.1–C.3 into eleven directed programs, which `cargo xtask spike diff` runs after the M3.2 ones with the same command line and the same boundary (the first exception taken in M, m3-2-spike-appendix D11): `vm-4k`, `vm-megapage`, `vm-invalid`, `vm-perm-s`, `vm-svade`, `vm-faults`, `vm-user`, `vm-sfence`, and one undelegated page fault each for fetch, load, and store. All eleven match the pinned Spike event for event, delegated page and access faults included.

Two log facts the comparison relies on:

- A fetch that faults has no instruction line: Spike writes the trap line alone, with `epc` the fetched address. The parser reads it as a trap of instruction word 0, which is what SystemScope's `rv32.exception` and `rv32.trap` records carry for a fetch fault.
- Spike's `mem` address is the virtual address. It is compared with SystemScope's `addr`; `paddr` is SystemScope's own translation, checked for its width and page offset only.

Spike caches translations, and without `SFENCE.VMA` the ISA leaves open whether a changed PTE is seen. Every program writes its PTEs before translation is on, except `vm-sfence`, which fences before it uses the PTE it rewrites. SystemScope has no TLB, so a changed PTE is seen by the next access without a fence; its own tests cover that, and it is not a divergence.
