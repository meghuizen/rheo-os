/* Userland link layout, x86-64 (docs/USERLAND.md). Base 1 GiB: free VA in the
   x86 cell root (kernel identity is the low 1 GiB), and below 2 GiB so the
   default `code-model=small` reaches it without a GOT. Sections are 4 KiB-
   aligned so the loader maps each PT_LOAD segment with its own W^X permission
   at page granularity. The loader reads e_entry, so section order is free. */

ENTRY(_start)

SECTIONS
{
    . = 0x40000000;

    .text : { *(.text._start) *(.text .text.*) *(.iplt) }
    . = ALIGN(4K);
    /* Synthetic: LLD creates this output section itself, so it must be named
       here or it is placed as an orphan - which lands it mid-page and starts
       the read-only segment inside the executable one, breaking the W^X page
       granularity this script's header promises (docs/LIBRHEO.md "ifunc"). */
    .rela.dyn : { *(.rela.dyn) *(.rela.iplt) }
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4K);
    .data : { *(.data .data.*) *(.got) *(.got.plt) *(.igot.plt) *(.data.rel.ro*) }
    . = ALIGN(4K);
    .bss : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    /DISCARD/ : { *(.eh_frame) *(.eh_frame_hdr) *(.comment) *(.note*) }
}
