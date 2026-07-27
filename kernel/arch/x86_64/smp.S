/*
 * x86-64 application-processor (AP) startup trampoline (docs/SMP.md 6).
 *
 * An AP leaves reset in **16-bit real mode**. The only way to release it is an
 * INIT-SIPI-SIPI sequence through the local APIC's interrupt command register,
 * and a SIPI carries an 8-bit vector that the AP turns into CS:IP =
 * (vector << 8):0 - i.e. it starts executing at physical `vector << 12`, which
 * is necessarily **below 1 MiB**. The kernel image is loaded at 1 MiB and above
 * and PVH boot gives no firmware to stage anything low, so the kernel copies
 * this trampoline into a low page itself (`AP_TRAMPOLINE_PA`, chosen and
 * *verified* free in mod.rs) and points the SIPI at it.
 *
 * The code is therefore **position-fixed, not position-independent**: it is
 * assembled as if based at AP_TRAMPOLINE_PA, and every intra-trampoline
 * absolute reference is written as `AP_BASE + (label - ap_trampoline)`. That is
 * what makes it copy-safe: the only addresses it forms are (a) those constants,
 * (b) 32-bit absolute immediates/addresses of *kernel* symbols, which the copy
 * does not move, and (c) `movabs` of 64-bit kernel VAs. Nothing is PC-relative,
 * so the kernel patches exactly nothing after the copy.
 *
 * Mode ladder (the standard one; each step is architecturally required):
 *   real 16 -> lgdt + CR0.PE -> protected 32 -> CR4, CR3, EFER, CR0.PG
 *   -> compatibility -> far jump to a 64-bit descriptor -> long 64.
 *
 * **The AP's mode is the primary's mode, by construction.** Rather than
 * hand-picking bits, the primary publishes its own CR4, EFER and CR0 into
 * AP_CR4/AP_EFER/AP_CR0 (in `.boot.bss`, identity low so the 32-bit stage can
 * read them with paging off) and the AP loads them verbatim. That is not
 * tidiness: the kernel's page tables carry NX on real entries - the APIC
 * register window among them - and with EFER.NXE clear a set bit 63 is a
 * *reserved-bit* page fault, so an AP that merely set LME triple-faulted on its
 * first LAPIC read. Copying the primary's control registers makes that class of
 * divergence impossible instead of enumerable.
 *
 * The page tables are the **primary's own** `boot_page_tables`, so the AP shares
 * the kernel address space from its first paged instruction. PML4[0] there is
 * the low identity map the primary's own paging turn-on left behind, which is
 * exactly why the trampoline's low PC stays valid across `mov %cr0` - the same
 * trick boot.S uses.
 *
 * The trampoline carries **its own GDT**, because the ladder needs a 32-bit code
 * descriptor and the boot GDT has no reason to contain one. Its layout is
 * deliberately *compatible* with the boot GDT at the selectors that matter -
 * 0x08 is the 64-bit code descriptor and 0x10 the data descriptor, exactly as in
 * boot.S - with the trampoline-only 32-bit code descriptor parked at 0x20. That
 * is what lets the AP keep this GDT after startup: the kernel's IDT gates all
 * name selector 0x08, so an exception on the AP resolves to a 64-bit code
 * descriptor and reaches the kernel handler rather than faulting while faulting.
 * (The first attempt instead swapped in the primary's `gdt64_ptr`, which is the
 * **6-byte** 32-bit form; `lgdt` in long mode reads 2 + 8 bytes, so it loaded a
 * garbage base and #GP'd. Observed as a triple fault, fixed by not needing the
 * swap.)
 *
 * The AP therefore keeps executing off, and keeps its GDT in, the low trampoline
 * page for the rest of its life. That page is never reclaimed: the frame
 * allocator's pool starts at 64 MiB (FRAME_POOL_BASE), so nothing else can ever
 * hand it out.
 */

/* Must equal AP_TRAMPOLINE_PA in kernel/src/arch/x86_64/mod.rs; published as
   AP_TRAMPOLINE_BASE below and asserted there, so the two cannot drift apart
   silently (the code is position-fixed - a disagreement lands the AP nowhere). */
.set AP_BASE, 0x8000

.section .boot.text, "ax"
.code16
.align 16
.global ap_trampoline
.global ap_trampoline_end
ap_trampoline:
    cli
    cld
    /* Flat real-mode addressing from segment 0: the trampoline sits below
       64 KiB, so `AP_BASE + offset` is directly addressable with ds = 0. */
    xor %ax, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss

    lgdtl AP_BASE + (ap_gdt_ptr - ap_trampoline)

    /* CR0.PE: enter 32-bit protected mode. */
    mov %cr0, %eax
    or $1, %eax
    mov %eax, %cr0
    ljmpl $0x20, $(AP_BASE + (ap_pm32 - ap_trampoline))

.code32
ap_pm32:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov %ax, %fs
    mov %ax, %gs

    /* The primary's CR4 (carries PAE, which long mode requires). */
    mov AP_CR4, %eax
    mov %eax, %cr4

    /* Share the primary's page tables. `boot_page_tables` is an identity-low
       symbol, so this 32-bit absolute immediate is its physical address and
       survives the copy unchanged. */
    mov $boot_page_tables, %eax
    mov %eax, %cr3

    /* The primary's EFER (carries LME, NXE and SCE). */
    mov $0xC0000080, %ecx
    mov AP_EFER, %eax
    mov AP_EFER+4, %edx
    wrmsr

    /* The primary's CR0 (carries PE and PG): paging on, long mode active,
       compatibility mode until the far jump below. */
    mov AP_CR0, %eax
    mov %eax, %cr0

    ljmpl $0x08, $(AP_BASE + (ap_lm64 - ap_trampoline))

.code64
ap_lm64:
    /* One dedicated secondary stack: this phase brings up exactly one AP
       (docs/SMP.md), so a single stack cannot be shared by two of them. */
    movabs $ap_stack_top, %rsp
    /* Hand off to portable kernel code. `x86_secondary_main` reads this CPU's
       APIC id from the local APIC itself rather than trusting anything the
       primary passed - the identity comes from the hardware. */
    movabs $x86_secondary_main, %rax
    call *%rax
    /* Never reached: secondary_main parks. Contained anyway. */
1:
    cli
    hlt
    jmp 1b

/* The trampoline's own GDT, laid out boot-GDT-compatibly at 0x08 (64-bit code)
   and 0x10 (data) so the kernel's IDT gates work on the AP, with the 32-bit
   code descriptor the ladder needs parked at 0x20. Assembled inside the copied
   region so it travels with the code; the pointer's base is the AP_BASE-relative
   absolute the ladder needs. */
.align 8
ap_gdt:
    .quad 0                     /* 0x00 null */
    .quad 0x00209A0000000000    /* 0x08 64-bit code, long mode (== boot gdt64) */
    .quad 0x00CF92000000FFFF    /* 0x10 data,        base 0, limit 4 GiB */
    .quad 0                     /* 0x18 unused (keeps 0x20 where it is) */
    .quad 0x00CF9A000000FFFF    /* 0x20 32-bit code, the ladder's middle rung */
ap_gdt_ptr:
    .word ap_gdt_ptr - ap_gdt - 1
    .long AP_BASE + (ap_gdt - ap_trampoline)
ap_trampoline_end:

/* The control-register values the primary publishes for the AP. Identity-low
   (`.boot.bss`, VMA == LMA) because the 32-bit stage reads them with paging off,
   so their symbol addresses are their physical addresses - the same convention
   BOOT_INFO uses. Written by smp_start_secondary before the SIPI. */
.section .boot.bss, "aw", @nobits
.align 8
.global AP_CR4
AP_CR4:
    .space 8
.global AP_EFER
AP_EFER:
    .space 8
.global AP_CR0
AP_CR0:
    .space 8

/* The base address the trampoline was assembled for, published so the Rust side
   can assert the two constants agree. Lives in the high .rodata, outside the
   copied region. */
.section .rodata
.align 8
.global AP_TRAMPOLINE_BASE
AP_TRAMPOLINE_BASE:
    .quad AP_BASE

/* The secondary's kernel stack: a high VA in ordinary BSS, mapped supervisor in
   every root (like the primary's boot_stack), so it is valid the instant the AP
   is in long mode on the shared page tables. */
.section .bss
.align 16
ap_stack:
    .space 16 * 1024
ap_stack_top:
