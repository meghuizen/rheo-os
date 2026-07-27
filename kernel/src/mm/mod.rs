//! Memory management (BUILD-ORDER.md step 3, docs/MEMORY.md): the physical
//! frame allocator and the address-space abstraction cells are built on.
//!
//! Scope at this stage: 4 KiB user mappings with W^X enforced, kernel
//! identity mappings shared into every address space (supervisor-only),
//! ASID/PCID-tagged switches. Huge pages back the kernel identity map;
//! elastic grants, reclaim, and pressure events are later steps.

pub mod frames;
pub mod frames_pmem;
pub mod grant;

use crate::arch::{self, MapPerm};

/// One address space: a per-cell root page table with the kernel mapped
/// supervisor-only and user frames mapped with the U/EL0 bit. The heavy
/// lifting is per-ISA (kernel/src/arch/<isa>/paging.rs).
pub struct AddressSpace {
    root: arch::PagingRoot,
    asid: u16,
}

impl AddressSpace {
    /// Create a fresh address space: the shared kernel mappings, no user
    /// pages yet. `asid` tags TLB entries so switches need no flush.
    pub fn new(asid: u16) -> AddressSpace {
        AddressSpace {
            root: arch::paging_new_root(),
            asid,
        }
    }

    /// Map one 4 KiB frame at `va` for user access. Mappings are identity
    /// (va is the physical address); isolation is real regardless: a frame
    /// not mapped in this address space is unreachable from this cell,
    /// MMU-enforced. `va` must be 4 KiB aligned.
    pub fn map_user(&mut self, va: usize, perm: MapPerm) {
        arch::paging_map(&mut self.root, va, perm);
    }

    /// Map one 4 KiB frame `pa` at an arbitrary user `va` (docs/USERLAND.md).
    /// Unlike `map_user`, `va` need not be identity or inside the `.user`
    /// window: intermediate page tables are created on demand. Used by the
    /// ELF loader to place a program at its own link base. Both must be 4 KiB
    /// aligned.
    pub fn map_user_frame(&mut self, va: usize, pa: usize, perm: MapPerm) {
        arch::paging_map_frame(&mut self.root, va, pa, perm);
    }

    /// Map every 4 KiB page overlapping [start, start+len) for user access.
    pub fn map_user_range(&mut self, start: usize, len: usize, perm: MapPerm) {
        let base = start & !(frames::FRAME_SIZE - 1);
        let end = start + len;
        let mut va = base;
        while va < end {
            self.map_user(va, perm);
            va += frames::FRAME_SIZE;
        }
    }

    /// Unmap one 4 KiB user page at `va`, returning the physical frame it
    /// pointed at (for the caller to `frames::free`) or None if it was not
    /// mapped (docs/LINUX-COMPAT.md L2). The TLB is flushed by the next
    /// `activate()`.
    pub fn unmap(&mut self, va: usize) -> Option<usize> {
        arch::paging_unmap_frame(&mut self.root, va)
    }

    /// Whether one 4 KiB page at `va` has a live mapping.
    ///
    /// The question a **demand-paging fault handler** must answer before anything
    /// else: is this "the page is not there" - populate it and retry the
    /// instruction - or "the page is there and the access was refused" - a genuine
    /// SIGSEGV? `arch::FaultCause` carries no read/write bit, so the page tables
    /// are the source of truth, and getting it wrong is not a small error: a
    /// permission fault treated as a missing page would be re-populated and
    /// re-faulted forever, with no diagnostic.
    pub fn is_mapped(&self, va: usize) -> bool {
        arch::paging_mapped(&self.root, va)
    }

    /// Change the permission of one 4 KiB user page at `va`, keeping its
    /// frame; a no-op if `va` is unmapped. The TLB is flushed by the next
    /// `activate()`.
    pub fn protect(&mut self, va: usize, perm: MapPerm) {
        arch::paging_protect(&mut self.root, va, perm);
    }

    /// Make this address space current (ASID-tagged, no full TLB flush).
    pub fn activate(&self) {
        arch::paging_activate(&self.root, self.asid);
    }

    /// Eager-copy every committed user page of `self` into a fresh address
    /// space with ASID `asid` - the POSIX `fork` primitive: the child gets its
    /// own private physical copy of the parent's memory (docs/LINUX-COMPAT.md
    /// L6). Fresh frames and page tables are allocated; contents are copied
    /// through the kernel linear map, so neither space need be active.
    /// Copy-on-write is deferred (documented).
    pub fn fork_from(&self, asid: u16) -> AddressSpace {
        let mut child = AddressSpace::new(asid);
        arch::paging_for_each_user_leaf(&self.root, &mut |va, src_pa, perm| {
            let dst_pa =
                frames::alloc().expect("fork page copy (bounded by the parent's charged frames)");
            // SAFETY: both frames are 4 KiB, reached through the kernel linear
            // map (identity on x86/riscv; the high map on aarch64); `dst_pa` is
            // freshly allocated and disjoint from `src_pa`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    arch::phys_to_virt(src_pa) as *const u8,
                    arch::phys_to_virt(dst_pa) as *mut u8,
                    frames::FRAME_SIZE,
                );
            }
            child.map_user_frame(va, dst_pa, perm);
        });
        child
    }

    /// Map every committed user frame of `self` in `[base, base+len)` into `dst`
    /// **read-only** at `dst_base + (va - base)`, returning the number of frames
    /// shared - the cross-cell sealed-buffer share (docs/LIBRHEO.md Phase E). The
    /// same physical frames back both spaces, so the peer reads the buffer with
    /// no copy (the dmabuf equivalent). Neither space need be active: leaves are
    /// read from `self`'s page table and mapped into `dst`'s (published when
    /// `dst` is next activated). The source stays sealed read-only; the peer gets
    /// read-only, so the shared bytes are immutable on both sides.
    pub fn share_ro_into(
        &self,
        dst: &mut AddressSpace,
        base: usize,
        len: usize,
        dst_base: usize,
    ) -> usize {
        let mut n = 0usize;
        arch::paging_for_each_user_leaf(&self.root, &mut |va, pa, _perm| {
            if va >= base && va < base + len {
                dst.map_user_frame(dst_base + (va - base), pa, MapPerm::UserRo);
                n += 1;
            }
        });
        n
    }

    /// Map every committed user frame of `self` in `[base, base+len)` into `dst`
    /// at `dst_base + (va - base)`, **read-write** - propagating a cross-cell
    /// shared channel to a spawned child (docs/LIBRHEO.md Phase J). The same
    /// physical frames back both spaces, so the child drives its end of the SPSC
    /// ring over the parent's channel frames (the sibling of [`share_ro_into`],
    /// which is read-only for a sealed buffer). `dst_base` lets a **service cell**
    /// hand its channel *slot k* to a child at the child's slot 0
    /// (docs/NETSTACK.md the service-cell section, rheo-net N4a), so a client
    /// binary is slot-agnostic. Returns the number of frames mapped.
    pub fn share_rw_into(
        &self,
        dst: &mut AddressSpace,
        base: usize,
        len: usize,
        dst_base: usize,
    ) -> usize {
        let mut n = 0usize;
        arch::paging_for_each_user_leaf(&self.root, &mut |va, pa, _perm| {
            if va >= base && va < base + len {
                dst.map_user_frame(dst_base + (va - base), pa, MapPerm::UserRw);
                n += 1;
            }
        });
        n
    }

    /// Return every committed user leaf frame of this space to the pool - the
    /// child-reap / `execve` / process-exit teardown (docs/LINUX-COMPAT.md L6).
    /// Intermediate page-table frames are intentionally NOT reclaimed (a small,
    /// bounded, documented per-dead-process leak); the leaf frames (stacks,
    /// heap, mmap arenas, the eager fork copy) are the pool pressure that
    /// matters and are freed here.
    pub fn free_user_frames(&self) {
        arch::paging_for_each_user_leaf(&self.root, &mut |_va, pa, _perm| {
            // `free_if_pool`, not `free`: a cell root can legitimately reference
            // a page this allocator does not own (the shared `.user` window is
            // part of the kernel image), and `free` would panic on it.
            frames::free_if_pool(pa);
        });
    }
}

unsafe extern "C" {
    static __user_start: u8;
    static __user_rodata_start: u8;
    static __user_data_start: u8;
    static __user_end: u8;
}

/// The `.user.text` VA range (code that runs in U-mode, mapped read+exec;
/// shared into every cell).
pub fn user_text_range() -> (usize, usize) {
    (
        core::ptr::addr_of!(__user_start) as usize,
        core::ptr::addr_of!(__user_rodata_start) as usize,
    )
}

/// The `.user.rodata` VA range (shared read-only constants used by U-mode
/// code, e.g. the shell's strings; mapped read-only into every cell).
pub fn user_rodata_range() -> (usize, usize) {
    (
        core::ptr::addr_of!(__user_rodata_start) as usize,
        core::ptr::addr_of!(__user_data_start) as usize,
    )
}

/// The full `.user` window; used only for bounds assertions.
pub fn user_window() -> (usize, usize) {
    (
        core::ptr::addr_of!(__user_start) as usize,
        core::ptr::addr_of!(__user_end) as usize,
    )
}
