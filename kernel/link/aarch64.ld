/* ARM64 higher-half kernel (docs/MEMORY.md).

   QEMU's virt machine loads the kernel ELF by physical address (LMA) and
   enters `_start` with the MMU off, so the boot trampoline must be
   identity-mapped low. Once it enables the MMU it runs the kernel proper at
   high (TTBR1_EL1) virtual addresses, leaving the entire low half (TTBR0_EL1)
   free for user programs - a stock Linux ET_EXEC at 0x400000 no longer
   collides with a supervisor mapping.

   Physical layout (LMA), all inside QEMU virt RAM at 0x4000_0000:
     0x4008_0000  .boot.text / .boot.bss   identity low (VMA == LMA)
     0x4020_0000  .user.*                  identity low, one 2 MiB slot
     0x4040_0000  .text/.rodata/.data/.bss high VMA (KERNEL_VA_BASE | LMA)

   KERNEL_VA_BASE is the linear-map offset: phys_to_virt(pa) = pa | BASE,
   virt_to_phys(va) = va & ~BASE (kernel/src/arch/aarch64/mod.rs). The kernel's
   high VMA therefore equals phys_to_virt(its LMA), so the boot-time TTBR1
   linear map and the link addresses agree. */

ENTRY(_start)

KERNEL_VA_BASE = 0xFFFF000000000000;

SECTIONS
{
    /* Boot trampoline + its early page tables: identity-mapped low, entered
       by QEMU with the MMU off (VMA == LMA). */
    . = 0x40080000;
    .boot.text : { KEEP(*(.boot.text)) }
    . = ALIGN(4K);
    .boot.bss (NOLOAD) : { *(.boot.bss) }

    /* User image: code and static data that runs in U-mode. Identity-mapped
       low so a cell maps it in TTBR0 at the same VA the kernel writes its
       backing store through. One 2 MiB-aligned, 2 MiB window (a single
       level-2 slot); per-cell roots differ only in that slot. Mapped with the
       U bit; W^X enforced by section. */
    . = ALIGN(2M);
    __user_start = .;
    .user.text   : { *(.user.text .user.text.*) }
    . = ALIGN(4K);
    __user_rodata_start = .;
    .user.rodata : { *(.user.rodata .user.rodata.*) }
    . = ALIGN(4K);
    __user_data_start = .;
    .user.data   : { *(.user.data .user.data.*) *(.user.bss .user.bss.*) }
    . = __user_start + 2M;
    __user_end = .;

    /* Kernel proper: linked at high (TTBR1) VMAs, loaded at low PAs via AT().
       The first high section carries an explicit AT(); the rest follow it in
       the load region contiguously. */
    __kernel_phys = .;
    . = __kernel_phys + KERNEL_VA_BASE;

    .text   : AT(__kernel_phys) { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }

    .bss (NOLOAD) : {
        . = ALIGN(8);
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(2M);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
