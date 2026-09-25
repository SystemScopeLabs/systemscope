# user.elf: the M3.1 user-image fixture (docs/m3-design.md §8.3).
#
# A U-mode program linked by the pinned toolchain's default linker script, so its segments
# are what a real linker makes: the ELF headers, text, and read-only data in one R+X
# PT_LOAD at 0x10000, data and .bss in one R+W PT_LOAD on a later page. It reads a .data
# word, stores it into .bss, writes a message with the write syscall (a7 = 64) and exits
# (a7 = 93), the Linux RV32 numbers the M3 syscall ABI keeps (§6.5).
#
# M3.1 only parses it; nothing runs it yet.

    .section .rodata
msg:
    .ascii "hello from user mode\n"
    .set msg_len, . - msg

    .section .data
    .balign 4
counter:
    .word 0x00005353

    .section .bss
    .balign 4
scratch:
    .space 64

    .section .text
    .globl _start
_start:
    la      t0, counter
    lw      t1, 0(t0)
    la      t2, scratch
    sw      t1, 0(t2)
    li      a0, 1
    la      a1, msg
    li      a2, msg_len
    li      a7, 64
    ecall
    li      a0, 0
    li      a7, 93
    ecall
1:
    j       1b
