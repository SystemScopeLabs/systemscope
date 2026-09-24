// rvmodel_macros.h
// DUT-specific ACT4 macros for SystemScope's M1 CPU on the m1-reference platform
// (docs/m1-design.md §10.4). ACT4 uses these only in the final self-checking ELF; the
// Sail signature build replaces them with Sail's own (tests/env/sail_macros.h).
// SPDX-License-Identifier: Apache-2.0

#ifndef _RVMODEL_MACROS_H
#define _RVMODEL_MACROS_H

// m1-reference's SimpleUart: a store of one byte to the base address transmits it.
#define SYSTEMSCOPE_UART_BASE 0x10000000

// No tohost/fromhost: SystemScope ends a program on ECALL, not through HTIF.
#define RVMODEL_DATA_SECTION

##### STARTUP #####

#define RVMODEL_BOOT

// SystemScope implements no M-mode and no CSRs, so bypass the boot to M-mode, as the
// ACT4 framework documents for such a DUT.
#define RVMODEL_BOOT_TO_MMODE

##### TERMINATION #####

// SystemScope's program-end convention (the rv32ui one): gp = 1, a0 = 0 for a pass
// or a0 != 0 for a failure, then ECALL, which SystemScope ends the run on.
#define RVMODEL_HALT_PASS \
  li gp, 1               ;\
  li a0, 0               ;\
  ecall                  ;\
1:                       ;\
  j 1b                   ;\

#define RVMODEL_HALT_FAIL \
  li gp, 1               ;\
  li a0, 1               ;\
  ecall                  ;\
1:                       ;\
  j 1b                   ;\

##### IO #####

#define RVMODEL_IO_INIT(_R1, _R2, _R3)

// Writes the null-terminated string at _STR_PTR to the UART, one byte at a time.
#define RVMODEL_IO_WRITE_STR(_R1, _R2, _R3, _STR_PTR) \
  li _R2, SYSTEMSCOPE_UART_BASE ;\
1:                             ;\
  lbu _R1, 0(_STR_PTR)         ;\
  beqz _R1, 2f                 ;\
  sb _R1, 0(_R2)               ;\
  addi _STR_PTR, _STR_PTR, 1   ;\
  j 1b                         ;\
2:

##### INTERRUPTS #####

// SystemScope implements no interrupts. ACT4's check_defines.h requires these macros
// whatever the DUT supports; only the privileged interrupt tests expand them, and
// include_priv_tests: False deselects those. Each expands to an assembler error, so an
// ELF whose test did use one would fail to build instead of silently doing nothing.
#define RVMODEL_SET_MEXT_INT(_R1, _R2) .error "SystemScope implements no interrupts";
#define RVMODEL_CLR_MEXT_INT(_R1, _R2) .error "SystemScope implements no interrupts";
#define RVMODEL_SET_MSW_INT(_R1, _R2) .error "SystemScope implements no interrupts";
#define RVMODEL_CLR_MSW_INT(_R1, _R2) .error "SystemScope implements no interrupts";

// Required as numbers by check_defines.h; used only by the interrupt tests above.
#define RVMODEL_INTERRUPT_LATENCY 0
#define RVMODEL_TIMER_INT_SOON_DELAY 0

#endif // _RVMODEL_MACROS_H
