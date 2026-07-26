/*
 * ARM64 SMP: a guarded PSCI CPU_ON attempt (docs/SMP.md, task #27).
 *
 * On this QEMU virt config the kernel runs at EL1 with no EL2/EL3 (secure=off,
 * virtualization=off), so a PSCI `smc #0` is UNDEFINED at EL1 with no EL3 to
 * service it and traps back into EL1. psci_cpu_on_guarded installs a temporary
 * exception vector, masks interrupts, issues the SMC, and restores - so the trap
 * is *observed* (the helper returns a sentinel) instead of reaching the kernel's
 * fatal sync handler and killing the primary. If QEMU instead emulates PSCI, the
 * real PSCI status is returned. Either way the caller reports skip-with-reason.
 *
 *   x0 = target MPIDR affinity, x1 = entry point PA, x2 = context id
 *   returns x0 = PSCI status, or 0xFFFF_FFFF_FFFF_FFFF if the SMC trapped.
 */

.section .text
.p2align 2
.global psci_cpu_on_guarded
psci_cpu_on_guarded:
    /* Save the callee-saved regs we use + lr (16-byte aligned frame). */
    stp x19, x20, [sp, #-48]!
    stp x21, x22, [sp, #16]
    stp x23, x30, [sp, #32]

    mov  x19, x0                 /* target  */
    mov  x20, x1                 /* entry   */
    mov  x21, x2                 /* ctx     */
    mrs  x22, vbar_el1           /* save real vectors */
    mrs  x23, daif               /* save interrupt mask */

    /* Mask IRQ/FIQ/SError so only the SMC sync trap can reach the temp vector. */
    msr  daifset, #0xf

    /* Install the temporary vector table. */
    adrp x9, smp_tmp_vectors
    add  x9, x9, :lo12:smp_tmp_vectors
    msr  vbar_el1, x9
    isb

    /* PSCI_CPU_ON (SMC64) function id 0xC4000003; args in x1..x3. */
    movz x0, #0x0003
    movk x0, #0xC400, lsl #16
    mov  x1, x19
    mov  x2, x20
    mov  x3, x21
    smc  #0
    /* Normal return lands here with x0 = PSCI status; the trap path redirects
       here too (via elr) with x0 = the sentinel. */
.global smp_after_smc
smp_after_smc:
    msr  vbar_el1, x22           /* restore real vectors */
    msr  daif, x23               /* restore interrupt mask */
    isb
    ldp x21, x22, [sp, #16]
    ldp x23, x30, [sp, #32]
    ldp x19, x20, [sp], #48
    ret

/* Temporary vectors: 2 KiB aligned, 16 slots of 128 bytes. Every slot redirects
   to smp_catch; only the current-EL SPx synchronous slot (the SMC trap) is
   expected to fire (interrupts are masked around the SMC). */
.p2align 11
smp_tmp_vectors:
.macro TMP_SLOT
    .p2align 7
    b smp_catch
.endm
    TMP_SLOT                     /* 0  current EL SP0 sync   */
    TMP_SLOT                     /* 1  current EL SP0 irq    */
    TMP_SLOT                     /* 2  current EL SP0 fiq    */
    TMP_SLOT                     /* 3  current EL SP0 serror */
    TMP_SLOT                     /* 4  current EL SPx sync   <- SMC trap */
    TMP_SLOT                     /* 5  current EL SPx irq    */
    TMP_SLOT                     /* 6  current EL SPx fiq    */
    TMP_SLOT                     /* 7  current EL SPx serror */
    TMP_SLOT                     /* 8  lower EL a64 sync     */
    TMP_SLOT                     /* 9  lower EL a64 irq      */
    TMP_SLOT                     /* 10 lower EL a64 fiq      */
    TMP_SLOT                     /* 11 lower EL a64 serror   */
    TMP_SLOT                     /* 12 lower EL a32 sync     */
    TMP_SLOT                     /* 13 lower EL a32 irq      */
    TMP_SLOT                     /* 14 lower EL a32 fiq      */
    TMP_SLOT                     /* 15 lower EL a32 serror   */

.p2align 2
smp_catch:
    /* The SMC trapped: report the sentinel and resume at smp_after_smc. */
    movn x0, #0                  /* x0 = 0xFFFF_FFFF_FFFF_FFFF */
    adrp x9, smp_after_smc
    add  x9, x9, :lo12:smp_after_smc
    msr  elr_el1, x9
    eret

/* Physical (low LMA) address of the park entry: _start parks any non-boot core
   in a wfe loop, so even a would-be-started secondary sits harmlessly. */
.section .rodata
.p2align 3
.global SMP_PARK_ENTRY_PA
SMP_PARK_ENTRY_PA:
    .dword _start
