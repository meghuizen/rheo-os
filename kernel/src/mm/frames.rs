//! Physical frame allocator: a bitmap over a fixed per-ISA pool of RAM
//! placed above the kernel image (the pool bounds live in the arch layer;
//! init() checks they clear the image). Frames are 4 KiB and are zeroed
//! on allocation - page tables and user memory must never leak previous
//! contents.

use crate::arch;

pub const FRAME_SIZE: usize = 4096;

/// Pool size: 32 MiB = 8192 frames -> 1 KiB bitmap.
pub const POOL_FRAMES: usize = 8192;

static mut BITMAP: [u64; POOL_FRAMES / 64] = [0; POOL_FRAMES / 64];
static mut NEXT_HINT: usize = 0;
static mut INITIALIZED: bool = false;

unsafe extern "C" {
    static __kernel_end: u8;
}

/// One-time setup; panics if the pool would overlap the kernel image.
pub fn init() {
    let kernel_end = core::ptr::addr_of!(__kernel_end) as usize;
    assert!(
        kernel_end <= arch::FRAME_POOL_BASE,
        "kernel image ({kernel_end:#x}) overlaps the frame pool ({:#x})",
        arch::FRAME_POOL_BASE
    );
    unsafe {
        *core::ptr::addr_of_mut!(INITIALIZED) = true;
    }
}

/// Allocate one zeroed 4 KiB frame; returns its physical address.
/// Panics on exhaustion - at this stage running out of the fixed pool is
/// a kernel bug, not a recoverable condition (reclaim is step 8).
pub fn alloc() -> usize {
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
                *core::ptr::addr_of_mut!(NEXT_HINT) = frame + 1;
                let pa = arch::FRAME_POOL_BASE + frame * FRAME_SIZE;
                core::ptr::write_bytes(pa as *mut u8, 0, FRAME_SIZE);
                return pa;
            }
        }
        panic!("frame pool exhausted ({POOL_FRAMES} frames)");
    }
}

/// (free frames, total frames) - used by the shell's `meminfo` builtin.
pub fn stats() -> (usize, usize) {
    unsafe {
        let bitmap = &*core::ptr::addr_of!(BITMAP);
        let used: usize = bitmap.iter().map(|w| w.count_ones() as usize).sum();
        (POOL_FRAMES - used, POOL_FRAMES)
    }
}

/// Return a frame to the pool.
pub fn free(pa: usize) {
    let offset = pa - arch::FRAME_POOL_BASE;
    assert!(offset.is_multiple_of(FRAME_SIZE) && offset / FRAME_SIZE < POOL_FRAMES);
    let frame = offset / FRAME_SIZE;
    unsafe {
        let bitmap = &mut *core::ptr::addr_of_mut!(BITMAP);
        assert!(bitmap[frame / 64] & (1 << (frame % 64)) != 0, "double free");
        bitmap[frame / 64] &= !(1 << (frame % 64));
    }
}
