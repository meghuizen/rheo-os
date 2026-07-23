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

    /* User image (see riscv64.ld for the rationale): one 2 MiB-aligned,
       2 MiB window, so a single level-2 table covers it and per-cell
       roots differ only in that slot. */
    . = ALIGN(2M);
    __user_start = .;
    .user.text   : { *(.user.text .user.text.*) }
    . = ALIGN(4K);
    __user_data_start = .;
    .user.data   : { *(.user.data .user.data.*) *(.user.bss .user.bss.*) }
    . = __user_start + 2M;
    __user_end = .;

    . = ALIGN(2M);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
