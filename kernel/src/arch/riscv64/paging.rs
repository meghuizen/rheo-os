//! RISC-V Sv39 paging (BUILD-ORDER.md step 3). Three levels, 512 entries
//! each, 4 KiB pages with 1 GiB and 2 MiB superpages.
//!
//! Higher-half split (docs/MEMORY.md): the kernel, all MMIO, and the `.user`
//! window are linked in the high canonical half (`phys_to_virt(pa) =
//! pa | KERNEL_VA_BASE`), built once by the boot trampoline. RISC-V has a
//! single `satp`, so a cell root still carries the kernel + MMIO (mapped
//! supervisor, high) - a trap enters with the cell root active and must reach
//! the handler - but the whole LOW half is left free, so a stock Linux
//! ET_EXEC (0x10000) loads unmodified. Unlike aarch64 the `.user` window sits
//! high too (RISC-V has no "large" code model; keeping it adjacent to the
//! kernel keeps every kernel->`.user` reference within medany's +-2 GiB
//! reach). Isolation is unchanged: it is the U bit on the leaf PTE, not the
//! address, that gates a cell.
//!
//! Address-space layout:
//! - VA base+0..1 GiB -> gigapage, supervisor R|W: all MMIO (UART, test
//!   device, PLIC, virtio, PCIe ECAM). Shared, identical in every root.
//! - VA base+2..3 GiB -> kernel RAM. A kernel root maps it as one supervisor
//!   R|W|X gigapage; a cell root maps it as a level-1 table of 2 MiB
//!   supervisor superpages with the one `.user` slot delegated to a level-0
//!   table where user pages carry the U bit.
//! - The low half is unmapped in a cell root except the pages the loader adds
//!   (paging_map_frame) - a stock ET_EXEC, its stack, mmap arenas.
//!
//! `table_mut` reaches page-table frames through the kernel's high linear map
//! (`super::phys_to_virt`); the kernel no longer identity-maps RAM low.

use crate::arch::MapPerm;
use crate::mm::frames;
use core::arch::asm;

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_G: u64 = 1 << 5;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;
/// Sv39 reserves bits 8-9 (`RSW`) for software. Bit 8 marks a page a
/// **copy-on-write** page: it was writable, `fork` cleared the write bit, and the
/// next write must private it rather than fault the process
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). The hardware ignores it.
const PTE_COW: u64 = 1 << 8;

const PAGE_SIZE: usize = 4096;
const GIB: usize = 1 << 30;
const MIB2: usize = 2 << 20;

/// Physical base of kernel RAM (QEMU virt: RAM starts at 2 GiB).
const RAM_PA_BASE: usize = 2 * GIB;

// The `.user` window (linker script: 2 MiB aligned, 2 MiB long, high VMA).
unsafe extern "C" {
    static __user_start: u8;
}

fn user_window_base() -> usize {
    core::ptr::addr_of!(__user_start) as usize
}

/// A page-table root: the physical address of the level-2 table.
#[derive(Copy, Clone)]
pub struct PagingRoot {
    l2_pa: usize,
}

fn l2_index(va: usize) -> usize {
    (va >> 30) & 0x1FF
}

fn pte_to_table(pte: u64) -> usize {
    // PPN is bits [53:10]; shift left 12 minus the 10-bit PPN offset.
    (((pte >> 10) & ((1 << 44) - 1)) as usize) << 12
}

fn table_to_pte(pa: usize, flags: u64) -> u64 {
    (((pa >> 12) as u64) << 10) | flags
}

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is an allocated table frame, reached through the kernel's
    // high linear map. The kernel runs high, so a physical frame is touched at
    // phys_to_virt(pa), never at its raw physical address. The kernel is the
    // sole writer of page tables.
    unsafe { &mut *(super::phys_to_virt(pa) as *mut [u64; 512]) }
}

/// Allocate a fresh cell root: kernel + MMIO mapped supervisor HIGH (so a trap
/// entering with this root active reaches the handler), the low half left free
/// for the loader. The kernel-RAM gigaregion is a level-1 table of supervisor
/// superpages with the one `.user` slot delegated to a level-0 table where user
/// pages carry the U bit.
pub fn paging_new_root() -> PagingRoot {
    let l2_pa = frames::alloc().expect("root page table (boot, reserve held)");
    let l2 = table_mut(l2_pa);

    // High MMIO gigapage (0..1 GiB), supervisor R|W.
    let mmio_va = super::phys_to_virt(0);
    l2[l2_index(mmio_va)] = table_to_pte(0, PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D);

    // High kernel RAM (2..3 GiB): level-1 table of 2 MiB supervisor superpages,
    // with the `.user` slot delegated to a per-cell level-0 table.
    let l1_pa = frames::alloc().expect("page table (boot, reserve held)");
    let l1 = table_mut(l1_pa);
    let user_slot_va = user_window_base() & !(MIB2 - 1);
    for (i, entry) in l1.iter_mut().enumerate() {
        let pa = RAM_PA_BASE + i * MIB2;
        let va = super::phys_to_virt(pa);
        if va == user_slot_va {
            // Delegate this 2 MiB slot to a per-cell level-0 table; user pages
            // are added later by paging_map. Empty for now.
            let l0_pa = frames::alloc().expect("page table (boot, reserve held)");
            *entry = table_to_pte(l0_pa, PTE_V); // pointer PTE (no R/W/X)
        } else {
            *entry = table_to_pte(pa, PTE_V | PTE_R | PTE_W | PTE_X | PTE_G | PTE_A | PTE_D);
        }
    }
    let ram_va = super::phys_to_virt(RAM_PA_BASE);
    l2[l2_index(ram_va)] = table_to_pte(l1_pa, PTE_V); // pointer to the level-1 table

    PagingRoot { l2_pa }
}

/// Map one 4 KiB page for user access inside the `.user` window. `va` must lie
/// in the (high) `.user` window and be page aligned; the backing frame is the
/// window's own physical page (`virt_to_phys(va)`), so this is an identity map
/// in the linear sense, carrying the U bit.
pub fn paging_map(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let user_base = user_window_base();
    assert!(
        (user_base..user_base + MIB2).contains(&va),
        "user map {va:#x} outside the .user window"
    );

    let l2 = table_mut(root.l2_pa);
    let ram_va = super::phys_to_virt(RAM_PA_BASE);
    let l1 = table_mut(pte_to_table(l2[l2_index(ram_va)]));
    let l1_idx = (va - ram_va) / MIB2;
    let l0 = table_mut(pte_to_table(l1[l1_idx]));
    let l0_idx = (va % MIB2) / PAGE_SIZE;

    let rights = match perm {
        MapPerm::UserRo => PTE_R,
        MapPerm::UserRw => PTE_R | PTE_W,
        MapPerm::UserRx => PTE_R | PTE_X,
        MapPerm::UserRwx => PTE_R | PTE_W | PTE_X,
        // Device MMIO. Identical bits to `UserRw`, because a base Sv39 PTE has **no**
        // cacheability field - the attribute is a property of the physical region
        // here, and Svpbmt (which would add one) is absent from QEMU 8.2. Named
        // rather than faked (docs/DRIVERS.md 4.1).
        MapPerm::UserDevice => PTE_R | PTE_W,
    };
    let pa = super::virt_to_phys(va);
    l0[l0_idx] = table_to_pte(pa, PTE_V | PTE_U | PTE_A | PTE_D | rights);
}

/// Get the level-N table a parent entry points at, creating an empty one
/// (a pointer PTE, no R/W/X) if the slot is not yet valid.
fn ensure_table(parent: &mut [u64; 512], idx: usize) -> usize {
    if parent[idx] & PTE_V == 0 {
        let t = frames::alloc().expect("page table (user reserve held)"); // zeroed
        parent[idx] = table_to_pte(t, PTE_V);
    }
    pte_to_table(parent[idx])
}

/// Map one 4 KiB frame `pa` at an arbitrary user `va`, creating intermediate
/// tables as needed (docs/USERLAND.md). The cell root leaves the whole low
/// half free, so any low `va` is available - including a stock ET_EXEC at
/// 0x10000, its stack (8 GiB), and mmap arenas (12 GiB).
pub fn paging_map_frame(root: &mut PagingRoot, va: usize, pa: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let l2 = table_mut(root.l2_pa);
    let l1 = table_mut(ensure_table(l2, (va >> 30) & 0x1FF));
    let l0 = table_mut(ensure_table(l1, (va >> 21) & 0x1FF));
    let rights = match perm {
        MapPerm::UserRo => PTE_R,
        MapPerm::UserRw => PTE_R | PTE_W,
        MapPerm::UserRx => PTE_R | PTE_X,
        MapPerm::UserRwx => PTE_R | PTE_W | PTE_X,
        // Device MMIO. Identical bits to `UserRw`, because a base Sv39 PTE has **no**
        // cacheability field - the attribute is a property of the physical region
        // here, and Svpbmt (which would add one) is absent from QEMU 8.2. Named
        // rather than faked (docs/DRIVERS.md 4.1).
        MapPerm::UserDevice => PTE_R | PTE_W,
    };
    l0[(va >> 12) & 0x1FF] = table_to_pte(pa, PTE_V | PTE_U | PTE_A | PTE_D | rights);
}

/// Walk to the 4 KiB leaf for `va`, returning `(l0_pa, l0_index)` if every
/// level is a valid pointer PTE (no superpage leaf on the way). Used by
/// unmap/protect on user VAs (docs/LINUX-COMPAT.md L2).
fn leaf(root: &PagingRoot, va: usize) -> Option<(usize, usize)> {
    let l2 = table_mut(root.l2_pa);
    let e = l2[(va >> 30) & 0x1FF];
    // A pointer PTE has V set and R/W/X clear; anything else is unmapped or a
    // gigapage leaf (never a user 4 KiB page).
    if e & PTE_V == 0 || e & (PTE_R | PTE_W | PTE_X) != 0 {
        return None;
    }
    let l1 = table_mut(pte_to_table(e));
    let e = l1[(va >> 21) & 0x1FF];
    if e & PTE_V == 0 || e & (PTE_R | PTE_W | PTE_X) != 0 {
        return None;
    }
    Some((pte_to_table(e), (va >> 12) & 0x1FF))
}

/// Clear the write bit on every **writable** user leaf and mark it copy-on-write,
/// returning how many were changed - the `fork` half of COW
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). The caller re-activates the root to
/// flush, and [`paging_cow_at`] recognises the pages afterwards.
///
/// Written here rather than as a loop over `paging_protect` in portable code because
/// it rewrites the very leaf entries a walk would be iterating over, and because a
/// software PTE bit is not something `MapPerm` can express.
pub fn paging_cow_protect_user(root: &mut PagingRoot) -> usize {
    let mut n = 0usize;
    for_each_user_leaf_slot(root, &mut |e: &mut u64| {
        if *e & PTE_W != 0 {
            *e = (*e & !PTE_W) | PTE_COW;
            n += 1;
        }
    });
    n
}

/// The frame behind `va` if its leaf is marked copy-on-write, else `None` - asked by
/// the page-fault handler to tell "this write must private a shared page" from "this
/// write is genuinely refused".
pub fn paging_cow_at(root: &PagingRoot, va: usize) -> Option<usize> {
    let (l0_pa, idx) = leaf(root, va)?;
    let e = table_mut(l0_pa)[idx];
    if e & PTE_V == 0 || e & PTE_COW == 0 {
        return None;
    }
    Some(pte_to_table(e))
}

/// Resolve a copy-on-write page: clear the COW mark, restore write access, and
/// repoint the leaf at `new_pa` when the page had to be copied (`None` keeps the
/// frame - the case where this mapping turned out to be the only holder). The caller
/// re-activates the root to flush the stale read-only entry.
pub fn paging_cow_clear(root: &mut PagingRoot, va: usize, new_pa: Option<usize>) {
    let Some((l0_pa, idx)) = leaf(root, va) else {
        return;
    };
    let l0 = table_mut(l0_pa);
    let e = l0[idx];
    let pa = new_pa.unwrap_or_else(|| pte_to_table(e));
    l0[idx] = table_to_pte(pa, (e & 0x3FF & !PTE_COW) | PTE_W);
}

/// Invoke `f` on every mapped 4 KiB **user** leaf entry of `root`, by mutable
/// reference so the callback may rewrite it in place. Kept private: handing out a
/// raw PTE is a paging-internal thing, and the portable callers want
/// [`paging_for_each_user_leaf`]'s decoded `(va, pa, perm)` instead.
fn for_each_user_leaf_slot(root: &mut PagingRoot, f: &mut dyn FnMut(&mut u64)) {
    for &e2 in table_mut(root.l2_pa).iter() {
        if e2 & PTE_V == 0 || e2 & (PTE_R | PTE_W | PTE_X) != 0 {
            continue;
        }
        for &e1 in table_mut(pte_to_table(e2)).iter() {
            if e1 & PTE_V == 0 || e1 & (PTE_R | PTE_W | PTE_X) != 0 {
                continue;
            }
            for e0 in table_mut(pte_to_table(e1)).iter_mut() {
                if *e0 & PTE_V == 0 || *e0 & PTE_U == 0 {
                    continue;
                }
                f(e0);
            }
        }
    }
}

/// Invoke `f(va, pa, perm)` for every mapped 4 KiB **user** leaf (U bit set) in
/// `root` - the primitive `fork`/`execve` teardown build on (docs/LINUX-COMPAT.md
/// L6). Supervisor mappings (the shared kernel + MMIO superpages, U bit clear)
/// are skipped, so only the cell's own pages are visited.
pub fn paging_for_each_user_leaf(root: &PagingRoot, f: &mut dyn FnMut(usize, usize, MapPerm)) {
    let l2 = table_mut(root.l2_pa);
    for (i2, &e2) in l2.iter().enumerate() {
        // A pointer PTE has V set and R/W/X clear; a leaf (gigapage) is
        // supervisor kernel/MMIO - never a user 4 KiB page.
        if e2 & PTE_V == 0 || e2 & (PTE_R | PTE_W | PTE_X) != 0 {
            continue;
        }
        let l1 = table_mut(pte_to_table(e2));
        for (i1, &e1) in l1.iter().enumerate() {
            if e1 & PTE_V == 0 || e1 & (PTE_R | PTE_W | PTE_X) != 0 {
                continue; // unmapped or a 2 MiB supervisor superpage
            }
            let l0 = table_mut(pte_to_table(e1));
            for (i0, &e0) in l0.iter().enumerate() {
                if e0 & PTE_V == 0 || e0 & PTE_U == 0 {
                    continue;
                }
                let va = (i2 << 30) | (i1 << 21) | (i0 << 12);
                let pa = pte_to_table(e0);
                let perm = if e0 & PTE_W != 0 {
                    MapPerm::UserRw
                } else if e0 & PTE_X != 0 {
                    MapPerm::UserRx
                } else {
                    MapPerm::UserRo
                };
                f(va, pa, perm);
            }
        }
    }
}

/// Clear the leaf mapping at `va`, returning the physical frame it pointed at
/// (for the caller to `frames::free`), or None if it was not mapped. The
/// caller flushes the TLB by re-activating the root (docs/LINUX-COMPAT.md L2).
pub fn paging_unmap_frame(root: &mut PagingRoot, va: usize) -> Option<usize> {
    let (l0_pa, idx) = leaf(root, va)?;
    let l0 = table_mut(l0_pa);
    if l0[idx] & PTE_V == 0 {
        return None;
    }
    let pa = pte_to_table(l0[idx]);
    l0[idx] = 0;
    Some(pa)
}

/// How many bytes from `va` are **certainly unmapped**, because a table above the
/// leaf level is absent - `0` when a leaf lookup is needed to answer.
///
/// A range walk that steps one 4 KiB page at a time is O(range), which is fine for
/// the mappings a program actually touches and hopeless for the ones it merely
/// reserves. JavaScriptCore's Gigacage is a single 128 GiB `PROT_NONE` reservation
/// (33 million pages, each a four-level walk) and the per-ISA `mmap` window is now
/// terabytes wide, so "step every page" is not a slow path, it is a hang.
///
/// This lets the portable walker skip the empty gigapage or megapage in one step
/// instead of 512 or 262,144 of them. It is deliberately conservative: it never
/// claims a *mapped* span, only an absent one, so a caller that ignores it is still
/// correct - just slow.
pub fn paging_unmapped_span(root: &PagingRoot, va: usize) -> usize {
    const GIB: usize = 1 << 30;
    const MIB2: usize = 1 << 21;
    let l2 = table_mut(root.l2_pa);
    let e = l2[(va >> 30) & 0x1FF];
    if e & PTE_V == 0 {
        return GIB - (va & (GIB - 1));
    }
    // A gigapage leaf is mapped, not a gap - let the leaf walk answer.
    if e & (PTE_R | PTE_W | PTE_X) != 0 {
        return 0;
    }
    let l1 = table_mut(pte_to_table(e));
    let e = l1[(va >> 21) & 0x1FF];
    if e & PTE_V == 0 {
        return MIB2 - (va & (MIB2 - 1));
    }
    0
}

/// Rewrite the leaf permission bits at `va`, keeping the mapped frame. A no-op
/// if `va` is unmapped. The caller flushes the TLB by re-activating the root.
/// Whether `va` has a **live 4 KiB leaf** in `root` - the question a demand-paging
/// fault handler has to answer before anything else: is this "the page is not
/// there" (populate it and retry) or "the page is there and the access was
/// refused" (a genuine SIGSEGV)? Without it a permission fault on a populated
/// page would be re-populated and re-faulted forever.
///
/// Portable callers reach this through `mm::AddressSpace::is_mapped`.
pub fn paging_mapped(root: &PagingRoot, va: usize) -> bool {
    match leaf(root, va) {
        Some((l0_pa, idx)) => table_mut(l0_pa)[idx] & PTE_V != 0,
        None => false,
    }
}

pub fn paging_protect(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    if let Some((l0_pa, idx)) = leaf(root, va) {
        let l0 = table_mut(l0_pa);
        if l0[idx] & PTE_V == 0 {
            return;
        }
        let pa = pte_to_table(l0[idx]);
        let rights = match perm {
            MapPerm::UserRo => PTE_R,
            MapPerm::UserRw => PTE_R | PTE_W,
            MapPerm::UserRx => PTE_R | PTE_X,
            MapPerm::UserRwx => PTE_R | PTE_W | PTE_X,
            // Device MMIO. Identical bits to `UserRw`, because a base Sv39 PTE has **no**
            // cacheability field - the attribute is a property of the physical region
            // here, and Svpbmt (which would add one) is absent from QEMU 8.2. Named
            // rather than faked (docs/DRIVERS.md 4.1).
            MapPerm::UserDevice => PTE_R | PTE_W,
        };
        l0[idx] = table_to_pte(pa, PTE_V | PTE_U | PTE_A | PTE_D | rights);
    }
}

/// Activate a root: write satp (Sv39, ASID-tagged) and fence.
pub fn paging_activate(root: &PagingRoot, asid: u16) {
    let satp = (8u64 << 60) | ((asid as u64) << 44) | ((root.l2_pa >> 12) as u64);
    // SAFETY: satp points at a well-formed root; sfence orders the switch.
    unsafe {
        asm!(
            "csrw satp, {0}",
            "sfence.vma zero, {1}",
            in(reg) satp,
            in(reg) asid as u64,
        );
    }
}

unsafe extern "C" {
    /// Physical address of the boot-built kernel working root, written by the
    /// boot trampoline (kernel/arch/riscv64/boot.S) once the MMU is on.
    static KERNEL_ROOT_PA: u64;
}

/// Re-activate the kernel's own address space (ASID 0). Called when a cell run
/// returns, so kernel setup code can again reach all of RAM (a cell root only
/// maps that cell's user pages in the low half). The kernel root maps the
/// kernel + MMIO high (supervisor gigapages) plus a low identity of kernel RAM
/// left over from the boot turn-on.
pub fn paging_activate_kernel() {
    let l2_pa = unsafe { core::ptr::addr_of!(KERNEL_ROOT_PA).read() } as usize;
    paging_activate(&PagingRoot { l2_pa }, 0);
}

/// Persistent-memory mapping window (docs/MEMORY.md real-PMEM path). QEMU's
/// riscv `virt` machine has **no** nvdimm support (the `virt-machine.nvdimm`
/// property does not exist in QEMU 8.2), so no `MemKind::Pmem` region is ever
/// discovered on RISC-V and this is never called at runtime (pmem skips-with-
/// reason here). The Sv39 high-half linear map covers the physical range, so the
/// inert fallback is simply `phys_to_virt`.
pub fn pmem_map_window(base_pa: usize, _len: usize) -> usize {
    super::phys_to_virt(base_pa)
}

/// Device-MMIO mapping window (docs/GPU-HARDWARE.md 12 stage 1). The boot
/// map's device gigapage covers phys 0..1 GiB (UART, PLIC, virtio, ECAM),
/// and RAM starts at 2 GiB - but QEMU virt's PCIe MMIO window (where a PCI
/// BAR lands) is the 1..2 GiB gap in between. Install the missing
/// supervisor R|W gigapage in the KERNEL root so `phys_to_virt` reaches
/// it (measured: a store to an assigned BAR faulted with scause 0xf until
/// this page existed). Only the kernel root gains it - cell roots are
/// untouched, exactly like the x86-64 window. Idempotent.
pub fn mmio_map_window(base_pa: usize, _len: usize) -> usize {
    let va = super::phys_to_virt(base_pa);
    let l2_pa = unsafe { core::ptr::addr_of!(KERNEL_ROOT_PA).read() } as usize;
    let l2 = table_mut(l2_pa);
    let idx = l2_index(va);
    if l2[idx] & PTE_V == 0 {
        let pa_gig = base_pa & !(GIB - 1);
        l2[idx] = table_to_pte(pa_gig, PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D);
        paging_activate_kernel(); // sfence: make the new gigapage visible
    }
    va
}

/// Finish paging bring-up. The MMU and the kernel working root are already
/// configured by the boot trampoline, which enabled paging and jumped the
/// kernel to its high VAs before any Rust ran. All that is left is the frame
/// allocator and the S-mode CSR bits U-mode relies on.
pub fn paging_kernel_init() {
    frames::init();
    super::user_mode_init_this_cpu();
}
