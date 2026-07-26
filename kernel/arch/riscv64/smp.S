#
# RISC-V 64 SMP secondary-core bring-up (docs/SMP.md, task #27).
#
# A secondary hart is started by the primary through the SBI HSM extension
# (hart_start). SBI enters `secondary_entry` in S-mode with the MMU OFF:
#   a0 = hartid, a1 = opaque = the satp value to load (the SAME kernel root the
#   primary already runs on, so the secondary shares the kernel address space
#   and page tables).
#
# The secondary loads satp (paging on). The kernel working root keeps a low
# identity of kernel RAM (l2[2], built by boot.S for the primary's own turn-on),
# so the instruction right after `csrw satp`, still executing at this low PC,
# stays mapped - exactly as the primary boot does. It then jumps to the high-half
# continuation via an absolute 64-bit pointer word (medany cannot reach a high VA
# from the low PC), sets gp + a dedicated secondary stack, and calls
# rv_secondary_main(hartid).
#
# secondary_entry lives in .boot.text (linked identity low, VMA == LMA), so its
# link address IS the physical address SBI hart_start needs. High kernel code
# cannot form that low address with medany (out of +-2 GiB PC-relative reach), so
# its physical value is published in SECONDARY_ENTRY_PA - a high .rodata word
# holding an absolute (R_RISCV_64) relocation the primary reads to get the entry.

.section .boot.text, "ax"
.p2align 2
.global secondary_entry
secondary_entry:
.option push
.option norelax
    csrw satp, a1               # a1 = kernel satp; paging on (shared root)
    sfence.vma
    # Jump to the high half. la reaches the low pointer word PC-relatively; the
    # word holds secondary_high's absolute high VA (R_RISCV_64 reloc).
    la   t0, secondary_high_ptr
    ld   t0, 0(t0)
    jr   t0
.option pop

.p2align 3
secondary_high_ptr:
    .dword secondary_high

# --- high half: runs at KERNEL_VA_BASE + PA, kernel mapped via l2[258] ---
.section .text
.p2align 2
secondary_high:
.option push
.option norelax
    la   gp, __global_pointer$
.option pop
    la   sp, secondary_stack_top   # dedicated secondary stack (high BSS)
    # a0 still = hartid (untouched since SBI entry).
    call rv_secondary_main
1:
    wfi
    j 1b

# Physical address (low LMA) of secondary_entry, published for the high-half
# primary. An absolute reloc: the value is secondary_entry's link address, which
# for .boot.text equals its physical load address - what SBI hart_start wants.
.section .rodata
.p2align 3
.global SECONDARY_ENTRY_PA
SECONDARY_ENTRY_PA:
    .dword secondary_entry

# Secondary stack: a high VA in ordinary BSS (mapped via l2[258]).
.section .bss
.p2align 4
secondary_stack:
    .space 16 * 1024
secondary_stack_top:
