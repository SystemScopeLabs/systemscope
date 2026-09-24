# hello.S: prints "Hello, SystemScope!\n" through the m1-reference SimpleUart
# (docs/m1-design.md §7.3, §9, M1-A5). Pure RV32I: no libc, no C runtime, no stack.
#
# Each byte is a one-byte store (SB) to TX, the UART's offset 0x0 at 0x1000_0000. STATUS
# always reads ready, so there is no polling. The loop stops at the end address, so the
# message needs no terminator and nothing but its 20 bytes reaches the UART. The program
# then ends with the riscv-tests pass convention the rv32ui runner uses (§10.2):
# gp = 1, a0 = 0, ECALL.

    .option norelax

    .section .text.init, "ax", @progbits
    .globl _start
_start:
    lui     t0, 0x10000             # t0 = UART base 0x1000_0000, TX at offset 0
    la      t1, message             # t1 = next byte
    la      t2, message_end         # t2 = one past the last byte
1:
    lbu     t3, 0(t1)
    sb      t3, 0(t0)
    addi    t1, t1, 1
    bne     t1, t2, 1b

    li      gp, 1
    li      a0, 0
    ecall

    .section .rodata, "a", @progbits
message:
    .ascii  "Hello, SystemScope!\n"
message_end:
