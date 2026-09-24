// SystemScope test environment for riscv-tests (docs/m1-design.md §10.2). See
// LICENSE.riscv-test-env: derived from env/p/riscv_test.h of riscv-test-env at
// 6de71edb142be36319e380ce782c3d1830c65d68, the env submodule of the pinned riscv-tests.
//
// Replaces the upstream `env/p/riscv_test.h` for the M1 RV32I machine, which has no
// CSRs, no privilege modes, and no trap handler. The upstream test bodies and
// `test_macros.h` are used unmodified; only the environment macros below differ.
//
// Differences from `env/p/riscv_test.h` at the pinned riscv-tests commit:
//
// - `RVTEST_CODE_BEGIN` keeps `INIT_XREG` (every register 0) and `li TESTNUM, 0`, then
//   runs the `init` macro and falls into the test. It drops everything that needs CSRs,
//   `mret`, or a trap handler: the trap vector, `RISCV_MULTICORE_DISABLE` (`mhartid`),
//   `INIT_RNMI`, `INIT_SATP`, `INIT_PMP`, `DELEGATE_NO_TRAPS`, the `mtvec`/`stvec`/
//   `medeleg`/`mstatus` writes, and the `mret` into the test.
// - It also drops `CHECK_XLEN`, which executes `RVTEST_PASS` when the XLEN check fails.
//   On SystemScope that branch could only turn a broken `slli` or `bltz` into a pass.
// - `RVTEST_PASS` and `RVTEST_FAIL` keep the upstream register convention (`gp` = 1 and
//   `a0` = 0 for a pass; `gp` = `a0` = `(TESTNUM << 1) | 1` for a failure; `a7` = 93),
//   then store `gp` to `tohost` as the upstream trap handler's `write_tohost` does, and
//   end with `ecall`. SystemScope halts on that `ecall` (Trap(EnvironmentCall)); the
//   `tohost` store is an ordinary RAM write there, kept so Spike can use the same ELFs.
// - `RVTEST_CODE_END` is an all-zero word, which the ISA reserves as illegal, instead of
//   `unimp`. Without the C extension the assembler encodes `unimp` as
//   `csrrw x0, cycle, x0`, a Zicsr instruction the image would otherwise contain. Neither
//   is ever executed: every test ends at the `ecall` before it.
// - There is no `#include "../encoding.h"`: nothing here uses CSR or cause numbers.
// - `RVTEST_DATA_BEGIN` and `RVTEST_DATA_END` are unchanged: the `.tohost` section with
//   `tohost` and `fromhost`, and the signature labels.

#ifndef _ENV_SYSTEMSCOPE_H
#define _ENV_SYSTEMSCOPE_H

//-----------------------------------------------------------------------
// Begin Macro
//-----------------------------------------------------------------------

#define RVTEST_RV32U                                                    \
  .macro init;                                                          \
  .endm

// The rv32ui wrappers redefine RVTEST_RV64U as RVTEST_RV32U.
#define RVTEST_RV64U RVTEST_RV32U

#define INIT_XREG                                                       \
  li x1, 0;                                                             \
  li x2, 0;                                                             \
  li x3, 0;                                                             \
  li x4, 0;                                                             \
  li x5, 0;                                                             \
  li x6, 0;                                                             \
  li x7, 0;                                                             \
  li x8, 0;                                                             \
  li x9, 0;                                                             \
  li x10, 0;                                                            \
  li x11, 0;                                                            \
  li x12, 0;                                                            \
  li x13, 0;                                                            \
  li x14, 0;                                                            \
  li x15, 0;                                                            \
  li x16, 0;                                                            \
  li x17, 0;                                                            \
  li x18, 0;                                                            \
  li x19, 0;                                                            \
  li x20, 0;                                                            \
  li x21, 0;                                                            \
  li x22, 0;                                                            \
  li x23, 0;                                                            \
  li x24, 0;                                                            \
  li x25, 0;                                                            \
  li x26, 0;                                                            \
  li x27, 0;                                                            \
  li x28, 0;                                                            \
  li x29, 0;                                                            \
  li x30, 0;                                                            \
  li x31, 0;

#define RVTEST_CODE_BEGIN                                               \
        .section .text.init;                                            \
        .align  6;                                                      \
        .globl _start;                                                  \
_start:                                                                 \
        INIT_XREG;                                                      \
        li TESTNUM, 0;                                                  \
        init;

//-----------------------------------------------------------------------
// End Macro
//-----------------------------------------------------------------------

#define RVTEST_CODE_END                                                 \
        .4byte 0

//-----------------------------------------------------------------------
// Pass/Fail Macro
//-----------------------------------------------------------------------

#define TESTNUM gp

#define RVTEST_PASS                                                     \
        fence;                                                          \
        li TESTNUM, 1;                                                  \
        li a7, 93;                                                      \
        li a0, 0;                                                       \
        sw TESTNUM, tohost, t5;                                         \
        sw zero, tohost + 4, t5;                                        \
        ecall

#define RVTEST_FAIL                                                     \
        fence;                                                          \
1:      beqz TESTNUM, 1b;                                               \
        sll TESTNUM, TESTNUM, 1;                                        \
        or TESTNUM, TESTNUM, 1;                                         \
        li a7, 93;                                                      \
        addi a0, TESTNUM, 0;                                            \
        sw TESTNUM, tohost, t5;                                         \
        sw zero, tohost + 4, t5;                                        \
        ecall

//-----------------------------------------------------------------------
// Data Section Macro
//-----------------------------------------------------------------------

#define EXTRA_DATA

#define RVTEST_DATA_BEGIN                                               \
        EXTRA_DATA                                                      \
        .pushsection .tohost,"aw",@progbits;                            \
        .align 6; .global tohost; tohost: .dword 0; .size tohost, 8;    \
        .align 6; .global fromhost; fromhost: .dword 0; .size fromhost, 8;\
        .popsection;                                                    \
        .align 4; .global begin_signature; begin_signature:

#define RVTEST_DATA_END .align 4; .global end_signature; end_signature:

#endif
