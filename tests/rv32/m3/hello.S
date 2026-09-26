# hello: entry 0 of the M3 reference disk (docs/m3-design.md §12.2). Writes
# "hello from pid N\n", with N from getpid, then exits 0. It has all four segment kinds:
# the code in text, the greeting in rodata, the newline in data, and the line being built
# in bss.
#
# gp-independent (§17.1): linked without relaxation, every address is pc-relative.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93
    .equ SYS_GETPID, 172
    .equ PREFIX_LEN, 15             # "hello from pid "

    .section .text.init, "ax"
    .globl _start
_start:
    # Copy the greeting into the line.
    la s0, line
    la t0, greeting
    li t1, PREFIX_LEN
    mv t2, s0
1:  lbu t3, 0(t0)
    sb t3, 0(t2)
    addi t0, t0, 1
    addi t2, t2, 1
    addi t1, t1, -1
    bnez t1, 1b
    # The PID, one decimal digit (a table has at most 8 entries).
    li a7, SYS_GETPID
    ecall
    addi a0, a0, '0'
    sb a0, PREFIX_LEN(s0)
    la t0, newline
    lbu t3, 0(t0)
    sb t3, PREFIX_LEN + 1(s0)
    # write(1, line, 17)
    li a0, 1
    mv a1, s0
    li a2, PREFIX_LEN + 2
    li a7, SYS_WRITE
    ecall
    # exit(0)
    li a0, 0
    li a7, SYS_EXIT
    ecall

    .section .rodata
greeting:
    .ascii "hello from pid "

    .section .data
newline:
    .byte '\n'

    .section .bss
line:
    .zero 32
