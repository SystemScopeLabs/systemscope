# pong: entry 2 of the M3 reference disk (docs/m3-design.md §12.2). Three rounds of
# write(1, "pong\n") and sched_yield, interleaving with ping, then exit(0).
#
# gp-independent (§17.1): linked without relaxation, every address is pc-relative.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93
    .equ SYS_SCHED_YIELD, 124
    .equ ROUNDS, 3

    .section .text.init, "ax"
    .globl _start
_start:
    li s0, ROUNDS
1:  li a0, 1
    la a1, message
    li a2, 5
    li a7, SYS_WRITE
    ecall
    li a7, SYS_SCHED_YIELD
    ecall
    addi s0, s0, -1
    bnez s0, 1b
    li a0, 0
    li a7, SYS_EXIT
    ecall

    .section .rodata
message:
    .ascii "pong\n"
