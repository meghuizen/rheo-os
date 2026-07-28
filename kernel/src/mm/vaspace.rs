//! **The per-cell virtual-address allocator** (docs/SUBSTRATE.md pillar 2): a
//! real record of what a cell has mapped where, replacing fixed region bases and
//! bump cursors.
//!
//! ## The defect this exists to remove
//!
//! A cell's address space was a set of compile-time constants and forward-only
//! cursors: image at 1-4 GiB, stack top 8 GiB, anonymous `mmap` from 12 GiB,
//! queue ring at 16 GiB, file `mmap` from 20 GiB, channels at 24 GiB, grants
//! from 32 GiB, the ELF interpreter at 64 GiB - every one a magic number in
//! `load.rs`/`user.rs`, and every allocator between them a bump pointer with no
//! record of what it had handed out.
//!
//! Three consequences, all observed rather than theorised:
//!
//! 1. **Collisions were silent.** The anonymous `mmap` cursor was unbounded, so
//!    a long enough run of mappings walked out of its 12 GiB region, through the
//!    cell's own queue-pair ring at 16 GiB and its channels at 24 GiB, and into
//!    the dynamic linker at 64 GiB - handing a program addresses aliasing its
//!    own `ld.so` with no error (docs/ARCHITECTURE-DEBT.md 4.0 blocker 2). The
//!    fix at the time was to *bound the cursor and refuse*, which stops the
//!    corruption without giving the space back.
//! 2. **Every large reservation forced a hand-edit of the map.** JavaScriptCore
//!    asks for a **128 GiB** Gigacage in one `MAP_NORESERVE` call; making that
//!    fit meant moving the `mmap` window to 80..252 GiB by hand (GOAL-BUN).
//!    V8 wants a 4 GiB pointer-compression cage plus separate code ranges. Each
//!    such program is a new edit to a shared constant, which is the same
//!    "limit raise, not a design change" pattern the fixed tables had.
//! 3. **All three ISAs were held to the narrowest one.** `USER_VA_MAX` was
//!    `2^38` - RISC-V Sv39's user half - so x86-64 (`2^47`) and ARM64 (`2^48`)
//!    gave up over 99% of their address space to keep one constant portable.
//!    The ceiling is now per-ISA ([`crate::arch::USER_VA_TOP`]) and this
//!    allocator works against whatever each reports.
//!
//! ## What replaces it
//!
//! A [`VaSpace`] per cell: a sorted list of [`Region`] records in
//! [`crate::mm::kmeta`]-funded storage (so the number of regions is bounded by
//! the cell's memory budget, not by a constant), plus first-fit allocation with
//! **guard gaps** between neighbours. Placement moves from "which constant did
//! we assign this subsystem" to "ask the allocator", and the answer is reported
//! to the cell through the interfaces that already exist for it -
//! `SYS_QUEUE_INFO` reports the queue VA, `SYS_CONNECT` the channel VA,
//! `SYS_GRANT` the grant VA - so **the ABI does not change**; the constants
//! simply stop being constants.
//!
//! Overlap becomes impossible rather than unlikely: [`VaSpace::reserve_fixed`]
//! refuses a request that intersects an existing region, and
//! [`VaSpace::reserve`] only ever returns a gap. That is the property the bump
//! cursors could not have, and it is checked here once instead of at each of the
//! callers that used to compare against a handful of region constants.
//!
//! ## Guard gaps
//!
//! Every allocated region is followed by at least [`GUARD_PAGES`] unmapped pages
//! (and the allocator never places a region *in* another's guard). A stray
//! sequential write past the end of one mapping therefore faults instead of
//! silently landing in the next one - which is what the old layout got for free
//! from its enormous inter-region distances, and which a compact allocator has
//! to provide deliberately. The stack's guard is the same mechanism: its
//! reservation's low bound is a gap, so growing past it faults (the behaviour
//! `linux::mem` already relies on).
//!
//! ## SMP
//!
//! A `VaSpace` belongs to exactly one cell, and pillar 3's model is that a cell
//! is scheduled by one core at a time - so the common path needs no lock. What
//! *does* race under SMP is a mapping operation against a concurrent page fault
//! on another core in the same cell (SMP.md 10.2 names this: "per-address-space
//! lock; a remote-TLB shootdown IPI for unmap/protect"). This type therefore
//! carries its own [`crate::smp::SpinLock`]-shaped discipline at the level above:
//! callers mutate a `VaSpace` only while holding the owning address space's lock.
//! The structure itself is deliberately lock-free and single-owner, so the lock
//! lives with the thing it protects (the address space) rather than being
//! duplicated here.

use super::kmeta::{Funded, Owner};
use crate::arch;

/// Page size all regions are aligned and rounded to.
pub const PAGE: usize = super::frames::FRAME_SIZE;

/// Unmapped pages the allocator keeps after every region. See the module docs.
pub const GUARD_PAGES: usize = 4;

/// Lowest VA the allocator will hand out.
///
/// Page zero must never be mappable (a null dereference has to fault), and the
/// first megabyte is left clear so that a small negative offset from a valid
/// pointer also faults rather than landing in a real mapping. Fixed placement
/// below this is refused too, which is what makes "a null pointer is never
/// valid" a property of the address space rather than of each caller.
pub const VA_FLOOR: usize = 0x10_0000;

/// What a region of a cell's address space is for.
///
/// Recorded per region so a fault, a `munmap`, or a diagnostic can say *what*
/// was at an address rather than inferring it from which constant range the
/// address falls in - the inference the old fixed map forced and that
/// docs/ENGINEERING.md 1 rules out.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RegionKind {
    /// A `PT_LOAD` segment of the program image.
    Image,
    /// The ELF interpreter's image (`ld.so`).
    Interp,
    /// The initial thread stack (grows down into its own reservation).
    Stack,
    /// Anonymous memory from `mmap`/`SYS_MMAP`.
    Anon,
    /// A file-backed `mmap`.
    File,
    /// The cell's queue-pair ring.
    Queue,
    /// A cross-cell channel ring.
    Channel,
    /// A typed memory grant (object 5).
    Grant,
    /// The shared `.user` window, or another kernel-placed fixed mapping.
    Fixed,
    /// A device BAR window (docs/GPU-HARDWARE.md 5, docs/DRIVERS.md 4.1).
    DeviceBar,
}

/// One contiguous span of a cell's address space.
#[derive(Copy, Clone, Debug)]
pub struct Region {
    /// Base VA, page-aligned.
    pub base: usize,
    /// Length in bytes, page-rounded. Zero means the record is free.
    pub len: usize,
    /// What it is.
    pub kind: RegionKind,
    /// Caller-defined tag: the grant id, channel slot, or fd behind this
    /// region. The allocator never interprets it.
    pub tag: u32,
}

impl Region {
    const FREE: Region = Region {
        base: 0,
        len: 0,
        kind: RegionKind::Fixed,
        tag: 0,
    };

    /// Whether this record is in use.
    pub fn live(&self) -> bool {
        self.len != 0
    }

    /// Exclusive end VA.
    pub fn end(&self) -> usize {
        self.base.saturating_add(self.len)
    }

    /// Whether `va` falls inside this region.
    pub fn contains(&self, va: usize) -> bool {
        self.live() && va >= self.base && va < self.end()
    }
}

/// Why a reservation was refused. Each is a distinct answer a caller can act on
/// rather than one catch-all failure (docs/ENGINEERING.md: a rejection is a
/// deliverable).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum VaError {
    /// Zero length, or a length that overflows the address space.
    BadLength,
    /// A fixed request below [`VA_FLOOR`] or above this ISA's user ceiling.
    OutOfRange,
    /// A fixed request overlaps a live region.
    Overlap,
    /// No gap of the requested size (with guards) remains below the ceiling.
    Exhausted,
    /// The region table could not grow - the owning cell is out of budget.
    NoMetadata,
}

/// A cell's virtual address space: the regions it has, and where the next one
/// can go.
pub struct VaSpace {
    regions: Funded<Region>,
    /// Highest region index ever used, so scans stop early instead of walking
    /// the whole grown capacity.
    high_water: usize,
    /// Exclusive ceiling for allocation in this space. Defaults to this ISA's
    /// [`crate::arch::USER_VA_TOP`]; a profile may lower it (a cell pinned to
    /// the Sv39-compatible floor for portability testing, say) but never raise
    /// it above what the hardware translates.
    top: usize,
    /// Where the next first-fit scan starts. A hint only - correctness never
    /// depends on it, so a stale value costs a slightly longer scan and nothing
    /// else.
    hint: usize,
}

impl VaSpace {
    /// An empty space holding no regions and no frames, allocating over this
    /// ISA's full user range.
    pub const fn new() -> VaSpace {
        VaSpace {
            regions: Funded::new(),
            high_water: 0,
            top: arch::USER_VA_TOP,
            hint: VA_FLOOR,
        }
    }

    /// Charge this space's region table to `owner`, and reset it to empty.
    /// Called when a cell is installed.
    pub fn init(&mut self, owner: Owner) {
        self.regions.release();
        self.regions.set_owner(owner);
        self.high_water = 0;
        self.hint = VA_FLOOR;
        self.top = arch::USER_VA_TOP;
    }

    /// Pre-fund the region table to `want` records, so the frames it needs are taken
    /// **now** rather than on the first reservation.
    ///
    /// The reason is measurement, not speed. A funded table grows lazily, so its first
    /// frame is charged to whichever operation happens to be first - and if that
    /// operation is inside a proof's frame-cost oracle, the oracle moves. That is the
    /// S1' lesson (docs/SUBSTRATE.md 15) recurring per cell: a first attempt at
    /// recording cell layouts broke the `security` kernel exactly this way. Funding
    /// every cell's table once at boot puts the cost outside every measurement.
    ///
    /// Returns false if the frames are not available.
    pub fn fund(&mut self, want: usize) -> bool {
        self.regions.reserve(want)
    }

    /// Empty the space **without** giving its frames back, so the next occupant of the
    /// slot reuses the table this one was funded with.
    ///
    /// The counterpart of [`VaSpace::fund`]: `init` releases and re-charges, which is
    /// right when the table's storage is the cell's own, and wrong when it is a boot
    /// cost being reused.
    pub fn clear(&mut self) {
        for i in 0..self.high_water {
            self.regions.set(i, Region::FREE);
        }
        self.high_water = 0;
        self.hint = VA_FLOOR;
        self.top = arch::USER_VA_TOP;
    }

    /// Lower the allocation ceiling (never above the ISA's own limit).
    ///
    /// The one legitimate use is running a cell inside a narrower address space
    /// than the hardware offers - the Sv39 floor profile, to prove portable code
    /// does not depend on the wider ISAs' room.
    pub fn set_ceiling(&mut self, top: usize) {
        self.top = top.min(arch::USER_VA_TOP);
    }

    /// The exclusive allocation ceiling in force.
    pub fn ceiling(&self) -> usize {
        self.top
    }

    /// Release every region record and its backing frames. Idempotent, so a
    /// teardown path may call it unconditionally. Does **not** unmap anything -
    /// this structure records placement; the page tables are unmapped by the
    /// address space that owns them.
    pub fn release(&mut self) {
        self.regions.release();
        self.high_water = 0;
        self.hint = VA_FLOOR;
    }

    /// Live regions, for a diagnostic or a teardown walk.
    pub fn iter(&self) -> impl Iterator<Item = Region> + '_ {
        (0..self.high_water).filter_map(move |i| match self.regions.get(i) {
            Some(r) if r.live() => Some(r),
            _ => None,
        })
    }

    /// Number of live regions.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Whether the space has no live regions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Frames the region table itself holds - what the owner is charged for
    /// address-space bookkeeping.
    pub fn metadata_frames(&self) -> usize {
        self.regions.frames_held()
    }

    /// The region containing `va`, if any.
    pub fn find(&self, va: usize) -> Option<Region> {
        self.iter().find(|r| r.contains(va))
    }

    /// Whether `[base, base+len)` is free of every live region.
    fn span_free(&self, base: usize, len: usize) -> bool {
        let end = base.saturating_add(len);
        !self.iter().any(|r| base < r.end() && r.base < end)
    }

    /// Store a record, reusing a freed slot or growing the table.
    fn insert(&mut self, region: Region) -> Result<(), VaError> {
        for i in 0..self.high_water {
            if !self.regions.get(i).map(|r| r.live()).unwrap_or(false) {
                self.regions.set(i, region);
                return Ok(());
            }
        }
        let index = self.high_water;
        if !self.regions.set_growing(index, region) {
            return Err(VaError::NoMetadata);
        }
        self.high_water = index + 1;
        Ok(())
    }

    /// Place `len` bytes anywhere free, honouring `align` (rounded up to a page)
    /// and leaving guard gaps on both sides.
    ///
    /// First-fit ascending from a rolling hint. First-fit rather than best-fit
    /// deliberately: address-space fragmentation is not the pressure here (the
    /// space is 128-256 TiB and a cell holds tens of regions), while a
    /// *predictable, low* allocation address is worth having - it keeps a cell's
    /// layout stable run to run, which is what makes a failure reproducible.
    pub fn reserve(
        &mut self,
        len: usize,
        align: usize,
        kind: RegionKind,
        tag: u32,
    ) -> Result<usize, VaError> {
        let len = round_up(len).ok_or(VaError::BadLength)?;
        if len == 0 {
            return Err(VaError::BadLength);
        }
        let align = align.max(PAGE);
        if !align.is_power_of_two() {
            return Err(VaError::BadLength);
        }
        let guard = GUARD_PAGES * PAGE;

        // Two passes: from the hint to the ceiling, then from the floor to the
        // hint, so a freed low region is reused once the hint has moved past it.
        for (from, to) in [(self.hint, self.top), (VA_FLOOR, self.hint)] {
            let mut candidate = align_up(from.max(VA_FLOOR), align);
            while candidate < to {
                let end = match candidate.checked_add(len) {
                    Some(e) => e,
                    None => break,
                };
                // The guard must also fit below the ceiling, so the region after
                // this one cannot be placed adjacent to it.
                if end.saturating_add(guard) > to {
                    break;
                }
                // Reject if the span *or its guards* touch a live region.
                let probe_base = candidate.saturating_sub(guard).max(VA_FLOOR);
                let probe_len = end.saturating_add(guard) - probe_base;
                if self.span_free(probe_base, probe_len) {
                    self.insert(Region {
                        base: candidate,
                        len,
                        kind,
                        tag,
                    })?;
                    self.hint = align_up(end.saturating_add(guard), PAGE);
                    return Ok(candidate);
                }
                // Skip to just past whatever blocked us, so the scan is O(regions)
                // per gap rather than O(address space / align).
                match self.blocking_end(probe_base, probe_len) {
                    Some(blocked_end) => candidate = align_up(blocked_end + guard, align),
                    None => candidate = align_up(candidate + align, align),
                }
            }
        }
        Err(VaError::Exhausted)
    }

    /// The highest end of any live region intersecting `[base, base+len)`, so a
    /// failed probe can jump past the obstruction instead of stepping.
    fn blocking_end(&self, base: usize, len: usize) -> Option<usize> {
        let end = base.saturating_add(len);
        self.iter()
            .filter(|r| base < r.end() && r.base < end)
            .map(|r| r.end())
            .max()
    }

    /// Record a caller-chosen placement (`MAP_FIXED`, the ELF image's own
    /// `p_vaddr`, a kernel-placed window).
    ///
    /// Refuses [`VaError::Overlap`] rather than evicting. Linux's `MAP_FIXED`
    /// silently unmaps whatever was there, which is a footgun this kernel does
    /// not have to inherit: the Linux personality asks explicitly
    /// ([`VaSpace::release_range`] then reserve) when it must emulate that, so
    /// the destructive step is visible at the call site.
    pub fn reserve_fixed(
        &mut self,
        base: usize,
        len: usize,
        kind: RegionKind,
        tag: u32,
    ) -> Result<usize, VaError> {
        let len = round_up(len).ok_or(VaError::BadLength)?;
        if len == 0 {
            return Err(VaError::BadLength);
        }
        if !base.is_multiple_of(PAGE) {
            return Err(VaError::OutOfRange);
        }
        let end = base.checked_add(len).ok_or(VaError::BadLength)?;
        if base < VA_FLOOR || end > self.top {
            return Err(VaError::OutOfRange);
        }
        if !self.span_free(base, len) {
            return Err(VaError::Overlap);
        }
        self.insert(Region {
            base,
            len,
            kind,
            tag,
        })?;
        Ok(base)
    }

    /// Drop the region based exactly at `base`, returning it.
    pub fn release_at(&mut self, base: usize) -> Option<Region> {
        for i in 0..self.high_water {
            if let Some(r) = self.regions.get(i)
                && r.live()
                && r.base == base
            {
                self.regions.set(i, Region::FREE);
                if base < self.hint {
                    self.hint = base;
                }
                return Some(r);
            }
        }
        None
    }

    /// Drop every region wholly inside `[base, base+len)`, and **split** any that
    /// straddles it, returning how many records were affected.
    ///
    /// This is the `munmap`/`MAP_FIXED`-over-existing shape. A straddling region
    /// is split rather than dropped whole, because dropping it would tell the
    /// caller it may unmap pages that are still legitimately mapped - which is
    /// how a partial `munmap` turns into a use-after-free.
    pub fn release_range(&mut self, base: usize, len: usize) -> usize {
        let Some(len) = round_up(len) else {
            return 0;
        };
        let end = base.saturating_add(len);
        let mut affected = 0;
        // Collect first: `insert` (used by the split) mutates the table, so the
        // scan cannot hold assumptions about slot contents across it.
        for i in 0..self.high_water {
            let Some(r) = self.regions.get(i) else {
                continue;
            };
            if !r.live() || base >= r.end() || r.base >= end {
                continue;
            }
            affected += 1;
            let (lo_keep, hi_keep) = (r.base < base, r.end() > end);
            match (lo_keep, hi_keep) {
                // Entirely covered: drop it.
                (false, false) => {
                    self.regions.set(i, Region::FREE);
                }
                // Keep the head.
                (true, false) => {
                    self.regions.set(
                        i,
                        Region {
                            len: base - r.base,
                            ..r
                        },
                    );
                }
                // Keep the tail.
                (false, true) => {
                    self.regions.set(
                        i,
                        Region {
                            base: end,
                            len: r.end() - end,
                            ..r
                        },
                    );
                }
                // A hole in the middle: keep the head here, record the tail.
                (true, true) => {
                    self.regions.set(
                        i,
                        Region {
                            len: base - r.base,
                            ..r
                        },
                    );
                    let tail = Region {
                        base: end,
                        len: r.end() - end,
                        ..r
                    };
                    // If the tail cannot be recorded the space would silently
                    // forget a live mapping, so restore the original and report
                    // nothing was released for this record.
                    if self.insert(tail).is_err() {
                        self.regions.set(i, r);
                        affected -= 1;
                    }
                }
            }
        }
        if base < self.hint {
            self.hint = base.max(VA_FLOOR);
        }
        affected
    }

    /// Whether every live region lies below the ceiling and no two overlap - the
    /// structural invariant, checkable at any time.
    ///
    /// The `substrate` test kernel asserts this after the allocation and release
    /// it drives, which is the evidence the old bump cursors could not offer: an
    /// overlap was only ever discovered by the program that got corrupted.
    pub fn invariant_holds(&self) -> bool {
        let regions: [Option<Region>; 0] = [];
        let _ = regions;
        for a in self.iter() {
            if a.base < VA_FLOOR || a.end() > self.top || !a.base.is_multiple_of(PAGE) {
                return false;
            }
            for b in self.iter() {
                if a.base == b.base {
                    continue;
                }
                if a.base < b.end() && b.base < a.end() {
                    return false;
                }
            }
        }
        true
    }
}

impl Default for VaSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Round `len` up to a whole number of pages, or `None` on overflow.
fn round_up(len: usize) -> Option<usize> {
    len.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1))
}

/// Round `va` up to `align` (a power of two).
fn align_up(va: usize, align: usize) -> usize {
    (va.wrapping_add(align - 1)) & !(align - 1)
}
