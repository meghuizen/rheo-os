/* RISC-V 64: QEMU virt machine RAM starts at 0x8000_0000; OpenSBI occupies
   the first 2 MiB and jumps to the kernel at 0x8020_0000 in S-mode. */

ENTRY(_start)

SECTIONS
{
    . = 0x80200000;

    .boot.text : { *(.boot.text) }
    .text      : { *(.text .text.*) }
    .rodata    : { *(.rodata .rodata.*) }
    .data      : { *(.data .data.*) }

    /* Linker-relaxed access to small data through gp. */
    __global_pointer$ = . + 0x800;
    .sdata     : { *(.sdata .sdata.*) }

    .bss : {
        *(.boot.bss)

        . = ALIGN(8);
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /DISCARD/ : { *(.eh_frame) *(.comment) }
}
