# badptr: entry 4 of the M3 reference disk (docs/m3-design.md §12.2). Three refused
# syscalls, each result checked: write(1, <kernel address>, 4) is -EFAULT, write(7, ...)
# is -EBADF, and syscall 999 is -ENOSYS. It then exits 0. A wrong result exits with the
# failed check's number (1, 2, or 3) instead, which the trace shows.
#
# gp-independent (§17.1): linked without relaxation, every address is pc-relative.

    .option norelax

    .equ SYS_WRITE, 64
    .equ SYS_EXIT, 93
    .equ SYS_UNKNOWN, 999
    .equ KERNEL_ADDRESS, 0x80000000 # the kernel megapage, U = 0 (§11.2)
    .equ EFAULT, 14
    .equ EBADF, 9
    .equ ENOSYS, 38

    .section .text.init, "ax"
    .globl _start
_start:
    # write(1, KERNEL_ADDRESS, 4) == -EFAULT
    li s0, 1
    li a0, 1
    li a1, KERNEL_ADDRESS
    li a2, 4
    li a7, SYS_WRITE
    ecall
    li t0, -EFAULT
    bne a0, t0, fail
    # write(7, message, 4) == -EBADF
    li s0, 2
    li a0, 7
    la a1, message
    li a2, 4
    li a7, SYS_WRITE
    ecall
    li t0, -EBADF
    bne a0, t0, fail
    # syscall 999 == -ENOSYS
    li s0, 3
    li a7, SYS_UNKNOWN
    ecall
    li t0, -ENOSYS
    bne a0, t0, fail
    li s0, 0
fail:
    mv a0, s0
    li a7, SYS_EXIT
    ecall

    .section .rodata
message:
    .ascii "bad\n"
