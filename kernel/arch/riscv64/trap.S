/*
 * RISC-V 64 trap entry (stvec, direct mode). Saves the caller-saved
 * registers, hands (scause, sepc, stval) to Rust, writes back the
 * resume sepc the handler returns, restores, and sret. Rust only
 * returns for ebreak (the doorbell); everything else exits QEMU.
 */

.section .text
.p2align 2
.global trap_vector
trap_vector:
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
