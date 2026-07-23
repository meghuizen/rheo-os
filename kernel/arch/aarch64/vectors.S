/*
 * ARM64 exception vector table: 16 slots of 128 bytes, 2 KiB aligned.
 * Only "current EL with SPx, synchronous" (slot 4) has a real handler -
 * it carries SVC, the doorbell stand-in. Every other slot is fatal at
 * this stage and reports through Rust.
 */

.section .text

.macro FATAL_SLOT idx
.p2align 7
    mov x0, #\idx
    b fatal_trap
.endm

.p2align 11
.global vector_table
vector_table:
    FATAL_SLOT 0            /* current EL, SP0: synchronous */
    FATAL_SLOT 1            /* current EL, SP0: IRQ */
    FATAL_SLOT 2            /* current EL, SP0: FIQ */
    FATAL_SLOT 3            /* current EL, SP0: SError */
.p2align 7
    b sync_current_spx      /* current EL, SPx: synchronous */
    FATAL_SLOT 5            /* current EL, SPx: IRQ */
    FATAL_SLOT 6            /* current EL, SPx: FIQ */
    FATAL_SLOT 7            /* current EL, SPx: SError */
    FATAL_SLOT 8            /* lower EL, AArch64: synchronous */
    FATAL_SLOT 9            /* lower EL, AArch64: IRQ */
    FATAL_SLOT 10           /* lower EL, AArch64: FIQ */
    FATAL_SLOT 11           /* lower EL, AArch64: SError */
    FATAL_SLOT 12           /* lower EL, AArch32: synchronous */
    FATAL_SLOT 13           /* lower EL, AArch32: IRQ */
    FATAL_SLOT 14           /* lower EL, AArch32: FIQ */
    FATAL_SLOT 15           /* lower EL, AArch32: SError */

/* Save the AAPCS caller-saved registers, hand (esr, elr) to Rust, restore,
   and eret. Rust only returns for SVC (the doorbell); everything else
   exits QEMU from Rust. */
sync_current_spx:
    sub sp, sp, #176
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x29, [sp, #144]
    str x30, [sp, #160]

    mrs x0, esr_el1
    mrs x1, elr_el1
    bl aarch64_sync_handler

    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x29, [sp, #144]
    ldr x30, [sp, #160]
    add sp, sp, #176
    eret

fatal_trap:
    mrs x1, esr_el1
    mrs x2, elr_el1
    b aarch64_fatal_handler
