# fault: entry 3 of the M3 reference disk (docs/m3-design.md §12.2). Writes "fault\n",
# then stores to its own read-only text: a StorePageFault with stval = _start, which the
# kernel answers by killing the process (§6.7). The exit after the store never runs; its
# status 99 would show in the trace if the store did not fault.
#
# gp-independent (§17.1): linked without relaxation, every address is pc-relative.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93

    .section .text.init, "ax"
    .globl _start
_start:
    li a0, 1
    la a1, message
    li a2, 6
    li a7, SYS_WRITE
    ecall
    la t0, _start
    sw zero, 0(t0)
    li a0, 99
    li a7, SYS_EXIT
    ecall

    .section .rodata
message:
    .ascii "fault\n"
