/*
 * ARM64 higher-half boot trampoline (docs/MEMORY.md).
 *
 * QEMU's virt machine loads the kernel ELF by physical address and enters
 * _start on the boot CPU with the MMU off, so this code runs identity-mapped
 * low (VMA == LMA in link/aarch64.ld). It builds the initial page tables -
 * TTBR1_EL1 for the kernel/MMIO high linear map, TTBR0_EL1 as a temporary low
 * identity map so the instruction right after `msr sctlr` is still mapped -
 * enables the MMU, then branches to the high-half continuation which sets the
 * kernel stack + vectors and calls kernel_main.
 *
 * Only three L0/L1 tables are needed: a shared L1 (device gigabyte block +
 * kernel-RAM gigabyte block) reached by both roots, since the low 48 bits of a
 * high VA equal its physical address, so the walk indexes identically.
 */

/* Descriptor bits (mirror kernel/src/arch/aarch64/paging.rs). */
.equ D_VALID,  (1 << 0)
.equ D_TABLE,  (1 << 1)
.equ D_ATTR_N, (1 << 2)          /* MAIR index 1 = Normal WB */
.equ D_SH_IN,  (0b11 << 8)       /* inner shareable */
.equ D_AF,     (1 << 10)
.equ D_PXN,    (1 << 53)
.equ D_UXN,    (1 << 54)

.section .boot.text, "ax"
.global _start
_start:
    /* QEMU passes the DTB pointer in x0; preserve it (callee-saved x19). */
    mov x19, x0

    /* Park any CPU that is not core 0. */
    mrs x0, mpidr_el1
    and x0, x0, #0xFF
    cbz x0, 2f
1:
    wfe
    b 1b

2:
    /* Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11): the target compiles
       with NEON on, so the compiler may vectorise ordinary copies. */
    mov x0, #(0x3 << 20)
    msr cpacr_el1, x0

    /* --- build the shared L1 table (identity within its gigabyte) --- */
    adrp x1, boot_l1_shared
    add  x1, x1, :lo12:boot_l1_shared

    /* L1[0]: device gigabyte block at PA 0 (Device, EL1-only, UXN|PXN). */
    movz x2, #(D_VALID | D_AF)
    movk x2, #0x0060, lsl #48        /* UXN | PXN (bits 54,53) */
    str  x2, [x1, #0]

    /* L1[1]: kernel-RAM gigabyte block at PA 0x4000_0000 (Normal WB, inner
       shareable, EL1 RW + executable, UXN). */
    movz x3, #(D_VALID | D_AF | D_ATTR_N | D_SH_IN)   /* 0x0705 */
    movk x3, #0x4000, lsl #16        /* base 0x4000_0000 */
    movk x3, #0x0040, lsl #48        /* UXN (bit 54) */
    str  x3, [x1, #8]

    /* L0[0] of both roots -> shared L1 (table descriptor). */
    orr  x4, x1, #(D_VALID | D_TABLE)
    adrp x5, boot_l0_low
    add  x5, x5, :lo12:boot_l0_low
    str  x4, [x5, #0]
    adrp x6, boot_l0_high
    add  x6, x6, :lo12:boot_l0_high
    str  x4, [x6, #0]

    /* --- system registers --- */
    /* MAIR: attr0 = Device-nGnRnE (0x00), attr1 = Normal WB (0xFF). */
    movz x0, #0xFF00
    msr  mair_el1, x0

    /* TCR_EL1: T0SZ=16, T1SZ=16, 4 KiB granules, inner-shareable WB-WA,
       IPS=40-bit, EPD1 clear (TTBR1 walk enabled). */
    movz x0, #0x1710
    movk x0, #0x9710, lsl #16
    movk x0, #0x0002, lsl #32
    msr  tcr_el1, x0

    msr  ttbr0_el1, x5               /* low identity (temporary) */
    msr  ttbr1_el1, x6               /* kernel/MMIO high linear map */
    isb

    /* Enable MMU + caches; SPAN=1 keeps PSTATE.PAN clear on exception entry. */
    mrs  x0, sctlr_el1
    orr  x0, x0, #(1 << 0)           /* M */
    orr  x0, x0, #(1 << 2)           /* C */
    orr  x0, x0, #(1 << 12)          /* I */
    orr  x0, x0, #(1 << 23)          /* SPAN */
    msr  sctlr_el1, x0
    isb

    /* Jump to the high-half continuation (absolute VA). */
    movz x0, #:abs_g0_nc:high_start
    movk x0, #:abs_g1_nc:high_start
    movk x0, #:abs_g2_nc:high_start
    movk x0, #:abs_g3:high_start
    br   x0

/* --- high half: runs at KERNEL_VA_BASE | PA, kernel mapped via TTBR1 --- */
.section .text
high_start:
    adrp x0, boot_stack_top
    add  x0, x0, :lo12:boot_stack_top
    mov  sp, x0

    adrp x0, vector_table
    add  x0, x0, :lo12:vector_table
    msr  vbar_el1, x0

    /* CNTKCTL: allow EL0 to read the virtual + physical counters. */
    mov  x0, #0b11
    msr  cntkctl_el1, x0
    isb

    /* Clear the BSS (high VAs, mapped via TTBR1). */
    adrp x0, __bss_start
    add  x0, x0, :lo12:__bss_start
    adrp x1, __bss_end
    add  x1, x1, :lo12:__bss_end
3:
    cmp  x0, x1
    b.hs 4f
    str  xzr, [x0], #8
    b    3b

4:
    /* Stash the DTB pointer for the hardware-discovery layer. */
    adrp x0, BOOT_DTB
    add  x0, x0, :lo12:BOOT_DTB
    str  x19, [x0]
    bl   kernel_main
5:
    wfe
    b 5b

/* Early page tables: identity-mapped low so the trampoline (adrp-reachable)
   can build them with the MMU off. Only the written entries are ever walked,
   so they need no pre-zeroing. */
.section .boot.bss, "aw", @nobits
.p2align 12
.global boot_l0_low
boot_l0_low:
    .space 4096
boot_l0_high:
    .space 4096
boot_l1_shared:
    .space 4096

/* Kernel stack + DTB slot: high VAs in ordinary BSS (mapped via TTBR1). */
.section .bss
.p2align 4
boot_stack:
    .space 64 * 1024
boot_stack_top:

.global BOOT_DTB
.p2align 3
BOOT_DTB:
    .space 8
