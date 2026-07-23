/*
 * x86-64 boot stub. QEMU's -kernel loader honours the PVH ELF note and
 * enters _start32 in 32-bit protected mode with paging off (ebx = PVH
 * start_info, unused for now). This stub builds identity page tables for
 * the first 1 GiB, switches to long mode, clears the BSS, and calls
 * kernel_main. See DEVELOPMENT.md 4 (fast dev path).
 */

/* PVH entry note: type 18 = XEN_ELFNOTE_PHYS32_ENTRY. */
.section .note.pvh, "a", @note
.align 4
.long 4                     /* name size */
.long 4                     /* desc size */
.long 18                    /* type */
.asciz "Xen"
.long _start32              /* 32-bit physical entry address */

.section .boot.text, "ax"
.code32
.global _start32
_start32:
    cli
    cld

    /* Zero the early page-table pages (this memory is not cleared yet). */
    mov $boot_page_tables, %edi
    xor %eax, %eax
    mov $(3 * 4096 / 4), %ecx
    rep stosl

    /* PML4[0] -> PDPT (present | writable) */
    mov $boot_page_tables, %edi
    lea 0x1000(%edi), %eax
    or $0x3, %eax
    mov %eax, (%edi)

    /* PDPT[0] -> PD (present | writable) */
    lea 0x1000(%edi), %ebx
    lea 0x2000(%edi), %eax
    or $0x3, %eax
    mov %eax, (%ebx)

    /* PD[0..512]: identity-map 1 GiB with 2 MiB pages
       (present | writable | huge). */
    lea 0x2000(%edi), %ebx
    mov $0x83, %eax
    xor %ecx, %ecx
1:
    mov %eax, (%ebx,%ecx,8)
    add $0x200000, %eax
    inc %ecx
    cmp $512, %ecx
    jne 1b

    /* Enable PAE. */
    mov %cr4, %eax
    or $(1 << 5), %eax
    mov %eax, %cr4

    /* Point CR3 at the PML4. */
    mov $boot_page_tables, %eax
    mov %eax, %cr3

    /* Set EFER.LME (long mode enable). */
    mov $0xC0000080, %ecx
    rdmsr
    or $(1 << 8), %eax
    wrmsr

    /* Enable paging (and keep protection on) -> now in compatibility mode. */
    mov %cr0, %eax
    or $0x80000001, %eax
    mov %eax, %cr0

    /* Far jump into a 64-bit code segment. */
    lgdt gdt64_ptr
    ljmp $0x08, $_start64

.code64
_start64:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %eax, %eax
    mov %ax, %fs
    mov %ax, %gs

    mov $boot_stack_top, %rsp

    /* Clear the BSS (the boot stack and page tables live outside it). */
    lea __bss_start(%rip), %rdi
    lea __bss_end(%rip), %rcx
    sub %rdi, %rcx
    shr $3, %rcx
    xor %eax, %eax
    rep stosq

    call kernel_main
2:
    hlt
    jmp 2b

.section .rodata
.align 16
gdt64:
    .quad 0                     /* null */
    .quad 0x00209A0000000000    /* 64-bit code: present, exec, long */
    .quad 0x0000920000000000    /* data: present, writable */
gdt64_ptr:
    .word gdt64_ptr - gdt64 - 1
    .long gdt64

/* Early memory the boot stub initialises itself, kept out of the
   __bss_start..__bss_end clear range (the page tables are live when the
   BSS is cleared). */
.section .boot.bss, "aw", @nobits
.align 4096
boot_page_tables:
    .space 3 * 4096             /* PML4, PDPT, PD */
.align 16
boot_stack:
    .space 64 * 1024
boot_stack_top:
