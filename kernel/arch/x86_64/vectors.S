/*
 * x86-64 exception vectors. 32 stubs (one per CPU exception) normalise the
 * stack to [saved regs][vector][error code][interrupt frame] and call the
 * Rust handler. Vectors 8, 10-14, 17 and 21 push a hardware error code;
 * the rest get a dummy 0 so the frame layout is uniform.
 */

.section .text

.macro VECTOR num, has_err
vector_stub_\num:
.if \has_err == 0
    pushq $0
.endif
    pushq $\num
    jmp common_trap
.endm

VECTOR 0, 0
VECTOR 1, 0
VECTOR 2, 0
VECTOR 3, 0
VECTOR 4, 0
VECTOR 5, 0
VECTOR 6, 0
VECTOR 7, 0
VECTOR 8, 1
VECTOR 9, 0
VECTOR 10, 1
VECTOR 11, 1
VECTOR 12, 1
VECTOR 13, 1
VECTOR 14, 1
VECTOR 15, 0
VECTOR 16, 0
VECTOR 17, 1
VECTOR 18, 0
VECTOR 19, 0
VECTOR 20, 0
VECTOR 21, 1
VECTOR 22, 0
VECTOR 23, 0
VECTOR 24, 0
VECTOR 25, 0
VECTOR 26, 0
VECTOR 27, 0
VECTOR 28, 0
VECTOR 29, 0
VECTOR 30, 0
VECTOR 31, 0

/* Save the SysV caller-saved registers, hand (vector, error, rip) to Rust,
   restore, and return. The Rust handler only comes back for the doorbell
   vector (3); everything else exits QEMU from Rust. */
common_trap:
    pushq %rax
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11

    mov 72(%rsp), %rdi      /* vector */
    mov 80(%rsp), %rsi      /* error code */
    mov 88(%rsp), %rdx      /* interrupt-frame RIP */
    call x86_trap_handler

    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rax
    add $16, %rsp           /* drop vector + error code */
    iretq

/* Stub address table consumed by trap_init() in Rust. */
.section .rodata
.align 8
.global VECTOR_STUBS
VECTOR_STUBS:
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad vector_stub_\n
.endr
