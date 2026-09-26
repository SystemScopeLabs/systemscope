# m3-firmware.elf: the M3 guest glue (docs/m3-design.md §7). An M-mode boot stub, the
# S-mode `s_boot`, and the S-mode trampoline, placed at the RAM base by the host-side
# loader, the equivalent of a boot ROM.
#
# The glue knows the trap frame ABI (§7.2), the trap frame's address, and `kgate.ENTER`,
# and nothing else: no process, no syscall number, no program. Every context change the
# modeled kernel decides reaches the hart when `restore` loads the trap frame and runs
# SRET; the kernel's only entry is the one store to `kgate.ENTER`, which it holds until
# its operation ends (§6.3).
#
# The trampoline must never fault (§6.7): it runs from the identity-mapped kernel
# megapage and touches only the trap frame (kernel megapage), `kgate` (MMIO megapage), and
# S-mode CSRs, so it runs unchanged under every process's `satp`.
#
# Built with -march=rv32i_zicsr and linked without relaxation: nothing here uses `gp`.

    .option norelax

    .equ TRAP_FRAME, 0x80010000     # §11.2
    .equ KGATE_ENTER, 0x10003000    # §11.1: kgate base + ENTER (offset 0)
    .equ MEDELEG_VALUE, 0xB1FF      # §7.1: every exception but ecall from S and M
    .equ MSTATUS_MPP, 0x1800
    .equ MSTATUS_MPP_S, 0x0800
    .equ SRST_EID, 0x53525354       # §7.4: the SBI SRST extension

    # Trap frame offsets (§7.2).
    .equ F_SEPC, 0x7C
    .equ F_SSTATUS, 0x80
    .equ F_SCAUSE, 0x84
    .equ F_STVAL, 0x88
    .equ F_SATP, 0x8C
    .equ F_ACTION, 0x90
    .equ F_REASON, 0x94

    .section .text.init, "ax"
    .globl _start

# The M-mode boot stub (§7.1). The CPU resets here in M-mode.
_start:
    li t0, MEDELEG_VALUE
    csrw medeleg, t0
    la t0, trampoline
    csrw stvec, t0
    li t0, TRAP_FRAME
    csrw sscratch, t0
    li t0, MSTATUS_MPP
    csrc mstatus, t0
    li t0, MSTATUS_MPP_S
    csrs mstatus, t0
    la t0, s_boot
    csrw mepc, t0
    mret

# S-mode, satp still Bare (§7.1): enter the kernel once to boot, then resume the first
# process through the trampoline's restore path.
s_boot:
    li t6, TRAP_FRAME
    li t5, KGATE_ENTER
    sw t6, 0(t5)
    j restore

# The trampoline (§7.3): `stvec` points here, in Direct mode.
    .balign 4
    .globl trampoline
trampoline:
    csrrw t6, sscratch, t6
    sw x1, 0x00(t6)
    sw x2, 0x04(t6)
    sw x3, 0x08(t6)
    sw x4, 0x0C(t6)
    sw x5, 0x10(t6)
    sw x6, 0x14(t6)
    sw x7, 0x18(t6)
    sw x8, 0x1C(t6)
    sw x9, 0x20(t6)
    sw x10, 0x24(t6)
    sw x11, 0x28(t6)
    sw x12, 0x2C(t6)
    sw x13, 0x30(t6)
    sw x14, 0x34(t6)
    sw x15, 0x38(t6)
    sw x16, 0x3C(t6)
    sw x17, 0x40(t6)
    sw x18, 0x44(t6)
    sw x19, 0x48(t6)
    sw x20, 0x4C(t6)
    sw x21, 0x50(t6)
    sw x22, 0x54(t6)
    sw x23, 0x58(t6)
    sw x24, 0x5C(t6)
    sw x25, 0x60(t6)
    sw x26, 0x64(t6)
    sw x27, 0x68(t6)
    sw x28, 0x6C(t6)
    sw x29, 0x70(t6)
    sw x30, 0x74(t6)
    csrr t5, sscratch
    sw t5, 0x78(t6)
    csrr t5, sepc
    sw t5, F_SEPC(t6)
    csrr t5, sstatus
    sw t5, F_SSTATUS(t6)
    csrr t5, scause
    sw t5, F_SCAUSE(t6)
    csrr t5, stval
    sw t5, F_STVAL(t6)
    li t5, KGATE_ENTER
    sw t6, 0(t5)

# The restore path, shared with s_boot; t6 holds the trap frame's address.
restore:
    lw t5, F_ACTION(t6)
    bnez t5, shutdown
    lw t5, F_SATP(t6)
    csrw satp, t5
    sfence.vma
    lw t5, F_SEPC(t6)
    csrw sepc, t5
    lw t5, F_SSTATUS(t6)
    csrw sstatus, t5
    csrw sscratch, t6
    lw x1, 0x00(t6)
    lw x2, 0x04(t6)
    lw x3, 0x08(t6)
    lw x4, 0x0C(t6)
    lw x5, 0x10(t6)
    lw x6, 0x14(t6)
    lw x7, 0x18(t6)
    lw x8, 0x1C(t6)
    lw x9, 0x20(t6)
    lw x10, 0x24(t6)
    lw x11, 0x28(t6)
    lw x12, 0x2C(t6)
    lw x13, 0x30(t6)
    lw x14, 0x34(t6)
    lw x15, 0x38(t6)
    lw x16, 0x3C(t6)
    lw x17, 0x40(t6)
    lw x18, 0x44(t6)
    lw x19, 0x48(t6)
    lw x20, 0x4C(t6)
    lw x21, 0x50(t6)
    lw x22, 0x54(t6)
    lw x23, 0x58(t6)
    lw x24, 0x5C(t6)
    lw x25, 0x60(t6)
    lw x26, 0x64(t6)
    lw x27, 0x68(t6)
    lw x28, 0x6C(t6)
    lw x29, 0x70(t6)
    lw x30, 0x74(t6)
    lw x31, 0x78(t6)
    sret

# Shutdown (§7.4): the SBI SRST call from S-mode, which halts the hart in M-mode.
shutdown:
    li a7, SRST_EID
    li a6, 0
    li a0, 0
    lw a1, F_REASON(t6)
    ecall
