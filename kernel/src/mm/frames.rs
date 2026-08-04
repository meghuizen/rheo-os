//! Physical frame allocator: a bitmap over a fixed per-ISA pool of RAM
//! placed above the kernel image (the pool bounds live in the arch layer;
//! init() checks they clear the image). Frames are 4 KiB and are zeroed
//! on allocation - page tables and user memory must never leak previous
//! contents.
//!
//! # Two layers, and why
//!
//! The **bookkeeping** (bitmap, reference counts, used counter, search hint, NUMA
//! ranges) is one data structure under one lock. The **zeroing** is not part of it:
//! once a frame's bit is set no other core can be handed it, so the 4 KiB `memset`
//! belongs outside the critical section. It used to be inside, where it dominated -
//! a bitmap word test-and-set is a handful of instructions against 4096 bytes of
//! stores - so every core allocating serialised on another core's `memset`.
//!
//! On top of that, the hot operations go through **flat combining**
//! ([`fc_op`], Hendler/Incze/Shavit/Tzafrir, SPAA 2010) rather than each core
//! taking the lock in turn.

use crate::arch;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const FRAME_SIZE: usize = 4096;

/// Pool size: **512 MiB = 131072 frames** -> a 16 KiB bitmap. QEMU runs `-m 1G`
/// on every ISA and the pool base sits 64 MiB into RAM
/// (`arch::FRAME_POOL_BASE`), so ~960 MiB is physically available and the pool
/// fits inside the kernel's linear map on all three ISAs.
///
/// **This is a limit raise, not a design change.** It was 128 MiB, sized for
/// static-glibc fixtures of a few hundred KiB. The binding target is a ~100 MB
/// dynamically-linked binary, which exhausted a 128 MiB pool (and the 96 MiB
/// per-cell budget) before reaching `main`. The *proper* fix is **demand
/// paging** - a fault handler that commits a page when it is first touched, so
/// an image's unreferenced pages cost nothing - which is a later rung of work
/// (docs/LINUX-COMPAT.md, "Demand paging"). Until then the image is committed
/// eagerly and the pool has to hold it.
///
/// Headroom, deliberately: the remaining ~450 MiB is not taken because firmware
/// places blobs near the **top** of RAM - with `-m 1G` QEMU's RISC-V `virt` puts
/// the device tree the kernel parses at ~`0xBFE0_0000`, i.e. 958 MiB in. A pool
/// that reached it would overwrite the DTB. Raising this further means checking
/// that first.
pub const POOL_FRAMES: usize = 131072;

static mut BITMAP: [u64; POOL_FRAMES / 64] = [0; POOL_FRAMES / 64];
static mut NEXT_HINT: usize = 0;
static mut INITIALIZED: bool = false;

/// Which pool frames sit on which NUMA node (docs/SUBSTRATE.md pillar 6).
///
/// One `(lo, hi)` frame-index range per node, `lo == hi` meaning "this node holds no
/// pool frames". A range and not a per-frame node id because the pool is **one
/// contiguous physical span** and the firmware's memory map is already split at node
/// boundaries (`hw::Inventory`), so each node's share of the pool is contiguous by
/// construction - 16 pairs of `usize` instead of a 128 KiB side table.
///
/// Empty until [`init_numa`] runs, which is after hardware discovery: the pool is
/// brought up inside `arch::init()`, long before any firmware table has been read, so
/// "which node is this frame on" is a question that cannot be answered at pool init.
/// Until it is answered every allocation is node-agnostic, which is exactly the
/// pre-NUMA behaviour.
static mut NODE_RANGE: [(usize, usize); crate::hw::MAX_NUMA_NODES] =
    [(0, 0); crate::hw::MAX_NUMA_NODES];
static mut NODES_KNOWN: usize = 0;

/// Allocations that asked for a node and were served from another one.
///
/// Counted rather than hidden. A node-affine allocation that quietly lands on the
/// wrong node is a placement decision the caller believes it made and did not, and on
/// real hardware that is a silent bandwidth cliff rather than an error - so the
/// fallback is a *reported* degradation (docs/ENGINEERING.md 1), the same treatment
/// `net_rx`'s poll tiers and `input`'s interrupt recoveries get.
static FALLBACKS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The allocator's mutual exclusion (docs/SMP.md 10.2, the SMP-safety audit).
///
/// The bitmap, the reference counts, the used counter and the search hint are **one
/// data structure with four fields**, and every operation below reads and writes
/// several of them. Two cores allocating at once without this would hand out the same
/// frame twice: the bitmap test-and-set is not atomic, and even if it were, `USED`
/// and `REFS` would drift from it - which is precisely what
/// [`used_matches_bitmap`] exists to notice, except that by then two cells share a
/// page neither knows about.
///
/// A `SpinLock<()>` rather than wrapping the state, because the state is four
/// separate `static mut`s reached through `addr_of_mut!` and moving them inside the
/// lock would be a large mechanical change to a file the isolation proofs depend on.
/// The guard is the discipline: every function that touches those statics either takes
/// it or is one of the `unsafe` primitives below whose contract requires it to be
/// held, which is checkable by reading this file.
///
/// Unconditional, not `#[cfg(feature = "smp")]`. Locking is a property of the data
/// structure, not of a build configuration (docs/SUBSTRATE.md pillar 3, the lesson
/// that produced the `SYS_YIELD` FP defect: state whose safety depends on which
/// features are enabled gets written twice and diverges).
///
/// It is taken **once per batch** rather than once per operation: the hot paths reach
/// it through [`fc_op`]'s combiner, and the diagnostics and boot-time paths take it
/// directly. What it no longer covers is the 4 KiB zeroing, which used to be the
/// dominant part of every critical section here and is now done by each requesting
/// core after its claim.
static POOL_LOCK: crate::smp::SpinLock<()> =
    crate::smp::SpinLock::named((), crate::obs::lock::LockId::FramePool);

/// How many mappings hold each allocated frame, for **copy-on-write `fork`**
/// (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2). One byte per frame = 128 KiB of
/// static, which buys a `share`/`free` pair simple enough to reason about; a
/// bitmap-plus-overflow-table would save 112 KiB and cost that.
///
/// Zero for a free frame, 1 for the normal case, higher only while a COW `fork`
/// has the frame mapped read-only in more than one address space. **`free` is a
/// decrement**, which is the property that lets every pre-existing caller stay
/// unchanged: with no sharing a decrement from 1 releases the frame exactly as
/// the old unconditional free did.
///
/// [`SHARE_MAX`] is the ceiling. It cannot be reached by `fork` alone (a fork adds
/// one reference and `MAX_CELLS` is 16), but a saturating count that silently
/// stopped counting would leak a frame forever, so the share is **refused** at the
/// ceiling and the caller copies instead.
static mut REFS: [u8; POOL_FRAMES] = [0; POOL_FRAMES];

/// The most mappings one frame may be shared by. `MAX_CELLS` is 16, so a chain of
/// forks cannot exceed it; the check exists so that if something ever could, the
/// answer is a copy rather than a lost count.
pub const SHARE_MAX: u8 = u8::MAX;
/// Frames currently allocated. Kept incrementally so [`stats`] - and therefore
/// [`user_available`], which every cell-driven allocation consults - is O(1)
/// rather than a 512-word popcount of the bitmap. The bitmap stays the source of
/// truth for *which* frames; this counts them, and `debug_used_matches_bitmap`
/// checks the two agree.
static mut USED: usize = 0;

unsafe extern "C" {
    static __kernel_end: u8;
}

/// One-time setup; panics if the pool would overlap the kernel image.
pub fn init() {
    // `__kernel_end` is a kernel virtual address; compare its physical address
    // to the (physical) frame-pool base. Identity on x86/riscv; the high
    // linear-map offset on aarch64 (docs/MEMORY.md).
    let kernel_end = arch::virt_to_phys(core::ptr::addr_of!(__kernel_end) as usize);
    assert!(
        kernel_end <= arch::FRAME_POOL_BASE,
        "kernel image ({kernel_end:#x}) overlaps the frame pool ({:#x})",
        arch::FRAME_POOL_BASE
    );
    unsafe {
        *core::ptr::addr_of_mut!(INITIALIZED) = true;
    }
}

/// Learn which pool frames sit on which NUMA node, from the discovered memory map.
///
/// Called by the boot sequencer **after** `hw::detect()` - the pool itself is brought
/// up inside `arch::init()`, before any firmware table has been read, so this cannot
/// be folded into [`init`].
///
/// With one node (or none reported) every range is left empty and
/// [`alloc_on`] degenerates to [`alloc`], so a machine without NUMA behaves exactly as
/// it did before this existed.
pub fn init_numa(inv: &crate::hw::Inventory) {
    if inv.nnodes < 2 {
        return;
    }
    let pool_lo = arch::FRAME_POOL_BASE as u64;
    let pool_hi = pool_lo + (POOL_FRAMES * FRAME_SIZE) as u64;
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held; this runs once at boot on the primary CPU.
    unsafe {
        let ranges = &mut *core::ptr::addr_of_mut!(NODE_RANGE);
        for r in inv.mem[..inv.nmem].iter() {
            if r.kind != crate::hw::MemKind::Ram || (r.node as usize) >= ranges.len() {
                continue;
            }
            // The pool's slice of this region, as frame indices into the pool.
            let lo = r.base.max(pool_lo);
            let hi = (r.base + r.len).min(pool_hi);
            if lo >= hi {
                continue;
            }
            let flo = ((lo - pool_lo) / FRAME_SIZE as u64) as usize;
            let fhi = ((hi - pool_lo) / FRAME_SIZE as u64) as usize;
            // A node can appear in several regions, so widen rather than overwrite.
            let e = &mut ranges[r.node as usize];
            if e.0 == e.1 {
                *e = (flo, fhi);
            } else {
                *e = (e.0.min(flo), e.1.max(fhi));
            }
        }
        // Widening is only sound while each node's pool slices are *contiguous*.
        // A machine that interleaves nodes inside one span - node 0, node 1, node 0
        // - would give node 0 a widened range that swallows node 1's, so
        // `alloc_on(0)` could hand out node 1's frames while reporting no fallback:
        // a wrong answer that looks like a right one, which is the failure mode
        // docs/ENGINEERING.md 1 exists to stop. Detected rather than assumed away,
        // and the response is to know nothing rather than to know something false -
        // every range cleared, so `alloc_on` degenerates to `alloc` exactly as on a
        // machine with no NUMA at all, with the reason printed.
        let mut overlap = None;
        for a in 0..ranges.len() {
            for b in (a + 1)..ranges.len() {
                let (x, y) = (ranges[a], ranges[b]);
                if x.0 < x.1 && y.0 < y.1 && x.0 < y.1 && y.0 < x.1 {
                    overlap = Some((a, b));
                }
            }
        }
        if let Some((a, b)) = overlap {
            *ranges = [(0, 0); crate::hw::MAX_NUMA_NODES];
            crate::println!(
                "frames: NUMA nodes {a} and {b} interleave inside the frame pool - \
                 node-affine allocation disabled (placement would be wrong, not just \
                 imprecise)"
            );
            return;
        }
        *core::ptr::addr_of_mut!(NODES_KNOWN) = inv.nnodes;
    }
}

/// The pool's frame-index range on `node`, `lo == hi` if it holds none.
pub fn node_range(node: u8) -> (usize, usize) {
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held.
    unsafe {
        let ranges = &*core::ptr::addr_of!(NODE_RANGE);
        ranges.get(node as usize).copied().unwrap_or((0, 0))
    }
}

/// Which NUMA node the frame **containing** `pa` sits on, or [`NODE_ANY`] if `pa` is
/// outside the pool or the node layout is unknown.
///
/// Derived from the same ranges [`alloc_on`] places against, so it answers "where did
/// this frame actually come from" - the question a placement proof and a diagnostic
/// both need, and one nothing else in the tree could answer.
///
/// `pa` need **not** be frame-aligned, and that is deliberate: the addresses a caller
/// has are usually interior - an element inside a funded table, a struct inside a
/// mapped page - and [`in_pool`] answers `false` for those, since it is a question
/// about frames rather than about addresses. Rounding down here rather than at each
/// caller is what stops "not on any node" from meaning "not frame-aligned".
pub fn node_of(pa: usize) -> u8 {
    let pa = pa & !(FRAME_SIZE - 1);
    if !in_pool(pa) {
        return NODE_ANY;
    }
    let frame = (pa - arch::FRAME_POOL_BASE) / FRAME_SIZE;
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held.
    unsafe {
        let ranges = &*core::ptr::addr_of!(NODE_RANGE);
        for (n, &(lo, hi)) in ranges.iter().enumerate() {
            if lo < hi && frame >= lo && frame < hi {
                return n as u8;
            }
        }
    }
    NODE_ANY
}

/// Free pool frames on `node` (`0` if it holds none, or if NUMA is unknown).
///
/// The exact oracle a placement proof needs: "how many frames could this node have
/// given me", so that running it dry is a counted event rather than a guess.
pub fn node_free(node: u8) -> usize {
    let (lo, hi) = node_range(node);
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held, so no other core is mid-update.
    unsafe {
        let bitmap = &*core::ptr::addr_of!(BITMAP);
        (lo..hi.min(POOL_FRAMES))
            .filter(|f| bitmap[f / 64] & (1 << (f % 64)) == 0)
            .count()
    }
}

/// How many NUMA nodes the pool knows about (`0` until [`init_numa`] finds >= 2).
pub fn nodes_known() -> usize {
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held.
    unsafe { *core::ptr::addr_of!(NODES_KNOWN) }
}

/// Allocations that asked for one node and were served from another. See [`FALLBACKS`].
pub fn numa_fallbacks() -> usize {
    FALLBACKS.load(core::sync::atomic::Ordering::Relaxed)
}

/// "No node preference" - what every pre-NUMA caller means, and what a cell that
/// does not ask gets.
pub const NODE_ANY: u8 = u8::MAX;

// ------------------------------------------------------- bookkeeping primitives
//
// The lock-held halves of the public API. Each one does *only* the bitmap /
// refcount / counter work: no zeroing, no copying, no lock of its own. They are the
// unit a flat-combining batch executes, and they are private so that the "every
// public function takes the pool lock exactly once" rule stays checkable by reading
// the file rather than by reading call graphs.

/// Zero `n` frames starting at `pa`, **outside** the pool lock.
///
/// Safe to do without the lock because the frames are already marked allocated: the
/// bitmap bit is what makes a frame unhandable to anyone else, so nothing can observe
/// the window between the claim and the zeroing.
fn zero_frames(pa: usize, n: usize) {
    // SAFETY: `pa` is a claimed pool frame run, reached through the kernel's linear
    // map (identity on x86/riscv, the high map on aarch64 - never the raw PA).
    unsafe { core::ptr::write_bytes(arch::phys_to_virt(pa) as *mut u8, 0, n * FRAME_SIZE) }
}

/// Claim one frame from anywhere in the pool, rotating [`NEXT_HINT`]. **Not zeroed.**
///
/// # Safety
/// The pool lock must be held.
unsafe fn claim_hinted() -> Option<usize> {
    // SAFETY: the pool lock is held, so no other core is mid-update.
    unsafe {
        assert!(
            *core::ptr::addr_of!(INITIALIZED),
            "frame allocator used before init"
        );
        let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
        let hint = *core::ptr::addr_of!(NEXT_HINT);
        for offset in 0..POOL_FRAMES {
            let frame = (hint + offset) % POOL_FRAMES;
            let (word, bit) = (frame / 64, frame % 64);
            if bitmap[word] & (1 << bit) == 0 {
                bitmap[word] |= 1 << bit;
                (*core::ptr::addr_of_mut!(REFS))[frame] = 1;
                *core::ptr::addr_of_mut!(USED) += 1;
                *core::ptr::addr_of_mut!(NEXT_HINT) = frame + 1;
                return Some(arch::FRAME_POOL_BASE + frame * FRAME_SIZE);
            }
        }
        None
    }
}

/// Claim one frame from pool frame indices `[lo, hi)`. **Not zeroed.**
///
/// Deliberately does **not** move [`NEXT_HINT`]: a bounded search must not steer the
/// unrestricted one, or a single node-affine allocation would send every subsequent
/// node-agnostic one hunting through that node's range first.
///
/// # Safety
/// The pool lock must be held.
unsafe fn claim_in(lo: usize, hi: usize) -> Option<usize> {
    // SAFETY: the pool lock is held, so no other core is mid-update.
    unsafe {
        assert!(
            *core::ptr::addr_of!(INITIALIZED),
            "frame allocator used before init"
        );
        let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
        for frame in lo..hi.min(POOL_FRAMES) {
            let (word, bit) = (frame / 64, frame % 64);
            if bitmap[word] & (1 << bit) == 0 {
                bitmap[word] |= 1 << bit;
                (*core::ptr::addr_of_mut!(REFS))[frame] = 1;
                *core::ptr::addr_of_mut!(USED) += 1;
                return Some(arch::FRAME_POOL_BASE + frame * FRAME_SIZE);
            }
        }
        None
    }
}

/// Claim one frame preferring `node`, counting the fallback. **Not zeroed.**
///
/// The whole node decision (read the node's range, search it, fall back to the pool)
/// under the **one** acquisition the caller already holds. It used to be three
/// separate ones, and a range read that is not in the same critical section as the
/// search it bounds is a range that can change under it.
///
/// # Safety
/// The pool lock must be held.
unsafe fn claim_on(node: u8) -> Option<usize> {
    // SAFETY: the pool lock is held.
    unsafe {
        let (lo, hi) = (*core::ptr::addr_of!(NODE_RANGE))
            .get(node as usize)
            .copied()
            .unwrap_or((0, 0));
        if lo < hi {
            if let Some(pa) = claim_in(lo, hi) {
                return Some(pa);
            }
            // The node is known but full. Fall through to the whole pool and say so.
            FALLBACKS.fetch_add(1, Ordering::Relaxed);
        }
        claim_hinted()
    }
}

/// Drop one reference to `pa`, releasing the frame at zero.
///
/// # Safety
/// The pool lock must be held, and `pa` must satisfy [`in_pool`].
unsafe fn release(pa: usize) {
    let frame = (pa - arch::FRAME_POOL_BASE) / FRAME_SIZE;
    // SAFETY: the pool lock is held.
    unsafe {
        let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
        assert!(
            bitmap[frame / 64] & (1 << (frame % 64)) != 0,
            "double free of {pa:#x}"
        );
        let refs = &mut *core::ptr::addr_of_mut!(REFS);
        // A live frame always has at least one reference; a zero here would mean the
        // bitmap and the counts disagreed, so treat it as the last reference rather
        // than wrapping.
        refs[frame] = refs[frame].saturating_sub(1);
        if refs[frame] != 0 {
            return; // still held by another mapping
        }
        bitmap[frame / 64] &= !(1 << (frame % 64));
        *core::ptr::addr_of_mut!(USED) -= 1;
    }
}

/// Take an extra reference to `pa`, or report that it cannot be shared.
///
/// # Safety
/// The pool lock must be held, and `pa` must satisfy [`in_pool`].
unsafe fn add_ref(pa: usize) -> bool {
    let frame = (pa - arch::FRAME_POOL_BASE) / FRAME_SIZE;
    // SAFETY: the pool lock is held.
    unsafe {
        let refs = &mut *core::ptr::addr_of_mut!(REFS);
        if refs[frame] == 0 || refs[frame] == SHARE_MAX {
            return false;
        }
        refs[frame] += 1;
    }
    true
}

/// How many mappings hold `pa`.
///
/// # Safety
/// The pool lock must be held, and `pa` must satisfy [`in_pool`].
unsafe fn ref_count(pa: usize) -> u8 {
    let frame = (pa - arch::FRAME_POOL_BASE) / FRAME_SIZE;
    // SAFETY: the pool lock is held.
    unsafe { (*core::ptr::addr_of!(REFS))[frame] }
}

/// The refcount test and the replacement claim as **one** decision - see
/// [`cow_resolve`].
///
/// # Safety
/// The pool lock must be held, and `pa` must satisfy [`in_pool`].
unsafe fn cow_claim(pa: usize) -> u64 {
    // SAFETY: the pool lock is held.
    unsafe {
        if ref_count(pa) <= 1 {
            return COW_SOLE;
        }
        match claim_hinted() {
            // The old frame keeps its reference: nothing has changed, so the caller
            // can refuse the fault and the mapping stays exactly as it was.
            None => COW_NO_FRAME,
            Some(dst) => dst as u64,
        }
    }
}

// ---------------------------------------------------------------- flat combining
//
// Flat combining (Hendler, Incze, Shavit, Tzafrir - "Flat Combining and the
// Synchronization-Parallelism Tradeoff", SPAA 2010).
//
// The lock above is correct and short, and that is exactly the case flat combining
// improves: N cores each taking a short lock pay N cache-line handoffs of the lock
// word plus N handoffs of every line the critical section touches. Instead, a core
// publishes its request to its **own** cache line, one core wins the *combiner* role,
// takes the lock once, executes the whole batch while the bitmap stays in its cache,
// and writes each result back. The others spin on a line nobody else writes.
//
// Two properties make it fit here rather than being a general-purpose wrapper:
//
// 1. **Every hot operation is a few words of work on one shared structure.** Alloc is
//    a bitmap scan plus three field updates; free is the inverse. Batching them costs
//    the combiner almost nothing per extra request, which is the regime the paper's
//    win comes from. The 4 KiB zeroing is *not* in the batch - it is done by each
//    requesting core, after its request completes, outside the lock. A combiner that
//    zeroed on behalf of the batch would re-serialise precisely what moving the
//    `memset` out of the lock unserialised.
//
// 2. **The uncontended path does not publish at all.** A core tries the combiner
//    election *first*; winning it (always, on a single-CPU boot or an idle machine)
//    means taking the lock and doing its own work directly, then draining a pending
//    mask that is normally zero. So the cost added to an uncontended allocation is two
//    atomics and one relaxed load - there is no separate "SMP path" to diverge from,
//    which is the mistake docs/SUBSTRATE.md pillar 3 records as the cause of the
//    `SYS_YIELD` FP defect.
//
// # Preconditions
//
// A frame operation must not be reached from a context that already holds the pool
// lock, and must not be reached from an interrupt that could land on a core inside a
// batch. Both already held before this existed (the lock is not recursive, so the
// first would self-deadlock), and every allocation path in the tree is synchronous
// kernel context - a syscall or a page fault. The lock-held diagnostics
// ([`stats`], [`used_matches_bitmap`], [`node_free`]) and the boot-time paths
// ([`init_numa`], [`alloc_contig`]) deliberately stay on the plain lock: they either
// allocate nothing or run once, so a batch would only add machinery.

/// Slot state: nothing published.
const FC_IDLE: u64 = 0;
/// Slot state: the combiner has executed this request; the result is readable.
const FC_DONE: u64 = 1;
/// Slot state: the combiner has claimed this request and is executing it. The
/// publisher must **not** withdraw - that is what stops one request running twice.
const FC_BUSY: u64 = 2;

/// The lowest opcode. Anything below is a state, not a request.
const FC_OP_MIN: u64 = 3;
const OP_ALLOC: u64 = 3;
/// `arg` = NUMA node id.
const OP_ALLOC_ON: u64 = 4;
/// `arg` = physical address.
const OP_FREE: u64 = 5;
/// `arg` = physical address.
const OP_SHARE: u64 = 6;
/// `arg` = physical address.
const OP_REFS: u64 = 7;
/// `arg` = physical address.
const OP_COW: u64 = 8;

/// [`OP_COW`] result: one holder, so there is nothing to copy.
const COW_SOLE: u64 = 0;
/// [`OP_COW`] result: shared, but the pool is exhausted.
const COW_NO_FRAME: u64 = 1;

/// One publication slot per CPU.
///
/// `align(64)` so two cores never share a line: the whole point is that a waiter
/// spins on a line nobody else writes, and two slots in one line would put the
/// handoff back.
#[repr(align(64))]
struct FcSlot {
    /// The state machine above, or an opcode while a request is outstanding.
    req: AtomicU64,
    arg: AtomicU64,
    res: AtomicU64,
}

impl FcSlot {
    const fn new() -> FcSlot {
        FcSlot {
            req: AtomicU64::new(FC_IDLE),
            arg: AtomicU64::new(0),
            res: AtomicU64::new(0),
        }
    }
}

static FC_SLOTS: [FcSlot; crate::smp::MAX_CPUS] = [const { FcSlot::new() }; crate::smp::MAX_CPUS];

/// Which CPUs have a request outstanding, one bit each - so a combiner with nothing
/// to do reads **one** word instead of scanning every slot's cache line. `MAX_CPUS`
/// is 64, asserted below, which is what makes a `u64` the whole registry.
static FC_PENDING: AtomicU64 = AtomicU64::new(0);

const _: () = assert!(
    crate::smp::MAX_CPUS <= 64,
    "FC_PENDING is a u64 bitmask over CPU indices"
);

/// Whether some core currently holds the combiner role.
static FC_COMBINER: AtomicBool = AtomicBool::new(false);

/// Requests a combiner executed **on behalf of another core** - the combining itself.
///
/// The witness, because a batch of size one is indistinguishable from the plain lock:
/// without this, "flat combining landed" would be a claim about the source rather than
/// about the machine (docs/ENGINEERING.md 1). It costs the uncontended path **nothing**
/// - it is bumped inside [`fc_drain`]'s loop body, which does not execute when the
/// pending mask is empty, and that is the whole of the single-core case.
static FC_COMBINED: AtomicU64 = AtomicU64::new(0);
/// Requests taken back after the spin bound because no combiner picked them up. Also
/// contended-path only.
static FC_WITHDRAWN: AtomicU64 = AtomicU64::new(0);

/// (requests served for another core, requests withdrawn) - see [`FC_COMBINED`].
pub fn fc_stats() -> (u64, u64) {
    (
        FC_COMBINED.load(Ordering::Relaxed),
        FC_WITHDRAWN.load(Ordering::Relaxed),
    )
}

/// Spin iterations a publisher waits for the combiner before taking its request back
/// and trying to combine itself.
///
/// A liveness backstop, not a tuning knob: the window it covers is a publication that
/// lands just after the combiner sampled the pending mask, which no protocol without
/// a second handshake can avoid. Large enough that a normal batch completes well
/// inside it, so the retry is rare rather than a second phase of the common path.
const FC_SPINS: u32 = 1024;

/// Submit one bookkeeping operation and return its result.
///
/// The single entry point for every operation the hot paths use. See the module note
/// above for the protocol; the invariant that matters for correctness is that a
/// request is executed **exactly once**, enforced by the [`FC_BUSY`] claim: a
/// publisher may only withdraw by moving its slot from its own opcode straight to
/// [`FC_IDLE`], which fails once a combiner has claimed it.
fn fc_op(op: u64, arg: u64) -> u64 {
    loop {
        // The election is also the fast path: whoever takes the role serves itself
        // with no publication and no spin.
        if !FC_COMBINER.swap(true, Ordering::Acquire) {
            let res = {
                let _g = POOL_LOCK.lock();
                // SAFETY: the pool lock is held for the whole batch, and this core is
                // the only combiner.
                unsafe {
                    let mine = fc_execute(op, arg);
                    fc_drain();
                    mine
                }
            };
            FC_COMBINER.store(false, Ordering::Release);
            return res;
        }

        // Someone else is combining. Publish and let them do the work; `arg` before
        // `req` so the release store on `req` publishes both.
        let cpu = crate::smp::cpu_index();
        let slot = &FC_SLOTS[cpu];
        slot.arg.store(arg, Ordering::Relaxed);
        slot.req.store(op, Ordering::Release);
        FC_PENDING.fetch_or(1u64 << cpu, Ordering::Release);

        let mut spins = 0u32;
        loop {
            match slot.req.load(Ordering::Acquire) {
                FC_DONE => {
                    let res = slot.res.load(Ordering::Relaxed);
                    slot.req.store(FC_IDLE, Ordering::Relaxed);
                    return res;
                }
                // Claimed: withdrawing now would run the request twice.
                FC_BUSY => core::hint::spin_loop(),
                // Still ours.
                _ => {
                    spins += 1;
                    if spins > FC_SPINS
                        && slot
                            .req
                            .compare_exchange(op, FC_IDLE, Ordering::Acquire, Ordering::Relaxed)
                            .is_ok()
                    {
                        // Took it back - go and combine ourselves.
                        FC_PENDING.fetch_and(!(1u64 << cpu), Ordering::Relaxed);
                        FC_WITHDRAWN.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                    if spins > FC_SPINS {
                        // The CAS lost: a combiner claimed it between the load above
                        // and here, so wait for it after all.
                        spins = 0;
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Execute one request. The whole of what a batch does per slot.
///
/// # Safety
/// The pool lock must be held. Every `arg` carrying an address reaches here only from
/// a public wrapper that has already checked [`in_pool`].
unsafe fn fc_execute(op: u64, arg: u64) -> u64 {
    // SAFETY: the pool lock is held; delegated to each primitive's contract.
    unsafe {
        match op {
            OP_ALLOC => claim_hinted().unwrap_or(0) as u64,
            OP_ALLOC_ON => claim_on(arg as u8).unwrap_or(0) as u64,
            OP_FREE => {
                release(arg as usize);
                0
            }
            OP_SHARE => u64::from(add_ref(arg as usize)),
            OP_REFS => u64::from(ref_count(arg as usize)),
            // The opcodes are private constants and `fc_op` is the only producer, so
            // there is no other value to reach here.
            _ => cow_claim(arg as usize),
        }
    }
}

/// Serve every published request. Called by the combiner with the lock held.
///
/// # Safety
/// The pool lock must be held, and the caller must hold the combiner role.
unsafe fn fc_drain() {
    let mut mask = FC_PENDING.swap(0, Ordering::Acquire);
    while mask != 0 {
        let cpu = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        let slot = &FC_SLOTS[cpu];
        let op = slot.req.load(Ordering::Acquire);
        if op < FC_OP_MIN {
            continue; // withdrawn before we got here
        }
        if slot
            .req
            .compare_exchange(op, FC_BUSY, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            continue; // withdrawn between the load and the claim
        }
        let arg = slot.arg.load(Ordering::Relaxed);
        // SAFETY: the pool lock is held for the whole batch.
        let res = unsafe { fc_execute(op, arg) };
        slot.res.store(res, Ordering::Relaxed);
        slot.req.store(FC_DONE, Ordering::Release);
        FC_COMBINED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Allocate one zeroed frame **on `node`** if that node has a free one, else anywhere.
///
/// A preference, not a guarantee, and the difference is counted
/// ([`numa_fallbacks`]) rather than hidden: refusing instead would turn a bandwidth
/// question into an out-of-memory one, and ARCHITECTURE.md 5 has no OOM killer to
/// appeal to. [`alloc`] is left untouched, so every pre-NUMA caller is unchanged.
///
/// [`NODE_ANY`], and any node the pool holds no frames on, go straight to [`alloc`]
/// and are **not** counted as fallbacks: nothing was asked for, so nothing was
/// missed.
pub fn alloc_on(node: u8) -> Option<usize> {
    if node == NODE_ANY {
        return alloc();
    }
    let pa = fc_op(OP_ALLOC_ON, u64::from(node)) as usize;
    if pa == 0 {
        return None;
    }
    zero_frames(pa, 1);
    Some(pa)
}

/// Frames held back from **cell-driven** allocation (docs/ENGINEERING.md 12):
/// `SYS_MMAP`/`SYS_COMMIT` and the Linux `mmap`/`mprotect` path refuse once the
/// free pool would drop below this, so a cell can never take the last frame out
/// from under the kernel's own allocations - the page tables its mappings need,
/// a driver ring, a `fork`'s copy. 4096 frames = 16 MiB.
///
/// It scales with the per-cell budget because page-table frames are **not**
/// charged to a cell: a 384 MiB mapping needs ~192 leaf tables plus their
/// parents, and up to `MAX_CELLS` cells can hold mappings at once, so the
/// reserve stays several times the worst-case page-table cost.
pub const USER_RESERVE_FRAMES: usize = 4096;

/// How many frames a **cell** may still be given: the free pool less the
/// kernel's reserve. The number a user-driven allocation must check against
/// before it allocates anything. O(1) - it is consulted per allocating syscall,
/// and per *page* on the Linux file-mmap path.
pub fn user_available() -> usize {
    stats().0.saturating_sub(USER_RESERVE_FRAMES)
}

/// Allocate one zeroed 4 KiB frame, or `None` when the pool is exhausted.
///
/// Exhaustion is a **return value, not a panic**: the pool is a fixed size
/// ([`POOL_FRAMES`]) and `len` on a cell's `SYS_MMAP` is cell-supplied, so "the pool ran out" is
/// a condition an unprivileged cell can drive at will and must therefore be
/// refusable (ARCHITECTURE.md 5 forbids an OOM killer; an OOM *panic* is
/// strictly worse). Kernel-internal callers that allocate a bounded, known
/// amount while the user reserve above is held may `expect` it, and each says
/// why at its call site.
///
/// The bookkeeping goes through the flat-combining batch; the 4 KiB zeroing is done
/// here, by the requesting core, **after** the claim and outside the lock.
pub fn alloc() -> Option<usize> {
    let pa = fc_op(OP_ALLOC, 0) as usize;
    if pa == 0 {
        return None;
    }
    zero_frames(pa, 1);
    Some(pa)
}

/// Allocate `n` **physically contiguous** zeroed frames, returning the first frame's
/// PA, or `None`.
///
/// The pool deliberately had no contiguous path - `Funded` exists to stitch single
/// frames precisely so nothing needs one - and two real callers have now asked
/// anyway, which is the admission test for adding it. The GICv3 ITS work needed a
/// contiguous 8 KiB config table and fell back to statics, recording "the frame
/// allocator offers neither contiguity nor that alignment" (docs/SMP.md); and the
/// observability event ring paid a page-split - a shift, a mask, and a dependent
/// directory load - **per recorded event** to work around frames that were not
/// adjacent (docs/OBSERVABILITY.md 11.4). A boot-time contiguous allocation deletes
/// a hot-path cost, which is the right trade in that direction.
///
/// First-fit, and the failure mode is honest: external fragmentation can refuse an
/// `n` that free-frame *count* suggests should fit, so a caller treats `None` as
/// "degrade and count it", never as a panic. In practice the callers ask for tens of
/// frames at bring-up from a pool of 131,072, where a refusal means something is
/// deeply wrong anyway. Does not move [`alloc`]'s rotating hint, for `alloc_in`'s
/// stated reason.
///
/// Freeing is per frame through the ordinary [`free`], `n` times - the run is not an
/// object, just frames that happen to be adjacent, so no bookkeeping beyond each
/// frame's own exists to get out of step.
/// Stays on the plain lock rather than the flat-combining batch: it runs at bring-up,
/// so batching would add machinery to a path with nothing to batch against.
pub fn alloc_contig(n: usize) -> Option<usize> {
    if n == 0 || n > POOL_FRAMES {
        return None;
    }
    let claimed = {
        let _g = POOL_LOCK.lock();
        // SAFETY: the pool lock is held, so no other core is mid-update.
        unsafe {
            assert!(
                *core::ptr::addr_of!(INITIALIZED),
                "frame allocator used before init"
            );
            let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
            let mut run = 0usize;
            let mut found = None;
            for frame in 0..POOL_FRAMES {
                let (word, bit) = (frame / 64, frame % 64);
                if bitmap[word] & (1 << bit) != 0 {
                    run = 0;
                    continue;
                }
                run += 1;
                if run < n {
                    continue;
                }
                let first = frame + 1 - n;
                for f in first..=frame {
                    let (w, b) = (f / 64, f % 64);
                    bitmap[w] |= 1 << b;
                    (*core::ptr::addr_of_mut!(REFS))[f] = 1;
                }
                *core::ptr::addr_of_mut!(USED) += n;
                found = Some(arch::FRAME_POOL_BASE + first * FRAME_SIZE);
                break;
            }
            found
        }
    };
    let pa = claimed?;
    // Zeroed exactly as `alloc` zeroes: callers' "a fresh frame is zero" contract must
    // not depend on which allocation path produced it.
    zero_frames(pa, n);
    Some(pa)
}

/// (free frames, total frames) - the shell's `meminfo` builtin, the reservation
/// memory floor, and the cell-allocation guard above.
pub fn stats() -> (usize, usize) {
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held, so no other core is mid-update.
    let used = unsafe { *core::ptr::addr_of!(USED) };
    (POOL_FRAMES - used, POOL_FRAMES)
}

/// Whether the incremental [`stats`] counter still agrees with the bitmap it
/// summarises - the cheap invariant that keeps the O(1) count honest. Asserted by
/// the `security` test kernel after every allocation and free it drives
/// (docs/ENGINEERING.md 1: observe, do not infer).
pub fn used_matches_bitmap() -> bool {
    let _g = POOL_LOCK.lock();
    // SAFETY: the pool lock is held, so the bitmap and the counter are consistent
    // with each other - without it this check could read a half-completed alloc and
    // report a drift that does not exist.
    unsafe {
        let bitmap = &*core::ptr::addr_of!(BITMAP);
        let counted: usize = bitmap.iter().map(|w| w.count_ones() as usize).sum();
        counted == *core::ptr::addr_of!(USED)
    }
}

/// Whether `pa` is a frame this allocator owns (inside the pool, page-aligned).
/// Not every physical page a cell's page tables reference comes from here: the
/// shared `.user` window is part of the kernel image, and an MMIO window is a
/// device aperture.
pub fn in_pool(pa: usize) -> bool {
    pa >= arch::FRAME_POOL_BASE && {
        let offset = pa - arch::FRAME_POOL_BASE;
        offset.is_multiple_of(FRAME_SIZE) && offset / FRAME_SIZE < POOL_FRAMES
    }
}

/// Return a frame to the pool **if it is one of ours**, reporting whether it
/// was. The teardown path for a user mapping uses this: a cell's page tables
/// can legitimately reference a non-pool page (the `.user` window, an MMIO
/// aperture), and handing that to [`free`] would trip its range assertion -
/// i.e. panic the kernel from an unprivileged `munmap`.
pub fn free_if_pool(pa: usize) -> bool {
    if !in_pool(pa) {
        return false;
    }
    free(pa);
    true
}

/// Take an extra reference to an already-allocated frame, so it survives until
/// every holder has [`free`]d it. Returns false - and takes no reference - if `pa`
/// is not a live pool frame or the count is at [`SHARE_MAX`]; the caller must then
/// copy the page instead of sharing it.
///
/// This is the COW `fork` primitive: the parent's pages are mapped read-only into
/// the child and shared rather than copied (docs/ARCHITECTURE-DEBT.md 4.0).
pub fn share(pa: usize) -> bool {
    if !in_pool(pa) {
        return false;
    }
    fc_op(OP_SHARE, pa as u64) != 0
}

/// What a **copy-on-write** write fault should do about the frame at `pa`.
pub enum Cow {
    /// One holder: nothing to copy, just make the existing frame writable. Also the
    /// answer for an address this allocator does not own.
    Sole,
    /// Shared. `.0` is a fresh frame for this mapping's private copy.
    ///
    /// **It is not zeroed.** The caller must overwrite all [`FRAME_SIZE`] bytes,
    /// which is what a COW break does by definition - zeroing a page that is about
    /// to be fully copied over is 4 KiB of stores nothing reads. The
    /// "never leak previous contents" rule is met by the copy, not by a `memset`.
    ///
    /// The old frame's reference is **not** dropped: the caller frees it after the
    /// copy. Folding the free in here would make it three lock acquisitions into one
    /// instead of two, and it would be wrong - a peer core faulting on the same page
    /// would then see a refcount of one, decide it was the sole owner, and start
    /// writing through the very frame this copy is reading.
    Private(usize),
    /// Shared, and the pool is exhausted. Nothing has changed, so the caller can
    /// refuse the fault and leave the mapping as it was.
    NoFrame,
}

/// Decide a copy-on-write break for the frame at `pa` in **one** acquisition.
///
/// "Is this frame shared" and "give me a frame to copy it into" are one decision, and
/// asking them separately means the answer to the first can be stale by the time the
/// second is served. The 4 KiB copy - and the release of the old reference after it -
/// happen at the caller, outside the lock.
pub fn cow_resolve(pa: usize) -> Cow {
    if !in_pool(pa) {
        return Cow::Sole;
    }
    match fc_op(OP_COW, pa as u64) {
        COW_SOLE => Cow::Sole,
        COW_NO_FRAME => Cow::NoFrame,
        dst => Cow::Private(dst as usize),
    }
}

/// How many mappings hold `pa`, or 0 if it is free or not ours. The witness a test
/// asserts against, and what a COW fault consults to decide between "copy this
/// page" and "it is mine alone, just make it writable".
pub fn refs(pa: usize) -> u8 {
    if !in_pool(pa) {
        return 0;
    }
    fc_op(OP_REFS, pa as u64) as u8
}

/// Drop one reference to a frame, releasing it to the pool at zero. `pa` must be a
/// pool frame (see [`free_if_pool`] for the path that cannot promise that).
///
/// A decrement rather than an unconditional release, so that every caller written
/// before COW existed still reads correctly: with no sharing the count is 1 and this
/// releases, exactly as before.
pub fn free(pa: usize) {
    assert!(in_pool(pa), "frames::free of a non-pool address {pa:#x}");
    fc_op(OP_FREE, pa as u64);
}
