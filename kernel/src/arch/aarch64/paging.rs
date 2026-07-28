//! ARM64 paging (BUILD-ORDER.md step 3): 4 KiB granule, 48-bit VAs, four
//! levels (L0 512 GiB, L1 1 GiB, L2 2 MiB, L3 4 KiB).
//!
//! Higher-half split (docs/MEMORY.md): the kernel + MMIO live in TTBR1_EL1
//! (built once by the boot trampoline, shared by every cell), so a cell's
//! **TTBR0_EL1 root maps only that cell's user pages** and the whole low half
//! is free. The one 2 MiB slot holding the `.user` window is a level-3 table
//! where per-cell user pages carry EL0 access; loader/stack pages are added at
//! arbitrary low VAs on demand. Isolation is the MMU faulting on a page a
//! cell's root does not map; W^X is UXN/PXN + AP. `table_mut` reaches page-
//! table frames through the kernel's high linear map (`super::phys_to_virt`).

use crate::arch::MapPerm;
use crate::mm::frames;

// Descriptor bits.
const VALID: u64 = 1 << 0;
const TABLE: u64 = 1 << 1; // table (at L0-L2) or page (at L3)
const ATTR_NORMAL: u64 = 1 << 2; // MAIR index 1
const AP_RW_ALL: u64 = 0b01 << 6; // RW at EL1 and EL0
const AP_RO_ALL: u64 = 0b11 << 6; // RO at EL1 and EL0
const SH_INNER: u64 = 0b11 << 8;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11; // ASID-tagged (per-cell) entry
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
/// AArch64 stage-1 reserves bits 55-58 for software. Bit 55 marks a
/// **copy-on-write** page: it was writable, `fork` made it read-only, and the next
/// write must private it rather than fault the process
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). The hardware ignores it.
const COW: u64 = 1 << 55;

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
    l0_pa: usize,
}

fn addr_bits(pa: usize) -> u64 {
    // Output address occupies bits [47:12]; low bits are attributes.
    (pa as u64) & 0x0000_FFFF_FFFF_F000
}

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is an allocated table frame, reached through the kernel's
    // high linear map (TTBR1). The kernel runs high, so a physical frame is
    // touched at phys_to_virt(pa), never at its raw physical address.
    unsafe { &mut *(super::phys_to_virt(pa) as *mut [u64; 512]) }
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

/// Allocate a cell root (TTBR0_EL1, low half): maps ONLY user pages. The
/// kernel and MMIO live in TTBR1_EL1 (set once at boot, shared by every cell),
/// so a cell root carries no device/kernel-RAM blocks and the whole low half
/// is free - a stock ET_EXEC at 0x400000 loads unmodified. Just the `.user`
/// 2 MiB slot is pre-built as a level-3 table; the loader adds program/stack
/// pages on demand via `paging_map_frame`.
pub fn paging_new_root() -> PagingRoot {
    let l0_pa = frames::alloc().expect("root page table (boot, reserve held)");
    let l0 = table_mut(l0_pa);
    let slot = user_window_base() & !(MIB2 - 1);
    let l1 = table_mut(ensure_table(l0, l0_index(slot)));
    let l2 = table_mut(ensure_table(l1, l1_index(slot)));
    let _l3 = ensure_table(l2, l2_index(slot)); // empty L3 for paging_map
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

    // The third element is the memory attribute + shareability. Device MMIO selects
    // MAIR attr **0** (Device-nGnRnE, set by `boot.S` and used by the kernel's own MMIO
    // window) and no inner-shareable hint, which is meaningless for device memory; a
    // Normal-cacheable device mapping would let a status register read return a stale
    // line (docs/DRIVERS.md 4.1).
    let (ap, xn, attr) = match perm {
        MapPerm::UserRo => (AP_RO_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserRw => (AP_RW_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserRx => (AP_RO_ALL, PXN, ATTR_NORMAL | SH_INNER), // EL0-exec (UXN clear)
        // Writable AND EL0-executable: PXN keeps EL1 from executing it (the kernel
        // must never run a cell's JIT output), UXN clear lets EL0.
        MapPerm::UserRwx => (AP_RW_ALL, PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserDevice => (AP_RW_ALL, UXN | PXN, 0),
    };
    l3[l3_index(va)] = addr_bits(va) | attr | ap | AF | NG | xn | TABLE | VALID;
}

/// Get the next-level table a descriptor points at, creating an empty one
/// (a table descriptor) if the slot is not yet valid.
fn ensure_table(parent: &mut [u64; 512], idx: usize) -> usize {
    if parent[idx] & VALID == 0 {
        let t = frames::alloc().expect("page table (user reserve held)"); // zeroed
        parent[idx] = addr_bits(t) | TABLE | VALID;
    }
    next_table(parent[idx])
}

/// Map one 4 KiB frame `pa` at an arbitrary user `va`, creating intermediate
/// tables as needed (docs/USERLAND.md). The cell root maps only user pages
/// (kernel + MMIO are in TTBR1), so any low `va` is available - including a
/// stock ET_EXEC at 0x400000.
pub fn paging_map_frame(root: &mut PagingRoot, va: usize, pa: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let l0 = table_mut(root.l0_pa);
    let l1 = table_mut(ensure_table(l0, l0_index(va)));
    let l2 = table_mut(ensure_table(l1, l1_index(va)));
    let l3 = table_mut(ensure_table(l2, l2_index(va)));
    // The third element is the memory attribute + shareability. Device MMIO selects
    // MAIR attr **0** (Device-nGnRnE, set by `boot.S` and used by the kernel's own MMIO
    // window) and no inner-shareable hint, which is meaningless for device memory; a
    // Normal-cacheable device mapping would let a status register read return a stale
    // line (docs/DRIVERS.md 4.1).
    let (ap, xn, attr) = match perm {
        MapPerm::UserRo => (AP_RO_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserRw => (AP_RW_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserRx => (AP_RO_ALL, PXN, ATTR_NORMAL | SH_INNER), // EL0-exec (UXN clear)
        // Writable AND EL0-executable: PXN keeps EL1 from executing it (the kernel
        // must never run a cell's JIT output), UXN clear lets EL0.
        MapPerm::UserRwx => (AP_RW_ALL, PXN, ATTR_NORMAL | SH_INNER),
        MapPerm::UserDevice => (AP_RW_ALL, UXN | PXN, 0),
    };
    l3[l3_index(va)] = addr_bits(pa) | attr | ap | AF | NG | xn | TABLE | VALID;
}

/// Walk to the 4 KiB leaf for `va`, returning `(l3_pa, l3_index)` if every
/// level is a valid table descriptor (no block leaf on the way). Used by
/// unmap/protect on user VAs (docs/LINUX-COMPAT.md L2).
fn leaf(root: &PagingRoot, va: usize) -> Option<(usize, usize)> {
    let l0 = table_mut(root.l0_pa);
    let e = l0[l0_index(va)];
    if e & VALID == 0 {
        return None;
    }
    let l1 = table_mut(next_table(e));
    let e = l1[l1_index(va)];
    // TABLE bit distinguishes a table descriptor from a 1 GiB/2 MiB block
    // (kernel supervisor RAM); a user 4 KiB page only sits under tables.
    if e & VALID == 0 || e & TABLE == 0 {
        return None;
    }
    let l2 = table_mut(next_table(e));
    let e = l2[l2_index(va)];
    if e & VALID == 0 || e & TABLE == 0 {
        return None;
    }
    Some((next_table(e), l3_index(va)))
}

/// How many bytes from `va` are **certainly unmapped**, because a table above the
/// leaf level is absent - `0` when a leaf lookup is needed to answer. See the
/// riscv64 twin for why a page-at-a-time range walk is a hang rather than a slow
/// path once reservations are terabytes wide.
pub fn paging_unmapped_span(root: &PagingRoot, va: usize) -> usize {
    const GIB512: usize = 1 << 39;
    const GIB: usize = 1 << 30;
    const MIB2: usize = 1 << 21;
    let l0 = table_mut(root.l0_pa);
    let e = l0[l0_index(va)];
    if e & VALID == 0 {
        return GIB512 - (va & (GIB512 - 1));
    }
    let l1 = table_mut(next_table(e));
    let e = l1[l1_index(va)];
    if e & VALID == 0 {
        return GIB - (va & (GIB - 1));
    }
    if e & TABLE == 0 {
        return 0; // a 1 GiB block is mapped, not a gap
    }
    let l2 = table_mut(next_table(e));
    let e = l2[l2_index(va)];
    if e & VALID == 0 {
        return MIB2 - (va & (MIB2 - 1));
    }
    0
}

/// Clear write access on every **writable** user leaf and mark it copy-on-write,
/// returning how many were changed - the `fork` half of COW
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). See the riscv64 twin for why this is
/// per-ISA rather than a loop over `paging_protect` in portable code.
pub fn paging_cow_protect_user(root: &mut PagingRoot) -> usize {
    let mut n = 0usize;
    for_each_user_leaf_slot(root, &mut |e: &mut u64| {
        if *e & (0b11 << 6) == AP_RW_ALL {
            *e = (*e & !(0b11u64 << 6)) | AP_RO_ALL | COW;
            n += 1;
        }
    });
    n
}

/// The frame behind `va` if its leaf is marked copy-on-write, else `None`.
pub fn paging_cow_at(root: &PagingRoot, va: usize) -> Option<usize> {
    let (l3_pa, idx) = leaf(root, va)?;
    let e = table_mut(l3_pa)[idx];
    if e & VALID == 0 || e & COW == 0 {
        return None;
    }
    Some((e & 0x0000_FFFF_FFFF_F000) as usize)
}

/// Resolve a copy-on-write page: clear the mark, restore write access, and repoint
/// the leaf at `new_pa` when the page had to be copied. The caller re-activates the
/// root to flush.
pub fn paging_cow_clear(root: &mut PagingRoot, va: usize, new_pa: Option<usize>) {
    let Some((l3_pa, idx)) = leaf(root, va) else {
        return;
    };
    let l3 = table_mut(l3_pa);
    let e = l3[idx];
    let pa = new_pa.unwrap_or((e & 0x0000_FFFF_FFFF_F000) as usize);
    // A writable page is never executable here (W^X), so the XN bits are the
    // read-write pair, matching `paging_protect`'s `MapPerm::UserRw` arm.
    l3[idx] =
        addr_bits(pa) | ATTR_NORMAL | SH_INNER | AP_RW_ALL | AF | NG | UXN | PXN | TABLE | VALID;
}

/// Invoke `f` on every mapped 4 KiB user leaf entry of `root`, by mutable reference
/// so the callback may rewrite it in place. Private for the same reason as the
/// riscv64 twin: a raw PTE is paging-internal.
fn for_each_user_leaf_slot(root: &mut PagingRoot, f: &mut dyn FnMut(&mut u64)) {
    for &e0 in table_mut(root.l0_pa).iter() {
        if e0 & VALID == 0 || e0 & TABLE == 0 {
            continue;
        }
        for &e1 in table_mut(next_table(e0)).iter() {
            if e1 & VALID == 0 || e1 & TABLE == 0 {
                continue;
            }
            for &e2 in table_mut(next_table(e1)).iter() {
                if e2 & VALID == 0 || e2 & TABLE == 0 {
                    continue;
                }
                for e3 in table_mut(next_table(e2)).iter_mut() {
                    if *e3 & VALID == 0 {
                        continue;
                    }
                    f(e3);
                }
            }
        }
    }
}

/// Invoke `f(va, pa, perm)` for every mapped 4 KiB user leaf in `root` - the
/// primitive `fork`/`execve` teardown build on (docs/LINUX-COMPAT.md L6). A
/// TTBR0 cell root maps only the cell's own user pages (kernel + MMIO are in
/// TTBR1), so every valid level-3 page found is a user page.
pub fn paging_for_each_user_leaf(root: &PagingRoot, f: &mut dyn FnMut(usize, usize, MapPerm)) {
    let l0 = table_mut(root.l0_pa);
    for (i0, &e0) in l0.iter().enumerate() {
        if e0 & VALID == 0 || e0 & TABLE == 0 {
            continue;
        }
        let l1 = table_mut(next_table(e0));
        for (i1, &e1) in l1.iter().enumerate() {
            if e1 & VALID == 0 || e1 & TABLE == 0 {
                continue; // unmapped or a 1 GiB block (never a user 4 KiB page)
            }
            let l2 = table_mut(next_table(e1));
            for (i2, &e2) in l2.iter().enumerate() {
                if e2 & VALID == 0 || e2 & TABLE == 0 {
                    continue; // unmapped or a 2 MiB block
                }
                let l3 = table_mut(next_table(e2));
                for (i3, &e3) in l3.iter().enumerate() {
                    if e3 & VALID == 0 {
                        continue;
                    }
                    let va = (i0 << 39) | (i1 << 30) | (i2 << 21) | (i3 << 12);
                    let pa = (e3 & 0x0000_FFFF_FFFF_F000) as usize;
                    let perm = if e3 & (0b11 << 6) == AP_RW_ALL {
                        MapPerm::UserRw
                    } else if e3 & UXN != 0 {
                        MapPerm::UserRo
                    } else {
                        MapPerm::UserRx
                    };
                    f(va, pa, perm);
                }
            }
        }
    }
}

/// Clear the leaf mapping at `va`, returning the physical frame it pointed at
/// (for the caller to `frames::free`), or None if it was not mapped. The
/// caller flushes the TLB by re-activating the root (docs/LINUX-COMPAT.md L2).
pub fn paging_unmap_frame(root: &mut PagingRoot, va: usize) -> Option<usize> {
    let (l3_pa, idx) = leaf(root, va)?;
    let l3 = table_mut(l3_pa);
    if l3[idx] & VALID == 0 {
        return None;
    }
    let pa = (l3[idx] & 0x0000_FFFF_FFFF_F000) as usize;
    l3[idx] = 0;
    Some(pa)
}

/// Rewrite the leaf permission bits at `va`, keeping the mapped frame. A no-op
/// if `va` is unmapped. The caller flushes the TLB by re-activating the root.
/// Whether `va` has a **live 4 KiB leaf** in `root` - see the riscv64 twin for why
/// a demand-paging fault handler needs this before anything else.
pub fn paging_mapped(root: &PagingRoot, va: usize) -> bool {
    match leaf(root, va) {
        Some((l3_pa, idx)) => table_mut(l3_pa)[idx] & VALID != 0,
        None => false,
    }
}

pub fn paging_protect(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    if let Some((l3_pa, idx)) = leaf(root, va) {
        let l3 = table_mut(l3_pa);
        if l3[idx] & VALID == 0 {
            return;
        }
        let pa = (l3[idx] & 0x0000_FFFF_FFFF_F000) as usize;
        let (ap, xn, attr) = match perm {
            MapPerm::UserRo => (AP_RO_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
            MapPerm::UserRw => (AP_RW_ALL, UXN | PXN, ATTR_NORMAL | SH_INNER),
            MapPerm::UserRx => (AP_RO_ALL, PXN, ATTR_NORMAL | SH_INNER),
            MapPerm::UserRwx => (AP_RW_ALL, PXN, ATTR_NORMAL | SH_INNER),
            // A reprotect to device attributes, kept for exhaustiveness rather than for
            // a caller: `mprotect` is the only path here and no cell can name this perm
            // (a BAR window is mapped once by the launcher, docs/DRIVERS.md 4.1). The
            // arm exists so that adding a device-reprotect path later is a decision
            // taken here rather than a `_` arm silently giving it Normal memory.
            MapPerm::UserDevice => (AP_RW_ALL, UXN | PXN, 0),
        };
        l3[idx] = addr_bits(pa) | attr | ap | AF | NG | xn | TABLE | VALID;
    }
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

unsafe extern "C" {
    /// L0 root of the boot-built low identity map (TTBR0_EL1 at boot). Its
    /// link address is its physical address (`.boot.bss` is identity-mapped).
    static boot_l0_low: u8;
}

/// Re-activate the kernel's low-half working map in TTBR0_EL1 (ASID 0). The
/// kernel proper lives in TTBR1_EL1 and is never switched; TTBR0 carries the
/// boot low identity map so kernel setup code can reach the `.user` window
/// (and RAM) at its low VA between cell runs. A cell run replaces TTBR0 with
/// that cell's root; this restores the working map afterwards.
pub fn paging_activate_kernel() {
    let l0_pa = core::ptr::addr_of!(boot_l0_low) as usize;
    paging_activate(&PagingRoot { l0_pa }, 0);
}

/// Persistent-memory mapping window (docs/MEMORY.md real-PMEM path). QEMU's
/// arm `virt` machine does **not** expose an nvdimm without an ACPI GED device,
/// and this kernel uses a built-in DT-less machine profile with no ACPI/NFIT
/// parser, so no `MemKind::Pmem` region is ever discovered on ARM64 and this is
/// never called at runtime (pmem skips-with-reason here). The kernel's
/// 48-bit-wide linear map covers any physical address, so the inert fallback is
/// simply `phys_to_virt`.
pub fn pmem_map_window(base_pa: usize, _len: usize) -> usize {
    super::phys_to_virt(base_pa)
}

/// Device-MMIO mapping window (docs/GPU-HARDWARE.md 12 stage 1). The
/// 48-bit-wide linear map covers the PCIe MMIO window (a PCI BAR at
/// 0x1000_0000..0x3E00_0000), so this is simply `phys_to_virt` - the same
/// path the ECAM and virtio-mmio accesses already take.
pub fn mmio_map_window(base_pa: usize, _len: usize) -> usize {
    super::phys_to_virt(base_pa)
}

/// Finish paging bring-up. The MMU, TCR, MAIR, TTBR0/TTBR1 and SCTLR are
/// already configured by the boot trampoline (kernel/arch/aarch64/boot.S),
/// which enabled the MMU and jumped the kernel to its high (TTBR1) VAs before
/// any Rust ran. All that is left is the frame allocator.
pub fn paging_kernel_init() {
    frames::init();
}
