/*
 * x86-64 cooperative context switch (one of the three allowed assembly
 * pieces, docs/TOOLING.md 1). SysV: rdi = *mut old_sp, rsi = *const new_sp.
 * Saves the callee-saved registers on the current stack, swaps stack
 * pointers, restores, and returns on the new stack.
 */

.section .text
.global context_switch_asm
context_switch_asm:
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    mov %rsp, (%rdi)
    mov (%rsi), %rsp

    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    ret
