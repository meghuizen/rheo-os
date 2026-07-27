//! Typed memory grants (docs/ARCHITECTURE.md 3 object 5, docs/MEMORY.md):
//! a typed kind, an explicit commit policy, hard or elastic, sealable to
//! immutable. Single-host scope: grants are backed by frame-pool pages;
//! commit allocates, decommit frees, seal makes the grant immutable
//! (further writes through the grant API are refused). Elastic grants and
//! pressure events (MEMORY.md 4-7, BUILD-ORDER.md step 8) are deferred.

use super::{frames, frames_pmem};

/// The typed kind of memory a grant is backed by (docs/MEMORY.md). DDR is
/// always real; `Pmem` is genuinely backed by a real QEMU nvdimm's physical
/// frames where one is present (x86-64 via NFIT; docs/MEMORY.md real-PMEM
/// path), falling back to DDR where no nvdimm is exposed. HBM/CXL/Remote have
/// no QEMU device model and stay honestly DDR-emulated; DeviceBar has no backing
/// and is refused. Callers must name their intent.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemKind {
    Ddr,
    Hbm,
    Cxl,
    Pmem,
    DeviceBar,
    Remote,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GrantError {
    NotCommitted,
    Sealed,
    TooLarge,
}

/// The maximum pages a single grant tracks (fixed for no-alloc kernel).
pub const MAX_GRANT_PAGES: usize = 16;

/// A memory grant: a set of committed frames of a declared kind, hard or
/// elastic, optionally sealed immutable.
pub struct Grant {
    pub kind: MemKind,
    pub hard: bool,
    frames: [usize; MAX_GRANT_PAGES],
    committed: usize,
    sealed: bool,
}

impl Grant {
    /// Create an empty grant of `kind` (hard = reserved up front vs
    /// elastic = reclaimable).
    pub fn new(kind: MemKind, hard: bool) -> Grant {
        Grant {
            kind,
            hard,
            frames: [0; MAX_GRANT_PAGES],
            committed: 0,
            sealed: false,
        }
    }

    /// Commit `pages` zeroed frames into the grant.
    pub fn commit(&mut self, pages: usize) -> Result<(), GrantError> {
        if self.sealed {
            return Err(GrantError::Sealed);
        }
        if self.committed + pages > MAX_GRANT_PAGES {
            return Err(GrantError::TooLarge);
        }
        for _ in 0..pages {
            // Exhaustion is a refusal, never a panic: the caller decommits what
            // it already got (docs/ENGINEERING.md 12).
            let Some(pa) = alloc_frame(self.kind) else {
                return Err(GrantError::TooLarge);
            };
            self.frames[self.committed] = pa;
            self.committed += 1;
        }
        Ok(())
    }

    /// Return `pages` frames from the tail of the grant (cooperative
    /// reclaim; refused once sealed).
    pub fn decommit(&mut self, pages: usize) -> Result<(), GrantError> {
        if self.sealed {
            return Err(GrantError::Sealed);
        }
        let n = pages.min(self.committed);
        for _ in 0..n {
            self.committed -= 1;
            free_frame(self.frames[self.committed]);
        }
        Ok(())
    }

    /// Seal the grant immutable. After this, commit/decommit/write refuse.
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
    pub fn committed_pages(&self) -> usize {
        self.committed
    }

    /// Physical base of committed page `i`, if committed.
    pub fn page(&self, i: usize) -> Result<usize, GrantError> {
        if i >= self.committed {
            return Err(GrantError::NotCommitted);
        }
        Ok(self.frames[i])
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        // Frames outlive a sealed grant only until the sealed object is
        // destroyed; unsealed grants free on drop (RAII reclaim).
        while self.committed > 0 {
            self.committed -= 1;
            free_frame(self.frames[self.committed]);
        }
    }
}

/// Allocate one frame to back a grant of `kind`. `Pmem` draws from the real
/// nvdimm region when one was discovered (docs/MEMORY.md real-PMEM path);
/// otherwise, and for every other kind, it draws a zeroed DDR frame. The Pmem
/// fallback is logged once so an emulated-as-DDR pmem grant is never silently
/// mistaken for a real one.
fn alloc_frame(kind: MemKind) -> Option<usize> {
    if kind == MemKind::Pmem {
        if let Some(pa) = frames_pmem::alloc() {
            return Some(pa);
        }
        note_pmem_ddr_fallback();
    }
    frames::alloc()
}

/// Free one frame back to whichever pool owns it (the pmem region, else DDR).
fn free_frame(pa: usize) {
    if frames_pmem::contains(pa) {
        frames_pmem::free(pa);
    } else {
        frames::free(pa);
    }
}

static mut PMEM_FALLBACK_WARNED: bool = false;

/// One-time honest note: a `Pmem` grant is being backed by DDR because no
/// nvdimm was discovered on this machine (docs/MEMORY.md).
fn note_pmem_ddr_fallback() {
    // SAFETY: single-threaded kernel init/commit path; a benign one-shot flag.
    unsafe {
        if !*core::ptr::addr_of!(PMEM_FALLBACK_WARNED) {
            *core::ptr::addr_of_mut!(PMEM_FALLBACK_WARNED) = true;
            crate::println!("mm: pmem grant backed by DDR (no nvdimm discovered)");
        }
    }
}
