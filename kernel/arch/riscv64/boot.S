/*
 * RISC-V 64 higher-half boot trampoline (docs/MEMORY.md).
 *
 * OpenSBI (QEMU's default -bios) enters _start in S-mode at physical
 * 0x8020_0000 with the MMU OFF (a0 = hart id, a1 = device tree pointer). This
 * code runs identity-mapped low (VMA == LMA in link/riscv64.ld). It builds an
 * initial Sv39 root:
 *   l2[2]   = low identity of kernel RAM (2-3 GiB) so the instruction right
 *             after `csrw satp`, still executing at its low PC, stays mapped;
 *   l2[256] = MMIO (0-1 GiB) at the high linear base, supervisor R|W;
 *   l2[258] = kernel RAM (2-3 GiB) at the high linear base, supervisor R|W|X.
 * It writes satp (Sv39), fences, and jumps to the high-half continuation via an
 * absolute 64-bit pointer word (medany cannot reach a high VA from the low PC).
 * The continuation sets gp/sp high, clears the BSS, records the boot root PA
 * (paging_activate_kernel reuses it as the kernel working root), stashes
 * hartid/dtb, and calls kernel_main - all before any Rust runs high.
 *
 * The boot root doubles as the kernel working root; it maps the low half only
 * transiently (l2[2]) for the turn-on. A cell root (paging_new_root) drops the
 * low identity, so the whole low half is free for a stock Linux ET_EXEC.
 */

# Sv39 leaf PTE flags (mirror kernel/src/arch/riscv64/paging.rs).
.equ PTE_KRAM, (0x80000 << 10) | 0xEF   # PPN(0x8000_0000) | V|R|W|X|G|A|D
.equ PTE_MMIO, 0xE7                      # PPN(0)          | V|R|W|G|A|D

.section .boot.text, "ax"
.global _start
_start:
.option push
.option norelax
    # Build the boot/kernel root in .boot.bss. satp is off, so addresses are
    # physical; the root is identity-reachable at its low PC-relative label.
    la   t0, boot_root
    mv   s1, t0                 # carry the root PA to the high continuation

    # Zero the 512-entry root table.
    mv   t2, t0
    li   t3, 512
1:
    sd   zero, 0(t2)
    addi t2, t2, 8
    addi t3, t3, -1
    bnez t3, 1b

    # l2[2] = low identity kernel RAM (byte offset 16, within the store range).
    li   t1, PTE_KRAM
    sd   t1, 16(t0)

    # l2[256] (byte 2048) and l2[258] (byte 2064) are beyond the 12-bit store
    # offset, so index them through a computed base.
    li   t4, 2048
    add  t2, t0, t4
    li   t1, PTE_MMIO
    sd   t1, 0(t2)              # entry 256 = high MMIO
    li   t1, PTE_KRAM
    sd   t1, 16(t2)            # entry 258 = high kernel RAM

    # satp = (Sv39 << 60) | (root >> 12); fence after the switch.
    srli t0, t0, 12
    li   t1, 8
    slli t1, t1, 60
    or   t0, t0, t1
    csrw satp, t0
    sfence.vma

    # Jump to the high half. la reaches the low pointer word PC-relatively; the
    # word holds high_start's absolute high VA (R_RISCV_64 reloc).
    la   t0, high_entry_ptr
    ld   t0, 0(t0)
    jr   t0
.option pop

.p2align 3
high_entry_ptr:
    .dword high_start

# --- high half: runs at KERNEL_VA_BASE + PA, kernel mapped via l2[258] ---
.section .text
.p2align 2
high_start:
.option push
.option norelax
    la   gp, __global_pointer$
.option pop
    la   sp, boot_stack_top

    # Clear the kernel BSS (high VAs).
    la   t0, __bss_start
    la   t1, __bss_end
2:
    bgeu t0, t1, 3f
    sd   zero, 0(t0)
    addi t0, t0, 8
    j    2b
3:
    # Record the boot root PA (paging_activate_kernel), then stash the boot
    # hart id (a0) and device-tree pointer (a1) for hardware discovery.
    la   t0, KERNEL_ROOT_PA
    sd   s1, 0(t0)
    la   t0, BOOT_HARTID
    sd   a0, 0(t0)
    la   t0, BOOT_DTB
    sd   a1, 0(t0)
    call kernel_main
4:
    wfi
    j 4b

# Early boot root: identity-mapped low so the trampoline (PC-relative) can
# build it with the MMU off.
.section .boot.bss, "aw", @nobits
.p2align 12
boot_root:
    .space 4096

# Kernel stack + boot slots: high VAs in ordinary BSS (mapped via l2[258]).
.section .bss
.p2align 4
boot_stack:
    .space 64 * 1024
boot_stack_top:

.global BOOT_HARTID
.global BOOT_DTB
.global KERNEL_ROOT_PA
.p2align 3
BOOT_HARTID:
    .space 8
BOOT_DTB:
    .space 8
KERNEL_ROOT_PA:
    .space 8
