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
pub mod kmeta;
pub mod vaspace;

use crate::arch::{self, MapPerm};

/// Copy-on-write faults resolved since boot - the witness that a `fork` shared its
/// pages rather than copying them, and that the sharing was actually broken on write.
static mut COW_FAULTS: u64 = 0;

fn bump_cow_faults() {
    // SAFETY: single CPU, synchronous trap.
    unsafe {
        let p = core::ptr::addr_of_mut!(COW_FAULTS);
        *p = (*p).wrapping_add(1);
    }
}

/// How many copy-on-write faults have been resolved since boot.
pub fn cow_faults() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(COW_FAULTS) }
}

/// Pages a `fork` **shared**, and pages it had to **copy** because they could not be
/// shared. The pair, not either alone: "the fork was cheap" is a claim about the
/// ratio, and a fork that silently fell back to copying everything would otherwise
/// look identical to one that shared everything.
static mut FORK_SHARED: u64 = 0;
static mut FORK_COPIED: u64 = 0;

/// Frames a `fork` actually consumed: the child's page tables, plus a copy for any
/// page that could not be shared. Measured **inside** `fork_from` rather than around
/// the call, because a delta taken around anything larger also counts the process's
/// own memory - which is how the first version of this oracle came to report 2431
/// frames for a fork that had copied nothing (docs/ENGINEERING.md 11).
static mut FORK_FRAMES: u64 = 0;

/// `(shared, copied)` pages across every `fork` since boot.
pub fn fork_pages() -> (u64, u64) {
    // SAFETY: single CPU.
    unsafe {
        (
            *core::ptr::addr_of!(FORK_SHARED),
            *core::ptr::addr_of!(FORK_COPIED),
        )
    }
}

/// Frames every `fork` since boot has consumed (page tables + unshareable copies).
pub fn fork_frames() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(FORK_FRAMES) }
}

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

    /// How many bytes from `va` are certainly unmapped, because a table above the
    /// leaf level is absent - `0` when only a leaf lookup can answer.
    ///
    /// What this is for: a range walk that steps 4 KiB at a time is O(range), which
    /// a program's *reservations* make untenable. JavaScriptCore reserves 128 GiB in
    /// one `PROT_NONE` mapping and the `mmap` window is terabytes wide on two of the
    /// three ISAs, so unmapping an untouched span page by page is a hang, not a slow
    /// path. Skipping an absent gigapage in one step turns it into a few thousand
    /// iterations. Conservative by construction: it never reports a mapped span, so
    /// ignoring it is still correct.
    pub fn unmapped_span(&self, va: usize) -> usize {
        arch::paging_unmapped_span(&self.root, va)
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

    /// **Copy-on-write** `fork`: give a fresh address space with ASID `asid` the
    /// same physical pages as `self`, mapped **read-only in both**, and take a
    /// reference to each frame (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
    ///
    /// A fork therefore costs page tables and nothing else. It used to copy every
    /// committed page, so a process paid its whole resident set to fork - which for
    /// a large program is more than its image ever cost.
    ///
    /// Two details carry the correctness:
    ///
    /// 1. **The parent is write-protected too.** Sharing the child's side alone
    ///    would let the parent write through to memory the child now sees. That is
    ///    the half that is easy to miss and produces no fault when wrong, only wrong
    ///    values, so `&mut self` is required - a read-only `fork_from` could not
    ///    express it.
    /// 2. **A frame that cannot be shared is copied.** `frames::share` refuses a
    ///    non-pool page (the `.user` window, an MMIO aperture) and a count at its
    ///    ceiling; those pages keep the old eager behaviour, so nothing is silently
    ///    aliased that should not be.
    ///
    /// The writable pages become writable again one at a time, in
    /// `linux::mem::fault`, when either side writes.
    pub fn fork_from(&mut self, asid: u16) -> AddressSpace {
        let free_at_entry = frames::stats().0;
        let mut child = AddressSpace::new(asid);
        arch::paging_for_each_user_leaf(&self.root, &mut |va, src_pa, perm| {
            // A page already marked COW by an **earlier** fork reads back as read-only,
            // because that is what its bits say - but it is *logically* writable, and a
            // fresh `map_user_frame` in the child would drop the mark and turn the
            // child's first write into a SIGSEGV. Restoring the writable permission
            // here lets the uniform cow-protect below re-derive it for both sides.
            let logically_writable =
                perm == MapPerm::UserRw || arch::paging_cow_at(&self.root, va).is_some();
            let perm = if logically_writable {
                MapPerm::UserRw
            } else {
                perm
            };
            if frames::share(src_pa) {
                child.map_user_frame(va, src_pa, perm);
                // SAFETY: single CPU, synchronous trap.
                unsafe {
                    let p = core::ptr::addr_of_mut!(FORK_SHARED);
                    *p = (*p).wrapping_add(1);
                }
                return;
            }
            // SAFETY: as above.
            unsafe {
                let p = core::ptr::addr_of_mut!(FORK_COPIED);
                *p = (*p).wrapping_add(1);
            }
            // Not shareable - a page outside the frame pool (the `.user` window, an
            // MMIO aperture) or one already at its share ceiling. Copy it, as every
            // fork used to, so nothing is aliased that must not be.
            let Some(dst_pa) = frames::alloc() else {
                crate::println!("fork: no frame to copy the unshareable page at {va:#x}");
                return;
            };
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
        // Both sides lose write access to every writable user page, and gain the COW
        // mark that tells the fault handler a write there is legitimate. The parent's
        // half is the one that is easy to miss and produces wrong values rather than a
        // fault: without it the parent writes through to memory the child now sees.
        //
        // A page that was *copied* above also gets marked, which costs one extra fault
        // on the first write and nothing else - the handler finds a single reference and
        // simply makes it writable again. Uniform beats a special case here.
        arch::paging_cow_protect_user(&mut self.root);
        arch::paging_cow_protect_user(&mut child.root);
        // SAFETY: single CPU, synchronous trap.
        unsafe {
            let p = core::ptr::addr_of_mut!(FORK_FRAMES);
            *p = (*p).wrapping_add(free_at_entry.saturating_sub(frames::stats().0) as u64);
        }
        // The parent is the address space running this syscall, so its TLB holds
        // writable entries for pages that are no longer writable. `activate` flushes on
        // all three ISAs (satp + sfence / TTBR0 + tlbi aside1is / a CR3 reload).
        self.activate();
        child
    }

    /// Resolve a **copy-on-write** write fault at `va`, returning true if the faulting
    /// instruction should be retried (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
    ///
    /// False means `va` is not a COW page, so the fault was a genuine refusal - a write
    /// to real read-only memory - and the caller must treat it as one. That distinction
    /// lives in the page table rather than in the VMA list on purpose: the stack and the
    /// `brk` heap have no VMA records, and a COW mechanism that only worked for `mmap`
    /// would break the first stack write after a fork.
    ///
    /// `self` must be the **active** address space; the mapping is changed, so the TLB
    /// is flushed here.
    /// Whether `va` is a copy-on-write page - present and readable, but a write to it
    /// must private it first. The predicate the **kernel's own** writes to a user
    /// buffer need: presence is not enough once a fork shares pages, because a kernel
    /// store to a shared read-only page faults at a kernel PC, which is not resumable
    /// here (docs/ENGINEERING.md 11, the `copy_from_user` hazard, one step on).
    pub fn is_cow(&self, va: usize) -> bool {
        arch::paging_cow_at(&self.root, va & !(frames::FRAME_SIZE - 1)).is_some()
    }

    pub fn cow_fault(&mut self, va: usize) -> bool {
        let page = va & !(frames::FRAME_SIZE - 1);
        let Some(pa) = arch::paging_cow_at(&self.root, page) else {
            return false;
        };
        // One holder means the other side already privated its copy (or this page was
        // never shared, just marked): keep the frame and make it writable.
        let new_pa = if frames::refs(pa) > 1 {
            let Some(dst) = frames::alloc() else {
                crate::println!("cow: no frame to private the shared page at {page:#x}");
                return false;
            };
            // SAFETY: both are 4 KiB pool frames reached through the kernel linear map;
            // `dst` is freshly allocated and disjoint from `pa`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    arch::phys_to_virt(pa) as *const u8,
                    arch::phys_to_virt(dst) as *mut u8,
                    frames::FRAME_SIZE,
                );
            }
            // This mapping no longer holds the shared frame.
            frames::free(pa);
            Some(dst)
        } else {
            None
        };
        arch::paging_cow_clear(&mut self.root, page, new_pa);
        self.activate();
        bump_cow_faults();
        true
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

    /// Unmap `[base, base+len)` in **this** address space and return its frames,
    /// reporting how many pages were actually mapped.
    ///
    /// The range form of [`AddressSpace::unmap`], for a caller acting on an
    /// address space that is **not** the running one - which is why it exists
    /// rather than the caller looping: `user::unmap_range` operates on the active
    /// cell, and a freshly forked child is not active. `madvise(MADV_WIPEONFORK)`
    /// applied to a child is exactly that case (docs/SUBSTRATE.md 10a).
    ///
    /// Frames are released with `free_if_pool`, so a range that happens to cover a
    /// non-pool page (the shared `.user` window) is skipped rather than panicking -
    /// the same reasoning as [`AddressSpace::free_user_frames`]. The TLB is flushed
    /// by the next `activate()`, which for a not-yet-run child is its first entry.
    pub fn free_user_range(&mut self, base: usize, len: usize) -> usize {
        let mut va = base & !(frames::FRAME_SIZE - 1);
        let end = base.saturating_add(len);
        let mut freed = 0;
        while va < end {
            if let Some(pa) = self.unmap(va) {
                frames::free_if_pool(pa);
                freed += 1;
            }
            va += frames::FRAME_SIZE;
        }
        freed
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
