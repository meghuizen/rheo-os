// A small free-list ("hole list") heap allocator - the OS has no allocator
// of its own (the kernel is allocation-free), so the strand runtime brings
// one for `alloc`: Box/Vec/String/BTreeMap and the executor's task table.
//
// Design (the classic address-sorted hole list): free regions are an
// intrusive singly linked list, sorted by address, each carrying its own
// {size, next} header. Allocation is first-fit with alignment; deallocation
// re-inserts the region and coalesces with adjacent holes so the heap does
// not fragment into unusable dust. Single-CPU cooperative use (the whole
// kernel is single-CPU today), so there is no internal lock.
//
// Dependency-free and includable on the host for fuzzing (comparison-style),
// so `//` comments, not `//!`.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// A hole header is two words: {size, next-addr}. Every free region is at
/// least this big and aligned to it, so any freed block can hold a header.
const HOLE_SIZE: usize = 2 * core::mem::size_of::<usize>();
const HOLE_ALIGN: usize = core::mem::align_of::<usize>();

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/// The allocation size/align actually reserved for a layout: at least a hole
/// (so the freed block can relist) and aligned to the header. Used by both
/// alloc and dealloc so they agree on region size.
#[inline]
fn size_align(layout: Layout) -> (usize, usize) {
    let align = layout.align().max(HOLE_ALIGN);
    let size = align_up(layout.size().max(HOLE_SIZE), HOLE_ALIGN);
    (size, align)
}

#[inline]
unsafe fn read_size(addr: usize) -> usize {
    unsafe { ptr::read(addr as *const usize) }
}
#[inline]
unsafe fn read_next(addr: usize) -> usize {
    unsafe { ptr::read((addr + core::mem::size_of::<usize>()) as *const usize) }
}
#[inline]
unsafe fn write_size(addr: usize, v: usize) {
    unsafe { ptr::write(addr as *mut usize, v) };
}
#[inline]
unsafe fn write_next(addr: usize, v: usize) {
    unsafe { ptr::write((addr + core::mem::size_of::<usize>()) as *mut usize, v) };
}

/// The hole list. `head_next` is a dummy head: its "next" is the address of
/// the first real hole (0 = none). Holes are kept sorted by address.
pub struct HoleList {
    head_next: usize,
}

impl HoleList {
    pub const fn empty() -> HoleList {
        HoleList { head_next: 0 }
    }

    /// # Safety
    /// `[base, base+size)` must be valid, writable, uniquely owned memory
    /// that outlives every allocation. `size` must be at least `HOLE_SIZE`.
    pub unsafe fn init(&mut self, base: usize, size: usize) {
        let base = align_up(base, HOLE_ALIGN);
        let size = size & !(HOLE_ALIGN - 1);
        unsafe {
            write_size(base, size);
            write_next(base, 0);
        }
        self.head_next = base;
    }

    /// First-fit allocation. Returns 0 if there is no fitting hole.
    pub fn allocate(&mut self, layout: Layout) -> usize {
        let (size, align) = size_align(layout);
        // `prev_next` is the address of the `next` field pointing at `cur`.
        let mut prev_next: *mut usize = &mut self.head_next;
        let mut cur = self.head_next;
        while cur != 0 {
            let hsize = unsafe { read_size(cur) };
            let next = unsafe { read_next(cur) };
            let hole_end = cur + hsize;

            let alloc_start = if cur.is_multiple_of(align) {
                cur
            } else {
                // Leave room for a front hole header before the aligned start.
                align_up(cur + HOLE_SIZE, align)
            };
            let alloc_end = alloc_start.wrapping_add(size);

            let fits = alloc_end >= alloc_start && alloc_end <= hole_end;
            if fits {
                let front = alloc_start - cur; // 0 or >= HOLE_SIZE by construction
                let back = hole_end - alloc_end; // 0 or must be >= HOLE_SIZE
                if back != 0 && back < HOLE_SIZE {
                    // Can't leave a valid trailing hole; skip this one.
                    prev_next = (cur + core::mem::size_of::<usize>()) as *mut usize;
                    cur = next;
                    continue;
                }
                // Unlink `cur`.
                unsafe { ptr::write(prev_next, next) };
                // Relist the front and back padding as holes.
                if front != 0 {
                    self.insert(cur, front);
                }
                if back != 0 {
                    self.insert(alloc_end, back);
                }
                return alloc_start;
            }
            prev_next = (cur + core::mem::size_of::<usize>()) as *mut usize;
            cur = next;
        }
        0
    }

    /// Free a region previously returned by `allocate` with the same layout.
    pub fn deallocate(&mut self, addr: usize, layout: Layout) {
        let (size, _align) = size_align(layout);
        self.insert(addr, size);
    }

    /// Add another owned region to the free list - arena growth for a heap
    /// that maps more backing memory on demand (librheo grows over SYS_MMAP,
    /// docs/LIBRHEO.md). Regions smaller than a hole header are ignored.
    ///
    /// # Safety
    /// `[base, base+size)` must be valid, writable, uniquely owned memory that
    /// outlives every allocation, and disjoint from every region already held.
    pub unsafe fn add_region(&mut self, base: usize, size: usize) {
        let base = align_up(base, HOLE_ALIGN);
        let size = size & !(HOLE_ALIGN - 1);
        if size >= HOLE_SIZE {
            self.insert(base, size);
        }
    }

    /// Insert a free region, keeping the list address-sorted, and coalesce
    /// with the immediately adjacent neighbours.
    fn insert(&mut self, addr: usize, size: usize) {
        // Find the hole after `addr`, tracking the one before it.
        let mut prev = 0usize; // 0 => dummy head
        let mut cur = self.head_next;
        while cur != 0 && cur < addr {
            prev = cur;
            cur = unsafe { read_next(cur) };
        }
        // Link the new hole in between prev and cur.
        unsafe {
            write_size(addr, size);
            write_next(addr, cur);
            if prev == 0 {
                self.head_next = addr;
            } else {
                write_next(prev, addr);
            }
        }
        // Coalesce forward: addr + size == cur.
        if cur != 0 && addr + size == cur {
            let cur_size = unsafe { read_size(cur) };
            let cur_next = unsafe { read_next(cur) };
            unsafe {
                write_size(addr, size + cur_size);
                write_next(addr, cur_next);
            }
        }
        // Coalesce backward: prev + prev_size == addr.
        if prev != 0 {
            let prev_size = unsafe { read_size(prev) };
            if prev + prev_size == addr {
                let addr_size = unsafe { read_size(addr) };
                let addr_next = unsafe { read_next(addr) };
                unsafe {
                    write_size(prev, prev_size + addr_size);
                    write_next(prev, addr_next);
                }
            }
        }
    }
}

/// A `GlobalAlloc` wrapper over the hole list, **behind a lock**.
///
/// The lock is not optional and not feature-gated. A free list is one data structure
/// whose every operation reads and writes several of its links, so two cores allocating
/// at once hand out overlapping blocks - and the symptom is not a fault, it is two
/// owners of one buffer writing over each other. Whether a structure needs a lock is a
/// property of the structure, not of which cargo features are enabled: the same call
/// `mm::frames` and the NVMe driver already made (docs/SMP.md 10).
///
/// This replaced a bare `unsafe impl Sync for Heap` whose stated justification was
/// "single-CPU kernel; no concurrent access to the allocator" - true when it was
/// written and false from the moment two cores ran cells, which is exactly the kind of
/// claim that has to be re-checked rather than inherited (docs/ENGINEERING.md 1).
///
/// Cost: an uncontended acquire is two atomics, against a free-list walk that is
/// already several dependent loads. `Sync` now comes from the lock rather than from an
/// assertion about the machine.
pub struct Heap {
    list: crate::lock::TicketLock<HoleList>,
}

impl Heap {
    pub const fn empty() -> Heap {
        Heap {
            list: crate::lock::TicketLock::new(HoleList::empty()),
        }
    }

    /// # Safety
    /// See `HoleList::init`. Call once before any allocation.
    pub unsafe fn init(&self, base: usize, size: usize) {
        unsafe { self.list.lock().init(base, size) };
    }

    /// Add another backing region to the heap (arena growth). See
    /// [`HoleList::add_region`].
    ///
    /// # Safety
    /// As [`HoleList::add_region`].
    pub unsafe fn add_region(&self, base: usize, size: usize) {
        unsafe { self.list.lock().add_region(base, size) };
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.list.lock().allocate(layout) as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.list.lock().deallocate(ptr as usize, layout);
    }
}
