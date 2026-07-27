//! The per-cell **VMA list**: one record per mapping, so the personality knows
//! what is mapped where and with what permission (docs/ARCHITECTURE-DEBT.md 4,
//! blocker 2).
//!
//! Before this, `mmap` was a forward **bump cursor** with no record of anything.
//! The cursor was bounded in an earlier slice, which made the *failure mode*
//! correct - a request past the region's end reports `-ENOMEM` instead of
//! silently aliasing `ld.so`. It did not make *placement* correct, and three
//! things followed from that:
//!
//! 1. **Freed space was never reused.** A program that mapped and unmapped in a
//!    loop walked the cursor to the region's end and then failed, with the whole
//!    region free behind it. For a long-running process that is not a corner
//!    case, it is the normal outcome.
//! 2. **Nothing detected a collision.** `MAP_FIXED` at an address the cursor had
//!    already handed out replaced those pages with no record that anyone else
//!    thought they owned them.
//! 3. **A page-fault handler had nothing to ask.** Demand paging needs to answer
//!    "which region is this address in, and what protection should the page
//!    get?", which is a lookup over exactly this list. That is why this lands
//!    before demand paging rather than after it.
//!
//! Like the fd table, the process tree and the signal state, this is **per-cell
//! synthesized state and adds no kernel object** (docs/LINUX-COMPAT.md 1). The
//! authority over the pages is still the cell's own address space and its frame
//! budget; this is bookkeeping about what the personality asked for.
//!
//! Fixed-capacity, so the kernel stays allocation-free. Adjacent mappings with
//! identical protection and flags are **merged**, which is what keeps a program
//! that maps many small ranges from exhausting the table.

use crate::mm::frames::FRAME_SIZE;

/// Records per cell. A dynamically linked glibc program needs roughly a dozen
/// (the program's segments, `ld.so`'s, `libc.so.6`'s, the stack, and glibc's
/// per-thread arenas); 128 leaves room for a program that maps aggressively, at
/// 32 bytes each = 4 KiB per cell.
pub const MAX_VMAS: usize = 128;

/// One mapping. `len == 0` marks a free slot - there is no separate occupancy
/// bit to get out of step with the length.
#[derive(Copy, Clone)]
pub struct Vma {
    pub base: usize,
    pub len: usize,
    /// The `mmap` prot bits as the caller gave them. Kept in the caller's
    /// vocabulary rather than as a `MapPerm`, because that is what `mprotect`
    /// compares against and what a fault handler must reproduce - converting
    /// early would lose the distinction between PROT_NONE and "no frames yet".
    pub prot: u64,
    /// The `mmap` flags, for the same reason.
    pub flags: u64,
}

impl Vma {
    const EMPTY: Vma = Vma {
        base: 0,
        len: 0,
        prot: 0,
        flags: 0,
    };

    fn end(&self) -> usize {
        self.base + self.len
    }
}

/// A cell's mapping table.
pub struct VmaList {
    v: [Vma; MAX_VMAS],
}

impl VmaList {
    pub const fn new() -> VmaList {
        VmaList {
            v: [Vma::EMPTY; MAX_VMAS],
        }
    }

    pub fn clear(&mut self) {
        self.v = [Vma::EMPTY; MAX_VMAS];
    }

    /// Copy `other`'s records - the `fork` step. A forked child's address space
    /// is a copy of the parent's, so its map of that space must be too.
    pub fn copy_from(&mut self, other: &VmaList) {
        self.v = other.v;
    }

    fn live(&self) -> impl Iterator<Item = &Vma> {
        self.v.iter().filter(|m| m.len != 0)
    }

    /// The mapping containing `addr`, if any. **This is the page-fault lookup**;
    /// everything else here exists to keep it correct.
    pub fn find(&self, addr: usize) -> Option<Vma> {
        self.live()
            .find(|m| addr >= m.base && addr < m.end())
            .copied()
    }

    /// Live record count (for tests and diagnostics).
    pub fn count(&self) -> usize {
        self.live().count()
    }

    /// The lowest free span of `bytes` in `[lo, hi)` that does not overlap any
    /// live mapping - **first fit**, which is what makes a freed span reusable.
    ///
    /// Walks the sorted list of live mappings and returns the first gap that
    /// fits. O(n^2) in the record count, which at 128 records is nothing next to
    /// the page mapping the caller is about to do; a sorted list or a tree is a
    /// later optimisation, and doing it now would be optimising the wrong thing.
    pub fn find_free(&self, lo: usize, hi: usize, bytes: usize) -> Option<usize> {
        let mut candidate = lo;
        loop {
            let end = candidate.checked_add(bytes)?;
            if end > hi {
                return None;
            }
            // The lowest live mapping that overlaps [candidate, end). If one
            // exists, the next place worth trying is just past it.
            match self
                .live()
                .filter(|m| m.base < end && m.end() > candidate)
                .map(|m| m.end())
                .max()
            {
                None => return Some(candidate),
                Some(past) => candidate = past,
            }
        }
    }

    /// True if any live mapping overlaps `[base, base+bytes)`.
    ///
    /// Not used by `mmap`, on purpose: plain `MAP_FIXED` *replaces* what is
    /// there, which is how `ld.so` overlays a library's segments onto the span it
    /// reserved. This is the predicate `MAP_FIXED_NOREPLACE` needs, and the one a
    /// collision diagnostic would use - kept because the check belongs with the
    /// list rather than with whichever caller wants it first.
    pub fn overlaps(&self, base: usize, bytes: usize) -> bool {
        let end = base.saturating_add(bytes);
        self.live().any(|m| m.base < end && m.end() > base)
    }

    fn free_slot(&mut self) -> Option<usize> {
        self.v.iter().position(|m| m.len == 0)
    }

    /// Record `[base, base+bytes)` with `prot`/`flags`, **replacing** whatever
    /// was recorded there.
    ///
    /// Replacing rather than rejecting is deliberate: `MAP_FIXED` over an
    /// existing mapping is exactly how `ld.so` works - it reserves a library's
    /// whole span PROT_NONE, then overlays each segment (text r-x, data rw) at
    /// computed offsets (docs/LINUX-COMPAT.md L7). So an insert punches the range
    /// out first and then adds the new record.
    ///
    /// Returns false only if the table is full *and* the record could not be
    /// merged into a neighbour - a real refusal the caller must report, not a
    /// silent drop that would leave the list disagreeing with the page tables.
    pub fn insert(&mut self, base: usize, bytes: usize, prot: u64, flags: u64) -> bool {
        if bytes == 0 {
            return true;
        }
        self.remove(base, bytes);

        // Merge with an adjacent mapping carrying the same protection: a program
        // that maps many small ranges back to back would otherwise consume one
        // record each and hit the ceiling for no reason.
        let end = base + bytes;
        let before = self
            .v
            .iter()
            .position(|m| m.len != 0 && m.end() == base && m.prot == prot && m.flags == flags);
        let after = self
            .v
            .iter()
            .position(|m| m.len != 0 && m.base == end && m.prot == prot && m.flags == flags);
        match (before, after) {
            (Some(b), Some(a)) => {
                // Bridging a hole between two neighbours: extend the first over
                // both and free the second.
                self.v[b].len = self.v[a].end() - self.v[b].base;
                self.v[a] = Vma::EMPTY;
                true
            }
            (Some(b), None) => {
                self.v[b].len += bytes;
                true
            }
            (None, Some(a)) => {
                self.v[a].len += bytes;
                self.v[a].base = base;
                true
            }
            (None, None) => match self.free_slot() {
                Some(i) => {
                    self.v[i] = Vma {
                        base,
                        len: bytes,
                        prot,
                        flags,
                    };
                    true
                }
                None => false,
            },
        }
    }

    /// Punch `[base, base+bytes)` out of the list, splitting a record that spans
    /// it into the two pieces that survive - which is what makes a partial
    /// `munmap` in the middle of a mapping produce two mappings rather than
    /// silently keeping one that claims to cover a hole.
    ///
    /// A split needs a spare slot. If none is free, the record is truncated to
    /// the piece below the hole rather than left claiming the hole - losing
    /// track of the tail is bad, but claiming to own unmapped memory is worse,
    /// and the tail is genuinely unmapped either way.
    pub fn remove(&mut self, base: usize, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let end = base + bytes;
        for i in 0..MAX_VMAS {
            let m = self.v[i];
            if m.len == 0 || m.base >= end || m.end() <= base {
                continue;
            }
            let (below, above) = (base > m.base, m.end() > end);
            match (below, above) {
                // Hole strictly inside: keep the head, add the tail.
                (true, true) => {
                    self.v[i].len = base - m.base;
                    match self.free_slot() {
                        Some(j) => {
                            self.v[j] = Vma {
                                base: end,
                                len: m.end() - end,
                                prot: m.prot,
                                flags: m.flags,
                            }
                        }
                        None => crate::println!(
                            "linux: VMA table full - the {:#x}..{:#x} tail of a split \
                             mapping is unmapped but no longer recorded",
                            end,
                            m.end()
                        ),
                    }
                }
                // Trim the tail.
                (true, false) => self.v[i].len = base - m.base,
                // Trim the head.
                (false, true) => {
                    self.v[i].base = end;
                    self.v[i].len = m.end() - end;
                }
                // Fully covered.
                (false, false) => self.v[i] = Vma::EMPTY,
            }
        }
    }

    /// Set `prot` over `[base, base+bytes)`, splitting records at the edges so a
    /// partial `mprotect` leaves the untouched parts with their old protection.
    ///
    /// Implemented as remove-then-insert over the affected pages, one record per
    /// distinct old protection, so the splitting logic lives in one place.
    pub fn set_prot(&mut self, base: usize, bytes: usize, prot: u64) {
        if bytes == 0 {
            return;
        }
        // Collect the flags of every record the range touches before mutating,
        // so a mapping's `flags` survive a protection change.
        let end = base + bytes;
        let mut page = base;
        while page < end {
            let flags = self.find(page).map(|m| m.flags).unwrap_or(0);
            // Extend over as many following pages as share this record's flags,
            // so the common case (one whole mapping) costs one insert.
            let mut run = FRAME_SIZE;
            while page + run < end && self.find(page + run).map(|m| m.flags) == Some(flags) {
                run += FRAME_SIZE;
            }
            self.insert(page, run, prot, flags);
            page += run;
        }
    }
}

impl Default for VmaList {
    fn default() -> Self {
        Self::new()
    }
}
