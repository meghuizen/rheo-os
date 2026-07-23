/*
 * RISC-V 64 boot stub. OpenSBI (QEMU's default -bios) starts the boot hart
 * in S-mode at _start with a0 = hart id, a1 = device tree pointer
 * (both currently unused; secondaries wait in SBI HSM). Sets up gp and a
 * stack, clears the BSS, and calls kernel_main.
 */

.section .boot.text, "ax"
.global _start
_start:
.option push
.option norelax
    la gp, __global_pointer$
.option pop

    la sp, boot_stack_top

    /* Clear the BSS. */
    la t0, __bss_start
    la t1, __bss_end
1:
    bgeu t0, t1, 2f
    sd zero, (t0)
    addi t0, t0, 8
    j 1b

2:
    call kernel_main
3:
    wfi
    j 3b

.section .boot.bss, "aw", @nobits
.p2align 4
boot_stack:
    .space 64 * 1024
boot_stack_top:
