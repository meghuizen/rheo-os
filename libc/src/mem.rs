//! The heap and C `malloc` family (docs/USERLAND.md M3). The heap is backed
//! by `runtime::Heap` (a free-list `GlobalAlloc`, host-fuzzed) over a region
//! obtained from `SYS_MMAP` at startup - so Rust `alloc` collections and C
//! `malloc` draw from the same store. `malloc`/`realloc`/`calloc` stash the
//! block's total size in a header before the returned pointer so `free` can
//! reconstruct the `Layout`.

use core::alloc::Layout;

/// Initial heap size (docs/USERLAND.md); grown lazily is future work.
const HEAP_SIZE: usize = 8 * 1024 * 1024;
/// Header (holds the total allocation size) kept before the user pointer;
/// 16 bytes keeps the returned pointer 16-aligned.
const HDR: usize = 16;

/// Map the heap region and hand it to the global allocator. Called once from
/// `_start` before `main`, before any allocation happens.
///
/// # Safety
/// Must be called exactly once, before the first allocation.
pub(crate) unsafe fn init_heap() {
    let base = crate::sys::mmap(HEAP_SIZE);
    unsafe {
        crate::HEAP.init(base, HEAP_SIZE);
    }
}

/// # Safety
/// C `malloc`: the returned pointer must be released with `free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let Ok(layout) = Layout::from_size_align(size + HDR, 16) else {
        return core::ptr::null_mut();
    };
    // SAFETY: layout has non-zero size.
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        return p;
    }
    unsafe {
        (p as *mut usize).write(size + HDR);
        p.add(HDR)
    }
}

/// # Safety
/// `ptr` must be null or a pointer returned by `malloc`/`calloc`/`realloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let base = ptr.sub(HDR);
        let total = (base as *mut usize).read();
        let layout = Layout::from_size_align_unchecked(total, 16);
        alloc::alloc::dealloc(base, layout);
    }
}

/// # Safety
/// C `calloc`: zeroed allocation of `n * size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(n: usize, size: usize) -> *mut u8 {
    let Some(total) = n.checked_mul(size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: malloc contract.
    let p = unsafe { malloc(total) };
    if !p.is_null() {
        // SAFETY: p points at `total` writable bytes.
        unsafe { core::ptr::write_bytes(p, 0, total) };
    }
    p
}

/// # Safety
/// C `realloc`: `ptr` null or from this allocator; grows/shrinks the block.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
    if ptr.is_null() {
        return unsafe { malloc(new_size) };
    }
    if new_size == 0 {
        unsafe { free(ptr) };
        return core::ptr::null_mut();
    }
    unsafe {
        let old_total = (ptr.sub(HDR) as *mut usize).read();
        let old_size = old_total - HDR;
        let new = malloc(new_size);
        if new.is_null() {
            return new;
        }
        core::ptr::copy_nonoverlapping(ptr, new, old_size.min(new_size));
        free(ptr);
        new
    }
}
