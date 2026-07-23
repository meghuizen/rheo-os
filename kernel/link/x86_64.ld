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

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
