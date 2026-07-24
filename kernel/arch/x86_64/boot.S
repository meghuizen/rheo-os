/*
 * x86-64 higher-half boot trampoline (docs/MEMORY.md).
 *
 * QEMU's -kernel loader honours the PVH ELF note and enters _start32 in
 * 32-bit protected mode with paging OFF (ebx = PVH hvm_start_info, which
 * carries the memory map and the ACPI RSDP address). This code runs
 * identity-mapped low (the .boot sections have VMA == LMA in link/x86_64.ld).
 * It builds initial 4-level page tables:
 *   PML4[0]   -> low identity of 0-1 GiB (so the instruction right after
 *                paging turns on, still executing at its low PC, stays mapped);
 *   PML4[511] -> the kernel high linear map: PDPT[510] = phys 0-1 GiB,
 *                PDPT[511] = phys 1-2 GiB, both as 2 MiB supervisor pages.
 * It enables PAE + long mode + paging, far-jumps into 64-bit code (still low),
 * then does an absolute jump to the high-half continuation (movabs, since a
 * high VA is out of a rel32's reach from the low PC). The continuation sets
 * rsp high, clears the BSS, and calls kernel_main - all before any Rust runs
 * at its high VAs.
 *
 * The boot page tables double as the kernel working root
 * (paging_activate_kernel reuses boot_page_tables); PML4[0] is the low-identity
 * leftover from the turn-on. A cell root (paging_new_root) drops PML4[0], so
 * the whole low half is free for a stock Linux ET_EXEC. See DEVELOPMENT.md 4.
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

    /* QEMU passes the PVH hvm_start_info pointer in ebx; save it before we
       reuse ebx for page-table setup (BOOT_INFO is identity-mapped low). */
    movl %ebx, BOOT_INFO
    movl $0, BOOT_INFO+4

    /* Zero the early page-table pages (this memory is not cleared yet):
       PML4, PDPT_low, PD_low, PDPT_high, PD_high_lo, PD_high_hi. */
    mov $boot_page_tables, %edi
    xor %eax, %eax
    mov $(6 * 4096 / 4), %ecx
    rep stosl

    mov $boot_page_tables, %edi

    /* PML4[0] -> PDPT_low (present | writable): low identity for the turn-on. */
    lea 0x1000(%edi), %eax
    or $0x3, %eax
    mov %eax, 0x0(%edi)

    /* PML4[511] -> PDPT_high (present | writable | user): the kernel high
       linear map. US is set on the upper tables so the one carved `.user`
       leaf can be user-accessible; per-page US at the PD/PT level keeps the
       kernel pages supervisor. */
    lea 0x3000(%edi), %eax
    or $0x7, %eax
    mov %eax, 0xFF8(%edi)

    /* PDPT_low[0] -> PD_low (identity). */
    lea 0x2000(%edi), %eax
    or $0x3, %eax
    mov %eax, 0x1000(%edi)

    /* PDPT_high[510] -> PD_high_lo (phys 0-1 GiB). */
    lea 0x4000(%edi), %eax
    or $0x7, %eax
    mov %eax, 0x3FF0(%edi)

    /* PDPT_high[511] -> PD_high_hi (phys 1-2 GiB). */
    lea 0x5000(%edi), %eax
    or $0x7, %eax
    mov %eax, 0x3FF8(%edi)

    /* PD_low[0..512]: identity 0-1 GiB, 2 MiB pages (present|writable|huge). */
    lea 0x2000(%edi), %ebx
    mov $0x83, %eax
    xor %ecx, %ecx
1:
    mov %eax, (%ebx,%ecx,8)
    add $0x200000, %eax
    inc %ecx
    cmp $512, %ecx
    jne 1b

    /* PD_high_lo[0..512]: phys 0-1 GiB, 2 MiB supervisor pages
       (present|writable|huge|global), executable (kernel code lives here). */
    lea 0x4000(%edi), %ebx
    mov $0x183, %eax
    xor %ecx, %ecx
2:
    mov %eax, (%ebx,%ecx,8)
    add $0x200000, %eax
    inc %ecx
    cmp $512, %ecx
    jne 2b

    /* PD_high_hi[0..512]: phys 1-2 GiB, same flags. */
    lea 0x5000(%edi), %ebx
    mov $0x40000183, %eax
    xor %ecx, %ecx
3:
    mov %eax, (%ebx,%ecx,8)
    add $0x200000, %eax
    inc %ecx
    cmp $512, %ecx
    jne 3b

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

    /* Far jump into a 64-bit code segment (still at a low PC). */
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

    /* Jump to the high-half continuation (absolute VA; a rel32 cannot reach
       the top-2 GiB kernel from this low PC). */
    movabs $high_start, %rax
    jmp *%rax

    /* Boot GDT: must stay low (loaded in 32-bit / compatibility mode before
       the jump to the high half), so it lives in .boot.text (identity low).
       The pointer's base is a 32-bit absolute (.long), which only reaches a
       low address. */
.align 16
gdt64:
    .quad 0                     /* null */
    .quad 0x00209A0000000000    /* 64-bit code: present, exec, long */
    .quad 0x0000920000000000    /* data: present, writable */
gdt64_ptr:
    .word gdt64_ptr - gdt64 - 1
    .long gdt64

/* --- high half: runs at KERNEL_VA_BASE | PA, kernel mapped via PML4[511] --- */
.section .text
.code64
high_start:
    lea boot_stack_top(%rip), %rsp

    /* Clear the BSS (high VAs, mapped via PML4[511]). Done after rsp is set
       but before the first push, so zeroing the stack region is harmless. */
    lea __bss_start(%rip), %rdi
    lea __bss_end(%rip), %rcx
    sub %rdi, %rcx
    shr $3, %rcx
    xor %eax, %eax
    rep stosq

    call kernel_main
4:
    hlt
    jmp 4b

/* Early page tables + PVH info: identity-mapped low so the 32-bit trampoline
   can build/write them with paging off. */
.section .boot.bss, "aw", @nobits
.align 4096
.global boot_page_tables
boot_page_tables:
    .space 6 * 4096             /* PML4, PDPT_low, PD_low, PDPT_high, PD_high_lo, PD_high_hi */

.global BOOT_INFO
.align 8
BOOT_INFO:
    .space 8

/* Kernel stack: a high VA in ordinary BSS (mapped via PML4[511], supervisor,
   reachable in every root). */
.section .bss
.align 16
boot_stack:
    .space 64 * 1024
boot_stack_top:
