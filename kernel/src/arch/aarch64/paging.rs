//! ARM64 paging (BUILD-ORDER.md step 3): 4 KiB granule, 48-bit VA over
//! TTBR0_EL1. Four levels (L0 512 GiB, L1 1 GiB, L2 2 MiB, L3 4 KiB).
//!
//! Layout mirrors the RISC-V port: MMIO and kernel RAM are identity-mapped
//! supervisor (1 GiB blocks in the kernel root; 2 MiB blocks in cell
//! roots), and the one 2 MiB slot holding the `.user` window is a level-3
//! table where per-cell user pages carry EL0 access. Isolation is the MMU
//! faulting on a page a cell's root does not map; W^X is UXN/PXN + AP.

use crate::arch::MapPerm;
use crate::mm::frames;

// Descriptor bits.
const VALID: u64 = 1 << 0;
const TABLE: u64 = 1 << 1; // table (at L0-L2) or page (at L3)
const BLOCK: u64 = 0; // block entry at L1/L2
const ATTR_DEVICE: u64 = 0 << 2; // MAIR index 0
const ATTR_NORMAL: u64 = 1 << 2; // MAIR index 1
const AP_RW_EL1: u64 = 0b00 << 6; // RW at EL1, no EL0
const AP_RW_ALL: u64 = 0b01 << 6; // RW at EL1 and EL0
const AP_RO_ALL: u64 = 0b11 << 6; // RO at EL1 and EL0
const SH_INNER: u64 = 0b11 << 8;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11; // ASID-tagged (per-cell) entry
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

const PAGE_SIZE: usize = 4096;
const GIB: usize = 1 << 30;
const MIB2: usize = 2 << 20;

const DEVICE_BASE: usize = 0; // MMIO gigabyte (UART, GIC, ...)
const RAM_BASE: usize = 0x4000_0000;

unsafe extern "C" {
    static __user_start: u8;
}

fn user_window_base() -> usize {
    core::ptr::addr_of!(__user_start) as usize
}

#[derive(Copy, Clone)]
pub struct PagingRoot {
    l0_pa: usize,
}

fn addr_bits(pa: usize) -> u64 {
    // Output address occupies bits [47:12]; low bits are attributes.
    (pa as u64) & 0x0000_FFFF_FFFF_F000
}

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is an allocated, identity-mapped table frame.
    unsafe { &mut *(pa as *mut [u64; 512]) }
}

fn next_table(entry: u64) -> usize {
    (entry & 0x0000_FFFF_FFFF_F000) as usize
}

fn l0_index(va: usize) -> usize {
    (va >> 39) & 0x1FF
}
fn l1_index(va: usize) -> usize {
    (va >> 30) & 0x1FF
}
fn l2_index(va: usize) -> usize {
    (va >> 21) & 0x1FF
}
fn l3_index(va: usize) -> usize {
    (va >> 12) & 0x1FF
}

/// Allocate a cell root: MMIO gigablock, kernel RAM as 2 MiB supervisor
/// blocks, and the `.user` slot delegated to a level-3 table.
pub fn paging_new_root() -> PagingRoot {
    let l0_pa = frames::alloc();
    let l1_pa = frames::alloc();
    let l0 = table_mut(l0_pa);
    l0[l0_index(0)] = addr_bits(l1_pa) | TABLE | VALID;

    let l1 = table_mut(l1_pa);
    // 1 GiB device block at VA 0.
    l1[l1_index(DEVICE_BASE)] =
        addr_bits(DEVICE_BASE) | ATTR_DEVICE | AP_RW_EL1 | AF | UXN | PXN | BLOCK | VALID;

    // Kernel RAM gigabyte as a level-2 table of 2 MiB supervisor blocks,
    // with the `.user` slot carved out to a level-3 table.
    let l2_pa = frames::alloc();
    let l2 = table_mut(l2_pa);
    let user_slot = user_window_base() & !(MIB2 - 1);
    let mut va = RAM_BASE;
    while va < RAM_BASE + GIB {
        let idx = l2_index(va);
        if va == user_slot {
            let l3_pa = frames::alloc();
            l2[idx] = addr_bits(l3_pa) | TABLE | VALID; // empty L3 for now
        } else {
            l2[idx] = addr_bits(va) | ATTR_NORMAL | SH_INNER | AP_RW_EL1 | AF | UXN | BLOCK | VALID;
        }
        va += MIB2;
    }
    l1[l1_index(RAM_BASE)] = addr_bits(l2_pa) | TABLE | VALID;

    PagingRoot { l0_pa }
}

/// Map one 4 KiB identity page for user access inside the `.user` window.
pub fn paging_map(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let base = user_window_base();
    assert!(
        (base..base + MIB2).contains(&va),
        "user map {va:#x} outside the .user window"
    );

    let l0 = table_mut(root.l0_pa);
    let l1 = table_mut(next_table(l0[l0_index(va)]));
    let l2 = table_mut(next_table(l1[l1_index(va)]));
    let l3 = table_mut(next_table(l2[l2_index(va)]));

    let (ap, xn) = match perm {
        MapPerm::UserRo => (AP_RO_ALL, UXN | PXN),
        MapPerm::UserRw => (AP_RW_ALL, UXN | PXN),
        MapPerm::UserRx => (AP_RO_ALL, PXN), // EL0-executable (UXN clear)
    };
    l3[l3_index(va)] = addr_bits(va) | ATTR_NORMAL | SH_INNER | ap | AF | NG | xn | TABLE | VALID;
}

/// Get the next-level table a descriptor points at, creating an empty one
/// (a table descriptor) if the slot is not yet valid.
fn ensure_table(parent: &mut [u64; 512], idx: usize) -> usize {
    if parent[idx] & VALID == 0 {
        let t = frames::alloc(); // zeroed
        parent[idx] = addr_bits(t) | TABLE | VALID;
    }
    next_table(parent[idx])
}

/// Map one 4 KiB frame `pa` at an arbitrary user `va`, creating intermediate
/// tables as needed (docs/USERLAND.md). `va` must avoid the MMIO/kernel-RAM
/// gigabytes the cell root maps supervisor (0-2 GiB); userland links at
/// 4 GiB+.
pub fn paging_map_frame(root: &mut PagingRoot, va: usize, pa: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let l0 = table_mut(root.l0_pa);
    let l1 = table_mut(ensure_table(l0, l0_index(va)));
    let l2 = table_mut(ensure_table(l1, l1_index(va)));
    let l3 = table_mut(ensure_table(l2, l2_index(va)));
    let (ap, xn) = match perm {
        MapPerm::UserRo => (AP_RO_ALL, UXN | PXN),
        MapPerm::UserRw => (AP_RW_ALL, UXN | PXN),
        MapPerm::UserRx => (AP_RO_ALL, PXN), // EL0-executable (UXN clear)
    };
    l3[l3_index(va)] = addr_bits(pa) | ATTR_NORMAL | SH_INNER | ap | AF | NG | xn | TABLE | VALID;
}

/// Activate a root: TTBR0_EL1 with ASID in the high bits, flush that
/// ASID's stale entries (roots are recreated per run reusing ASIDs), isb.
pub fn paging_activate(root: &PagingRoot, asid: u16) {
    let ttbr0 = ((asid as u64) << 48) | (root.l0_pa as u64);
    // SAFETY: ttbr0 points at a well-formed root; the barriers order the
    // switch and the ASID-scoped TLB invalidation.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "dsb ishst",
            "tlbi aside1is, {asid}",
            "dsb ish",
            "isb",
            ttbr = in(reg) ttbr0,
            asid = in(reg) (asid as u64) << 48,
        );
    }
}

static mut KERNEL_ROOT_PA: usize = 0;

/// Re-activate the kernel's own address space (ASID 0).
pub fn paging_activate_kernel() {
    let l0_pa = unsafe { *core::ptr::addr_of!(KERNEL_ROOT_PA) };
    paging_activate(&PagingRoot { l0_pa }, 0);
}

/// Build the kernel address space and turn the MMU on. Kernel root: MMIO
/// gigablock + kernel RAM gigablock, both supervisor.
pub fn paging_kernel_init() {
    frames::init();

    let l0_pa = frames::alloc();
    let l1_pa = frames::alloc();
    let l0 = table_mut(l0_pa);
    l0[l0_index(0)] = addr_bits(l1_pa) | TABLE | VALID;
    let l1 = table_mut(l1_pa);
    l1[l1_index(DEVICE_BASE)] =
        addr_bits(DEVICE_BASE) | ATTR_DEVICE | AP_RW_EL1 | AF | UXN | PXN | BLOCK | VALID;
    // Kernel RAM gigablock: supervisor RWX (EL1 exec, no EL0).
    l1[l1_index(RAM_BASE)] =
        addr_bits(RAM_BASE) | ATTR_NORMAL | SH_INNER | AP_RW_EL1 | AF | UXN | BLOCK | VALID;

    unsafe {
        *core::ptr::addr_of_mut!(KERNEL_ROOT_PA) = l0_pa;

        // MAIR: attr0 = Device-nGnRnE (0x00), attr1 = Normal WB (0xFF).
        let mair: u64 = 0xFF << 8;
        // TCR: T0SZ=16 (48-bit), TG0=4KB, inner-shareable WB-WA, IPS=40-bit,
        // disable the TTBR1 walk (EPD1).
        let tcr: u64 = 16 | (0b11 << 8) | (0b01 << 10) | (0b01 << 12) | (0b10 << 32) | (1 << 23);
        // CNTKCTL: allow EL0 to read the virtual + physical counters.
        let cntkctl: u64 = (1 << 0) | (1 << 1);
        core::arch::asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr}",
            "msr cntkctl_el1, {cnt}",
            "isb",
            mair = in(reg) mair,
            tcr = in(reg) tcr,
            ttbr = in(reg) l0_pa as u64,
            cnt = in(reg) cntkctl,
        );
        // Enable MMU + caches; SPAN=1 keeps PSTATE.PAN clear on exception
        // entry so the EL1 handler can touch a cell's EL0 ring.
        let mut sctlr: u64;
        core::arch::asm!("mrs {0}, sctlr_el1", out(reg) sctlr);
        sctlr |= (1 << 0) | (1 << 2) | (1 << 12) | (1 << 23);
        core::arch::asm!(
            "msr sctlr_el1, {0}",
            "isb",
            in(reg) sctlr,
        );
    }
}
