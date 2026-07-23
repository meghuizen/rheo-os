/*
 * ARM64 cooperative context switch (one of the three allowed assembly
 * pieces, docs/TOOLING.md 1). AAPCS: x0 = *mut old_sp, x1 = *const new_sp.
 * Saves the callee-saved registers on the current stack, swaps stack
 * pointers, restores, and returns on the new stack.
 */

.section .text
.global context_switch_asm
context_switch_asm:
    sub sp, sp, #96
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]

    mov x2, sp
    str x2, [x0]
    ldr x2, [x1]
    mov sp, x2

    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    add sp, sp, #96
    ret
