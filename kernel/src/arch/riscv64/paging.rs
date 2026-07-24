//! RISC-V Sv39 paging (BUILD-ORDER.md step 3). Three levels, 512 entries
//! each, 4 KiB pages with 1 GiB and 2 MiB superpages.
//!
//! Address-space layout (identity-mapped; isolation comes from *which*
//! frames each root maps with the U bit, not from address separation):
//!
//! - VA 0..1 GiB   -> gigapage, supervisor R|W: all MMIO (UART, test
//!   device, PLIC, CLINT). Shared, identical in every root.
//! - VA 2..3 GiB   -> a per-root level-1 table of 2 MiB supervisor R|W|X
//!   superpages covering kernel RAM (image, page tables, frame pool),
//!   except the one 2 MiB slot holding the `.user` window, which points
//!   to a per-cell level-0 table where user pages carry the U bit.
//!
//! A cell can only reach a user page its own root maps U; another cell's
//! root does not map it, so the MMU faults. W^X is enforced by the R/W/X
//! bits per page.

use crate::arch::MapPerm;
use crate::mm::frames;

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_G: u64 = 1 << 5;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const PAGE_SIZE: usize = 4096;
const GIB: usize = 1 << 30;
const MIB2: usize = 2 << 20;

// The `.user` window (linker script: 2 MiB aligned, 2 MiB long).
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

fn pte_to_table(pte: u64) -> usize {
    // PPN is bits [53:10]; shift left 12 minus the 10-bit PPN offset.
    (((pte >> 10) & ((1 << 44) - 1)) as usize) << 12
}

fn table_to_pte(pa: usize, flags: u64) -> u64 {
    (((pa >> 12) as u64) << 10) | flags
}

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is a frame we allocated and identity-map; it holds 512
    // PTEs. The kernel is the sole writer of page tables.
    unsafe { &mut *(pa as *mut [u64; 512]) }
}

/// Allocate a fresh root and install the shared kernel mappings.
pub fn paging_new_root() -> PagingRoot {
    let l2_pa = frames::alloc();
    let l2 = table_mut(l2_pa);

    // VA 0..1 GiB: MMIO gigapage, supervisor R|W.
    l2[0] = table_to_pte(0, PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D);

    // VA 2..3 GiB: kernel RAM. A level-1 table of 2 MiB supervisor
    // superpages, with the `.user` slot delegated to a level-0 table.
    let l1_pa = frames::alloc();
    let l1 = table_mut(l1_pa);
    let ram_base = 2 * GIB;
    let user_base = user_window_base();
    for (i, entry) in l1.iter_mut().enumerate() {
        let va = ram_base + i * MIB2;
        if va == (user_base & !(MIB2 - 1)) {
            // Delegate this 2 MiB slot to a per-cell level-0 table; user
            // pages are added later by paging_map. Empty for now.
            let l0_pa = frames::alloc();
            *entry = table_to_pte(l0_pa, PTE_V); // pointer PTE (no R/W/X)
        } else {
            *entry = table_to_pte(va, PTE_V | PTE_R | PTE_W | PTE_X | PTE_G | PTE_A | PTE_D);
        }
    }
    l2[2] = table_to_pte(l1_pa, PTE_V); // pointer to the level-1 table

    PagingRoot { l2_pa }
}

/// Map one 4 KiB identity page for user access. `va` must lie in the
/// `.user` window and be page aligned.
pub fn paging_map(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let user_base = user_window_base();
    assert!(
        (user_base..user_base + MIB2).contains(&va),
        "user map {va:#x} outside the .user window"
    );

    let l2 = table_mut(root.l2_pa);
    let l1 = table_mut(pte_to_table(l2[2]));
    let l1_idx = (va - 2 * GIB) / MIB2;
    let l0 = table_mut(pte_to_table(l1[l1_idx]));
    let l0_idx = (va % MIB2) / PAGE_SIZE;

    let rights = match perm {
        MapPerm::UserRo => PTE_R,
        MapPerm::UserRw => PTE_R | PTE_W,
        MapPerm::UserRx => PTE_R | PTE_X,
    };
    l0[l0_idx] = table_to_pte(va, PTE_V | PTE_U | PTE_A | PTE_D | rights);
}

/// Get the level-N table a parent entry points at, creating an empty one
/// (a pointer PTE, no R/W/X) if the slot is not yet valid.
fn ensure_table(parent: &mut [u64; 512], idx: usize) -> usize {
    if parent[idx] & PTE_V == 0 {
        let t = frames::alloc(); // zeroed
        parent[idx] = table_to_pte(t, PTE_V);
    }
    pte_to_table(parent[idx])
}

/// Map one 4 KiB frame `pa` at an arbitrary user `va`, creating intermediate
/// tables as needed (docs/USERLAND.md). `va` must avoid the kernel/MMIO
/// ranges the cell root maps supervisor (0-1 GiB, 2-3 GiB); userland lives at
/// 4 GiB+.
pub fn paging_map_frame(root: &mut PagingRoot, va: usize, pa: usize, perm: MapPerm) {
    assert!(va.is_multiple_of(PAGE_SIZE), "unaligned user map {va:#x}");
    let l2 = table_mut(root.l2_pa);
    let l1 = table_mut(ensure_table(l2, (va >> 30) & 0x1FF));
    let l0 = table_mut(ensure_table(l1, (va >> 21) & 0x1FF));
    let rights = match perm {
        MapPerm::UserRo => PTE_R,
        MapPerm::UserRw => PTE_R | PTE_W,
        MapPerm::UserRx => PTE_R | PTE_X,
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

/// Rewrite the leaf permission bits at `va`, keeping the mapped frame. A no-op
/// if `va` is unmapped. The caller flushes the TLB by re-activating the root.
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
        };
        l0[idx] = table_to_pte(pa, PTE_V | PTE_U | PTE_A | PTE_D | rights);
    }
}

/// Activate a root: write satp (Sv39, ASID-tagged) and fence.
pub fn paging_activate(root: &PagingRoot, asid: u16) {
    let satp = (8u64 << 60) | ((asid as u64) << 44) | ((root.l2_pa >> 12) as u64);
    // SAFETY: satp points at a well-formed root; sfence orders the switch.
    unsafe {
        core::arch::asm!(
            "csrw satp, {0}",
            "sfence.vma zero, {1}",
            in(reg) satp,
            in(reg) asid as u64,
        );
    }
}

/// Build the kernel's own address space and turn the MMU on. The kernel
/// root is deliberately simple: MMIO gigapage + a single RAM gigapage,
/// both supervisor, so setup code can read/write all of RAM (including
/// the `.user` window it initialises before entering a cell). Cell roots
/// (paging_new_root) do the fine-grained U-page carve-out.
static mut KERNEL_ROOT_PA: usize = 0;

/// Re-activate the kernel's own address space (ASID 0). Called when a cell
/// run returns, so kernel setup code can again reach all of RAM.
pub fn paging_activate_kernel() {
    let l2_pa = unsafe { *core::ptr::addr_of!(KERNEL_ROOT_PA) };
    paging_activate(&PagingRoot { l2_pa }, 0);
}

pub fn paging_kernel_init() {
    frames::init();

    let l2_pa = frames::alloc();
    let l2 = table_mut(l2_pa);
    // VA 0..1 GiB: MMIO, supervisor R|W.
    l2[0] = table_to_pte(0, PTE_V | PTE_R | PTE_W | PTE_G | PTE_A | PTE_D);
    // VA 2..3 GiB: kernel RAM, supervisor R|W|X gigapage.
    l2[2] = table_to_pte(
        2 * GIB,
        PTE_V | PTE_R | PTE_W | PTE_X | PTE_G | PTE_A | PTE_D,
    );

    // SUM lets the S-mode kernel read/write U pages (needed so the
    // doorbell handler can touch a cell's shared ring); scounteren lets
    // U-mode read the cycle counter for the benchmark's own timing.
    // sstatus.FS = Initial (0b01) enables the F/D floating-point unit for
    // U-mode (docs/LINUX-COMPAT.md L1): glibc's ifunc string routines and
    // ordinary FP both trap with FS=Off. No FP context save/restore is
    // needed yet (one U-mode context per cell; the kernel is soft-float).
    // SAFETY: plain CSR writes.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {sum}",
            "csrs sstatus, {fs}",
            "csrw scounteren, {cen}",
            sum = in(reg) 1u64 << 18,
            fs = in(reg) 1u64 << 13,
            cen = in(reg) 0x7u64,
        );
    }
    unsafe {
        *core::ptr::addr_of_mut!(KERNEL_ROOT_PA) = l2_pa;
    }
    paging_activate(&PagingRoot { l2_pa }, 0);
}
