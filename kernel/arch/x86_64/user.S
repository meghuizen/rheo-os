/*
 * x86-64 U-mode entry via SYSCALL/SYSRET (att syntax).
 *
 * Single CPU, so the "current frame", kernel stack, and a user-rsp scratch
 * are plain globals (no GS/swapgs needed). SYSCALL leaves the user stack
 * live and puts the return RIP in RCX and RFLAGS in R11; the stub saves
 * the full register set into the current cell's TrapFrame, switches to the
 * kernel stack, and calls x86_user_trap, which returns the frame to resume
 * (this cell or a peer, for a cross-cell switch) or null to unwind.
 *
 * TrapFrame offsets (see x86_64/mod.rs): rax0 rbx8 rcx16 rdx24 rsi32 rdi40
 * rbp48 r8 56 r9 64 r10 72 r11 80 r12 88 r13 96 r14 104 r15 112 rip120
 * rflags128 rsp136 kernel_sp144.
 */

.section .text

.global syscall_entry
syscall_entry:
    mov %rsp, USER_RSP_SCRATCH(%rip)
    mov KERNEL_RSP(%rip), %rsp
    push %rax                          /* stash syscall number */
    mov CUR_FRAME(%rip), %rax          /* rax = &frame */
    mov %rbx, 8(%rax)
    mov %rdx, 24(%rax)
    mov %rsi, 32(%rax)
    mov %rdi, 40(%rax)
    mov %rbp, 48(%rax)
    mov %r8, 56(%rax)
    mov %r9, 64(%rax)
    mov %r10, 72(%rax)
    mov %r12, 88(%rax)
    mov %r13, 96(%rax)
    mov %r14, 104(%rax)
    mov %r15, 112(%rax)
    mov %rcx, 120(%rax)                /* return rip */
    mov %r11, 128(%rax)                /* rflags */
    mov USER_RSP_SCRATCH(%rip), %rcx
    mov %rcx, 136(%rax)                /* user rsp */
    pop %rcx                           /* rcx = stashed syscall number */
    mov %rcx, 0(%rax)                  /* rax slot */

    xor %edi, %edi                     /* kind = Syscall (0) */
    xor %esi, %esi                     /* fault_addr = 0 */
    mov %rax, %rdx                     /* frame */
    call x86_user_trap                 /* -> resume frame in rax, or 0 */
    test %rax, %rax
    jz return_to_kernel_asm
    /* fall through to sysret_resume with rax = frame */

.global sysret_resume
sysret_resume:                         /* rax = &frame */
    mov %rax, CUR_FRAME(%rip)          /* next syscall saves here */
    mov 120(%rax), %rcx                /* rip -> rcx (sysret target) */
    mov 128(%rax), %r11                /* rflags */
    mov 8(%rax), %rbx
    mov 24(%rax), %rdx
    mov 32(%rax), %rsi
    mov 40(%rax), %rdi
    mov 48(%rax), %rbp
    mov 56(%rax), %r8
    mov 64(%rax), %r9
    mov 72(%rax), %r10
    mov 88(%rax), %r12
    mov 96(%rax), %r13
    mov 104(%rax), %r14
    mov 112(%rax), %r15
    mov 136(%rax), %rsp                /* user rsp */
    mov 0(%rax), %rax                  /* rax last */
    sysretq

/* enter_user_first(rdi = &frame): save callee-saved state so
   return_to_kernel can unwind here, then drop to ring 3 via sysret. */
.global enter_user_first
enter_user_first:
    push %rbx
    push %rbp
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, KERNEL_CTX(%rip)
    mov %rdi, %rax
    jmp sysret_resume

/* return_to_kernel_asm(): restore enter_user_first's state and return. */
.global return_to_kernel_asm
return_to_kernel_asm:
    mov KERNEL_CTX(%rip), %rsp
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %rbp
    pop %rbx
    ret
