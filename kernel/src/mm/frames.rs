//! Physical frame allocator: a bitmap over a fixed per-ISA pool of RAM
//! placed above the kernel image (the pool bounds live in the arch layer;
//! init() checks they clear the image). Frames are 4 KiB and are zeroed
//! on allocation - page tables and user memory must never leak previous
//! contents.

use crate::arch;

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
pub fn alloc() -> Option<usize> {
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
                *core::ptr::addr_of_mut!(USED) += 1;
                *core::ptr::addr_of_mut!(NEXT_HINT) = frame + 1;
                let pa = arch::FRAME_POOL_BASE + frame * FRAME_SIZE;
                // Zero through the kernel's linear map (identity on x86/riscv;
                // the high map on aarch64), never the raw physical address.
                core::ptr::write_bytes(arch::phys_to_virt(pa) as *mut u8, 0, FRAME_SIZE);
                return Some(pa);
            }
        }
        None
    }
}

/// (free frames, total frames) - the shell's `meminfo` builtin, the reservation
/// memory floor, and the cell-allocation guard above.
pub fn stats() -> (usize, usize) {
    // SAFETY: single CPU, synchronous traps.
    let used = unsafe { *core::ptr::addr_of!(USED) };
    (POOL_FRAMES - used, POOL_FRAMES)
}

/// Whether the incremental [`stats`] counter still agrees with the bitmap it
/// summarises - the cheap invariant that keeps the O(1) count honest. Asserted by
/// the `security` test kernel after every allocation and free it drives
/// (docs/ENGINEERING.md 1: observe, do not infer).
pub fn used_matches_bitmap() -> bool {
    // SAFETY: single CPU, synchronous traps.
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

/// Return a frame to the pool. `pa` must be a pool frame (see [`free_if_pool`]
/// for the path that cannot promise that).
pub fn free(pa: usize) {
    assert!(in_pool(pa), "frames::free of a non-pool address {pa:#x}");
    let offset = pa - arch::FRAME_POOL_BASE;
    let frame = offset / FRAME_SIZE;
    unsafe {
        let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
        assert!(bitmap[frame / 64] & (1 << (frame % 64)) != 0, "double free");
        bitmap[frame / 64] &= !(1 << (frame % 64));
        *core::ptr::addr_of_mut!(USED) -= 1;
    }
}
