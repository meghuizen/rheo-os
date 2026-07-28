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
    # Load this secondary's stack from `secondary_sp`, which the primary set to
    # the right stack top *before* releasing this hart (start-all is sequential:
    # the primary waits for hart N online before setting the pointer and starting
    # hart N+1, so there is no race on this shared word - docs/SMP.md 10).
    la   t0, secondary_sp
    ld   sp, 0(t0)
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

# The stack pointer the next secondary should load, set by the primary before
# each hart_start (sequential bring-up - see secondary_high). A high .bss word,
# mapped via l2[258] into the shared kernel root.
.section .bss
.p2align 3
.global secondary_sp
secondary_sp:
    .space 8

# Two secondary stacks: high VAs in ordinary BSS (mapped via l2[258]). Two, so a
# second secondary brought up while the first is still parked on its own stack
# does not share it (docs/SMP.md 10, the start-all slice). Each is exported so the
# primary can hand its top to `secondary_sp`.
.section .bss
.p2align 4
secondary_stack:
    .space 16 * 1024
.global secondary_stack_top
secondary_stack_top:
secondary_stack2:
    .space 16 * 1024
.global secondary_stack2_top
secondary_stack2_top:
secondary_stack3:
    .space 16 * 1024
.global secondary_stack3_top
secondary_stack3_top:
