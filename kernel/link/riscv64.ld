/* RISC-V 64 higher-half kernel (docs/MEMORY.md).

   QEMU's virt machine loads the kernel ELF by physical address (LMA); OpenSBI
   occupies the first 2 MiB of RAM (0x8000_0000) and enters `_start` at
   0x8020_0000 in S-mode with paging OFF, so the boot trampoline must be
   identity-mapped low. Once it turns the MMU on it runs the kernel proper at
   high (Sv39 top-half) virtual addresses, leaving the entire low half free for
   user programs - a stock Linux ET_EXEC at 0x10000 no longer collides with a
   supervisor mapping.

   RISC-V has no "large" code model (medany reaches only +-2 GiB PC-relative),
   so unlike aarch64 the `.user` window is linked HIGH too, adjacent to the
   kernel - every kernel->.user symbol reference then stays within medany's
   reach. Isolation is unaffected: a cell is gated by the U bit on the leaf
   PTE, not by the address (kernel/src/arch/riscv64/paging.rs).

   Physical layout (LMA), inside QEMU virt RAM at 0x8000_0000:
     0x8020_0000  .boot.text / .boot.bss   identity low (VMA == LMA)
     0x8040_0000  .user.*                  high VMA (KERNEL_VA_BASE + LMA)
     0x8060_0000  .text/.rodata/.data/.bss high VMA (KERNEL_VA_BASE + LMA)

   KERNEL_VA_BASE is the linear-map offset: phys_to_virt(pa) = pa | BASE,
   virt_to_phys(va) = va & ~BASE (kernel/src/arch/riscv64/mod.rs). A high VMA
   therefore equals phys_to_virt(its LMA), so the boot-time linear map and the
   link addresses agree. */

ENTRY(_start)

KERNEL_VA_BASE = 0xFFFFFFC000000000;

SECTIONS
{
    /* Boot trampoline + its early page table: identity-mapped low, entered by
       OpenSBI with the MMU off (VMA == LMA). */
    . = 0x80200000;
    .boot.text : { KEEP(*(.boot.text)) }
    . = ALIGN(4K);
    .boot.bss (NOLOAD) : { *(.boot.bss) }

    /* User image: code and static data that runs in U-mode. Linked at high
       VMAs, loaded at low PAs via AT(). One 2 MiB-aligned, 2 MiB window (a
       single Sv39 level-1 slot); per-cell roots differ only in that slot,
       mapped with the U bit. W^X enforced by section (text = R|X, data = R|W). */
    . = ALIGN(2M);
    __user_phys = .;
    . = __user_phys + KERNEL_VA_BASE;
    __user_start = .;
    .user.text   : AT(__user_phys) { *(.user.text .user.text.*) }
    . = ALIGN(4K);
    __user_rodata_start = .;
    .user.rodata : { *(.user.rodata .user.rodata.*) }
    . = ALIGN(4K);
    __user_data_start = .;
    .user.data   : { *(.user.data .user.data.*) *(.user.bss .user.bss.*) }
    . = __user_start + 2M;
    __user_end = .;

    /* Kernel proper: linked at high VMAs, loaded at low PAs via AT(). */
    __kernel_phys = __user_phys + 2M;
    .text   : AT(__kernel_phys) { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }

    /* Linker-relaxed access to small data through gp. */
    __global_pointer$ = . + 0x800;
    .sdata  : { *(.sdata .sdata.*) }

    .bss (NOLOAD) : {
        . = ALIGN(8);
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(2M);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
