//! x86-64 paging (BUILD-ORDER.md step 3): 4-level, 4 KiB pages.
//!
//! Higher-half split (docs/MEMORY.md): the kernel and the `.user` window are
//! linked in the top-2 GiB high half (`phys_to_virt(pa) = pa | KERNEL_VA_BASE`),
//! and the boot trampoline builds that linear map before any Rust runs. x86-64
//! has a single CR3, so - like riscv64, unlike aarch64's TTBR split - a cell
//! root still carries the kernel (mapped supervisor, high): a trap enters with
//! the cell root active and must reach the handler. But the whole LOW half is
//! left free, so a stock Linux ET_EXEC (0x400000) loads unmodified. The `.user`
//! window sits HIGH too (the kernel code model reaches only the top 2 GiB, so
//! keeping `.user` adjacent to the kernel keeps every kernel->`.user` reference
//! in range). Isolation is unchanged: it is the US bit on the leaf PTE, not the
//! address, that gates a cell.
//!
//! - VA base+0..1 GiB -> a PD of 2 MiB supervisor pages (kernel image, page
//!   tables, frame pool), global so they survive a CR3 reload, except the one
//!   2 MiB slot holding the `.user` window, which is a 4 KiB page table whose
//!   leaves carry the US bit (per-cell). VA base+1..2 GiB is a second such PD.
//! - The low half is unmapped in a cell root except the pages the loader adds
//!   (paging_map_frame) - a stock ET_EXEC, its stack, mmap arenas.
//!
//! `table_mut` reaches page-table frames through the high linear map
//! (`super::phys_to_virt`); the kernel no longer identity-maps RAM low. W^X is
//! the RW and NX bits per page; SMAP is left off so the kernel can touch a
//! cell's ring during a doorbell.

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

/// Physical base of the top-2 GiB high half (KERNEL_VA_BASE), and the PML4 /
/// PDPT indices that select it.
const KVA: usize = super::KERNEL_VA_BASE;

fn table_mut(pa: usize) -> &'static mut [u64; 512] {
    // SAFETY: `pa` is an allocated table frame, reached through the kernel's
    // high linear map. The kernel runs high, so a physical frame is touched at
    // phys_to_virt(pa), never at its raw physical address. The kernel is the
    // sole writer of page tables.
    unsafe { &mut *(super::phys_to_virt(pa) as *mut [u64; 512]) }
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

/// Fill one PD with 2 MiB supervisor pages covering the 1 GiB of physical
/// memory starting at `phys_base`, carving the `.user` 2 MiB slot out to a
/// fresh 4 KiB page table when `carve` (only the PD that spans the `.user`
/// window's physical address does). The PD is the high linear map for that
/// gigabyte: entry `i` maps `phys_base + i*2 MiB` at `phys_to_virt(...)`.
fn fill_high_pd(pd: &mut [u64; 512], phys_base: usize, carve: bool) {
    let user_slot_pa = super::virt_to_phys(user_window_base()) & !(MIB2 - 1);
    for (i, entry) in pd.iter_mut().enumerate() {
        let pa = phys_base + i * MIB2;
        if carve && pa == user_slot_pa {
            let pt_pa = frames::alloc();
            *entry = addr_bits(pt_pa) | P | RW | US; // empty PT, filled by paging_map
        } else {
            // Supervisor 2 MiB page, executable (kernel code lives here).
            *entry = addr_bits(pa) | P | RW | PS | G;
        }
    }
}

/// Allocate a cell root: the kernel mapped supervisor HIGH (so a trap entering
/// with this root active reaches the handler), the low half left free for the
/// loader. The kernel's high gigabytes are PDs of 2 MiB supervisor pages, with
/// the one `.user` slot delegated to a 4 KiB page table whose leaves carry the
/// US bit.
pub fn paging_new_root() -> PagingRoot {
    let pml4_pa = frames::alloc();
    let pdpt_pa = frames::alloc();
    let pd_lo_pa = frames::alloc();
    let pd_hi_pa = frames::alloc();
    let pml4 = table_mut(pml4_pa);
    let pdpt = table_mut(pdpt_pa);
    // US on the upper tables lets the one carved `.user` leaf be user-
    // accessible; per-page US at the PD/PT level keeps the kernel supervisor.
    pml4[pml4_index(KVA)] = addr_bits(pdpt_pa) | P | RW | US;
    pdpt[pdpt_index(KVA)] = addr_bits(pd_lo_pa) | P | RW | US; // phys 0-1 GiB
    pdpt[pdpt_index(KVA) + 1] = addr_bits(pd_hi_pa) | P | RW | US; // phys 1-2 GiB
    fill_high_pd(table_mut(pd_lo_pa), 0, true);
    fill_high_pd(table_mut(pd_hi_pa), 1 << 30, false);
    PagingRoot { pml4_pa }
}

/// Map one 4 KiB page for user access inside the (high) `.user` window. `va`
/// must lie in the window and be page aligned; the backing frame is the
/// window's own physical page (`virt_to_phys(va)`), carrying the US bit.
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
    pt[pt_index(va)] = addr_bits(super::virt_to_phys(va)) | bits;
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

/// Walk to the 4 KiB leaf for `va`, returning `(pt_pa, pt_index)` if every
/// level is present and the leaf level is a 4 KiB page table (not a 2 MiB
/// block). Used by unmap/protect on user VAs (docs/LINUX-COMPAT.md L2).
fn leaf(root: &PagingRoot, va: usize) -> Option<(usize, usize)> {
    let pml4 = table_mut(root.pml4_pa);
    let e = pml4[pml4_index(va)];
    if e & P == 0 {
        return None;
    }
    let pdpt = table_mut(next_table(e));
    let e = pdpt[pdpt_index(va)];
    if e & P == 0 {
        return None;
    }
    let pd = table_mut(next_table(e));
    let e = pd[pd_index(va)];
    if e & P == 0 || e & PS != 0 {
        return None; // unmapped, or a 2 MiB supervisor block (never user)
    }
    Some((next_table(e), pt_index(va)))
}

/// Invoke `f(va, pa, perm)` for every mapped 4 KiB **user** leaf (US bit set) in
/// `root` - the primitive `fork`/`execve` teardown build on (docs/LINUX-COMPAT.md
/// L6). The kernel high half (its PML4 slot) and the supervisor 2 MiB blocks are
/// skipped, so only the cell's own low-half user pages are visited.
pub fn paging_for_each_user_leaf(root: &PagingRoot, f: &mut dyn FnMut(usize, usize, MapPerm)) {
    let pml4 = table_mut(root.pml4_pa);
    let kva_i = pml4_index(KVA);
    for (i4, &e4) in pml4.iter().enumerate() {
        if i4 == kva_i || e4 & P == 0 {
            continue; // the kernel high half, or an empty slot
        }
        let pdpt = table_mut(next_table(e4));
        for (i3, &e3) in pdpt.iter().enumerate() {
            if e3 & P == 0 {
                continue;
            }
            let pd = table_mut(next_table(e3));
            for (i2, &e2) in pd.iter().enumerate() {
                if e2 & P == 0 || e2 & PS != 0 {
                    continue; // unmapped or a 2 MiB supervisor block
                }
                let pt = table_mut(next_table(e2));
                for (i1, &e1) in pt.iter().enumerate() {
                    if e1 & P == 0 || e1 & US == 0 {
                        continue;
                    }
                    let va = (i4 << 39) | (i3 << 30) | (i2 << 21) | (i1 << 12);
                    let pa = (e1 & 0x000F_FFFF_FFFF_F000) as usize;
                    let perm = if e1 & RW != 0 {
                        MapPerm::UserRw
                    } else if e1 & NX != 0 {
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
    let (pt_pa, idx) = leaf(root, va)?;
    let pt = table_mut(pt_pa);
    if pt[idx] & P == 0 {
        return None;
    }
    let pa = (pt[idx] & 0x000F_FFFF_FFFF_F000) as usize;
    pt[idx] = 0;
    Some(pa)
}

/// Rewrite the leaf permission bits at `va`, keeping the mapped frame. A no-op
/// if `va` is unmapped. The caller flushes the TLB by re-activating the root.
pub fn paging_protect(root: &mut PagingRoot, va: usize, perm: MapPerm) {
    if let Some((pt_pa, idx)) = leaf(root, va) {
        let pt = table_mut(pt_pa);
        if pt[idx] & P == 0 {
            return;
        }
        let pa = pt[idx] & 0x000F_FFFF_FFFF_F000;
        let bits = match perm {
            MapPerm::UserRo => P | US | NX,
            MapPerm::UserRw => P | RW | US | NX,
            MapPerm::UserRx => P | US,
        };
        pt[idx] = pa | bits;
    }
}

/// Activate a root: load CR3. Kernel pages are global, so they survive the
/// reload; user pages (non-global) are flushed - the isolation guarantee.
pub fn paging_activate(root: &PagingRoot, _asid: u16) {
    // SAFETY: pml4_pa is a well-formed root.
    unsafe {
        core::arch::asm!("mov cr3, {0}", in(reg) root.pml4_pa as u64, options(nostack));
    }
}

unsafe extern "C" {
    /// The boot-built PML4 (kernel/arch/x86_64/boot.S), identity-mapped low, so
    /// its symbol address is its physical address. It doubles as the kernel
    /// working root: kernel high (supervisor) plus the low-identity leftover
    /// from the paging turn-on.
    static boot_page_tables: u8;
}

/// Re-activate the kernel's own address space. Called when a cell run returns,
/// so kernel setup code can again reach all of RAM through the high linear map
/// (a cell root only maps that cell's user pages in the low half).
pub fn paging_activate_kernel() {
    let pml4_pa = core::ptr::addr_of!(boot_page_tables) as usize;
    paging_activate(&PagingRoot { pml4_pa }, 0);
}

/// Kernel VA base of the persistent-memory mapping window (docs/MEMORY.md
/// real-PMEM path). A real QEMU nvdimm's physical span is placed at 4 GiB,
/// **above** the kernel's top-2 GiB linear map (`phys_to_virt` cannot reach it -
/// the x86-64 "kernel" code-model constraint), so pmem frames are reached
/// through this dedicated window instead. It occupies a fresh PML4 slot (384,
/// canonical, distinct from the kernel high half at 511 and the low identity at
/// 0); only the `pmem` test kernel installs it (via `frames_pmem::init` on an
/// nvdimm machine), so every other kernel leaves PML4[384] empty and the DDR
/// path is byte-for-byte unchanged.
const PMEM_WINDOW_VA: usize = 0xFFFF_C000_0000_0000;

/// Map the persistent-memory region `[base_pa, base_pa+len)` into the kernel
/// working root at `PMEM_WINDOW_VA` with 2 MiB supervisor (RW, NX) pages, and
/// return that VA. `base_pa` must be 2 MiB aligned (the nvdimm SPA base is);
/// `len` is rounded up to a 2 MiB multiple. Idempotent tables are created on
/// demand; the TLB is flushed by reloading CR3 (the kernel root is active).
pub fn pmem_map_window(base_pa: usize, len: usize) -> usize {
    assert!(
        base_pa.is_multiple_of(MIB2),
        "pmem base {base_pa:#x} not 2 MiB"
    );
    let pml4_pa = core::ptr::addr_of!(boot_page_tables) as usize;
    let npages = len.div_ceil(MIB2);
    for p in 0..npages {
        let va = PMEM_WINDOW_VA + p * MIB2;
        let pa = base_pa + p * MIB2;
        let pml4 = table_mut(pml4_pa);
        let pdpt = table_mut(ensure_table(pml4, pml4_index(va)));
        let pd = table_mut(ensure_table(pdpt, pdpt_index(va)));
        // 2 MiB supervisor page: present, writable, size, global, non-executable.
        pd[pd_index(va)] = addr_bits(pa) | P | RW | PS | G | NX;
    }
    paging_activate_kernel(); // flush the TLB (reload CR3)
    PMEM_WINDOW_VA
}

/// Finish paging bring-up. The MMU and the kernel working root are already
/// configured by the boot trampoline, which enabled paging and jumped the
/// kernel to its high VAs before any Rust ran. All that is left is the frame
/// allocator, enabling NX, and ring 3 (GDT/TSS + syscall MSRs).
pub fn paging_kernel_init() {
    frames::init();

    // Enable NXE (EFER bit 11) so the NX bit is honoured (the boot tables set
    // no NX; cell roots and paging_map set NX on user data pages).
    unsafe {
        let efer = rdmsr(0xC000_0080) | (1 << 11);
        wrmsr(0xC000_0080, efer);
    }

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
