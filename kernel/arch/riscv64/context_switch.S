/*
 * RISC-V 64 cooperative context switch (one of the three allowed assembly
 * pieces, docs/TOOLING.md 1). a0 = *mut old_sp, a1 = *const new_sp.
 * Saves the callee-saved registers on the current stack, swaps stack
 * pointers, restores, and returns on the new stack.
 */

.section .text
.global context_switch_asm
context_switch_asm:
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

    mv t0, sp
    sd t0, 0(a0)
    ld t0, 0(a1)
    mv sp, t0

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
