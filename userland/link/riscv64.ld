/* Userland link layout, RISC-V 64 (docs/USERLAND.md). Base 4 GiB: free VA in
   every cell root above the kernel/MMIO ranges. Sections are 4 KiB-aligned so
   the loader maps each PT_LOAD segment with its own W^X permission. The
   loader reads e_entry, so section order is free. */

ENTRY(_start)

SECTIONS
{
    . = 0x100000000;

    .text : { *(.text._start) *(.text .text.*) }
    . = ALIGN(4K);
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4K);
    .data : { *(.data .data.*) *(.sdata .sdata.*) *(.got) *(.got.plt) }
    . = ALIGN(4K);
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /DISCARD/ : { *(.eh_frame) *(.eh_frame_hdr) *(.comment) *(.note*) *(.riscv.attributes) }
}
