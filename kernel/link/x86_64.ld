/* x86-64: linked at 1 MiB physical, identity-mapped by the boot stub.
   Entered via the PVH ELF note (see kernel/arch/x86_64/boot.S). */

ENTRY(_start32)

SECTIONS
{
    . = 1M;

    .boot.text : { *(.boot.text) }
    .text      : { *(.text .text.*) }
    .rodata    : { *(.rodata .rodata.*) }
    .note.pvh  : { *(.note.pvh) }
    .data      : { *(.data .data.*) }

    .bss : {
        /* Boot stack + early page tables: initialised by the boot stub
           itself, deliberately outside the clear range below. */
        *(.boot.bss)

        . = ALIGN(8);
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /* User image (see riscv64.ld for the rationale): one 2 MiB-aligned,
       2 MiB window, so a single PD table covers it and per-cell roots
       differ only in that slot. */
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

    . = ALIGN(2M);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
