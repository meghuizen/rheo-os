//! Memory management (BUILD-ORDER.md step 3, docs/MEMORY.md): the physical
//! frame allocator and the address-space abstraction cells are built on.
//!
//! Scope at this stage: 4 KiB user mappings with W^X enforced, kernel
//! identity mappings shared into every address space (supervisor-only),
//! ASID/PCID-tagged switches. Huge pages back the kernel identity map;
//! elastic grants, reclaim, and pressure events are later steps.

pub mod frames;
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

    /// Make this address space current (ASID-tagged, no full TLB flush).
    pub fn activate(&self) {
        arch::paging_activate(&self.root, self.asid);
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
