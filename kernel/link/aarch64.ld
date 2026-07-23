/* ARM64: QEMU virt machine RAM starts at 0x4000_0000; load a little above
   the base to leave room for firmware data. MMU is off, so link = load. */

ENTRY(_start)

SECTIONS
{
    . = 0x40080000;

    .boot.text : { *(.boot.text) }
    .text      : { *(.text .text.*) }
    .rodata    : { *(.rodata .rodata.*) }
    .data      : { *(.data .data.*) }

    .bss : {
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
