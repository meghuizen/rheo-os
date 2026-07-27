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
//! The table is **funded, not fixed** (docs/SUBSTRATE.md pillar 1): its storage
//! is frames charged to the owning cell, so it grows past any workload's appetite
//! and what refuses a mapping is the cell's own frame budget rather than a global
//! array dimension. Adjacent mappings with identical protection and flags are
//! still **merged**, which keeps the common case cheap - a program that maps many
//! small ranges back to back costs one record, not one per call.
//!
//! Why that matters here specifically: `MAX_VMAS = 128` was measured to be the
//! wrong shape for the target workloads. V8 reserves a pointer-compression cage
//! plus code ranges, JSC reserves its Gigacage, and glibc's malloc adds a 64 MiB
//! arena *per thread* - each of those is one or more records, and a program with
//! a dozen threads and a JIT is already past a hundred before it runs a line of
//! its own code. A full table did not fail cleanly either: `remove` printed a
//! diagnostic and **dropped the tail of a split mapping**, leaving the page-fault
//! handler with no record for pages that were genuinely reserved.

use crate::linux::filemap;
use crate::mm::frames::FRAME_SIZE;
use crate::mm::kmeta::{Funded, Owner};

/// Records a cell's table starts with room for. **Not a ceiling** - the table is
/// [`Funded`] and doubles on demand.
///
/// A dynamically linked glibc program needs roughly a dozen (the program's
/// segments, `ld.so`'s, `libc.so.6`'s, the stack, and glibc's per-thread arenas),
/// so 128 covers the ordinary case without a second growth; a JIT-bearing runtime
/// grows past it and pays for the frames it causes.
pub const INITIAL_VMAS: usize = 128;

/// A hard sanity ceiling on records per cell.
///
/// Deliberately **not** the mechanism that limits anything in practice: a cell
/// exhausts its frame budget long before this, and that refusal is the meaningful
/// one because it names an owner. This exists only so a runaway `mmap` loop is
/// bounded by something nameable, in the shape of Linux's `vm.max_map_count`
/// (whose default is 65530).
pub const VMA_CEILING: usize = 65536;

/// Where a file-backed mapping's missing pages come from. One value rather than
/// three parallel arguments, because the three are only ever meaningful together -
/// a handle with the wrong offset or the wrong length serves the wrong bytes.
#[derive(Copy, Clone)]
pub struct Backing {
    /// The [`filemap`] entry. The record created from this owns one reference to it.
    pub file: filemap::Handle,
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
    pub file: Option<filemap::Handle>,
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
    /// `madvise` state for this range: [`ADV_DONTFORK`], [`ADV_WIPEONFORK`].
    ///
    /// Per record rather than per process because that is the granularity
    /// `madvise` works at - a library marks *its own* pages - and a split or merge
    /// must carry it, which the operations below do.
    pub advice: u16,
}

/// [`Vma::advice`] bit: `MADV_DONTFORK` - the range is **not** inherited by a
/// child, so `fork` leaves it unmapped there.
pub const ADV_DONTFORK: u16 = 1 << 0;
/// [`Vma::advice`] bit: `MADV_WIPEONFORK` - the range is inherited but **zeroed**
/// in the child (docs/SUBSTRATE.md 10a).
///
/// The reason this bit is worth carrying: a userspace CSPRNG's state lives in
/// ordinary anonymous memory, and copy-on-write `fork` duplicates it, so parent
/// and child would generate **identical random streams** - which looks perfectly
/// random in isolation and is therefore not something a test notices by accident.
/// OpenSSL and other libraries defend themselves by asking for exactly this, and
/// a kernel that accepted the request and did nothing would leave them believing
/// they were safe. Honouring it is what makes the promise real.
pub const ADV_WIPEONFORK: u16 = 1 << 1;

impl Vma {
    /// The free-slot value, and - because it is all-zero-bytes in every field
    /// (`Option<Handle>::None` included) - exactly what a freshly allocated
    /// [`Funded`] frame already contains. That is load-bearing: growth does not
    /// have to initialise the new slots, so a grow-then-use path cannot forget to.
    pub(crate) const EMPTY: Vma = Vma {
        base: 0,
        len: 0,
        prot: 0,
        flags: 0,
        file: None,
        file_off: 0,
        file_len: 0,
        advice: 0,
    };

    /// Whether this range carries an advice bit.
    pub fn has_advice(&self, bit: u16) -> bool {
        self.advice & bit != 0
    }

    /// Bytes of file content still available at `at` (>= `base`), i.e. how much of a
    /// page starting there may be read from the file before zero-fill takes over.
    fn avail_at(&self, at: usize) -> usize {
        self.file_len.saturating_sub(at - self.base)
    }

    fn end(&self) -> usize {
        self.base + self.len
    }
}

/// A cell's mapping table: a [`Funded`] array of records, charged to the cell.
pub struct VmaList {
    v: Funded<Vma>,
}

impl VmaList {
    /// An empty list holding no frames. `const`, so it still lives in the
    /// `static mut` per-cell `LinuxState` array; only the *contents* stopped being
    /// fixed. The first [`Self::insert`] grows it.
    pub const fn new() -> VmaList {
        VmaList { v: Funded::new() }
    }

    /// Point this list's frame charges at `owner`, then start empty.
    ///
    /// The one initialisation entry point, used by `install_cell` and
    /// `exec_reinit`: it closes every backing-store reference, hands the frames
    /// back, and re-owns the table - in that order, because [`Funded::set_owner`]
    /// only takes effect while no frames are held (charging a release to the wrong
    /// owner would corrupt the ledger).
    pub fn reinit(&mut self, owner: Owner) {
        self.teardown();
        self.v.set_owner(owner);
    }

    /// Drop every record and release the frames, retiring one `filemap` reference
    /// per file-backed record. Idempotent - a teardown path may call it
    /// unconditionally.
    pub fn teardown(&mut self) {
        self.close_all();
        self.v.release();
    }

    /// Drop every record, releasing one `filemap` reference per file-backed one,
    /// **keeping** the frames for the records about to replace them.
    pub fn clear(&mut self) {
        self.close_all();
        self.v.fill(Vma::EMPTY);
    }

    fn close_all(&mut self) {
        for i in 0..self.v.capacity() {
            if let Some(m) = self.v.get(i)
                && m.len != 0
                && let Some(h) = m.file
            {
                filemap::close(h);
            }
        }
    }

    /// Slots currently addressable without growth, and frames held - diagnostics
    /// and the witness the `substrate`/`mmapdp` proofs assert against.
    pub fn slots(&self) -> usize {
        self.v.capacity()
    }

    /// Frames this list holds, directory included.
    pub fn frames_held(&self) -> usize {
        self.v.frames_held()
    }

    /// Copy `src`'s records into this list - the **`fork`** step
    /// (docs/LINUX-COMPAT.md L6).
    ///
    /// A [`Funded`] table cannot be duplicated by the raw `copy_nonoverlapping`
    /// that clones the rest of a `LinuxState`: that copies the table's
    /// *descriptor*, so parent and child would address one shared directory frame
    /// and every child mapping would appear in the parent. So the records are
    /// copied explicitly here, and `linux::dup_state` overwrites the aliased
    /// descriptor before calling this.
    ///
    /// References are **not** taken here; the caller follows with
    /// [`Self::inherit_files`], keeping the addref adjacent to the other
    /// fork-inherited refcounts rather than hidden inside a copy helper.
    ///
    /// Returns false when the child's frame budget cannot fund a table this size,
    /// which is a `fork` the caller must refuse - a partially copied list would
    /// leave the child faulting on mappings it believes it has.
    pub fn copy_from(&mut self, src: &VmaList) -> bool {
        let n = src.v.capacity();
        if n > 0 && !self.v.reserve(n) {
            return false;
        }
        for i in 0..n {
            if let Some(m) = src.v.get(i) {
                self.v.set(i, m);
            }
        }
        true
    }

    /// Every live record, by value.
    ///
    /// Yields `Vma` rather than `&Vma`: a funded element's address is stable, but
    /// handing out references would borrow the table for the iterator's whole life
    /// and block the mutating walks below. The record is 7 words, so a copy is
    /// cheaper than the borrow discipline would be.
    fn live(&self) -> impl Iterator<Item = Vma> + '_ {
        (0..self.v.capacity()).filter_map(move |i| match self.v.get(i) {
            Some(m) if m.len != 0 => Some(m),
            _ => None,
        })
    }

    /// The index of the first live record satisfying `pred`.
    fn position(&self, pred: impl Fn(&Vma) -> bool) -> Option<usize> {
        (0..self.v.capacity()).find(|&i| match self.v.get_ref(i) {
            Some(m) => m.len != 0 && pred(m),
            None => false,
        })
    }

    /// The mapping containing `addr`, if any. **This is the page-fault lookup**;
    /// everything else here exists to keep it correct.
    pub fn find(&self, addr: usize) -> Option<Vma> {
        self.live().find(|m| addr >= m.base && addr < m.end())
    }

    /// The file backing `addr`, if any - the second half of the page-fault lookup:
    /// **which** file, **where** in it, and **how many** bytes of this page come from
    /// it (the rest are zero). Returns `None` for an anonymous mapping, and also once
    /// the page lies wholly past the file content, because then there is nothing to
    /// read and a zeroed frame is already the right answer.
    pub fn file_at(&self, addr: usize) -> Option<(filemap::Handle, u64, usize)> {
        let m = self.find(addr)?;
        let h = m.file?;
        let page = addr & !(FRAME_SIZE - 1);
        let avail = m.avail_at(page).min(FRAME_SIZE);
        (avail > 0).then_some((h, m.file_off + (page - m.base) as u64, avail))
    }

    /// Add a backing-store reference for every live file-backed record - the **`fork`**
    /// step, and the exact twin of `fd::inherit_pipe_ends`.
    ///
    /// [`Self::copy_from`] duplicates the records while touching no refcount, so the
    /// addref lives here, called by `linux::dup_state` beside the other fork-inherited
    /// refcounts rather than hidden inside the copy.
    ///
    /// Without this call the child's records name entries it holds no reference to, the
    /// child's exit releases one per record and drives the count to zero, and the
    /// **parent** then faults against a freed entry and gets a zero page - in the
    /// process that did nothing wrong, long after the fork.
    ///
    /// Every refcounted thing a fork shares needs a reference added here; the calls are
    /// kept adjacent in `dup_state` so that reads as a list rather than a habit.
    pub fn inherit_files(&self) {
        for m in self.live() {
            if let Some(h) = m.file {
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
    /// Walks the live mappings and returns the first gap that fits. O(n^2) in the
    /// record count, which at the ordinary hundred-odd records is nothing next to
    /// the page mapping the caller is about to do. Now that the table grows, that
    /// cost grows too: a cell with thousands of mappings pays a quadratic scan per
    /// `mmap`, so a sorted list or an interval tree is the named follow-on - and the
    /// reason it is a follow-on rather than part of this change is that a placement
    /// index is a different piece of work from a table that funds itself, and mixing
    /// them would make neither reviewable.
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

    /// A slot that can hold a new record, **growing the table** when every existing
    /// slot is live.
    ///
    /// `None` is now a genuine resource refusal - the cell's frame budget or the
    /// pool's metadata reserve - rather than "the array dimension was reached", which
    /// is the whole point of the change. Growth doubles, so a program that maps in a
    /// loop pays a logarithmic number of growths rather than one per mapping.
    fn free_slot(&mut self) -> Option<usize> {
        let cap = self.v.capacity();
        for i in 0..cap {
            if self.v.get(i).is_some_and(|m| m.len == 0) {
                return Some(i);
            }
        }
        let want = if cap == 0 {
            INITIAL_VMAS
        } else {
            (cap * 2).min(VMA_CEILING)
        };
        if want <= cap || !self.v.reserve(want) {
            return None;
        }
        // A grown frame is zeroed, and `Vma::EMPTY` is all-zero, so the first new
        // slot is already free without initialising anything.
        Some(cap)
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
        let before = self.position(joins_below);
        let after = self.position(joins_above);
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
                // `b` and `a` came from `position`, so both are within capacity;
                // the `if let` is how that is stated without an unreachable panic
                // path (and without a `return false` that would drop the incoming
                // reference already retired just above).
                if let (Some(lo), Some(hi)) = (self.v.get(b), self.v.get(a)) {
                    let grown = hi.end() - lo.base;
                    if let Some(m) = self.v.get_mut(b) {
                        m.len = grown;
                        m.file_len = if file.is_some() { grown } else { 0 };
                    }
                    if let Some(h) = hi.file {
                        filemap::close(h);
                    }
                    self.v.set(a, Vma::EMPTY);
                }
                true
            }
            (Some(b), None) => {
                if let Some(m) = self.v.get_mut(b) {
                    m.len += bytes;
                    m.file_len = if file.is_some() { m.len } else { 0 };
                }
                true
            }
            (None, Some(a)) => {
                if let Some(m) = self.v.get_mut(a) {
                    m.len += bytes;
                    m.base = base;
                    // The record now starts lower, so its file range starts earlier too.
                    m.file_off = file_off;
                    m.file_len = if file.is_some() { m.len } else { 0 };
                }
                true
            }
            (None, None) => match self.free_slot() {
                Some(i) => self.v.set(
                    i,
                    Vma {
                        base,
                        len: bytes,
                        prot,
                        flags,
                        file,
                        file_off,
                        file_len,
                        // A fresh mapping starts with no advice; `madvise` sets it.
                        advice: 0,
                    },
                ),
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
        // The bound is read once: `free_slot` below may grow the table, and the
        // slots it adds are empty, so a record can never be missed by not
        // re-reading the capacity - while re-reading it would rescan the tail of a
        // freshly doubled table on every split.
        let cap = self.v.capacity();
        for i in 0..cap {
            let Some(m) = self.v.get(i) else { continue };
            if m.len == 0 || m.base >= end || m.end() <= base {
                continue;
            }
            let (below, above) = (base > m.base, m.end() > end);
            match (below, above) {
                // Hole strictly inside: keep the head, add the tail.
                (true, true) => {
                    let head = base - m.base;
                    if let Some(h) = self.v.get_mut(i) {
                        h.len = head;
                        h.file_len = m.file_len.min(head);
                    }
                    match self.free_slot() {
                        Some(j) => {
                            // One record became two, so the tail takes its own
                            // reference, and its file offset advances by the part
                            // of the file the head plus the hole covered.
                            if let Some(h) = m.file {
                                filemap::addref(h);
                            }
                            self.v.set(
                                j,
                                Vma {
                                    base: end,
                                    len: m.end() - end,
                                    prot: m.prot,
                                    flags: m.flags,
                                    file: m.file,
                                    file_off: m.file_off + (end - m.base) as u64,
                                    file_len: m.avail_at(end),
                                    // The advice belongs to the *range*, so both
                                    // halves of a split keep it - otherwise a
                                    // WIPEONFORK region silently loses the property
                                    // above the hole, which is precisely the kind of
                                    // partial guarantee that is worse than none.
                                    advice: m.advice,
                                },
                            );
                        }
                        // No longer "the array was full": the table grows, so this
                        // is the cell's frame budget or the metadata reserve
                        // refusing, and the diagnostic says which resource ran out
                        // rather than naming a constant.
                        None => crate::println!(
                            "linux: no funded VMA slot (frame budget) - the {:#x}..{:#x} \
                             tail of a split mapping is unmapped but no longer recorded",
                            end,
                            m.end()
                        ),
                    }
                }
                // Trim the tail.
                (true, false) => {
                    let head = base - m.base;
                    if let Some(h) = self.v.get_mut(i) {
                        h.len = head;
                        h.file_len = m.file_len.min(head);
                    }
                }
                // Trim the head: the surviving pages start further into the file.
                (false, true) => {
                    let (nlen, noff, nflen) = (
                        m.end() - end,
                        m.file_off + (end - m.base) as u64,
                        m.avail_at(end),
                    );
                    if let Some(h) = self.v.get_mut(i) {
                        h.base = end;
                        h.len = nlen;
                        h.file_off = noff;
                        h.file_len = nflen;
                    }
                }
                // Fully covered: the record - and its reference - go.
                (false, false) => {
                    if let Some(h) = m.file {
                        filemap::close(h);
                    }
                    self.v.set(i, Vma::EMPTY);
                }
            }
        }
    }

    /// Set `prot` over `[base, base+bytes)`, splitting records at the edges so a
    /// partial `mprotect` leaves the untouched parts with their old protection.
    ///
    /// Implemented as remove-then-insert over the affected pages, one record per
    /// distinct old protection, so the splitting logic lives in one place.
    /// Set or clear advice bits over `[base, base+bytes)`.
    ///
    /// Unlike [`VmaList::set_prot`] this does **not** re-record the mappings: an
    /// advice change alters no permission and no backing, so splitting a record
    /// would be pure churn (and would consume table slots a program calling
    /// `madvise` in a loop cannot spare). Instead every record that *overlaps* the
    /// range takes the bits.
    ///
    /// The cost of that simplification, stated: advice applied to part of a record
    /// is recorded for the whole record. For [`ADV_WIPEONFORK`] that errs toward
    /// wiping more than asked, which is the safe direction (a wiped page the caller
    /// did not need wiped costs a fault; an unwiped page it did need wiped is a
    /// duplicated random stream). For [`ADV_DONTFORK`] it errs toward *not*
    /// inheriting, which a child could observe as a missing mapping - so
    /// `MADV_DONTFORK` over a partial record is reported as unsupported by the
    /// caller in [`crate::linux::mem::madvise`] rather than silently widened.
    pub fn set_advice(&mut self, base: usize, bytes: usize, set: u16, clear: u16) {
        if bytes == 0 {
            return;
        }
        let end = base.saturating_add(bytes);
        for i in 0..self.v.capacity() {
            if let Some(m) = self.v.get_mut(i) {
                if m.len == 0 {
                    continue;
                }
                let m_end = m.base + m.len;
                if base < m_end && m.base < end {
                    m.advice = (m.advice | set) & !clear;
                }
            }
        }
    }

    /// Whether any record overlapping `[base, base+bytes)` extends outside it -
    /// i.e. whether an advice change here would widen beyond what was asked.
    pub fn advice_would_widen(&self, base: usize, bytes: usize) -> bool {
        let end = base.saturating_add(bytes);
        self.live()
            .any(|m| base < m.end() && m.base < end && (m.base < base || m.end() > end))
    }

    /// Every live record overlapping `[base, base+bytes)`, as `(base, len,
    /// advice)`. Used by `fork` to apply [`ADV_WIPEONFORK`]/[`ADV_DONTFORK`] to a
    /// child, and by `madvise` to walk what it is about to change.
    pub fn overlapping(&self, base: usize, bytes: usize) -> impl Iterator<Item = Vma> + '_ {
        let end = base.saturating_add(bytes);
        self.live().filter(move |m| base < m.end() && m.base < end)
    }

    /// Every live record carrying `bit`.
    pub fn with_advice(&self, bit: u16) -> impl Iterator<Item = Vma> + '_ {
        self.live().filter(move |m| m.advice & bit != 0)
    }

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

    /// Render this list in Linux `/proc/self/maps` format into `out`, returning the
    /// bytes written.
    ///
    /// Every line is derived from a record actually held, so the addresses, lengths and
    /// permissions are the ones the page tables were built from - the point of
    /// generating this rather than seeding a static file (see
    /// [`crate::linux::fd::FdKind::ProcMaps`]).
    ///
    /// The format is the fields a reader parses: `start-end perms offset dev inode
    /// path`. `offset` is the mapping's file offset where there is one and 0 otherwise;
    /// `dev` and `inode` are zero, because a mapping here names a `filemap` entry
    /// rather than a device inode and inventing plausible numbers would be fabricating
    /// identity a program might then try to match. The path column is `[anon]` or
    /// `[file]` for the same reason - the list holds a backing-store *handle*, not the
    /// path it was opened from, so printing a filename would mean guessing one.
    ///
    /// Truncates at a **line boundary** if `out` fills, so the last entry is never a
    /// half-line; the caller reports the truncation.
    pub fn render_maps(&self, out: &mut [u8]) -> usize {
        let mut n = 0usize;
        for m in self.live() {
            let start = n;
            let mut w = |b: u8| {
                if n < out.len() {
                    out[n] = b;
                    n += 1;
                }
            };
            let hex = |v: usize, w: &mut dyn FnMut(u8)| {
                // Lowercase, no leading zeros, at least one digit - `%lx`, which is
                // what the format specifies and what every parser expects.
                let mut buf = [0u8; 16];
                let mut i = 0;
                let mut v = v;
                loop {
                    buf[i] = b"0123456789abcdef"[v & 0xf];
                    i += 1;
                    v >>= 4;
                    if v == 0 {
                        break;
                    }
                }
                while i > 0 {
                    i -= 1;
                    w(buf[i]);
                }
            };
            hex(m.base, &mut w);
            w(b'-');
            hex(m.end(), &mut w);
            w(b' ');
            w(if m.prot & 1 != 0 { b'r' } else { b'-' });
            w(if m.prot & 2 != 0 { b'w' } else { b'-' });
            w(if m.prot & 4 != 0 { b'x' } else { b'-' });
            // MAP_SHARED is 0x01; everything here is private, and `p` is what a reader
            // checks for a copy-on-write mapping.
            w(if m.flags & 1 != 0 { b's' } else { b'p' });
            w(b' ');
            hex(m.file_off as usize, &mut w);
            for b in b" 00:00 0 " {
                w(*b);
            }
            for b in if m.file.is_some() {
                &b"[file]"[..]
            } else {
                &b"[anon]"[..]
            } {
                w(*b);
            }
            w(b'\n');
            if n >= out.len() {
                // The line did not fit: drop it whole rather than emit a fragment.
                return start;
            }
        }
        n
    }
}

impl Default for VmaList {
    fn default() -> Self {
        Self::new()
    }
}
