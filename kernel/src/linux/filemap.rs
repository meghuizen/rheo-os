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
/// linked program maps its own image plus `ld.so` plus every shared library it
/// pulls in - a C or Rust hello is 3-4 (image + ld.so + libc + libgcc_s), but a
/// **production** binary links a dozen or more (libstdc++, libm, libpthread,
/// libdl, libssl, ...), which is the shape the real target (an unmodified
/// dynamically-linked application) needs. 64 is headroom for that, still a small
/// fixed static array (the kernel stays allocation-free). A program mapping more
/// than this gets a clean `mmap` refusal, never a wrong mapping. This is a
/// limit-raise, not a design change (docs/LINUX-COMPAT.md), like the frame-pool
/// and object-table raises before it.
pub const MAX_MAPPED_FILES: usize = 64;

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

static mut TBL: [MappedFile; MAX_MAPPED_FILES] = [const { MappedFile::new() }; MAX_MAPPED_FILES];

fn tbl() -> &'static mut [MappedFile; MAX_MAPPED_FILES] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(TBL) }
}

/// Close every handle and clear the table (called from `linux::reset`).
pub fn reset() {
    for e in tbl().iter_mut() {
        if e.used {
            release(e);
        }
        *e = MappedFile::new();
    }
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
        store: Store::Vfs(fd),
    };
    Some(idx as u8)
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
pub unsafe fn open_mem(addr: usize, len: usize) -> Option<u8> {
    let t = tbl();
    let idx = (0..MAX_MAPPED_FILES).find(|&i| !t[i].used)?;
    t[idx] = MappedFile {
        used: true,
        refs: 1,
        store: Store::Mem(addr, len),
    };
    Some(idx as u8)
}

/// Is entry `h` live? A caller that recorded a handle and then had it cleared out
/// from under it (a `reset` between load and install) would otherwise present as a
/// mapping full of zeros - which on RISC-V is an illegal instruction at the entry
/// point and nothing more informative (docs/ENGINEERING.md 11).
pub fn alive(h: u8) -> bool {
    (h as usize) < MAX_MAPPED_FILES && tbl()[h as usize].used
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
        release(e);
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
    tbl().iter().filter(|e| e.used).count()
}
