//! The **mapped-file registry**: the VFS handles that file-backed mappings own,
//! so a page can be filled from its file long after `mmap` returned
//! (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
//!
//! ## Why a mapping cannot just remember the caller's fd
//!
//! `ld.so` maps a library and **closes the fd immediately**, which is legal and
//! normal: on Linux the mapping holds a reference to the *file*, not to the
//! descriptor. A `Vma` that recorded `fd` would therefore reference a closed - and
//! soon reused - descriptor, and a fault would read the wrong file or none at all.
//! That is not a corner case, it is the very first thing the dynamic loader does.
//!
//! So a file-backed `mmap` **opens the path again** through
//! [`crate::svc::FileOps`] and stores the resulting handle here. The mapping owns
//! it; closing the caller's fd is irrelevant to it.
//!
//! ## No kernel object
//!
//! A global, fixed-size, refcounted registry - the `linux::pipe` / `epoll` /
//! `eventfd` pattern. It is refcounted rather than per-cell because `fork`
//! deep-copies the VMA list and the child's mappings must keep working: the child
//! adds a reference to the same handle instead of re-opening (re-opening could
//! fail, and a `fork` that half-succeeds is worse than one that does not).
//!
//! ## Funded, not fixed
//!
//! The table's storage is frames from the pool (docs/SUBSTRATE.md pillar 1), so it
//! grows with demand. It is charged to [`Owner::KERNEL`] rather than to a cell,
//! and that is a deliberate accounting choice rather than an omission: an entry is
//! **shared** - a parent opens it, `fork` gives the child a reference, and either
//! may be the one that drops the last one - so attributing the frames to whichever
//! cell happened to grow the table would bill a cell for storage that outlives it
//! and mis-attribute the release. Registry entries are small and their count is
//! bounded by the mappings alive across the machine, which is the kernel's own
//! bookkeeping.
//!
//! ## Scope (honest)
//! - **`MAP_PRIVATE` only**, which is what `ld.so` and every mapping in this tree
//!   use. A private mapping's page is a *copy*, so a write after the fill is not
//!   reflected back to the file - correct for PRIVATE, and `MAP_SHARED` of a file
//!   is refused elsewhere rather than modelled here.

use crate::mm::kmeta::{Funded, Owner};

/// A registry handle: the index of a [`MappedFile`] entry.
///
/// `u16`, not `u8`. The width was the *real* ceiling once the table itself could
/// grow: 256 entries is reachable by a process tree where each member maps a
/// dozen files (a container running a Node app is exactly that), and a handle that
/// silently wraps would point a mapping at **another file's** bytes - the worst
/// available failure mode, since it is neither a fault nor a refusal. Widening it
/// is what makes the growth below mean anything.
pub type Handle = u16;

/// Entries the registry starts with room for. **Not a ceiling** - the table is
/// [`Funded`] and doubles on demand.
///
/// A dynamically linked program maps its own image plus `ld.so` plus every shared
/// library it pulls in: a C or Rust hello is 3-4, a production binary a dozen or
/// more (libstdc++, libm, libssl, ...), and a process *tree* multiplies that by its
/// members. 64 covers the single-program case without a growth.
pub const INITIAL_MAPPED_FILES: usize = 64;

/// A hard sanity ceiling on registry entries, well inside what [`Handle`] can
/// address. As elsewhere, the meaningful refusal is the pool's metadata reserve;
/// this only keeps a leak bounded by something nameable.
pub const MAPPED_FILE_CEILING: usize = 8192;

/// Where a mapping's bytes come from.
///
/// Two kinds, because the two loaders differ in where the image already is. A
/// program `mmap`s a **file**, and `ld.so` streams the interpreter from one, so
/// those read through [`crate::svc::FileOps`]. But `load::load_elf_linux` is handed
/// the image as a plain `&[u8]` that is already resident in kernel memory (a test
/// kernel's `include_bytes!`, or any caller that has the bytes), and eagerly copying
/// it into frames allocates a *second* copy of every page - which is exactly the cost
/// demand paging is here to remove. So a resident image is a backing store too.
#[derive(Copy, Clone)]
enum Store {
    /// A VFS handle this registry owns and closes.
    Vfs(i64),
    /// Bytes already resident in kernel memory: `(address, length)`. Nothing to
    /// close, and nothing is copied until a page faults.
    Mem(usize, usize),
}

#[derive(Copy, Clone)]
struct MappedFile {
    used: bool,
    /// One reference per `Vma` that names this entry (so `fork` and a split
    /// `munmap` are both just counter changes).
    refs: u16,
    store: Store,
}

impl MappedFile {
    const fn new() -> MappedFile {
        MappedFile {
            used: false,
            refs: 0,
            store: Store::Vfs(-1),
        }
    }
}

static mut TBL: Funded<MappedFile> = Funded::new();

fn tbl() -> &'static mut Funded<MappedFile> {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *core::ptr::addr_of_mut!(TBL) }
}

/// Close every handle and release the table's frames (called from `linux::reset`).
pub fn reset() {
    let t = tbl();
    for i in 0..t.capacity() {
        if let Some(e) = t.get(i)
            && e.used
        {
            release(&e);
        }
    }
    t.release();
}

/// A slot for a new entry, growing the table when every existing one is used.
fn free_slot() -> Option<usize> {
    let t = tbl();
    let cap = t.capacity();
    for i in 0..cap {
        if t.get(i).is_some_and(|e| !e.used) {
            return Some(i);
        }
    }
    let want = if cap == 0 {
        INITIAL_MAPPED_FILES
    } else {
        (cap * 2).min(MAPPED_FILE_CEILING)
    };
    if want <= cap {
        return None;
    }
    t.set_owner(Owner::KERNEL);
    if !t.reserve(want) {
        return None;
    }
    // A grown frame is zeroed, and `MappedFile::new()`'s `used` is false, so the
    // first new slot reads as free without initialising it. The `store` field is
    // the one place that matters: `Store::Vfs(-1)` is *not* all-zero, so the slot
    // is written in full by `open`/`open_mem` before anything reads it - which they
    // do, and which is why `used` is the only field consulted here.
    Some(cap)
}

/// Give back whatever the store owns. Only a VFS handle owns anything; resident
/// bytes belong to the caller that supplied them.
fn release(e: &MappedFile) {
    if let Store::Vfs(fd) = e.store
        && fd >= 0
        && let Some(o) = crate::svc::file_ops()
    {
        (o.close)(fd as u64);
    }
}

/// Open `path` for a new file-backed mapping and return its registry index.
///
/// `path_va`/`path_len` name the path in **kernel** memory (the caller has already
/// copied it out of the cell), because the cell's address space is not active when
/// a fault later refills a page - and the same handle must serve then.
pub fn open(path_va: u64, path_len: u64) -> Option<Handle> {
    let ops = crate::svc::file_ops()?;
    // The slot is taken before the open, so a table that cannot grow refuses
    // without leaving a descriptor open that nothing owns.
    let idx = free_slot()?;
    // O_RDONLY: a MAP_PRIVATE mapping never writes back, so read access is all the
    // authority the mapping needs - asking for more would be authority we do not
    // use (ARCHITECTURE.md 5).
    let fd = (ops.open)(path_va, path_len, 0);
    if fd < 0 {
        return None;
    }
    tbl().set(
        idx,
        MappedFile {
            used: true,
            refs: 1,
            store: Store::Vfs(fd),
        },
    );
    Some(idx as Handle)
}

/// Register bytes already resident in kernel memory as a backing store - the
/// `load::load_elf_linux` case, where the caller holds the whole image and eagerly
/// copying it would allocate a second copy of every page.
///
/// # Safety
/// `[addr, addr+len)` must be readable kernel memory that stays valid and unchanged
/// for as long as any mapping references this entry. In practice that means a
/// `'static` image (a `include_bytes!` blob, or a buffer the caller owns for the
/// lifetime of the cell) - not a stack buffer.
pub unsafe fn open_mem(addr: usize, len: usize) -> Option<Handle> {
    let idx = free_slot()?;
    tbl().set(
        idx,
        MappedFile {
            used: true,
            refs: 1,
            store: Store::Mem(addr, len),
        },
    );
    Some(idx as Handle)
}

/// Is entry `h` live? A caller that recorded a handle and then had it cleared out
/// from under it (a `reset` between load and install) would otherwise present as a
/// mapping full of zeros - which on RISC-V is an illegal instruction at the entry
/// point and nothing more informative (docs/ENGINEERING.md 11).
pub fn alive(h: Handle) -> bool {
    tbl().get(h as usize).is_some_and(|e| e.used)
}

/// A new `Vma` names entry `h` (a split `munmap`, or `fork`'s copy of the list).
pub fn addref(h: Handle) {
    if let Some(e) = tbl().get_mut(h as usize)
        && e.used
    {
        e.refs = e.refs.saturating_add(1);
    }
}

/// Drop a reference; close the file and free the slot at zero.
pub fn close(h: Handle) {
    let t = tbl();
    let Some(e) = t.get_mut(h as usize) else {
        return;
    };
    if !e.used {
        return;
    }
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        // Copied out before `release`, which calls back into `svc::FileOps` and must
        // not hold a borrow of the table across it.
        let dead = *e;
        *e = MappedFile::new();
        release(&dead);
    }
}

/// Read `len` bytes of entry `h` at file offset `off` into **kernel** VA
/// `dst_kva`, returning the count read (0 at or past end of file).
///
/// The destination is a kernel VA on purpose: a fault fills a freshly allocated
/// frame through the kernel's linear map *before* it is user-mapped, so the read
/// cannot alias the cell's memory and cannot be steered by the cell.
pub fn read_at(h: Handle, dst_kva: u64, len: u64, off: i64) -> i64 {
    let Some(e) = tbl().get(h as usize) else {
        return -1;
    };
    if !e.used || off < 0 {
        return -1;
    }
    match e.store {
        Store::Vfs(fd) if fd >= 0 => match crate::svc::file_ops() {
            Some(o) => {
                // `FileOps` has no pread, so seek then read - which is safe here
                // because the handle is the *mapping's own*: nothing else shares its
                // file position (the very reason it is not the caller's fd).
                if (o.lseek)(fd as u64, off, 0) < 0 {
                    return -1;
                }
                (o.read)(fd as u64, dst_kva, len)
            }
            None => -1,
        },
        Store::Mem(addr, total) => {
            // Past the end is a short read of zero, exactly as a file read is, so a
            // partial last page needs no special case at either call site.
            let off = off as usize;
            let n = total.saturating_sub(off).min(len as usize);
            if n > 0 {
                // SAFETY: `[addr, addr+total)` is readable kernel memory for the
                // entry's lifetime (`open_mem`'s contract) and `off+n <= total`;
                // `dst_kva` is a freshly allocated frame through the linear map.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (addr + off) as *const u8,
                        dst_kva as *mut u8,
                        n,
                    );
                }
            }
            n as i64
        }
        Store::Vfs(_) => -1,
    }
}

/// How many registry slots are in use - the witness a test asserts against, so
/// "the mapping owns a handle and gives it back" is observed rather than assumed.
pub fn in_use() -> usize {
    let t = tbl();
    (0..t.capacity())
        .filter(|&i| t.get(i).is_some_and(|e| e.used))
        .count()
}

/// Slots currently addressable without growth, and frames held - the witnesses a
/// proof asserts growth and release against.
pub fn slots() -> usize {
    tbl().capacity()
}

/// Frames the registry holds, directory included.
pub fn frames_held() -> usize {
    tbl().frames_held()
}
