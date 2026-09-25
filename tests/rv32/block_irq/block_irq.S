# block_irq.S: the M2 end-to-end program on m2-reference (docs/m2-design.md §11, §12).
# RV32I + Zicsr assembly: no libc, no C runtime, no stack.
#
# Three DMA transfers, each ended by the block controller's completion interrupt:
#
#   1. READ  LBA 0 -> buffer A, then check A against the disk fixture's LBA 0 pattern;
#   2. WRITE buffer A -> LBA 1, after inverting every word of A (w <- ~w);
#   3. READ  LBA 1 -> buffer B, then compare B with A word by word.
#
# The main program waits for each completion by loading `flag` from RAM until it is
# non-zero (§12.2). It never uses WFI and never reads a device register, so the only way
# forward is the interrupt, and every loop iteration is a RAM load that competes with the
# DMA beats for the RAM region. The handler (§12.3) reads STATUS; on DONE it saves STATUS
# and counts the entry, acknowledges the controller, then sets `flag`; then MRET.
#
# LBA 0 pattern: word k (k = 0..127, little-endian) is (k * 0x01010101) ^ 0x5aa5c33c.
#
# On success the program prints "M2 PASS\n" to the UART and ends with gp = 1, a0 = 0,
# ECALL; on any failure it prints "M2 FAIL\n" and ends with gp = 1, a0 = 1, ECALL.
#
# Registers: s0 = block controller base, s1 = &vars, both set once and never changed.
# The handler uses only t3-t6 (and reads s0, s1), which the main program never uses, so
# it needs to save nothing wherever it interrupts.

    .option norelax

    # m2-reference (§11).
    .equ UART,          0x10000000      # SimpleUart: TX at offset 0x0
    .equ IRQC,          0x10001000      # SimpleIrqController
    .equ IRQC_ENABLE,   0x4
    .equ BLK,           0x10002000      # DmaBlockController (§9.2)
    .equ COMMAND,       0x00
    .equ STATUS,        0x04
    .equ LBA,           0x08
    .equ MEM_ADDR,      0x0c
    .equ BLOCK_COUNT,   0x10
    .equ IRQ_ENABLE,    0x14
    .equ ACK,           0x18
    .equ READ,          1
    .equ WRITE,         2
    .equ DONE,          2               # STATUS bit 1
    .equ MEIE,          0x800           # mie bit 11
    .equ MIE,           8               # mstatus bit 3

    # Offsets in `vars`.
    .equ FLAG,          0               # set to 1 by the handler
    .equ SAVED_STATUS,  4               # STATUS as the handler read it
    .equ ENTRIES,       8               # completions the handler saw

    .equ PATTERN_STEP,  0x01010101
    .equ PATTERN_XOR,   0x5aa5c33c

    .section .text.init, "ax", @progbits
    .globl _start
_start:
    li      s0, BLK
    la      s1, vars

    # Setup (§12.1 steps 1-5).
    la      t0, handler
    csrw    mtvec, t0                   # Direct mode: handler is 4-byte aligned
    li      t0, 1
    li      t1, IRQC
    sw      t0, IRQC_ENABLE(t1)         # enable source 0, the block controller
    sw      t0, IRQ_ENABLE(s0)          # enable the completion interrupt
    li      t0, MEIE
    csrs    mie, t0
    csrsi   mstatus, MIE                # interrupts are live

    # Transfer 1: READ LBA 0 -> A (steps 6-11).
    li      a0, READ
    li      a1, 0
    la      a2, buffer_a
    jal     ra, transfer
    la      a2, buffer_a
    li      t0, 0                       # k * 0x01010101
    li      t1, PATTERN_STEP
    li      t2, PATTERN_XOR
    addi    a3, a2, 512
1:
    xor     a4, t0, t2
    lw      a5, 0(a2)
    bne     a4, a5, fail
    add     t0, t0, t1
    addi    a2, a2, 4
    bne     a2, a3, 1b

    # Transfer 2: invert A, then WRITE A -> LBA 1 (steps 12-17).
    la      a2, buffer_a
    addi    a3, a2, 512
1:
    lw      a5, 0(a2)
    xori    a5, a5, -1
    sw      a5, 0(a2)
    addi    a2, a2, 4
    bne     a2, a3, 1b
    li      a0, WRITE
    li      a1, 1
    la      a2, buffer_a
    jal     ra, transfer

    # Transfer 3: READ LBA 1 -> B, then B == A (steps 18-22).
    li      a0, READ
    li      a1, 1
    la      a2, buffer_b
    jal     ra, transfer
    la      a2, buffer_a
    la      a3, buffer_b
    addi    a6, a2, 512
1:
    lw      a4, 0(a2)
    lw      a5, 0(a3)
    bne     a4, a5, fail
    addi    a2, a2, 4
    addi    a3, a3, 4
    bne     a2, a6, 1b

    # Finish (step 23).
    la      a2, pass_message
    li      a0, 0
    j       finish
fail:
    la      a2, fail_message
    li      a0, 1
finish:
    li      t0, UART
    addi    a3, a2, 8
1:
    lbu     t1, 0(a2)
    sb      t1, 0(t0)
    addi    a2, a2, 1
    bne     a2, a3, 1b
    li      gp, 1
    ecall

# transfer(a0 = COMMAND, a1 = LBA, a2 = MEM_ADDR): one block. Clears `flag`, programs
# and starts the command, waits on `flag`, and fails unless the handler saved exactly
# DONE: ERROR = 0 and REJECTED clear.
transfer:
    sw      zero, FLAG(s1)
    sw      a1, LBA(s0)
    sw      a2, MEM_ADDR(s0)
    li      t0, 1
    sw      t0, BLOCK_COUNT(s0)
    sw      a0, COMMAND(s0)
poll:
    lw      t0, FLAG(s1)                # the wait: a RAM load, never a device read
    beqz    t0, poll
    lw      t0, SAVED_STATUS(s1)
    li      t1, DONE
    bne     t0, t1, fail
    ret

# The machine external interrupt handler (§12.3).
    .balign 4
handler:
    lw      t3, STATUS(s0)              # 1. read STATUS
    andi    t4, t3, DONE
    beqz    t4, 1f                      # 2. not DONE: spurious
    sw      t3, SAVED_STATUS(s1)        # 3. save STATUS, count the entry
    lw      t4, ENTRIES(s1)
    addi    t4, t4, 1
    sw      t4, ENTRIES(s1)
    li      t4, 1
    sw      t4, ACK(s0)                 # 4. ACK: the controller deasserts its line
    sw      t4, FLAG(s1)                # 5. publish the completion
1:
    mret                                # 6.

    .section .rodata, "a", @progbits
pass_message:
    .ascii  "M2 PASS\n"
fail_message:
    .ascii  "M2 FAIL\n"

    .section .bss, "aw", @nobits
    .balign 16
    .globl vars
vars:
    .space  16
    .balign 512
    .globl buffer_a
buffer_a:
    .space  512
    .globl buffer_b
buffer_b:
    .space  512
