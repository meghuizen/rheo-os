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

    /* User image: code and static data that runs in U-mode. Kept in one
       2 MiB-aligned, 2 MiB window so a single Sv39 level-0 table covers
       it and per-cell roots differ only in that one level-1 slot
       (kernel/src/arch/riscv64/paging.rs). Mapped with the U bit; W^X
       enforced by section (text = R|X, data = R|W). */
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
