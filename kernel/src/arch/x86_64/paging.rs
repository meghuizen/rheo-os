//! x86-64 paging (BUILD-ORDER.md step 3): 4-level, 4 KiB pages, identity
//! mapped. The processor is already in long mode with a minimal boot map
//! (kernel/arch/x86_64/boot.S); this replaces it with allocator-backed
//! tables and per-cell roots.
//!
//! Layout mirrors the other ports: the low 1 GiB (kernel image, page
//! tables, frame pool) is mapped supervisor with 2 MiB pages (global, so
//! it survives a CR3 reload), except the one 2 MiB slot holding the
//! `.user` window, which is a page table of 4 KiB entries carrying the
//! user (US) bit. W^X is the RW and NX bits per page; SMAP is left off so
//! the kernel can touch a cell's ring during a doorbell.

use crate::arch::MapPerm;
use crate::mm::frames;

const P: u64 = 1 << 0; // present
const RW: u64 = 1 << 1; // writable
const US: u64 = 1 << 2; // user-accessible
const PS: u64 = 1 << 7; // page size (2 MiB leaf at PD level)
const G: u64 = 1 << 8; // global
const NX: u64 = 1 << 63; // no-execute

const PAGE_SIZE: usize = 4096;
const MIB2: usize = 2 << 20;

unsafe extern "C" {
    static __user_start: u8;
}

fn user_window_base() -> usize {
    core::ptr::addr_of!(__user_start) as usize
}

#[derive(Copy, Clone)]
pub struct PagingRoot {
    pml4_pa: usize,
}

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is an allocated, identity-mapped table frame.
    unsafe { &mut *(pa as *mut [u64; 512]) }
}

fn addr_bits(pa: usize) -> u64 {
    (pa as u64) & 0x000F_FFFF_FFFF_F000
}

fn next_table(entry: u64) -> usize {
    (entry & 0x000F_FFFF_FFFF_F000) as usize
}

fn pml4_index(va: usize) -> usize {
    (va >> 39) & 0x1FF
}
fn pdpt_index(va: usize) -> usize {
    (va >> 30) & 0x1FF
}
fn pd_index(va: usize) -> usize {
    (va >> 21) & 0x1FF
}
fn pt_index(va: usize) -> usize {
    (va >> 12) & 0x1FF
}

/// Build the low-1 GiB identity map (2 MiB supervisor pages) into `pd`,
/// carving the `.user` 2 MiB slot out to a fresh page table when `carve`.
fn fill_low_gib(pd: &mut [u64; 512], carve: bool) {
    let user_slot = user_window_base() & !(MIB2 - 1);
    for (i, entry) in pd.iter_mut().enumerate() {
        let va = i * MIB2;
        if carve && va == user_slot {
            let pt_pa = frames::alloc();
            *entry = addr_bits(pt_pa) | P | RW | US; // empty PT, filled by paging_map
        } else {
            // Supervisor 2 MiB page, executable (kernel code lives here).
            *entry = addr_bits(va) | P | RW | PS | G;
        }
    }
}

/// Allocate a cell root: low 1 GiB supervisor with the `.user` slot carved
/// to a 4 KiB page table.
pub fn paging_new_root() -> PagingRoot {
    let pml4_pa = frames::alloc();
    let pdpt_pa = frames::alloc();
    let pd_pa = frames::alloc();
    let pml4 = table_mut(pml4_pa);
    let pdpt = table_mut(pdpt_pa);
    pml4[0] = addr_bits(pdpt_pa) | P | RW | US;
    pdpt[0] = addr_bits(pd_pa) | P | RW | US;
    fill_low_gib(table_mut(pd_pa), true);
    PagingRoot { pml4_pa }
}

/// Map one 4 KiB identity page for user access inside the `.user` window.
pub fn paging_map(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let base = user_window_base();
    assert!(
        (base..base + MIB2).contains(&va),
        "user map {va:#x} outside the .user window"
    );

    let pml4 = table_mut(root.pml4_pa);
    let pdpt = table_mut(next_table(pml4[pml4_index(va)]));
    let pd = table_mut(next_table(pdpt[pdpt_index(va)]));
    let pt = table_mut(next_table(pd[pd_index(va)]));

    let bits = match perm {
        MapPerm::UserRo => P | US | NX,
        MapPerm::UserRw => P | RW | US | NX,
        MapPerm::UserRx => P | US, // read + execute, not writable
    };
    pt[pt_index(va)] = addr_bits(va) | bits;
}

/// Get the table a parent entry points at, creating an empty one (present,
/// writable, user-accessible so the walk can descend) if the slot is empty.
fn ensure_table(parent: &mut [u64; 512], idx: usize) -> usize {
    if parent[idx] & P == 0 {
        let t = frames::alloc(); // zeroed
        parent[idx] = addr_bits(t) | P | RW | US;
    }
    next_table(parent[idx])
}

/// Map one 4 KiB frame `pa` at an arbitrary user `va`, creating intermediate
/// tables as needed (docs/USERLAND.md). `va` must lie above the kernel's low
/// 1 GiB identity map (userland links at 4 GiB+).
pub fn paging_map_frame(root: &mut PagingRoot, va: usize, pa: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let pml4 = table_mut(root.pml4_pa);
    let pdpt = table_mut(ensure_table(pml4, pml4_index(va)));
    let pd = table_mut(ensure_table(pdpt, pdpt_index(va)));
    let pt = table_mut(ensure_table(pd, pd_index(va)));
    let bits = match perm {
        MapPerm::UserRo => P | US | NX,
        MapPerm::UserRw => P | RW | US | NX,
        MapPerm::UserRx => P | US, // read + execute, not writable
    };
    pt[pt_index(va)] = addr_bits(pa) | bits;
}

/// Activate a root: load CR3. Kernel pages are global, so they survive the
/// reload; user pages (non-global) are flushed - the isolation guarantee.
pub fn paging_activate(root: &PagingRoot, _asid: u16) {
    // SAFETY: pml4_pa is a well-formed root.
    unsafe {
        core::arch::asm!("mov cr3, {0}", in(reg) root.pml4_pa as u64, options(nostack));
    }
}

static mut KERNEL_ROOT_PA: usize = 0;

pub fn paging_activate_kernel() {
    let pml4_pa = unsafe { *core::ptr::addr_of!(KERNEL_ROOT_PA) };
    paging_activate(&PagingRoot { pml4_pa }, 0);
}

/// Build the kernel address space (low 1 GiB supervisor, no user carve),
/// enable NX, and set up ring 3 (GDT/TSS + syscall MSRs).
pub fn paging_kernel_init() {
    frames::init();

    let pml4_pa = frames::alloc();
    let pdpt_pa = frames::alloc();
    let pd_pa = frames::alloc();
    let pml4 = table_mut(pml4_pa);
    let pdpt = table_mut(pdpt_pa);
    pml4[0] = addr_bits(pdpt_pa) | P | RW | US;
    pdpt[0] = addr_bits(pd_pa) | P | RW | US;
    fill_low_gib(table_mut(pd_pa), false);

    unsafe {
        *core::ptr::addr_of_mut!(KERNEL_ROOT_PA) = pml4_pa;

        // Enable NXE (EFER bit 11) so the NX bit is honoured.
        let efer = rdmsr(0xC000_0080) | (1 << 11);
        wrmsr(0xC000_0080, efer);
    }
    paging_activate(&PagingRoot { pml4_pa }, 0);

    // GDT, TSS, and the syscall/sysret MSRs (kernel/arch/x86_64/mod.rs).
    super::user_init();
}

unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        );
    }
}
