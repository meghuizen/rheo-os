/*
 * ARM64 exception vectors + EL0 (user) trampoline.
 *
 * 16 slots of 128 bytes, 2 KiB aligned. Slot 4 (current EL SPx sync)
 * carries the kernel self-test's SVC. Slot 8 (lower EL AArch64 sync) is
 * the U-mode entry: SVC and faults from EL0. All other slots are fatal at
 * this stage.
 *
 * The current cell's TrapFrame pointer lives in TPIDR_EL1 while EL0 runs;
 * the trampoline saves the full register state into it, switches to the
 * cell's kernel stack, and calls aarch64_user_trap, which returns the
 * frame to resume (possibly a peer cell's, for a cross-cell switch).
 */

// TrapFrame offsets (see aarch64/mod.rs).
.equ TF_SP,    248
.equ TF_ELR,   256
.equ TF_SPSR,  264
.equ TF_KSP,   272
.equ TF_TPIDR, 280

.section .text

// How many per-core saved kernel contexts KERNEL_CTX holds. A power of two, so the
// index is a mask: an MPIDR affinity outside the range aliases slot 0 rather than
// scribbling past the array.
.equ KERNEL_CTX_SLOTS, 8

// KCTX_PTR dst, tmp: point \dst at this core's KERNEL_CTX slot.
//
// A cell running at EL0 must unwind back into **its own core's** kernel stack, so
// the saved kernel context cannot be one global (docs/SMP.md 10.0). The index comes
// straight from the hardware - MPIDR_EL1 affinity 0, the same value `cpu_index()`
// reads - so no register has to be kept live across EL0 to carry it. With one core
// this is affinity 0 and the previous single-slot behaviour exactly.
.macro KCTX_PTR dst, tmp
    adrp \dst, KERNEL_CTX
    add  \dst, \dst, :lo12:KERNEL_CTX
    mrs  \tmp, mpidr_el1
    and  \tmp, \tmp, #(KERNEL_CTX_SLOTS - 1)
    add  \dst, \dst, \tmp, lsl #3
.endm

.macro FATAL_SLOT idx
.p2align 7
    mov x0, #\idx
    b fatal_trap
.endm

.p2align 11
.global vector_table
vector_table:
    FATAL_SLOT 0                // current EL SP0: synchronous
    FATAL_SLOT 1                // current EL SP0: IRQ
    FATAL_SLOT 2                // current EL SP0: FIQ
    FATAL_SLOT 3                // current EL SP0: SError
.p2align 7
    b sync_current_spx          // current EL SPx: synchronous (kernel SVC)
.p2align 7
    b irq_current_spx           // current EL SPx: IRQ (GICv3, docs/LIBRHEO.md D/F)
    FATAL_SLOT 6                // current EL SPx: FIQ
    FATAL_SLOT 7                // current EL SPx: SError
.p2align 7
    b el0_sync                  // lower EL AArch64: synchronous (user)
// Each slot is 128 bytes and only FATAL_SLOT aligns itself, so a bare branch needs
// its own `.p2align 7`. Without it this instruction sits four bytes into slot 8 and
// the EL0 IRQ lands in slot *10*'s handler - which is exactly what happened first
// time, reported as "unexpected vector slot 10" with a stale ESR from the previous
// SVC (docs/ENGINEERING.md 11: the diagnostic named a slot nothing had branched to).
.p2align 7
    b el0_irq                   // lower EL AArch64: IRQ (timer preemption)
    FATAL_SLOT 10               // lower EL AArch64: FIQ
    FATAL_SLOT 11               // lower EL AArch64: SError
    FATAL_SLOT 12               // lower EL AArch32: synchronous
    FATAL_SLOT 13               // lower EL AArch32: IRQ
    FATAL_SLOT 14               // lower EL AArch32: FIQ
    FATAL_SLOT 15               // lower EL AArch32: SError

// Kernel-mode SVC (the boot self-test doorbell). Saves caller-saved regs,
// hands (esr, elr) to Rust, restores, eret. Rust returns only for SVC.
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

// Current-EL SPx IRQ (docs/LIBRHEO.md Phase D/F): a GICv3 interrupt taken while
// the kernel idles at `wfi` (EL1, SP_EL1). Save the caller-saved registers (which
// also 16-aligns the frame), ack+service+EOI in Rust, restore, eret back to the
// instruction after `wfi`. Mirrors sync_current_spx's save set (x0-x18, x29, x30).
irq_current_spx:
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
    bl aarch64_irq_handler
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

// EL0 IRQ entry (docs/SUBSTRATE.md pillar 3, timer preemption). A GICv3 interrupt
// taken while a **cell** is running at EL0. This slot used to be FATAL_SLOT 9,
// because cells ran with IRQ masked in their SPSR and so could not be interrupted at
// all - which is precisely why nothing could take the CPU away from a compute-bound
// cell.
//
// The frame capture is el0_sync's, register for register, and the exit is the shared
// el0_resume. It has to be the full frame and not irq_current_spx's caller-saved
// subset: an interrupted cell may be resumed *later*, after another cell has run, so
// every callee-saved register has to be in the frame rather than on a kernel stack
// that will have been reused. Returning the same frame pointer resumes the cell at
// exactly the interrupted instruction, which is the case where no preemption is due.
el0_irq:
    msr tpidrro_el0, x0
    mrs x0, tpidr_el1           // x0 = &frame
    stp x1, x2, [x0, #8]
    stp x3, x4, [x0, #24]
    stp x5, x6, [x0, #40]
    stp x7, x8, [x0, #56]
    stp x9, x10, [x0, #72]
    stp x11, x12, [x0, #88]
    stp x13, x14, [x0, #104]
    stp x15, x16, [x0, #120]
    stp x17, x18, [x0, #136]
    stp x19, x20, [x0, #152]
    stp x21, x22, [x0, #168]
    stp x23, x24, [x0, #184]
    stp x25, x26, [x0, #200]
    stp x27, x28, [x0, #216]
    stp x29, x30, [x0, #232]
    mrs x1, tpidrro_el0         // recover original x0
    str x1, [x0, #0]
    mrs x1, sp_el0
    str x1, [x0, #TF_SP]
    mrs x1, elr_el1
    str x1, [x0, #TF_ELR]
    mrs x1, spsr_el1
    str x1, [x0, #TF_SPSR]
    mrs x1, tpidr_el0
    str x1, [x0, #TF_TPIDR]

    ldr x1, [x0, #TF_KSP]
    mov sp, x1
    bl aarch64_user_irq         // x0 = &frame -> frame* to resume, in x0
    b el0_resume

// EL0 synchronous entry. TPIDR_EL1 = &TrapFrame. Stash x0 in TPIDRRO_EL0
// (writable at EL1) so we have a scratch, save the full frame, switch to
// the kernel stack, and dispatch.
el0_sync:
    msr tpidrro_el0, x0
    mrs x0, tpidr_el1           // x0 = &frame
    stp x1, x2, [x0, #8]
    stp x3, x4, [x0, #24]
    stp x5, x6, [x0, #40]
    stp x7, x8, [x0, #56]
    stp x9, x10, [x0, #72]
    stp x11, x12, [x0, #88]
    stp x13, x14, [x0, #104]
    stp x15, x16, [x0, #120]
    stp x17, x18, [x0, #136]
    stp x19, x20, [x0, #152]
    stp x21, x22, [x0, #168]
    stp x23, x24, [x0, #184]
    stp x25, x26, [x0, #200]
    stp x27, x28, [x0, #216]
    stp x29, x30, [x0, #232]
    mrs x1, tpidrro_el0         // recover original x0
    str x1, [x0, #0]
    mrs x1, sp_el0
    str x1, [x0, #TF_SP]
    mrs x1, elr_el1
    str x1, [x0, #TF_ELR]
    mrs x1, spsr_el1
    str x1, [x0, #TF_SPSR]
    mrs x1, tpidr_el0           // per-thread TLS pointer
    str x1, [x0, #TF_TPIDR]

    ldr x1, [x0, #TF_KSP]
    mov sp, x1
    mrs x0, esr_el1
    mrs x1, far_el1
    mrs x2, tpidr_el1
    bl aarch64_user_trap        // -> frame* to resume, in x0
    // fall through

.global el0_resume
el0_resume:                     // x0 = &TrapFrame
    msr tpidr_el1, x0
    ldr x1, [x0, #TF_ELR]
    msr elr_el1, x1
    ldr x1, [x0, #TF_SPSR]
    msr spsr_el1, x1
    ldr x1, [x0, #TF_TPIDR]     // per-thread TLS pointer
    msr tpidr_el0, x1
    ldr x1, [x0, #TF_SP]
    msr sp_el0, x1
    ldp x2, x3, [x0, #16]
    ldp x4, x5, [x0, #32]
    ldp x6, x7, [x0, #48]
    ldp x8, x9, [x0, #64]
    ldp x10, x11, [x0, #80]
    ldp x12, x13, [x0, #96]
    ldp x14, x15, [x0, #112]
    ldp x16, x17, [x0, #128]
    ldp x18, x19, [x0, #144]
    ldp x20, x21, [x0, #160]
    ldp x22, x23, [x0, #176]
    ldp x24, x25, [x0, #192]
    ldp x26, x27, [x0, #208]
    ldp x28, x29, [x0, #224]
    ldr x30, [x0, #240]
    ldr x1, [x0, #8]            // x1
    ldr x0, [x0, #0]            // x0 last
    eret

// enter_user_first(x0 = &TrapFrame): save callee-saved state for
// return_to_kernel, then drop to EL0 via el0_resume.
.global enter_user_first
enter_user_first:
    sub sp, sp, #(16 * 6)
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    KCTX_PTR x1, x3
    mov x2, sp
    str x2, [x1]
    b el0_resume

// return_to_kernel_asm(): restore enter_user_first's state and return.
.global return_to_kernel_asm
return_to_kernel_asm:
    KCTX_PTR x1, x3
    ldr x2, [x1]
    mov sp, x2
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    add sp, sp, #(16 * 6)
    ret

.section .bss
.p2align 3
// One saved kernel context **per core** (see KCTX_PTR above).
KERNEL_CTX:
    .space 8 * KERNEL_CTX_SLOTS
