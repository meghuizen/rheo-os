/* x86-64 higher-half kernel (docs/MEMORY.md).

   QEMU's -kernel loader honours the PVH ELF note and loads the kernel by
   physical address (LMA), entering `_start32` in 32-bit protected mode with
   paging OFF - so the boot trampoline must be identity-mapped low. Once it
   turns paging on it runs the kernel proper at high (top-2 GiB) virtual
   addresses, leaving the entire low half free for user programs - a stock
   Linux ET_EXEC at 0x400000 no longer collides with a supervisor mapping.

   Unlike aarch64 (which has a TTBR0/TTBR1 split) x86-64 has a single CR3, so -
   like riscv64 - the `.user` window is linked HIGH too, adjacent to the
   kernel. x86-64 kernel code uses the "kernel" code model (top-2 GiB, signed
   32-bit relocations), whose reach is the top 2 GiB; keeping `.user` next to
   the kernel keeps every kernel->.user reference in range. Isolation is
   unaffected: a cell is gated by the U bit on the leaf PTE, not by the address
   (kernel/src/arch/x86_64/paging.rs).

   Physical layout (LMA), inside QEMU low RAM:
     0x0010_0000  .boot.text / .note.pvh / .boot.bss   identity low (VMA == LMA)
     0x0020_0000  .user.*                              high VMA (KERNEL_VA_BASE | LMA)
     0x0040_0000  .text/.rodata/.data/.bss             high VMA (KERNEL_VA_BASE | LMA)

   KERNEL_VA_BASE is the linear-map offset: phys_to_virt(pa) = pa | BASE,
   virt_to_phys(va) = va & ~BASE (kernel/src/arch/x86_64/mod.rs). A high VMA
   therefore equals phys_to_virt(its LMA), so the boot-time linear map and the
   link addresses agree. */

ENTRY(_start32)

KERNEL_VA_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    /* Boot trampoline, the PVH note, and the early page tables: identity-
       mapped low, entered by QEMU with paging off (VMA == LMA). */
    . = 1M;
    .boot.text : { KEEP(*(.boot.text)) }
    .note.pvh  : { KEEP(*(.note.pvh)) }
    . = ALIGN(4K);
    .boot.bss (NOLOAD) : { *(.boot.bss) }

    /* User image: code and static data that runs in ring 3. Linked at high
       VMAs, loaded at low PAs via AT(). One 2 MiB-aligned, 2 MiB window (a
       single PD slot); per-cell roots differ only in that slot, mapped with
       the US bit. W^X enforced by section (text = R|X, data = R|W|NX). */
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

    /* Kernel proper: linked at high VMAs, loaded at low PAs via AT(). The GOT
       (the kernel code model can still emit one) must sit HIGH with the rest -
       a low GOT would be unmapped in a cell root, faulting a trap handler that
       reads it - so it is placed explicitly rather than left an orphan. */
    __kernel_phys = __user_phys + 2M;
    .text     : AT(__kernel_phys) { *(.text .text.*) }
    .rodata   : { *(.rodata .rodata.*) }
    .got      : { *(.got .got.*) }
    .got.plt  : { *(.got.plt .got.plt.*) }
    .data.rel.ro : { *(.data.rel.ro .data.rel.ro.*) }
    .data     : { *(.data .data.*) }

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
