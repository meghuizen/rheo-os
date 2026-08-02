//! **Funded kernel metadata** (docs/SUBSTRATE.md pillar 1): frame-backed,
//! owner-charged, typed storage for the kernel's own per-cell tables.
//!
//! ## The defect this exists to remove
//!
//! "The kernel is allocation-free" was implemented as "every kernel table is a
//! fixed global array", and the two are not the same statement. The consequence
//! is recorded across this tree's history: `MAX_CELLS` 8 -> 16, `MAX_OBJECTS`
//! 128 -> 512, `MAX_MAPPED_FILES` 8 -> 64, the per-cell grant table 16 -> 64,
//! `MAX_CELL_CHANNELS` frozen at 4 ("the fixed-array ceiling"), `MAX_THREADS` 8.
//! Every one of those was a *raise*, forced by a real workload, and each left the
//! next workload to fail identically - docs/ARCHITECTURE-DEBT.md calls one of them
//! "a limit raise, not a design change" in as many words.
//!
//! A fixed global array also gets the *accounting* wrong, which is the deeper
//! problem: a table shared by every cell means one cell's appetite is another
//! cell's refusal, and the refusal arrives as "table full" - a global condition
//! with no owner - rather than as "you are out of budget", which is attributable.
//!
//! ## What replaces it
//!
//! A [`Funded<T>`] is a growable table of `T` whose backing store is **frames from
//! the ordinary pool, charged to the cell that caused the growth**. The only
//! global limit left is physical memory, and exhaustion is per-owner and
//! attributable. This is MEMORY.md 1's split (the kernel grants coarse memory and
//! meters it; the holder sub-allocates) applied to the kernel's *own* metadata,
//! and it is the seL4 retype idea adapted to budgets rather than to user-visible
//! untyped objects: a cell pays for the kernel structures it causes to exist,
//! without being handed the authority to shape them.
//!
//! What "allocation-free" was actually protecting is kept intact:
//!
//! - **No general-purpose allocator.** There is no malloc, no free list of mixed
//!   sizes, no fragmentation policy. A `Funded<T>` grows by whole frames and
//!   releases whole frames; the only shapes that exist are "one frame" and "a
//!   directory of frames".
//! - **No hidden allocation on a hot path.** Growth happens at the boundary where
//!   a cell asks for something new (mint a capability, open a descriptor, spawn a
//!   context) and is `Option`-fallible there, exactly like [`frames::alloc`]. A
//!   lookup never allocates.
//! - **Bounded, predictable cost.** Indexing is two loads (directory entry, then
//!   the element) and no locking.
//!
//! ## Structure - why a directory rather than a contiguous array
//!
//! The frame allocator is a bitmap over 4 KiB frames and hands out **one frame at
//! a time**; there is no contiguous-range allocator, and adding one would make
//! table growth fail on fragmentation - the worst possible failure mode for
//! kernel metadata, because it is unpredictable and unattributable.
//!
//! So a `Funded<T>` is a **page directory**: one frame holding up to
//! [`PTRS_PER_DIR`] kernel virtual addresses of data frames, each data frame
//! holding [`elems_per_page::<T>()`] elements. Element `i` lives at directory
//! entry `i / per_page`, offset `i % per_page`. No data frame need be adjacent to
//! any other, so growth cannot fail while any frame at all is free.
//!
//! One directory frame therefore spans `PTRS_PER_DIR` data frames = 2 MiB of
//! table, which is ~131,000 capability slots or ~262,000 descriptor entries -
//! four orders of magnitude above the ceilings this replaces. A second directory
//! level (a directory of directories) is the documented extension when a single
//! table genuinely needs more than 2 MiB; it is deliberately not built now,
//! because building it would be speculation rather than headroom, and
//! [`Funded::reserve`] refuses cleanly at the boundary instead of silently
//! truncating.
//!
//! ## Addressing
//!
//! Directory entries hold **kernel virtual addresses** (what `phys_to_virt` of an
//! allocated frame returns), not physical addresses. These frames are read and
//! written by the kernel through its own linear map and are never handed to a
//! device or mapped into a cell, so the physical address is not the useful form.
//! Frames are released to the pool by converting back with `virt_to_phys`.
//!
//! ## SMP
//!
//! Single-CPU today (docs/SMP.md 10.2 lists this module's statics in the audit
//! set). The natural shape is per-CPU: the charge ledger becomes per-CPU
//! partitioned state and a `Funded<T>` belonging to a cell is mutated only by the
//! CPU that owns the cell, so the tables need no lock of their own - only the
//! frame allocator underneath does. Nothing here assumes a global lock.

use super::frames::{self, FRAME_SIZE};
use crate::arch;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::ptr::{addr_of, addr_of_mut};

/// Kernel virtual addresses of data frames that fit in one directory frame.
pub const PTRS_PER_DIR: usize = FRAME_SIZE / size_of::<usize>();

/// How many owners the charge ledger tracks: one per possible cell, plus the
/// kernel's own slot.
///
/// Unlike the ceilings this module removes, this one bounds **only the
/// accounting**, not any table's capacity, and it is a property of the cell
/// index space rather than of any workload. It is sized well above
/// [`crate::user::MAX_CELLS`] so raising the cell count never has to touch it.
pub const MAX_OWNERS: usize = 1024;

/// Frames held back from **metadata** allocation, so kernel tables can never
/// consume the last of the pool that page tables and I/O buffers need.
///
/// This is the [`frames::USER_RESERVE_FRAMES`] idea applied to the other
/// direction of pressure: that reserve stops a *cell's* mappings from starving
/// the kernel, this one stops the *kernel's own tables* from doing it. 1024
/// frames = 4 MiB.
pub const META_RESERVE_FRAMES: usize = 1024;

/// Who pays for a metadata allocation, and whose teardown releases it.
///
/// Not a capability and not authority: it is an accounting tag the kernel stamps
/// itself, in the shape of docs/IDENTITY.md's principal (something the kernel
/// derives and refuses to let a cell choose).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Owner(u16);

impl Owner {
    /// The kernel itself: boot-time structures that outlive every cell and are
    /// never released by a cell's teardown.
    pub const KERNEL: Owner = Owner(u16::MAX);

    /// The owner tag for a cell index.
    pub const fn cell(index: usize) -> Owner {
        // A cell index is always far below MAX_OWNERS; the mask keeps this a
        // `const fn` with no panic path rather than trusting the caller.
        Owner((index & (MAX_OWNERS - 1)) as u16)
    }

    /// Whether this is the kernel's own tag.
    pub fn is_kernel(self) -> bool {
        self.0 == u16::MAX
    }

    /// The cell index, or `None` for [`Owner::KERNEL`].
    pub fn cell_index(self) -> Option<usize> {
        if self.is_kernel() {
            None
        } else {
            Some(self.0 as usize)
        }
    }

    fn ledger_slot(self) -> usize {
        if self.is_kernel() {
            MAX_OWNERS - 1
        } else {
            (self.0 as usize).min(MAX_OWNERS - 2)
        }
    }
}

/// Frames currently charged to each owner. The witness a test asserts against
/// (docs/ENGINEERING.md 1: observe, never infer) and what makes an exhaustion
/// refusal attributable to the cell that caused it.
static mut CHARGED: [u32; MAX_OWNERS] = [0; MAX_OWNERS];
/// Total frames held as kernel metadata, across all owners.
static mut META_FRAMES: usize = 0;
/// Metadata frame allocations performed, and releases. Counters, not policy -
/// they exist so growth can be observed rather than assumed.
static mut META_ALLOCS: u64 = 0;
static mut META_FREES: u64 = 0;

/// The NUMA node each owner's metadata is placed on (docs/SUBSTRATE.md pillar 6),
/// [`frames::NODE_ANY`] for "wherever".
///
/// Held **here** rather than read from the owning cell, so the dependency stays
/// one-way: `user` sets it at install, `mm` never reaches up into `user`. That is the
/// same layering the boot sequencer exists to preserve (docs/ARCHITECTURE-DEBT.md
/// 3.6) - a cell's page tables and capability tables are metadata *about* a cell,
/// which is not a reason for the memory manager to depend on the cell module.
static mut OWNER_NODE: [u8; MAX_OWNERS] = [frames::NODE_ANY; MAX_OWNERS];

/// How many frames `owner` currently holds as kernel metadata.
pub fn charged(owner: Owner) -> usize {
    // SAFETY: single CPU, synchronous traps; a plain table read.
    unsafe { (*addr_of!(CHARGED))[owner.ledger_slot()] as usize }
}

/// Total frames held as kernel metadata across every owner.
pub fn total_frames() -> usize {
    // SAFETY: single CPU.
    unsafe { *addr_of!(META_FRAMES) }
}

/// (metadata frame allocations, releases) since boot.
pub fn counters() -> (u64, u64) {
    // SAFETY: single CPU.
    unsafe { (*addr_of!(META_ALLOCS), *addr_of!(META_FREES)) }
}

/// Place `owner`'s future metadata frames on `node` (docs/SUBSTRATE.md pillar 6).
///
/// Called by `user::install` with the cell's home node. Only affects allocations made
/// **after** it: a table that already grew keeps the frames it has, which is why this
/// is set before a cell's tables are funded rather than adjusted later.
pub fn set_owner_node(owner: Owner, node: u8) {
    // SAFETY: single CPU, synchronous trap; a plain table write.
    unsafe { (*addr_of_mut!(OWNER_NODE))[owner.ledger_slot()] = node };
}

/// The node `owner`'s metadata is placed on.
pub fn owner_node(owner: Owner) -> u8 {
    // SAFETY: single CPU; a plain table read.
    unsafe { (*addr_of!(OWNER_NODE))[owner.ledger_slot()] }
}

/// Take one zeroed frame for kernel metadata, charged to `owner`, and return its
/// **kernel virtual address**.
///
/// `None` when the pool has reached [`META_RESERVE_FRAMES`] or is empty. Metadata
/// growth is therefore refusable at every call site, the [`frames::alloc`]
/// discipline: a cell can drive this (it grows when the cell mints a capability
/// or opens a descriptor), so exhaustion must be an answer rather than a panic.
fn alloc_frame(owner: Owner) -> Option<usize> {
    let (free, _) = frames::stats();
    if free <= META_RESERVE_FRAMES {
        return None;
    }
    // On the owner's own node, so a cell's page tables and capability tables sit
    // beside the memory they describe (docs/SUBSTRATE.md pillar 6). `NODE_ANY` -
    // every owner before `set_owner_node`, and every owner on a machine with one
    // node - is `frames::alloc` exactly as before.
    let pa = frames::alloc_on(owner_node(owner))?;
    // SAFETY: single CPU; plain counter updates.
    unsafe {
        let charged = &mut *addr_of_mut!(CHARGED);
        let slot = owner.ledger_slot();
        charged[slot] = charged[slot].saturating_add(1);
        *addr_of_mut!(META_FRAMES) += 1;
        *addr_of_mut!(META_ALLOCS) = (*addr_of!(META_ALLOCS)).wrapping_add(1);
    }
    // The ledger, as a *stream*. A per-owner total says a number changed; this says who
    // caused it and when, which is the difference between "the pool is 2 frames short"
    // and "cell 3 took two and never gave them back" (docs/LOGGING.md 5).
    trace_owner(crate::trace::Kind::Acquire, owner, 1, pa as u64);
    Some(arch::phys_to_virt(pa))
}

/// The trace tag for an owner: the cell index, or [`crate::trace::OWNER_KERNEL`].
fn owner_tag(owner: Owner) -> u16 {
    match owner.cell_index() {
        Some(i) => i as u16,
        None => crate::trace::OWNER_KERNEL,
    }
}

/// Record one metadata-frame movement against `owner`.
fn trace_owner(kind: crate::trace::Kind, owner: Owner, n: u64, detail: u64) {
    crate::trace::emit(
        crate::trace::Subsys::Kmeta,
        kind,
        owner_tag(owner),
        n,
        detail,
    );
}

/// Move the charge for `n` frames from one owner's ledger to another's.
///
/// The accounting half of a table changing hands. A [`Funded`] credits frames back to the
/// owner recorded **at release time**, so a table that grew under one owner and is later
/// adopted by another would credit a ledger that was never charged - a silent corruption
/// of the very accounting that makes exhaustion attributable (docs/SUBSTRATE.md pillar 1).
/// Modelling the transfer explicitly is what lets a launcher build a table and a cell
/// adopt it, which is the real lifecycle of a capability table.
fn move_charge(from: Owner, to: Owner, n: usize) {
    if from == to || n == 0 {
        return;
    }
    crate::trace::emit(
        crate::trace::Subsys::Kmeta,
        crate::trace::Kind::Transfer,
        owner_tag(to),
        n as u64,
        owner_tag(from) as u64,
    );
    // SAFETY: single CPU; plain counter updates.
    unsafe {
        let charged = &mut *addr_of_mut!(CHARGED);
        let (f, t) = (from.ledger_slot(), to.ledger_slot());
        charged[f] = charged[f].saturating_sub(n as u32);
        charged[t] = charged[t].saturating_add(n as u32);
    }
}

/// Release a metadata frame taken by [`alloc_frame`], by its kernel VA.
fn free_frame(va: usize, owner: Owner) {
    let pa = arch::virt_to_phys(va);
    trace_owner(crate::trace::Kind::Release, owner, 1, pa as u64);
    // SAFETY: single CPU; plain counter updates.
    unsafe {
        let charged = &mut *addr_of_mut!(CHARGED);
        let slot = owner.ledger_slot();
        charged[slot] = charged[slot].saturating_sub(1);
        *addr_of_mut!(META_FREES) = (*addr_of!(META_FREES)).wrapping_add(1);
        if *addr_of!(META_FRAMES) > 0 {
            *addr_of_mut!(META_FRAMES) -= 1;
        }
    }
    frames::free_if_pool(pa);
}

/// Take one zeroed frame of kernel metadata, charged to `owner`, returning its
/// **kernel virtual address**.
///
/// The public form of [`alloc_frame`], for a subsystem whose storage is exactly
/// one frame and needs no directory - the metrics histograms
/// ([`crate::metrics`]) are the case this exists for. Everything about the
/// charge, the reserve and the refusal is identical to a [`Funded`] table's
/// growth; only the shape is simpler.
///
/// The caller owns the frame and must return it with [`free_metric_frame`].
pub fn alloc_metric_frame(owner: Owner) -> Option<usize> {
    alloc_frame(owner)
}

/// Return a frame taken by [`alloc_metric_frame`], by its kernel VA.
pub fn free_metric_frame(va: usize, owner: Owner) {
    free_frame(va, owner)
}

/// Elements of `T` that fit in one frame.
pub const fn elems_per_page<T>() -> usize {
    let size = size_of::<T>();
    if size == 0 || size > FRAME_SIZE {
        // A zero-sized or oversized element is a programming error, not a runtime
        // condition. `Funded::reserve` refuses rather than dividing by zero; this
        // keeps the function total so it stays usable in const position.
        0
    } else {
        FRAME_SIZE / size
    }
}

/// A growable, frame-backed table of `T`, charged to an [`Owner`].
///
/// `T` must be `Copy` and must not need drop: this is kernel metadata (integers,
/// small `repr(C)` records, `Option<NonZero>`-shaped handles), stored in raw
/// frames with no drop glue and zero-initialised on growth. That constraint is
/// what keeps the module free of a general allocator's obligations.
///
/// Created empty and `const`, so it can live in the `static mut` per-cell arrays
/// the kernel already uses while the *contents* stop being fixed. It holds no
/// frames until something calls [`Funded::reserve`].
pub struct Funded<T: Copy> {
    /// Kernel VA of the directory frame, or 0 before first growth.
    dir: usize,
    /// Data frames currently held.
    pages: usize,
    /// Element capacity = `pages * elems_per_page::<T>()`.
    cap: usize,
    /// Who is charged for the frames.
    owner: Owner,
    _marker: PhantomData<T>,
}

impl<T: Copy> Funded<T> {
    /// An empty table holding no frames, charged to nobody until it grows.
    pub const fn new() -> Funded<T> {
        Funded {
            dir: 0,
            pages: 0,
            cap: 0,
            owner: Owner::KERNEL,
            _marker: PhantomData,
        }
    }

    /// Point this table's future charges at `owner`. Call before the first
    /// [`Funded::reserve`]; changing it while frames are held would misattribute
    /// the release, so it is refused then (the table keeps its current owner).
    /// Charge this table's frames to `owner`, **transferring** any it already holds.
    ///
    /// It used to change the owner only while the table was still empty and otherwise do
    /// nothing at all - safe for the ledger, but silently wrong about who owns the
    /// frames, which is the one thing the per-owner ledger exists to get right
    /// (docs/SUBSTRATE.md pillar 1: exhaustion must be attributable). Silence there also
    /// forced an ordering rule on every caller - "set the owner before the first growth" -
    /// that a capability table cannot obey, because a launcher builds it and a cell adopts
    /// it at `install` (docs/EXECUTION-MODEL.md 9.7).
    pub fn set_owner(&mut self, owner: Owner) {
        move_charge(self.owner, owner, self.frames_held());
        self.owner = owner;
    }

    /// Who is charged for this table's frames.
    pub fn owner(&self) -> Owner {
        self.owner
    }

    /// Elements currently addressable without further growth.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Data frames currently held (excluding the directory frame).
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Frames this table holds in total, directory included - what its owner is
    /// charged.
    pub fn frames_held(&self) -> usize {
        self.pages + usize::from(self.dir != 0)
    }

    /// The largest capacity this table can reach with one directory frame.
    pub fn max_capacity() -> usize {
        PTRS_PER_DIR * elems_per_page::<T>()
    }

    /// Read directory entry `slot`: the kernel VA of a data frame, or 0 if that
    /// slot is unpopulated.
    ///
    /// The directory holds plain `usize`s, never `T`, so these accessors are
    /// deliberately raw-pointer-based rather than handing out a reference to the
    /// frame: a `&'static mut [usize; N]` returned from a method on `Funded<T>`
    /// would force a `T: 'static` bound on the whole type for no reason, and
    /// would also claim a unique borrow that outlives the call.
    fn dir_get(&self, slot: usize) -> usize {
        if self.dir == 0 || slot >= PTRS_PER_DIR {
            return 0;
        }
        // SAFETY: `dir` is a frame this table allocated (zeroed, exactly one
        // frame, reached through the kernel's linear map) and `slot` is bounded
        // above, so the read lies wholly inside it.
        unsafe { *(self.dir as *const usize).add(slot) }
    }

    /// Write directory entry `slot`. No-op when there is no directory frame yet
    /// or the slot is out of range.
    fn dir_set(&mut self, slot: usize, va: usize) {
        if self.dir == 0 || slot >= PTRS_PER_DIR {
            return;
        }
        // SAFETY: as `dir_get`, and `&mut self` gives exclusivity.
        unsafe { *(self.dir as *mut usize).add(slot) = va }
    }

    /// Grow so that at least `want` elements are addressable.
    ///
    /// Returns false and changes **nothing** when growth is impossible: the pool
    /// is exhausted (below [`META_RESERVE_FRAMES`]), `want` exceeds
    /// [`Funded::max_capacity`], or `T` has no valid page layout. A partial
    /// allocation is rolled back, so a failed reserve never leaks a frame and
    /// never leaves a half-grown table - the [`crate::user`] `SYS_MMAP` rollback
    /// discipline.
    pub fn reserve(&mut self, want: usize) -> bool {
        let per_page = elems_per_page::<T>();
        if per_page == 0 || align_of::<T>() > FRAME_SIZE {
            return false;
        }
        if want <= self.cap {
            return true;
        }
        let need_pages = want.div_ceil(per_page);
        if need_pages > PTRS_PER_DIR {
            return false;
        }

        // The directory frame comes first, and is the one allocation that can be
        // rolled back on its own.
        let fresh_dir = if self.dir == 0 {
            match alloc_frame(self.owner) {
                Some(va) => {
                    self.dir = va;
                    true
                }
                None => return false,
            }
        } else {
            false
        };

        let mut added = 0usize;
        let ok = loop {
            if self.pages + added >= need_pages {
                break true;
            }
            match alloc_frame(self.owner) {
                Some(va) => {
                    // The directory exists by this point (allocated just above or
                    // already present) and the index is below PTRS_PER_DIR
                    // because `need_pages` is.
                    self.dir_set(self.pages + added, va);
                    added += 1;
                }
                None => break false,
            }
        };

        if !ok {
            // Roll back exactly what this call took.
            for i in 0..added {
                let va = self.dir_get(self.pages + i);
                self.dir_set(self.pages + i, 0);
                if va != 0 {
                    free_frame(va, self.owner);
                }
            }
            if fresh_dir {
                let dir = self.dir;
                self.dir = 0;
                free_frame(dir, self.owner);
            }
            return false;
        }

        self.pages += added;
        self.cap = self.pages * per_page;
        true
    }

    /// Raw pointer to element `index`, or `None` when it is beyond the current
    /// capacity. The single place index arithmetic happens.
    fn slot_ptr(&self, index: usize) -> Option<*mut T> {
        if index >= self.cap {
            return None;
        }
        let per_page = elems_per_page::<T>();
        let (page, offset) = (index / per_page, index % per_page);
        // `page < self.pages <= PTRS_PER_DIR` because `index < self.cap`.
        let base = self.dir_get(page);
        if base == 0 {
            return None;
        }
        // SAFETY: `offset < per_page`, so the element lies wholly inside the
        // frame; the frame is kernel-owned metadata reached through the linear
        // map, and `T: Copy` needs no drop tracking.
        Some(unsafe { (base as *mut T).add(offset) })
    }

    /// A reference to element `index`, or `None` beyond capacity.
    ///
    /// Exists beside [`Funded::get`] (which copies) because a table migrated from
    /// a fixed array has call sites that take a reference - and, more importantly,
    /// because **an element's address is stable for as long as it is in
    /// capacity**. Growth allocates *new* frames and files them in the directory;
    /// it never moves or reallocates the frames already there. That is what lets a
    /// caller hand out a long-lived pointer to a slot - the Linux personality's
    /// per-context `TrapFrame` is addressed exactly that way - which a `Vec`-shaped
    /// container could not support.
    pub fn get_ref(&self, index: usize) -> Option<&T> {
        // SAFETY: `slot_ptr` bounds the index; the frame is zero-initialised, so
        // every in-capacity slot holds a valid bit pattern for `T`.
        self.slot_ptr(index).map(|p| unsafe { &*p })
    }

    /// Read element `index`, or `None` beyond capacity.
    pub fn get(&self, index: usize) -> Option<T> {
        // SAFETY: `slot_ptr` bounds the index and the frame is zero-initialised,
        // so every in-capacity slot holds a valid bit pattern for `T` (the
        // `T: Copy` + zeroable contract of this module).
        self.slot_ptr(index).map(|p| unsafe { *p })
    }

    /// A mutable reference to element `index`, or `None` beyond capacity.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        // SAFETY: as `get`, plus exclusivity from `&mut self` on a single CPU.
        self.slot_ptr(index).map(|p| unsafe { &mut *p })
    }

    /// Write element `index`, returning false beyond capacity (the caller should
    /// [`Funded::reserve`] first).
    pub fn set(&mut self, index: usize, value: T) -> bool {
        match self.slot_ptr(index) {
            // SAFETY: as `get`.
            Some(p) => unsafe {
                *p = value;
                true
            },
            None => false,
        }
    }

    /// Grow if needed, then write element `index`. False only when growth failed.
    pub fn set_growing(&mut self, index: usize, value: T) -> bool {
        if index >= self.cap && !self.reserve(index + 1) {
            return false;
        }
        self.set(index, value)
    }

    /// Overwrite every element in the current capacity with `value`. Used by the
    /// reset paths that used to assign a fixed array wholesale.
    pub fn fill(&mut self, value: T) {
        for i in 0..self.cap {
            self.set(i, value);
        }
    }

    /// Release every frame and return to the empty state, uncharging the owner.
    /// Idempotent, so a teardown path may call it unconditionally.
    pub fn release(&mut self) {
        for i in 0..self.pages {
            let va = self.dir_get(i);
            self.dir_set(i, 0);
            if va != 0 {
                free_frame(va, self.owner);
            }
        }
        if self.dir != 0 {
            let dir = self.dir;
            self.dir = 0;
            free_frame(dir, self.owner);
        }
        self.pages = 0;
        self.cap = 0;
    }
}

impl<T: Copy> Default for Funded<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Reset the charge ledger and its counters. Called from the boot/reset path
/// between runs, after every [`Funded`] has been released - it does **not** free
/// frames, because it does not know which tables hold them.
pub fn reset_ledger() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(CHARGED) = [0; MAX_OWNERS];
        *addr_of_mut!(META_FRAMES) = 0;
        *addr_of_mut!(META_ALLOCS) = 0;
        *addr_of_mut!(META_FREES) = 0;
    }
}

/// Whether the ledger's per-owner charges still sum to the total it reports -
/// the cheap invariant that keeps the accounting honest, in the shape of
/// [`frames::used_matches_bitmap`]. Asserted by the `substrate` test kernel after
/// the growth and release it drives.
pub fn ledger_consistent() -> bool {
    // SAFETY: single CPU.
    unsafe {
        let charged = &*addr_of!(CHARGED);
        let sum: usize = charged.iter().map(|&c| c as usize).sum();
        sum == *addr_of!(META_FRAMES)
    }
}

/// A per-cell table that is **`N` slots inline and unbounded after that**
/// (docs/EXECUTION-MODEL.md 9.6).
///
/// # Why this exists as a type rather than as a pattern
///
/// Every fixed `[[Slot; K]; MAX_CELLS]` in this kernel is the same defect: an array
/// dimension standing in for a resource limit, raised reactively when a real workload
/// pushed against it (the grant table went 16 -> 64 for the tile battle tier, the object
/// table 128 -> 512). And every one of them has the same *wrong* obvious fix - a
/// [`Funded`] table per cell - which was measured and refused for the vcore records: a
/// directory frame plus a data frame per cell is 8 KiB to hold a few dozen bytes, so
/// funding all of it costs more than the array it replaces.
///
/// The shape that works is inline-plus-tail, and it had been written twice by hand
/// (`user::CELL_VCORES`, `nproc::PROC_WAITS`) before it was written once here. Three
/// hazards come with it and each is easy to get right in one place and easy to forget in
/// the fifth copy:
///
/// - **Growth arrives zeroed.** [`Funded`] hands back freshly allocated frames, and
///   all-zero is a valid `T` only by accident of layout. [`Elastic::grow`] therefore
///   writes `empty` over *every* slot the growth added, not just the one being returned.
///   Skipping that is what invariant I7 caught in `sched::entity` (nine scenarios).
/// - **A release path is needed wherever a slot is handed back**, or the frames leak
///   until the next boot. That is the S1' scar, found twice.
/// - **The descriptor must not be raw-copied.** `Elastic` is deliberately not `Copy`, so
///   a struct holding one cannot be either - which is what forces these tables to live
///   beside the `Copy` per-cell record rather than inside it.
///
/// The common case - a cell using no more than `N` slots - allocates **nothing**, which
/// is what makes the inline half worth its `.bss`.
pub struct Elastic<T: Copy, const N: usize> {
    inline: [T; N],
    tail: Funded<T>,
}

impl<T: Copy, const N: usize> Elastic<T, N> {
    /// A table whose inline half is `inline` and whose tail is empty (holds no frames).
    pub const fn with_inline(inline: [T; N]) -> Elastic<T, N> {
        Elastic {
            inline,
            tail: Funded::new(),
        }
    }

    /// Charge the tail's frames to `owner`, transferring any it already holds
    /// ([`Funded::set_owner`]).
    pub fn set_owner(&mut self, owner: Owner) {
        self.tail.set_owner(owner);
    }

    /// Slots addressable right now: the inline half plus whatever the tail has grown to.
    pub fn capacity(&self) -> usize {
        N + self.tail.capacity()
    }

    /// Frames the tail currently holds. 0 for a cell that never exceeded `N`.
    pub fn frames_held(&self) -> usize {
        self.tail.frames_held()
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        if i < N {
            return self.inline.get(i);
        }
        self.tail.get_ref(i - N)
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i < N {
            return self.inline.get_mut(i);
        }
        self.tail.get_mut(i - N)
    }

    /// Append one slot and return its index, or `None` when the owner's budget refuses
    /// the frame - a clean refusal naming the cell, never a global "table full"
    /// (docs/MEMORY.md 7, no OOM killer).
    pub fn grow(&mut self, empty: T) -> Option<usize> {
        let want = self.tail.capacity();
        let was = want;
        if !self.tail.set_growing(want, empty) {
            return None;
        }
        // Initialise every slot the growth added, not just the one asked for: `Funded`
        // grows by whole frames and those arrive zeroed.
        for i in was..self.tail.capacity() {
            self.tail.set(i, empty);
        }
        Some(N + want)
    }

    /// The first slot for which `is_free` holds, growing by one if none is.
    ///
    /// The allocation shape every caller of a fixed per-cell table already had -
    /// "scan for a free slot, fail if there is none" - with the failure replaced by
    /// growth.
    pub fn alloc(&mut self, empty: T, is_free: impl Fn(&T) -> bool) -> Option<usize> {
        for i in 0..self.capacity() {
            if self.get(i).is_some_and(&is_free) {
                return Some(i);
            }
        }
        self.grow(empty)
    }

    /// Write `value` at `i`. False when `i` is past the capacity.
    pub fn set(&mut self, i: usize, value: T) -> bool {
        match self.get_mut(i) {
            Some(x) => {
                *x = value;
                true
            }
            None => false,
        }
    }

    /// Grow until index `i` is addressable, returning the capacity reached.
    pub fn grow_to(&mut self, i: usize, empty: T) -> Option<usize> {
        while i >= self.capacity() {
            self.grow(empty)?;
        }
        Some(self.capacity())
    }

    /// The index of the first slot satisfying `pred`.
    pub fn position(&self, pred: impl Fn(&T) -> bool) -> Option<usize> {
        (0..self.capacity()).find(|&i| self.get(i).is_some_and(&pred))
    }

    /// The first slot satisfying `pred`.
    pub fn find(&self, pred: impl Fn(&T) -> bool) -> Option<&T> {
        self.get(self.position(pred)?)
    }

    /// [`Elastic::find`], for writing.
    pub fn find_mut(&mut self, pred: impl Fn(&T) -> bool) -> Option<&mut T> {
        let i = self.position(pred)?;
        self.get_mut(i)
    }

    /// Whether `pred` holds for every slot.
    pub fn all(&self, pred: impl Fn(&T) -> bool) -> bool {
        (0..self.capacity()).all(|i| self.get(i).is_some_and(&pred))
    }

    /// Run `f` over every slot, for writing.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(&mut T)) {
        for i in 0..self.capacity() {
            if let Some(x) = self.get_mut(i) {
                f(x);
            }
        }
    }

    /// Clear the inline half and **release the tail's frames**.
    ///
    /// One call, so a slot-handback path cannot reset the table while leaking its frames -
    /// which is exactly the pair of leaks S1' found.
    pub fn reset(&mut self, empty: T) {
        self.inline = [empty; N];
        self.tail.release();
    }
}
