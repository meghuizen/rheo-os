//! Persistent-memory frame allocator (docs/MEMORY.md, real-PMEM path). A bitmap
//! allocator over a **real QEMU nvdimm** physical region discovered from
//! firmware (the NFIT SPA Range on x86-64), kept entirely separate from the DDR
//! `frames` pool so a `MemKind::Pmem` grant is genuinely nvdimm-backed rather
//! than DDR-emulated. Inert until `init` runs: with no nvdimm the region stays
//! `None`, the grant path falls back to DDR, and every machine without an nvdimm
//! is byte-for-byte unchanged.
//!
//! Two things differ from the DDR pool:
//!   1. **No zeroing on allocation.** Persistent memory *retains* its contents
//!      across allocation - that is the point of it - so a pmem frame is handed
//!      back as-is (a fresh nvdimm's backing store starts zeroed anyway).
//!   2. **A dedicated mapping window.** QEMU places the nvdimm's physical span at
//!      4 GiB, above the kernel's top-2 GiB linear map on x86-64, so the kernel
//!      cannot reach it through `phys_to_virt`. `arch::pmem_map_window` installs a
//!      supervisor mapping window for the region and returns its kernel VA;
//!      `phys_to_virt` here reaches a pmem frame through that window.
//!
//! Cross-reboot persistence (the bytes surviving a power cycle) is a real-
//! hardware property of the backing DIMM; it is not headlessly assertable in a
//! single QEMU boot and is documented, not claimed here.

use crate::arch;

pub const FRAME_SIZE: usize = 4096;

/// Frames tracked at most: 32 MiB of nvdimm. The bitmap is a fixed static (the
/// kernel is allocation-free); a larger DIMM is tracked up to this cap.
const MAX_PMEM_FRAMES: usize = 8192;

static mut BITMAP: [u64; MAX_PMEM_FRAMES / 64] = [0; MAX_PMEM_FRAMES / 64];
static mut BASE_PA: usize = 0;
static mut NFRAMES: usize = 0;
static mut WINDOW_VA: usize = 0;
static mut READY: bool = false;
static mut NEXT_HINT: usize = 0;

/// Serialises the mutable pmem state - the bitmap and the search hint - which
/// one operation reads and writes together, so two cores could otherwise both
/// see a bit clear and both claim the frame.
///
/// **Unconditional, not `#[cfg(feature = "smp")]`.** Whether a structure needs a
/// lock is a property of the structure, not of which cargo features are enabled.
/// That is the call `frames::POOL_LOCK` and the NVMe driver already made, and the
/// lesson the `SYS_YIELD` FP defect taught: state whose safety depends on a build
/// configuration gets written twice and diverges. An uncontended acquire is one
/// atomic exchange, unmeasurable next to the bitmap scan.
///
/// `BASE_PA`/`NFRAMES`/`WINDOW_VA`/`READY` are written once by `init` before any
/// secondary starts, so reading them needs no lock. Every acquire below is in a
/// leaf function, so this non-reentrant lock is never taken twice on one core.
static PMEM_LOCK: crate::smp::SpinLock<()> = crate::smp::SpinLock::new(());

/// Bring up the allocator over a discovered persistent-memory region
/// `[base_pa, base_pa + len)`. Called once from `hw::detect` when firmware
/// surfaced a `MemKind::Pmem` region; a no-op (and left `!ready`) otherwise.
/// `base_pa` must be 2 MiB aligned (the nvdimm SPA base is), so the arch mapping
/// window can use 2 MiB pages.
pub fn init(base_pa: usize, len: usize) {
    if ready() || len < FRAME_SIZE {
        return;
    }
    let frames = (len / FRAME_SIZE).min(MAX_PMEM_FRAMES);
    // Map the region into the kernel so pmem frames are reachable (the span sits
    // above the linear map on x86-64). The window covers the frames we track.
    let window = arch::pmem_map_window(base_pa, frames * FRAME_SIZE);
    unsafe {
        *core::ptr::addr_of_mut!(BASE_PA) = base_pa;
        *core::ptr::addr_of_mut!(NFRAMES) = frames;
        *core::ptr::addr_of_mut!(WINDOW_VA) = window;
        *core::ptr::addr_of_mut!(NEXT_HINT) = 0;
        *core::ptr::addr_of_mut!(READY) = true;
    }
}

fn ready() -> bool {
    unsafe { *core::ptr::addr_of!(READY) }
}

/// The discovered persistent-memory region as `(base_pa, len)`, or `None` if no
/// nvdimm was surfaced (the DDR-fallback case).
pub fn region() -> Option<(usize, usize)> {
    if ready() {
        let base = unsafe { *core::ptr::addr_of!(BASE_PA) };
        let n = unsafe { *core::ptr::addr_of!(NFRAMES) };
        Some((base, n * FRAME_SIZE))
    } else {
        None
    }
}

/// Is `pa` a frame inside the discovered persistent-memory region?
pub fn contains(pa: usize) -> bool {
    if !ready() {
        return false;
    }
    let base = unsafe { *core::ptr::addr_of!(BASE_PA) };
    let n = unsafe { *core::ptr::addr_of!(NFRAMES) };
    pa >= base && pa < base + n * FRAME_SIZE
}

/// Kernel VA at which pmem physical address `pa` is reachable (through the
/// mapping window installed by `init`). Panics if `pa` is not in the region.
pub fn phys_to_virt(pa: usize) -> usize {
    assert!(contains(pa), "pmem phys_to_virt outside region: {pa:#x}");
    let base = unsafe { *core::ptr::addr_of!(BASE_PA) };
    let window = unsafe { *core::ptr::addr_of!(WINDOW_VA) };
    window + (pa - base)
}

/// Allocate one 4 KiB persistent frame; returns its physical address, or `None`
/// if the region is absent or full. **Not** zeroed (persistence semantics).
pub fn alloc() -> Option<usize> {
    if !ready() {
        return None;
    }
    let _g = PMEM_LOCK.lock();
    let base = unsafe { *core::ptr::addr_of!(BASE_PA) };
    let n = unsafe { *core::ptr::addr_of!(NFRAMES) };
    let hint = unsafe { *core::ptr::addr_of!(NEXT_HINT) };
    let bitmap = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP) };
    for offset in 0..n {
        let frame = (hint + offset) % n;
        let (word, bit) = (frame / 64, frame % 64);
        if bitmap[word] & (1 << bit) == 0 {
            bitmap[word] |= 1 << bit;
            unsafe {
                *core::ptr::addr_of_mut!(NEXT_HINT) = frame + 1;
            }
            return Some(base + frame * FRAME_SIZE);
        }
    }
    None
}

/// Return a persistent frame to the region.
pub fn free(pa: usize) {
    if !contains(pa) {
        return;
    }
    let _g = PMEM_LOCK.lock();
    let base = unsafe { *core::ptr::addr_of!(BASE_PA) };
    let frame = (pa - base) / FRAME_SIZE;
    let bitmap = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP) };
    // A live pmem frame has its bit set, so a clear bit here is a double free -
    // which is exactly what an unserialised alloc handing one frame to two cores
    // produces. Asserting it (as the DDR pool does) is what lets a contention
    // proof catch a broken lock instead of silently tolerating the damage.
    assert!(
        bitmap[frame / 64] & (1 << (frame % 64)) != 0,
        "pmem double free at {pa:#x}"
    );
    bitmap[frame / 64] &= !(1 << (frame % 64));
}

/// `(free frames, total frames)` in the region; `(0, 0)` if absent.
pub fn stats() -> (usize, usize) {
    if !ready() {
        return (0, 0);
    }
    let _g = PMEM_LOCK.lock();
    let n = unsafe { *core::ptr::addr_of!(NFRAMES) };
    let bitmap = unsafe { &*core::ptr::addr_of!(BITMAP) };
    let used: usize = bitmap.iter().map(|w| w.count_ones() as usize).sum();
    (n - used, n)
}

/// Scan the machine inventory for a persistent-memory region and, if one is
/// present, bring the allocator up over the first such region. Called once from
/// `hw::detect` after discovery; inert (leaves the allocator `!ready`) on every
/// machine without an nvdimm, so the DDR path is unchanged.
pub fn init_from_inventory(inv: &crate::hw::Inventory) {
    for r in &inv.mem[..inv.nmem] {
        if r.kind == crate::hw::MemKind::Pmem && r.len as usize >= FRAME_SIZE {
            init(r.base as usize, r.len as usize);
            return;
        }
    }
}
