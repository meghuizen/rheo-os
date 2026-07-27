# 
# RISC-V 64 trap trampoline + U-mode entry (stvec, direct mode).
# 
# sscratch is the mode flag: 0 while the kernel runs, and the current
# TrapFrame pointer while a cell runs in U-mode. On any trap we swap sp
# with sscratch; a nonzero old value means the trap came from user.
# 
# User traps save the full GP register set + sepc into the TrapFrame,
# switch to the kernel stack, and call riscv_user_trap, which returns the
# TrapFrame to resume (possibly a different cell's, for a cross-cell
# switch). Kernel traps (the boot self-test's ebreak) take the original
# S-mode path into riscv_trap_handler.

.equ TF_SEPC, 256
.equ TF_KSP,  264
# The kernel's own tp (its CPU index). tp is a saved GPR the cell owns as TLS, so
# the kernel must park its own value somewhere across U-mode and take it back on
# the way in - otherwise every trap handler runs with the cell's tp and reads the
# wrong CPU's per-CPU state (docs/SMP.md 10.0).
.equ TF_KTP,  272

.equ KERNEL_CTX_SLOTS, 8

.section .text
.p2align 2
.global trap_vector
trap_vector:
    csrrw sp, sscratch, sp    # sp = old sscratch; sscratch = old sp
    bnez sp, .Lfrom_user
# From kernel: restore sp and sscratch(=0), take the S-mode path.
    csrrw sp, sscratch, sp
    j kernel_trap_entry

.Lfrom_user:
# sp = &TrapFrame; the user sp is now in sscratch.
    sd x1,  1*8(sp)
    sd x3,  3*8(sp)
    sd x4,  4*8(sp)
    sd x5,  5*8(sp)
    sd x6,  6*8(sp)
    sd x7,  7*8(sp)
    sd x8,  8*8(sp)
    sd x9,  9*8(sp)
    sd x10, 10*8(sp)
    sd x11, 11*8(sp)
    sd x12, 12*8(sp)
    sd x13, 13*8(sp)
    sd x14, 14*8(sp)
    sd x15, 15*8(sp)
    sd x16, 16*8(sp)
    sd x17, 17*8(sp)
    sd x18, 18*8(sp)
    sd x19, 19*8(sp)
    sd x20, 20*8(sp)
    sd x21, 21*8(sp)
    sd x22, 22*8(sp)
    sd x23, 23*8(sp)
    sd x24, 24*8(sp)
    sd x25, 25*8(sp)
    sd x26, 26*8(sp)
    sd x27, 27*8(sp)
    sd x28, 28*8(sp)
    sd x29, 29*8(sp)
    sd x30, 30*8(sp)
    sd x31, 31*8(sp)
    csrr t0, sscratch
    sd t0, 2*8(sp)    # user sp = x2
    csrw sscratch, x0    # mark in-kernel
    csrr t0, sepc
    sd t0, TF_SEPC(sp)

    csrr a0, scause
    csrr a1, stval
    mv a2, sp
    ld tp, TF_KTP(sp)    # kernel CPU index (the cell's tp stays saved in the frame)
    ld sp, TF_KSP(sp)    # kernel stack
    call riscv_user_trap    # -> TrapFrame* to resume, in a0
# fall through

.global user_resume
user_resume:    # a0 = &TrapFrame
    mv t6, a0
    sd tp, TF_KTP(t6)    # this CPU reclaims its identity on the next trap
    ld t0, TF_SEPC(t6)
    csrw sepc, t0
    csrw sscratch, t6    # next user trap saves into this frame
    ld x1,  1*8(t6)
    ld x2,  2*8(t6)    # user sp
    ld x3,  3*8(t6)
    ld x4,  4*8(t6)
    ld x5,  5*8(t6)
    ld x6,  6*8(t6)
    ld x7,  7*8(t6)
    ld x8,  8*8(t6)
    ld x9,  9*8(t6)
    ld x10, 10*8(t6)
    ld x11, 11*8(t6)
    ld x12, 12*8(t6)
    ld x13, 13*8(t6)
    ld x14, 14*8(t6)
    ld x15, 15*8(t6)
    ld x16, 16*8(t6)
    ld x17, 17*8(t6)
    ld x18, 18*8(t6)
    ld x19, 19*8(t6)
    ld x20, 20*8(t6)
    ld x21, 21*8(t6)
    ld x22, 22*8(t6)
    ld x23, 23*8(t6)
    ld x24, 24*8(t6)
    ld x25, 25*8(t6)
    ld x26, 26*8(t6)
    ld x27, 27*8(t6)
    ld x28, 28*8(t6)
    ld x29, 29*8(t6)
    ld x30, 30*8(t6)
    ld x31, 31*8(t6)    # last: overwrites the base register
    sret

# Kernel trap path (unchanged self-test handler): saves caller-saved
# registers, hands (scause, sepc, stval) to Rust, writes back the
# resume sepc, restores, sret.
kernel_trap_entry:
    addi sp, sp, -128
    sd ra, 0(sp)
    sd t0, 8(sp)
    sd t1, 16(sp)
    sd t2, 24(sp)
    sd t3, 32(sp)
    sd t4, 40(sp)
    sd t5, 48(sp)
    sd t6, 56(sp)
    sd a0, 64(sp)
    sd a1, 72(sp)
    sd a2, 80(sp)
    sd a3, 88(sp)
    sd a4, 96(sp)
    sd a5, 104(sp)
    sd a6, 112(sp)
    sd a7, 120(sp)

    csrr a0, scause
    csrr a1, sepc
    csrr a2, stval
    call riscv_trap_handler
    csrw sepc, a0

    ld ra, 0(sp)
    ld t0, 8(sp)
    ld t1, 16(sp)
    ld t2, 24(sp)
    ld t3, 32(sp)
    ld t4, 40(sp)
    ld t5, 48(sp)
    ld t6, 56(sp)
    ld a0, 64(sp)
    ld a1, 72(sp)
    ld a2, 80(sp)
    ld a3, 88(sp)
    ld a4, 96(sp)
    ld a5, 104(sp)
    ld a6, 112(sp)
    ld a7, 120(sp)
    addi sp, sp, 128
    sret

# enter_user_first(a0 = &TrapFrame): save the kernel's callee-saved state
# so return_to_kernel can unwind here, drop to U-mode, and resume the
# frame.
.global enter_user_first
enter_user_first:
    addi sp, sp, -112
    sd ra, 0(sp)
    sd s0, 8(sp)
    sd s1, 16(sp)
    sd s2, 24(sp)
    sd s3, 32(sp)
    sd s4, 40(sp)
    sd s5, 48(sp)
    sd s6, 56(sp)
    sd s7, 64(sp)
    sd s8, 72(sp)
    sd s9, 80(sp)
    sd s10, 88(sp)
    sd s11, 96(sp)
    # Per-hart slot: tp holds this CPU's registry index (smp_set_this_cpu), masked
    # so an uninitialised tp can only alias slot 0 rather than scribble past the
    # array. Making this per-hart is what lets a *secondary* core run a cell in user
    # mode and unwind back into its own kernel stack (docs/SMP.md 10.0); with one
    # CPU, tp is 0 and this is the previous single-slot behaviour exactly.
    la t0, KERNEL_CTX
    andi t1, tp, (KERNEL_CTX_SLOTS - 1)
    slli t1, t1, 3
    add t0, t0, t1
    sd sp, 0(t0)
    li t1, (1 << 8)    # sstatus.SPP: 0 = return to U-mode
    csrc sstatus, t1
    j user_resume    # a0 still = &TrapFrame

# return_to_kernel(): restore the state enter_user_first saved and return
# out of it. Diverges from the caller's point of view.
.global return_to_kernel_asm
return_to_kernel_asm:
    la t0, KERNEL_CTX
    andi t1, tp, (KERNEL_CTX_SLOTS - 1)
    slli t1, t1, 3
    add t0, t0, t1
    ld sp, 0(t0)
    ld ra, 0(sp)
    ld s0, 8(sp)
    ld s1, 16(sp)
    ld s2, 24(sp)
    ld s3, 32(sp)
    ld s4, 40(sp)
    ld s5, 48(sp)
    ld s6, 56(sp)
    ld s7, 64(sp)
    ld s8, 72(sp)
    ld s9, 80(sp)
    ld s10, 88(sp)
    ld s11, 96(sp)
    addi sp, sp, 112
    ret

.section .bss
.p2align 3
# One saved kernel context per hart, indexed by tp (see KERNEL_CTX_SLOTS above).
KERNEL_CTX:
    .space 8 * KERNEL_CTX_SLOTS
