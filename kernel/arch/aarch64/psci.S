/*
 * ARM64 PSCI: a guarded firmware call over either conduit.
 *
 * **Always compiled, not behind the `smp` feature.** PSCI is the platform's
 * firmware interface, and two things every build needs come through it: how many
 * CPUs this machine has (`AFFINITY_INFO`, which the inventory enumerates with) and
 * how to start one (`CPU_ON`, which only an SMP build uses). Which CPUs exist is a
 * property of the hardware, not of a build configuration - the same reasoning that
 * keeps `mpidr_aff0` ungated - and a kernel that reported one CPU because it was
 * compiled without SMP would be stating something false about the machine. It used
 * to live entirely inside the SMP-only file, which is exactly why the ARM64
 * inventory said "1 CPU" on a four-core boot (docs/SMP.md, docs/SUBSTRATE.md S5).
 *
 * ## Why the conduit is probed, not assumed
 *
 * The kernel runs at EL1 with no EL2/EL3 (QEMU `virt`, secure=off,
 * virtualization=off), and this port originally issued PSCI as `smc #0`. With no
 * EL3 to service it that SMC is UNDEFINED at EL1 and traps straight back, which
 * is what the earlier phase observed and recorded as "PSCI is unusable here".
 *
 * That conclusion was about the *instruction*, not about PSCI. QEMU's `virt`
 * machine implements PSCI **itself**, and it picks the conduit from the machine
 * configuration: `hvc` for the plain machine, `smc` when `virtualization=on`
 * gives the guest an EL2 of its own (which would want HVC for itself). So the
 * default configuration - the one this tree boots - answers PSCI on **HVC**, the
 * one conduit the port never tried.
 *
 * `psci_call_guarded` therefore takes the conduit as an argument, and Rust
 * *probes* with PSCI_VERSION over each in turn, using whichever actually answers
 * (docs/ENGINEERING.md 1). Both instructions stay guarded: a temporary exception
 * vector is installed and interrupts masked around the call, so a conduit that
 * traps is **observed** (the helper returns a sentinel) instead of reaching the
 * kernel's fatal sync handler and killing the primary.
 *
 *   x0 = conduit (0 = HVC, 1 = SMC), x1 = function id, x2..x4 = args
 *   returns x0 = PSCI return value, or 0xFFFF_FFFF_FFFF_FFFF if it trapped.
 */

.section .text
.p2align 2
.global psci_call_guarded
psci_call_guarded:
    /* Save the callee-saved regs we use + lr (16-byte aligned frame). */
    stp x19, x20, [sp, #-48]!
    stp x21, x22, [sp, #16]
    stp x23, x30, [sp, #32]

    mov  x19, x0                 /* conduit selector */
    mrs  x22, vbar_el1           /* save real vectors */
    mrs  x23, daif               /* save interrupt mask */

    /* Mask IRQ/FIQ/SError so only the call's sync trap can reach the temp
       vector. */
    msr  daifset, #0xf

    /* Install the temporary vector table. */
    adrp x9, smp_tmp_vectors
    add  x9, x9, :lo12:smp_tmp_vectors
    msr  vbar_el1, x9
    isb

    /* Shift the PSCI arguments down into x0..x3. */
    mov  x0, x1
    mov  x1, x2
    mov  x2, x3
    mov  x3, x4
    cbnz x19, .Lsmc_conduit
    hvc  #0
    b    smp_after_call
.Lsmc_conduit:
    smc  #0
    /* Normal return lands here with x0 = the PSCI return value; the trap path
       redirects here too (via elr) with x0 = the sentinel. */
.global smp_after_call
smp_after_call:
    msr  vbar_el1, x22           /* restore real vectors */
    msr  daif, x23               /* restore interrupt mask */
    isb
    ldp x21, x22, [sp, #16]
    ldp x23, x30, [sp, #32]
    ldp x19, x20, [sp], #48
    ret

/* Temporary vectors: 2 KiB aligned, 16 slots of 128 bytes. Every slot redirects
   to smp_catch; only the current-EL SPx synchronous slot (the HVC/SMC trap) is
   expected to fire (interrupts are masked around the call). */
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
    TMP_SLOT                     /* 4  current EL SPx sync   <- the trap */
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
    /* The conduit trapped: report the sentinel and resume at smp_after_call. */
    movn x0, #0                  /* x0 = 0xFFFF_FFFF_FFFF_FFFF */
    adrp x9, smp_after_call
    add  x9, x9, :lo12:smp_after_call
    msr  elr_el1, x9
    eret

