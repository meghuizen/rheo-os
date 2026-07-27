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

// ============================================================================
// Typed memory grants (docs/LIBRHEO.md Phase B, docs/MEMORY.md, ARCHITECTURE.md
// 3 object 5). The terabytes/DuckDB/warehouse substrate: real typed memory a
// cell reserves large and commits on demand, seals to share zero-copy, and maps
// a dataset into. These wrap the kernel's grant syscalls (`SYS_GRANT`/`COMMIT`/
// `DECOMMIT`/`SEAL`/`MMAP_FILE`/`MUNMAP`); the kernel grant-checks every use.
// ============================================================================

use crate::sys;

/// The typed kind of memory a grant is backed by (docs/MEMORY.md 2.1). Mirrors
/// the kernel's `mm::grant::MemKind`.
///
/// What each kind actually gets, on this machine:
/// - `Ddr` - the DDR frame pool. Always real.
/// - `Pmem` - **real persistent memory** where the platform exposes an nvdimm
///   (x86-64 q35 via the ACPI NFIT): the kernel commits from a separate pmem
///   allocator, physically distinct from the DDR pool. Where no nvdimm exists
///   (arm/riscv `virt`) it falls back to DDR and the kernel **prints the
///   reason** - it is not silently aliased.
/// - `Hbm`/`Cxl`/`Remote` - emulated as DDR; QEMU models no such memory. Also
///   reported once, for the same reason.
/// - `DeviceBar` - no backing; refused by the kernel.
///
/// This doc used to say `Pmem` was DDR-backed like the others. That was true of
/// the `SYS_GRANT` path and false of the design, because the kind was recorded
/// and then ignored at commit time (docs/ARCHITECTURE-DEBT.md 3.6). The kind now
/// reaches the allocator.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum MemKind {
    Ddr = 0,
    Hbm = 1,
    Cxl = 2,
    Pmem = 3,
    DeviceBar = 4,
    Remote = 5,
}

/// A typed memory grant (ARCHITECTURE.md 3 object 5): a reservation of address
/// space of a declared [`MemKind`], demand-committed with frames, sealable to
/// immutable. RAII - the reservation and its frames are released on `Drop`.
pub struct Grant {
    base: usize,
    len: usize,
    cap_id: u32,
    sealed: bool,
}

impl Grant {
    /// Reserve `len` bytes of grant address space of `kind` (no frames yet -
    /// demand commit). `None` if the kernel refuses (bad kind / table full).
    /// The `node` NUMA hint is recorded but single-node in QEMU (honest).
    pub fn reserve_on(kind: MemKind, len: usize, node: u32) -> Option<Grant> {
        let mut info = sys::GrantInfo { base: 0, cap_id: 0 };
        let out = &mut info as *mut sys::GrantInfo as u64;
        // `flags` low bits carry the NUMA node hint (ignored by the kernel today,
        // single-node; documented). See docs/LIBRHEO.md Phase B.
        let r = sys::grant(out, len, kind as u64, node as u64);
        if r != 0 {
            return None;
        }
        Some(Grant {
            base: info.base as usize,
            len: (len + 0xFFF) & !0xFFF,
            cap_id: info.cap_id as u32,
            sealed: false,
        })
    }

    /// Reserve on the default node.
    pub fn reserve(kind: MemKind, len: usize) -> Option<Grant> {
        Grant::reserve_on(kind, len, 0)
    }

    /// Reserve and immediately commit the whole grant (the common working-buffer
    /// case). `None` on failure.
    pub fn alloc(kind: MemKind, len: usize) -> Option<Grant> {
        let g = Grant::reserve(kind, len)?;
        if g.commit(0, g.len).is_err() {
            return None;
        }
        Some(g)
    }

    /// Back `[offset, offset+len)` with fresh zeroed RW frames. `Err` if sealed
    /// or out of range.
    #[allow(clippy::result_unit_err)] // kernel returns only ok/fail, no detail yet
    pub fn commit(&self, offset: usize, len: usize) -> Result<(), ()> {
        if sys::commit(self.cap_id, offset, len) == 0 {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Return the frames backing `[offset, offset+len)` to the pool.
    #[allow(clippy::result_unit_err)] // kernel returns only ok/fail, no detail yet
    pub fn decommit(&self, offset: usize, len: usize) -> Result<(), ()> {
        if sys::decommit(self.cap_id, offset, len) == 0 {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Seal the grant immutable (its pages become read-only, shareable) - the
    /// zero-copy-buffer / dmabuf precursor. After this, commit/decommit refuse.
    #[allow(clippy::result_unit_err)] // kernel returns only ok/fail, no detail yet
    pub fn seal(&mut self) -> Result<(), ()> {
        if sys::seal(self.cap_id) == 0 {
            self.sealed = true;
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }
    pub fn base(&self) -> usize {
        self.base
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// The 32-bit capability id (for a zero-copy read straight into this grant).
    pub fn cap_id(&self) -> u32 {
        self.cap_id
    }

    /// The committed region as a shared byte slice.
    ///
    /// # Safety
    /// `[offset, offset+len)` must be committed and, if sealed, is read-only.
    pub unsafe fn slice(&self, offset: usize, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts((self.base + offset) as *const u8, len) }
    }

    /// The committed region as a mutable byte slice (grant must be unsealed).
    ///
    /// # Safety
    /// `[offset, offset+len)` must be committed and writable.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self, offset: usize, len: usize) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut((self.base + offset) as *mut u8, len) }
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        // Release the reservation and its frames (RAII reclaim).
        sys::munmap(self.base, self.len);
    }
}

/// A bump arena over a committed [`Grant`]: hand out aligned sub-slices without
/// per-object syscalls. The out-of-core / columnar-scratch allocator.
pub struct Arena {
    grant: Grant,
    next: usize,
}

impl Arena {
    /// A fully-committed DDR arena of `len` bytes.
    pub fn new(len: usize) -> Option<Arena> {
        Some(Arena {
            grant: Grant::alloc(MemKind::Ddr, len)?,
            next: 0,
        })
    }

    /// Carve `len` bytes aligned to `align` from the arena, or `None` if full.
    pub fn alloc(&mut self, len: usize, align: usize) -> Option<&mut [u8]> {
        let start = (self.grant.base() + self.next + align - 1) & !(align - 1);
        let off = start - self.grant.base();
        if off + len > self.grant.len() {
            return None;
        }
        self.next = off + len;
        // SAFETY: `[off, off+len)` is within the committed grant, uniquely handed
        // out (the bump cursor never revisits it).
        Some(unsafe { self.grant.slice_mut(off, len) })
    }

    pub fn reset(&mut self) {
        self.next = 0;
    }
}

/// A file mapped zero-copy into the cell (docs/LIBRHEO.md Phase B): the file
/// range is read into frames and mapped read-only; the scan then touches mapped
/// memory directly, no syscall per access. RAII - unmapped on `Drop`.
pub struct Mapping {
    base: usize,
    len: usize,
}

impl Mapping {
    /// Map `len` bytes of the file open on `fd` starting at `offset`. `None` on
    /// failure. The bytes are copied into frames once at map time; reads
    /// afterward are pure memory access (zero-copy at scan time).
    pub fn file(fd: u64, offset: u64, len: usize) -> Option<Mapping> {
        let base = sys::mmap_file(fd, offset, len, 0);
        if base == 0 {
            return None;
        }
        Some(Mapping { base, len })
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `[base, base+len)` is mapped read-only for this Mapping's life.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, self.len) }
    }

    pub fn base(&self) -> usize {
        self.base
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        sys::munmap(self.base, (self.len + 0xFFF) & !0xFFF);
    }
}
