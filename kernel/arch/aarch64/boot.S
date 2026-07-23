/*
 * ARM64 boot stub. QEMU's virt machine loads the kernel ELF and jumps to
 * _start on the boot CPU (secondaries wait in PSCI). Sets up a stack,
 * clears the BSS, and calls kernel_main. MMU stays off for now.
 */

.section .boot.text, "ax"
.global _start
_start:
    /* Park any CPU that is not core 0 (safety net; QEMU should only
       start core 0). */
    mrs x0, mpidr_el1
    and x0, x0, #0xFF
    cbz x0, 2f
1:
    wfe
    b 1b

2:
    /* Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11): the
       aarch64-unknown-none target compiles with NEON on, so the
       compiler may vectorise ordinary copies. */
    mov x0, #(0x3 << 20)
    msr cpacr_el1, x0
    isb

    ldr x0, =boot_stack_top
    mov sp, x0

    /* Clear the BSS. */
    ldr x0, =__bss_start
    ldr x1, =__bss_end
3:
    cmp x0, x1
    b.hs 4f
    str xzr, [x0], #8
    b 3b

4:
    bl kernel_main
5:
    wfe
    b 5b

.section .boot.bss, "aw", @nobits
.p2align 4
boot_stack:
    .space 64 * 1024
boot_stack_top:
