//! rheo-os std allocator (installed into std as `sys/alloc/rheo.rs` by
//! targets/patch-std.py; docs/USERLAND.md M4). A self-contained free-list
//! ("hole list") heap over arenas obtained from the kernel's `SYS_MMAP`
//! syscall, grown on demand. This is the same address-sorted, coalescing
//! design that `runtime::Heap` uses (host-fuzzed there); std cannot depend on
//! that crate, so the logic is duplicated here. Single-CPU (SMP deferred), so
//! no lock is needed.
#![allow(dead_code)]

use crate::alloc::Layout;

const SYS_MMAP: u64 = 21;

/// Map `len` bytes of fresh zeroed RW pages; returns the base VA (0 on fail).
unsafe fn sys_mmap(len: usize) -> usize {
    let ret: usize;
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("ecall", in("a7") SYS_MMAP, inlateout("a0") len => ret, options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("svc #0", in("x8") SYS_MMAP, inlateout("x0") len => ret, options(nostack));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") SYS_MMAP => ret, in("rdi") len,
            out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

const WORD: usize = core::mem::size_of::<usize>();
const HOLE_SIZE: usize = 2 * WORD;
const HOLE_ALIGN: usize = core::mem::align_of::<usize>();
/// How much to map when the heap needs to grow (rounded up to cover a request).
const GROW: usize = 2 * 1024 * 1024;

#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[inline]
fn size_align(layout: Layout) -> (usize, usize) {
    let align = layout.align().max(HOLE_ALIGN);
    let size = align_up(layout.size().max(HOLE_SIZE), HOLE_ALIGN);
    (size, align)
}

unsafe fn read_size(a: usize) -> usize {
    unsafe { core::ptr::read(a as *const usize) }
}
unsafe fn read_next(a: usize) -> usize {
    unsafe { core::ptr::read((a + WORD) as *const usize) }
}
unsafe fn write_size(a: usize, v: usize) {
    unsafe { core::ptr::write(a as *mut usize, v) }
}
unsafe fn write_next(a: usize, v: usize) {
    unsafe { core::ptr::write((a + WORD) as *mut usize, v) }
}

struct HoleList {
    head_next: usize,
}

static mut HEAP: HoleList = HoleList { head_next: 0 };

impl HoleList {
    fn allocate(&mut self, size: usize, align: usize) -> usize {
        let mut prev_next: *mut usize = &mut self.head_next;
        let mut cur = self.head_next;
        while cur != 0 {
            let hsize = unsafe { read_size(cur) };
            let next = unsafe { read_next(cur) };
            let hole_end = cur + hsize;
            let alloc_start = if cur.is_multiple_of(align) {
                cur
            } else {
                align_up(cur + HOLE_SIZE, align)
            };
            let alloc_end = alloc_start.wrapping_add(size);
            if alloc_end >= alloc_start && alloc_end <= hole_end {
                let front = alloc_start - cur;
                let back = hole_end - alloc_end;
                if back != 0 && back < HOLE_SIZE {
                    prev_next = (cur + WORD) as *mut usize;
                    cur = next;
                    continue;
                }
                unsafe { core::ptr::write(prev_next, next) };
                if front != 0 {
                    self.insert(cur, front);
                }
                if back != 0 {
                    self.insert(alloc_end, back);
                }
                return alloc_start;
            }
            prev_next = (cur + WORD) as *mut usize;
            cur = next;
        }
        0
    }

    fn insert(&mut self, addr: usize, size: usize) {
        let mut prev = 0usize;
        let mut cur = self.head_next;
        while cur != 0 && cur < addr {
            prev = cur;
            cur = unsafe { read_next(cur) };
        }
        unsafe {
            write_size(addr, size);
            write_next(addr, cur);
            if prev == 0 {
                self.head_next = addr;
            } else {
                write_next(prev, addr);
            }
        }
        if cur != 0 && addr + size == cur {
            let (cs, cn) = unsafe { (read_size(cur), read_next(cur)) };
            unsafe {
                write_size(addr, size + cs);
                write_next(addr, cn);
            }
        }
        if prev != 0 {
            let ps = unsafe { read_size(prev) };
            if prev + ps == addr {
                let (as_, an) = unsafe { (read_size(addr), read_next(addr)) };
                unsafe {
                    write_size(prev, ps + as_);
                    write_next(prev, an);
                }
            }
        }
    }
}

/// Map another arena and add it to the free list. Returns false if the kernel
/// could not satisfy the mapping.
fn grow(min: usize) -> bool {
    let want = align_up(min.max(GROW), WORD);
    let base = unsafe { sys_mmap(want) };
    if base == 0 {
        return false;
    }
    unsafe { (*core::ptr::addr_of_mut!(HEAP)).insert(base, want) };
    true
}

pub unsafe fn alloc(layout: Layout) -> *mut u8 {
    let (size, align) = size_align(layout);
    let heap = unsafe { &mut *core::ptr::addr_of_mut!(HEAP) };
    let p = heap.allocate(size, align);
    if p != 0 {
        return p as *mut u8;
    }
    if grow(size + align) {
        let p = heap.allocate(size, align);
        if p != 0 {
            return p as *mut u8;
        }
    }
    core::ptr::null_mut()
}

pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    let (size, _align) = size_align(layout);
    unsafe { (*core::ptr::addr_of_mut!(HEAP)).insert(ptr as usize, size) };
}

pub unsafe fn realloc(ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
    let new_ptr = unsafe { alloc(new_layout) };
    if !new_ptr.is_null() {
        let copy = layout.size().min(new_size);
        unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr, copy) };
        unsafe { dealloc(ptr, layout) };
    }
    new_ptr
}

pub unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    // SYS_MMAP hands back zeroed pages, but freed-and-reused holes are not
    // re-zeroed, so zero explicitly.
    let ptr = unsafe { alloc(layout) };
    if !ptr.is_null() {
        unsafe { core::ptr::write_bytes(ptr, 0, layout.size()) };
    }
    ptr
}
