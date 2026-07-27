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
//! ## Scope (honest)
//! - **`MAP_PRIVATE` only**, which is what `ld.so` and every mapping in this tree
//!   use. A private mapping's page is a *copy*, so a write after the fill is not
//!   reflected back to the file - correct for PRIVATE, and `MAP_SHARED` of a file
//!   is refused elsewhere rather than modelled here.
//! - The registry is small ([`MAX_MAPPED_FILES`]). A program mapping more distinct
//!   files than that gets a clean refusal from `mmap`, not a wrong mapping.

use core::ptr::addr_of_mut;

/// Distinct files that may be mapped at once, across all cells. A dynamically
/// linked program maps its own image plus `ld.so` plus `libc.so.6`; 8 leaves room
/// and keeps the kernel allocation-free.
pub const MAX_MAPPED_FILES: usize = 8;

#[derive(Copy, Clone)]
struct MappedFile {
    used: bool,
    /// One reference per `Vma` that names this entry (so `fork` and a split
    /// `munmap` are both just counter changes).
    refs: u16,
    /// The handle `FileOps::open` returned - the mapping's own, not the caller's.
    vfs_fd: i64,
}

impl MappedFile {
    const fn new() -> MappedFile {
        MappedFile {
            used: false,
            refs: 0,
            vfs_fd: -1,
        }
    }
}

static mut TBL: [MappedFile; MAX_MAPPED_FILES] = [const { MappedFile::new() }; MAX_MAPPED_FILES];

fn tbl() -> &'static mut [MappedFile; MAX_MAPPED_FILES] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(TBL) }
}

/// Close every handle and clear the table (called from `linux::reset`).
pub fn reset() {
    for e in tbl().iter_mut() {
        if e.used
            && e.vfs_fd >= 0
            && let Some(o) = crate::svc::file_ops()
        {
            (o.close)(e.vfs_fd as u64);
        }
        *e = MappedFile::new();
    }
}

/// Open `path` for a new file-backed mapping and return its registry index.
///
/// `path_va`/`path_len` name the path in **kernel** memory (the caller has already
/// copied it out of the cell), because the cell's address space is not active when
/// a fault later refills a page - and the same handle must serve then.
pub fn open(path_va: u64, path_len: u64) -> Option<u8> {
    let ops = crate::svc::file_ops()?;
    let t = tbl();
    let idx = (0..MAX_MAPPED_FILES).find(|&i| !t[i].used)?;
    // O_RDONLY: a MAP_PRIVATE mapping never writes back, so read access is all the
    // authority the mapping needs - asking for more would be authority we do not
    // use (ARCHITECTURE.md 5).
    let fd = (ops.open)(path_va, path_len, 0);
    if fd < 0 {
        return None;
    }
    t[idx] = MappedFile {
        used: true,
        refs: 1,
        vfs_fd: fd,
    };
    Some(idx as u8)
}

/// A new `Vma` names entry `h` (a split `munmap`, or `fork`'s copy of the list).
pub fn addref(h: u8) {
    let e = &mut tbl()[h as usize];
    if e.used {
        e.refs = e.refs.saturating_add(1);
    }
}

/// Drop a reference; close the file and free the slot at zero.
pub fn close(h: u8) {
    let e = &mut tbl()[h as usize];
    if !e.used {
        return;
    }
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        if e.vfs_fd >= 0
            && let Some(o) = crate::svc::file_ops()
        {
            (o.close)(e.vfs_fd as u64);
        }
        *e = MappedFile::new();
    }
}

/// Read `len` bytes of entry `h` at file offset `off` into **kernel** VA
/// `dst_kva`, returning the count read (0 at or past end of file).
///
/// The destination is a kernel VA on purpose: a fault fills a freshly allocated
/// frame through the kernel's linear map *before* it is user-mapped, so the read
/// cannot alias the cell's memory and cannot be steered by the cell.
pub fn read_at(h: u8, dst_kva: u64, len: u64, off: i64) -> i64 {
    let e = tbl()[h as usize];
    if !e.used || e.vfs_fd < 0 {
        return -1;
    }
    match crate::svc::file_ops() {
        Some(o) => {
            // `FileOps` has no pread, so seek then read - which is safe here
            // because the handle is the *mapping's own*: nothing else shares its
            // file position (the very reason it is not the caller's fd).
            if (o.lseek)(e.vfs_fd as u64, off, 0) < 0 {
                return -1;
            }
            (o.read)(e.vfs_fd as u64, dst_kva, len)
        }
        None => -1,
    }
}

/// How many registry slots are in use - the witness a test asserts against, so
/// "the mapping owns a handle and gives it back" is observed rather than assumed.
pub fn in_use() -> usize {
    tbl().iter().filter(|e| e.used).count()
}
