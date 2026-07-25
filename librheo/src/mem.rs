//! Allocation (docs/LIBRHEO.md). The heap is `runtime::Heap` (the host-fuzzed
//! free-list allocator the strand runtime already uses) wrapped so it **grows
//! on demand**: when an allocation cannot be satisfied it maps another arena
//! from the kernel (`SYS_MMAP`) and adds it to the free list, then retries.
//! This is the grow variant the plan calls for (mirroring
//! targets/std-rheo/alloc.rs), kept as a thin wrapper so the arena-growth
//! logic (which must call a syscall) stays in librheo while the allocator
//! itself remains the OS-agnostic `runtime` crate.

use core::alloc::{GlobalAlloc, Layout};

/// Initial arena mapped at startup.
const INIT_ARENA: usize = 1024 * 1024;
/// Minimum size mapped when the heap must grow.
const GROW: usize = 2 * 1024 * 1024;

#[inline]
fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// The process heap: a `runtime::Heap` that maps more memory when it runs out.
pub struct GrowHeap {
    inner: runtime::Heap,
}

impl GrowHeap {
    pub const fn new() -> GrowHeap {
        GrowHeap {
            inner: runtime::Heap::empty(),
        }
    }

    /// Map the initial arena and hand it to the allocator. Called once from
    /// `_start` before any allocation.
    ///
    /// # Safety
    /// Must be called exactly once, before the first allocation.
    pub unsafe fn init(&self) {
        let base = crate::sys::mmap(INIT_ARENA);
        // A cell always gets its anon mmap; if it ever failed there is no heap
        // to fall back to, so a null base simply leaves the allocator empty and
        // the first allocation returns null (the caller's problem).
        if base != 0 {
            unsafe { self.inner.init(base, INIT_ARENA) };
        }
    }
}

impl Default for GrowHeap {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl GlobalAlloc for GrowHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.inner.alloc(layout) };
        if !p.is_null() {
            return p;
        }
        // Grow: map a fresh arena large enough for the request plus slack, add
        // it to the free list, and retry once.
        let want = align_up(layout.size() + layout.align() + GROW, 4096);
        let base = crate::sys::mmap(want);
        if base == 0 {
            return core::ptr::null_mut();
        }
        // SAFETY: the mapping is fresh, uniquely owned, and outlives every
        // allocation (a cell never unmaps its heap).
        unsafe { self.inner.add_region(base, want) };
        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.inner.dealloc(ptr, layout) };
    }
}

/// The global allocator instance (installed by `lib.rs`).
#[global_allocator]
pub(crate) static HEAP: GrowHeap = GrowHeap::new();

/// Map the initial heap arena. Called once from `_start`.
///
/// # Safety
/// Exactly once, before the first allocation.
pub(crate) unsafe fn init_heap() {
    unsafe { HEAP.init() };
}
