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

use crate::linux::filemap;
use crate::mm::frames::FRAME_SIZE;

/// Records per cell. A dynamically linked glibc program needs roughly a dozen
/// (the program's segments, `ld.so`'s, `libc.so.6`'s, the stack, and glibc's
/// per-thread arenas); 128 leaves room for a program that maps aggressively, at
/// 32 bytes each = 4 KiB per cell.
pub const MAX_VMAS: usize = 128;

/// Where a file-backed mapping's missing pages come from. One value rather than
/// three parallel arguments, because the three are only ever meaningful together -
/// a handle with the wrong offset or the wrong length serves the wrong bytes.
#[derive(Copy, Clone)]
pub struct Backing {
    /// The [`filemap`] entry. The record created from this owns one reference to it.
    pub file: u8,
    /// File offset of the mapping's **first** page.
    pub off: u64,
    /// Bytes of file content from the mapping's base; see [`Vma::file_len`].
    pub len: usize,
}

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
    /// Where a missing page's contents come from: `None` = anonymous (zero-fill),
    /// `Some(h)` = the file behind [`filemap`] entry `h`, starting at
    /// [`Self::file_off`].
    ///
    /// **One live record holds exactly one `filemap` reference.** That is the whole
    /// refcount rule, and every operation below obeys it: a merge turns two records
    /// into one and drops a reference, a split turns one into two and adds one, a
    /// full removal drops one, and `fork`'s `inherit_files` adds one per record.
    /// Getting it wrong closes a file another mapping still faults against, so it is
    /// stated here rather than left to be inferred from the code.
    pub file: Option<u8>,
    /// File offset of this mapping's **first** page. A split recomputes it for the
    /// piece above the hole; without that, the tail of a split file mapping would
    /// silently serve the wrong bytes.
    pub file_off: u64,
    /// How many of this mapping's bytes, counted from [`Self::base`], come from the
    /// file. Everything past that is **zero**, not "whatever is next in the file".
    ///
    /// It exists because an ELF segment's file content does not end on a page
    /// boundary: `p_filesz` is a byte count, and the tail of its last page is
    /// zero-fill. Without this the fault handler would read a whole page and serve
    /// the *following* segment's bytes in that tail - not a crash, which is what
    /// makes it worth a field rather than a comment.
    ///
    /// For a `mmap` of a file it is simply the mapping length: a read past end of
    /// file already short-reads, leaving the rest of the frame zero.
    pub file_len: usize,
}

impl Vma {
    const EMPTY: Vma = Vma {
        base: 0,
        len: 0,
        prot: 0,
        flags: 0,
        file: None,
        file_off: 0,
        file_len: 0,
    };

    /// Bytes of file content still available at `at` (>= `base`), i.e. how much of a
    /// page starting there may be read from the file before zero-fill takes over.
    fn avail_at(&self, at: usize) -> usize {
        self.file_len.saturating_sub(at - self.base)
    }

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

    /// Drop every record, releasing one `filemap` reference per file-backed one.
    pub fn clear(&mut self) {
        for m in self.v.iter() {
            if m.len != 0
                && let Some(h) = m.file
            {
                filemap::close(h);
            }
        }
        self.v = [Vma::EMPTY; MAX_VMAS];
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

    /// The file backing `addr`, if any - the second half of the page-fault lookup:
    /// **which** file, **where** in it, and **how many** bytes of this page come from
    /// it (the rest are zero). Returns `None` for an anonymous mapping, and also once
    /// the page lies wholly past the file content, because then there is nothing to
    /// read and a zeroed frame is already the right answer.
    pub fn file_at(&self, addr: usize) -> Option<(u8, u64, usize)> {
        let m = self.find(addr)?;
        let h = m.file?;
        let page = addr & !(FRAME_SIZE - 1);
        let avail = m.avail_at(page).min(FRAME_SIZE);
        (avail > 0).then_some((h, m.file_off + (page - m.base) as u64, avail))
    }

    /// Add a backing-store reference for every live file-backed record - the **`fork`**
    /// step, and the exact twin of `fd::inherit_pipe_ends`.
    ///
    /// `linux::dup_state` copies a whole `LinuxState` with one raw
    /// `copy_nonoverlapping`, which duplicates these records while touching no
    /// refcount, so the addref cannot live inside a per-list copy helper - there is no
    /// per-list copy. (One used to exist here and nothing called it: two ways to
    /// inherit a VMA list, only one of them reachable. It is gone.)
    ///
    /// Without this call the child's records name entries it holds no reference to, the
    /// child's exit releases one per record and drives the count to zero, and the
    /// **parent** then faults against a freed entry and gets a zero page - in the
    /// process that did nothing wrong, long after the fork.
    ///
    /// Every refcounted thing a fork shares needs a reference added here; the calls are
    /// kept adjacent in `dup_state` so that reads as a list rather than a habit.
    pub fn inherit_files(&self) {
        for m in self.v.iter() {
            if m.len != 0
                && let Some(h) = m.file
            {
                filemap::addref(h);
            }
        }
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
        self.insert_backed(base, bytes, prot, flags, None)
    }

    /// [`Self::insert`] for a mapping that may be **file-backed**: `backing` names
    /// where a missing page's contents come from and how far they go. The caller must
    /// hand over a `filemap` reference for the new record (see [`Vma::file`]); a
    /// refusal here gives it back, so a full table cannot leak the handle.
    pub fn insert_backed(
        &mut self,
        base: usize,
        bytes: usize,
        prot: u64,
        flags: u64,
        backing: Option<Backing>,
    ) -> bool {
        let (file, file_off, file_len) = match backing {
            Some(b) => (Some(b.file), b.off, b.len),
            None => (None, 0, 0),
        };
        if bytes == 0 {
            if let Some(h) = file {
                filemap::close(h);
            }
            return true;
        }
        self.remove(base, bytes);

        // Merge with an adjacent mapping carrying the same protection: a program
        // that maps many small ranges back to back would otherwise consume one
        // record each and hit the ceiling for no reason.
        // Merging two file-backed records is only sound when they name the same
        // file AND their file ranges are genuinely contiguous AND both are backed all
        // the way to their end - otherwise the merged record's single `file_off` and
        // `file_len` would serve the wrong bytes, or zeros, for half of it. `ld.so`
        // overlays segments at computed offsets, so non-contiguous neighbours in the
        // same file are the normal case, not a corner one; and an ELF segment ending
        // mid-page is exactly a record that is not backed to its end.
        let end = base + bytes;
        let whole = file.is_none() || file_len >= bytes;
        let joins_below = |m: &Vma| {
            m.len != 0
                && m.end() == base
                && m.prot == prot
                && m.flags == flags
                && m.file == file
                && (file.is_none()
                    || (whole && m.file_len >= m.len && m.file_off + m.len as u64 == file_off))
        };
        let joins_above = |m: &Vma| {
            m.len != 0
                && m.base == end
                && m.prot == prot
                && m.flags == flags
                && m.file == file
                && (file.is_none()
                    || (whole && m.file_len >= m.len && file_off + bytes as u64 == m.file_off))
        };
        let before = self.v.iter().position(joins_below);
        let after = self.v.iter().position(joins_above);
        // A merge turns N records into fewer, so the incoming reference is surplus.
        if (before.is_some() || after.is_some())
            && let Some(h) = file
        {
            filemap::close(h);
        }
        match (before, after) {
            // Every merge below only happens when all the parts are backed to their
            // end (`whole` above), so the grown record is too: its `file_len` is
            // simply its new length. Adding the pieces' `file_len`s instead would be
            // the same number by a route a reader has to re-derive.
            (Some(b), Some(a)) => {
                // Bridging a hole between two neighbours: extend the first over
                // both and free the second - which also retires that record's
                // reference.
                self.v[b].len = self.v[a].end() - self.v[b].base;
                self.v[b].file_len = if file.is_some() { self.v[b].len } else { 0 };
                if let Some(h) = self.v[a].file {
                    filemap::close(h);
                }
                self.v[a] = Vma::EMPTY;
                true
            }
            (Some(b), None) => {
                self.v[b].len += bytes;
                self.v[b].file_len = if file.is_some() { self.v[b].len } else { 0 };
                true
            }
            (None, Some(a)) => {
                self.v[a].len += bytes;
                self.v[a].base = base;
                // The record now starts lower, so its file range starts earlier too.
                self.v[a].file_off = file_off;
                self.v[a].file_len = if file.is_some() { self.v[a].len } else { 0 };
                true
            }
            (None, None) => match self.free_slot() {
                Some(i) => {
                    self.v[i] = Vma {
                        base,
                        len: bytes,
                        prot,
                        flags,
                        file,
                        file_off,
                        file_len,
                    };
                    true
                }
                None => {
                    // Refused: hand the reference back rather than leak the handle.
                    if let Some(h) = file {
                        filemap::close(h);
                    }
                    false
                }
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
                    self.v[i].file_len = m.file_len.min(self.v[i].len);
                    match self.free_slot() {
                        Some(j) => {
                            // One record became two, so the tail takes its own
                            // reference, and its file offset advances by the part
                            // of the file the head plus the hole covered.
                            if let Some(h) = m.file {
                                filemap::addref(h);
                            }
                            self.v[j] = Vma {
                                base: end,
                                len: m.end() - end,
                                prot: m.prot,
                                flags: m.flags,
                                file: m.file,
                                file_off: m.file_off + (end - m.base) as u64,
                                file_len: m.avail_at(end),
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
                (true, false) => {
                    self.v[i].len = base - m.base;
                    self.v[i].file_len = m.file_len.min(self.v[i].len);
                }
                // Trim the head: the surviving pages start further into the file.
                (false, true) => {
                    self.v[i].base = end;
                    self.v[i].len = m.end() - end;
                    self.v[i].file_off = m.file_off + (end - m.base) as u64;
                    self.v[i].file_len = m.avail_at(end);
                }
                // Fully covered: the record - and its reference - go.
                (false, false) => {
                    if let Some(h) = m.file {
                        filemap::close(h);
                    }
                    self.v[i] = Vma::EMPTY;
                }
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
            let (flags, file, avail) = match self.find(page) {
                Some(m) => (m.flags, m.file, m.avail_at(page)),
                None => (0, None, 0),
            };
            let off = match self.find(page) {
                Some(m) => m.file_off + (page - m.base) as u64,
                None => 0,
            };
            // Extend over as many following pages as share this record's flags and
            // backing, so the common case (one whole mapping) costs one insert.
            let mut run = FRAME_SIZE;
            while page + run < end
                && self.find(page + run).map(|m| (m.flags, m.file)) == Some((flags, file))
            {
                run += FRAME_SIZE;
            }
            // A reprotect keeps the mapping's backing. `insert_backed` consumes a
            // reference, and `remove` inside it releases the old record's, so the
            // count is unchanged - but the new reference has to be taken first.
            if let Some(h) = file {
                filemap::addref(h);
            }
            // The run may end before the file content does, so clamp: a reprotect
            // must not extend how far a record claims to be backed.
            let backing = file.map(|h| Backing {
                file: h,
                off,
                len: avail.min(run),
            });
            self.insert_backed(page, run, prot, flags, backing);
            page += run;
        }
    }
}

impl Default for VmaList {
    fn default() -> Self {
        Self::new()
    }
}
