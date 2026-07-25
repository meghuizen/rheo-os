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

/* Save the SysV caller-saved registers, then split on the trap source.
 *
 * Stack at this point (after the 9 pushes below), low to high:
 *   r11@0 r10@8 r9@16 r8@24 rdi@32 rsi@40 rdx@48 rcx@56 rax@64
 *   vector@72 error@80 rip@88 cs@96 rflags@104 rsp@112 ss@120
 *
 * A ring-3 fault (CS RPL == 3) is captured into the current cell's TrapFrame
 * and handed to x86_fault_trap, which either returns a rewritten frame to
 * resume (a Linux signal handler entry) via sysret_resume, or null to unwind.
 * A ring-0 trap (doorbell int3 or a fatal kernel exception) takes the old
 * iretq path through x86_trap_handler. */
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

    testb $3, 96(%rsp)     /* interrupt-frame CS: low 2 bits = RPL */
    jnz .Lfault_from_user

    /* Ring 0: doorbell or fatal. */
    mov 72(%rsp), %rdi      /* vector */
    mov 80(%rsp), %rsi      /* error code */
    mov 88(%rsp), %rdx      /* interrupt-frame RIP */
    mov 96(%rsp), %rcx      /* interrupt-frame CS */
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

/* Ring-3 fault: build the current cell's TrapFrame from the saved regs +
   interrupt frame, then dispatch. TrapFrame offsets match x86_64/mod.rs:
   rax0 rbx8 rcx16 rdx24 rsi32 rdi40 rbp48 r8_56 r9_64 r10_72 r11_80
   r12_88 r13_96 r14_104 r15_112 rip120 rflags128 rsp136. */
.Lfault_from_user:
    mov CUR_FRAME(%rip), %rax     /* rax = &frame */
    mov 64(%rsp), %rcx            /* saved rax */
    mov %rcx, 0(%rax)
    mov %rbx, 8(%rax)             /* rbx (never clobbered above) */
    mov 56(%rsp), %rcx           /* saved rcx */
    mov %rcx, 16(%rax)
    mov 48(%rsp), %rcx           /* saved rdx */
    mov %rcx, 24(%rax)
    mov 40(%rsp), %rcx           /* saved rsi */
    mov %rcx, 32(%rax)
    mov 32(%rsp), %rcx           /* saved rdi */
    mov %rcx, 40(%rax)
    mov %rbp, 48(%rax)
    mov 24(%rsp), %rcx           /* saved r8 */
    mov %rcx, 56(%rax)
    mov 16(%rsp), %rcx           /* saved r9 */
    mov %rcx, 64(%rax)
    mov 8(%rsp), %rcx            /* saved r10 */
    mov %rcx, 72(%rax)
    mov 0(%rsp), %rcx            /* saved r11 */
    mov %rcx, 80(%rax)
    mov %r12, 88(%rax)
    mov %r13, 96(%rax)
    mov %r14, 104(%rax)
    mov %r15, 112(%rax)
    mov 88(%rsp), %rcx           /* interrupt-frame rip */
    mov %rcx, 120(%rax)
    mov 104(%rsp), %rcx          /* rflags */
    mov %rcx, 128(%rax)
    mov 112(%rsp), %rcx          /* user rsp */
    mov %rcx, 136(%rax)

    mov 72(%rsp), %rdi           /* arg0 = vector */
    mov 88(%rsp), %rsi           /* arg1 = fault addr (default: rip) */
    cmpq $14, %rdi
    jne 1f
    mov %cr2, %rsi               /* #PF: faulting address in CR2 */
1:
    mov %rax, %rdx               /* arg2 = frame */
    call x86_fault_trap          /* -> frame* to resume in rax, or null */
    test %rax, %rax
    jz return_to_kernel_asm
    jmp sysret_resume            /* rax = frame; resume (handler or resumed pc) */

/* Hardware-interrupt stubs (docs/LIBRHEO.md Phase F). These fire in ring 0
 * (the kernel's sti;hlt idle idiom), so there is no privilege/stack switch -
 * the CPU pushes the 5-word long-mode frame onto the current kernel stack. Save
 * the 9 SysV caller-saved registers (which also 16-aligns rsp for the call),
 * run the Rust handler, restore, iretq. Callee-saved regs are preserved by the
 * C handler, so they need no save here. */
.macro IRQ_STUB name, handler
.global \name
\name:
    push %rax
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %r11
    call \handler
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rax
    iretq
.endm

IRQ_STUB timer_irq_stub, x86_timer_irq

/* The LAPIC spurious vector requires no EOI and does no work: just return. */
.global spurious_irq_stub
spurious_irq_stub:
    iretq

/* Stub address table consumed by trap_init() in Rust. */
.section .rodata
.align 8
.global VECTOR_STUBS
VECTOR_STUBS:
.irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad vector_stub_\n
.endr
