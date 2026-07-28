/*
 * ARM64 SMP: the secondary-core entry trampoline (docs/SMP.md 7, task #27).
 *
 * The guarded PSCI call this uses to start a core lives in `psci.S`, which is
 * always compiled - see its header for why enumeration cannot be behind the `smp`
 * feature.
 */

.equ AP_STACK_SHIFT, 14          /* 16 KiB per secondary stack */
.equ AP_STACK_SLOTS, 8           /* power of two: the index is a mask */

/*
 * ## The secondary entry
 *
 * PSCI CPU_ON enters the secondary at EL1 with the **MMU off**, at the physical
 * address the primary passed, with x0 = the context id. This code lives in
 * `.boot.text`, which is linked identity-low (VMA == LMA in link/aarch64.ld), so
 * the PA the primary hands PSCI is this label's own address and `adrp` works
 * because the PC *is* the link address - no copy and no relocation needed (the
 * x86 AP trampoline has to be copied only because a SIPI vector cannot name an
 * address above 1 MiB).
 *
 * It adopts the primary's translation regime **verbatim** - MAIR, TCR and SCTLR
 * as the primary is running them, published in AP_SYSREGS - rather than
 * re-deriving the bits, so the two cores cannot diverge (the same reasoning as
 * the x86 trampoline's control registers; a divergence there cost a triple
 * fault). TTBR0/TTBR1 are the boot tables by symbol: TTBR1 is the kernel's high
 * linear map, and TTBR0 is the low identity map the primary's own MMU turn-on
 * built, which is what keeps this low PC mapped across `msr sctlr_el1`.
 */
.section .boot.text, "ax"
.p2align 2
.global aarch64_secondary_entry
aarch64_secondary_entry:
    /* Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11): the target compiles with
       NEON on, so the compiler may vectorise ordinary copies. */
    mov x0, #(0x3 << 20)
    msr cpacr_el1, x0

    adrp x1, AP_SYSREGS
    add  x1, x1, :lo12:AP_SYSREGS
    ldp  x2, x3, [x1, #0]            /* mair_el1, tcr_el1 */
    ldr  x6, [x1, #16]               /* sctlr_el1 */
    msr  mair_el1, x2
    msr  tcr_el1, x3

    adrp x4, boot_l0_low
    add  x4, x4, :lo12:boot_l0_low
    msr  ttbr0_el1, x4               /* low identity (keeps this PC mapped) */
    adrp x5, boot_l0_high
    add  x5, x5, :lo12:boot_l0_high
    msr  ttbr1_el1, x5               /* kernel/MMIO high linear map */
    isb

    msr  sctlr_el1, x6               /* MMU + caches on, as the primary has */
    isb

    /* Jump to the high-half continuation (absolute VA; the kernel is linked
       high and a low PC cannot reach it PC-relatively). */
    movz x0, #:abs_g0_nc:aarch64_secondary_high
    movk x0, #:abs_g1_nc:aarch64_secondary_high
    movk x0, #:abs_g2_nc:aarch64_secondary_high
    movk x0, #:abs_g3:aarch64_secondary_high
    br   x0

.section .text
.p2align 2
aarch64_secondary_high:
    /* One stack per core, indexed by this core's own MPIDR affinity 0 - the same
       value `cpu_index()` and the KERNEL_CTX slot use - so every secondary the
       primary starts has its own; a shared stack would have the second core to
       arrive overwrite the first's frames (docs/SMP.md 10.0). Masked, so an
       affinity outside the array aliases slot 0; sp is the *top* of slot n. */
    adrp x0, ap_stacks
    add  x0, x0, :lo12:ap_stacks
    mrs  x1, mpidr_el1
    and  x1, x1, #(AP_STACK_SLOTS - 1)
    add  x1, x1, #1
    add  x0, x0, x1, lsl #AP_STACK_SHIFT
    mov  sp, x0
    /* Containment: the kernel's real vectors, so a fault on this core reaches
       the kernel handler instead of looping. */
    adrp x0, vector_table
    add  x0, x0, :lo12:vector_table
    msr  vbar_el1, x0
    isb
    bl   aarch64_secondary_main
1:
    wfe
    b 1b

/* The system-register values the primary publishes for the secondary to adopt.
   Identity-low (`.boot.bss`, VMA == LMA) because the secondary reads them with
   the MMU off, so the symbol address is the physical address - the same
   convention BOOT_DTB uses. Written by smp_start_secondary before CPU_ON. */
.section .boot.bss, "aw", @nobits
.p2align 3
.global AP_SYSREGS
AP_SYSREGS:
    .space 8                         /* +0  mair_el1  */
    .space 8                         /* +8  tcr_el1   */
    .space 8                         /* +16 sctlr_el1 */

/* Physical (low LMA) address of the secondary entry, published so Rust can pass
   it to PSCI CPU_ON. A `.dword` of a low symbol is an absolute relocation, so it
   is the physical address the MMU-off secondary needs. */
.section .rodata
.p2align 3
.global SMP_SECONDARY_ENTRY_PA
SMP_SECONDARY_ENTRY_PA:
    .dword aarch64_secondary_entry

/* The secondaries' kernel stacks: high VAs in ordinary BSS (mapped via TTBR1),
   like the primary's boot_stack - one per core. */
.section .bss
.p2align 4
ap_stacks:
    .space (1 << AP_STACK_SHIFT) * AP_STACK_SLOTS
