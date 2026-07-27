//! The Linux personality's memory syscalls (docs/LINUX-COMPAT.md L2): `brk`,
//! anonymous `mmap`, `munmap`, `mprotect`, `madvise`. These are pure
//! translation over the cell's own address space - the kernel mechanisms
//! (`user::map_anon_at`/`unmap_range`/`protect_range`, backed by
//! `AddressSpace::{map_user_frame,unmap,protect}` + `frames`) each pass the
//! ARCHITECTURE.md 6 admission rule as memory-grant mechanics, independent of
//! Linux. A Linux process's heap cannot exceed its hosting cell's grants.

use crate::arch::{self, MapPerm};
use crate::linux::LinuxState;
use crate::linux::errno::*;
use crate::mm::frames::FRAME_SIZE;
use crate::user;

// mmap prot bits.
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
/// Any access bit set (PROT_NONE is the absence of all of them).
const PROT_ANY: u64 = PROT_READ | PROT_WRITE | PROT_EXEC;
// mmap flags.
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

/// Base of the per-cell anonymous mmap region: 12 GiB, above the image
/// (1-2 GiB), stack (8 GiB), free in every cell root. Each cell has its own
/// address space, so the same VA in two cells is isolated.
const MMAP_BASE: usize = 0x3_0000_0000;

fn page_up(x: usize) -> usize {
    (x + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

/// The initial value for `LinuxState::mmap_cursor`.
pub const fn mmap_base() -> usize {
    MMAP_BASE
}

/// Map mmap `prot` bits onto a W^X `MapPerm`. PROT_EXEC without PROT_WRITE is
/// executable-read; PROT_WRITE is read-write; anything else (PROT_READ,
/// PROT_NONE) is read-only.
fn perm_from_prot(prot: u64) -> MapPerm {
    if prot & PROT_EXEC != 0 && prot & PROT_WRITE == 0 {
        MapPerm::UserRx
    } else if prot & PROT_WRITE != 0 {
        MapPerm::UserRw
    } else {
        MapPerm::UserRo
    }
}

/// brk(addr): grow or shrink the heap. `brk(0)` (and any address below the
/// heap base) returns the current break unchanged - glibc's `__brk` treats the
/// returned break as authoritative.
pub fn brk(st: &mut LinuxState, addr: u64) -> u64 {
    let addr = addr as usize;
    if addr < st.brk_start {
        return st.brk_cur as u64;
    }
    let new = page_up(addr);
    if new > st.brk_cur {
        // A refused grow leaves the break where it was: glibc treats the
        // returned break as authoritative and falls back to mmap
        // (docs/ENGINEERING.md 13).
        if !user::map_anon_at(st.brk_cur, new - st.brk_cur, MapPerm::UserRw) {
            return st.brk_cur as u64;
        }
    } else if new < st.brk_cur {
        user::unmap_range(new, st.brk_cur - new);
    }
    st.brk_cur = new;
    st.brk_cur as u64
}

/// mmap(addr, len, prot, flags, fd, offset).
///
/// - **Anonymous** (`MAP_ANONYMOUS`): a PROT_NONE mapping only *reserves*
///   address space (demand-commit, L4) - glibc reserves large PROT_NONE arenas
///   and commits sub-ranges via `mprotect`; an accessible mapping is committed
///   now (fresh zeroed frames).
/// - **File-backed private** (`MAP_PRIVATE` of an fd, L7): the file range
///   `[offset, offset+len)` is read into fresh frames (partial last page
///   zero-filled) and mapped with `prot`. This is exactly what ld.so does to
///   map the program + libc; `MAP_SHARED` of a file is not modeled (ld.so uses
///   PRIVATE).
/// - **MAP_FIXED**: the mapping is placed at the caller's `addr`, replacing any
///   existing pages there; without it, placement bumps the per-cell mmap
///   cursor. ld.so reserves a library's whole span then `MAP_FIXED`-overlays
///   each segment (text r-x, data rw) at computed offsets.
pub fn mmap(
    st: &mut LinuxState,
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: i64,
    offset: u64,
) -> i64 {
    if len == 0 {
        return -EINVAL;
    }
    let bytes = page_up(len as usize);
    let fixed = flags & MAP_FIXED != 0;
    let anon = flags & MAP_ANONYMOUS != 0;

    let base = if fixed {
        (addr as usize) & !(FRAME_SIZE - 1)
    } else {
        let b = st.mmap_cursor;
        st.mmap_cursor = b + bytes;
        b
    };

    if anon {
        // Anonymous mmap always yields ZEROED memory. When MAP_FIXED overlays
        // an already-mapped range (ld.so maps a library's bss this way, over
        // the file-backed reservation it made first), the existing frames must
        // be discarded and replaced with fresh zeroed ones - NOT reprotected in
        // place (that would leak the reservation's file bytes into the bss and
        // corrupt, e.g., libc's stdio locks). So free any existing pages, then
        // map fresh zeroed frames.
        if fixed {
            user::unmap_range(base, bytes);
        }
        if prot & PROT_ANY != 0 && !user::map_anon_at(base, bytes, perm_from_prot(prot)) {
            return -ENOMEM;
        }
        // PROT_NONE: leave it a bare reservation (no frames).
    } else {
        // File-backed private mapping (L7).
        if fd < 0 {
            return -EBADF;
        }
        if !map_file(st, base, bytes, prot, fd, offset as i64) {
            return -ENOMEM;
        }
    }
    base as i64
}

/// Map `bytes` (page count) of a VFS file at `base`, one page at a time: unmap
/// any page already there (MAP_FIXED overlay), allocate a fresh zeroed frame,
/// read the file range for that page into it through the kernel linear map, and
/// map it with `perm`. A short read (EOF) leaves the page tail zero
/// (docs/LINUX-COMPAT.md L7).
fn map_file(
    st: &mut LinuxState,
    base: usize,
    bytes: usize,
    prot: u64,
    fd: i64,
    offset: i64,
) -> bool {
    let perm = perm_from_prot(prot);
    let pages = bytes / FRAME_SIZE;
    for i in 0..pages {
        let va = base + i * FRAME_SIZE;
        // Reclaim any existing page at this VA (MAP_FIXED replaces it); this
        // also uncharges it, and refuses a VA outside the cell's user range.
        user::unmap_range(va, FRAME_SIZE);
        // A file mapping is charged to the cell like an anonymous one; a
        // refusal maps nothing further (docs/ENGINEERING.md 13).
        let Some(pa) = user::alloc_user_frame() else {
            return false;
        };
        let file_off = offset + (i * FRAME_SIZE) as i64;
        // Read into the frame via the kernel high linear map (valid under any
        // active cell root; the VFS handler runs in kernel context). The frame
        // is not yet user-mapped, so this cannot alias user memory.
        let kva = arch::phys_to_virt(pa) as u64;
        st.fds.pread(fd, kva, FRAME_SIZE as u64, file_off);
        user::with_current_aspace(|aspace| {
            aspace.map_user_frame(va, pa, perm);
        });
    }
    true
}

/// mremap(old_addr, old_size, new_size, flags, new_addr): resize a mapping.
/// Shrinking unmaps the tail in place. Growing requires MREMAP_MAYMOVE (the
/// bump mmap region cannot extend in place): a fresh region is mapped, the old
/// contents copied, and the old range freed. glibc's `realloc` of large blocks
/// depends on this; without it the malloc-copy-free fallback leaks our frames
/// (docs/LINUX-COMPAT.md L3).
pub fn mremap(st: &mut LinuxState, old_addr: u64, old_size: u64, new_size: u64, flags: u64) -> i64 {
    const MREMAP_MAYMOVE: u64 = 1;
    let old_addr = old_addr as usize;
    let old_len = page_up(old_size as usize);
    let new_len = page_up(new_size as usize);
    if new_size == 0 {
        return -EINVAL;
    }
    if new_len <= old_len {
        if new_len < old_len {
            user::unmap_range(old_addr + new_len, old_len - new_len);
        }
        return old_addr as i64;
    }
    // Grow: only by moving (the region is a forward bump allocator).
    if flags & MREMAP_MAYMOVE == 0 {
        return -ENOMEM;
    }
    let base = st.mmap_cursor;
    if !user::map_anon_at(base, new_len, MapPerm::UserRw) {
        return -ENOMEM;
    }
    st.mmap_cursor = base + new_len;
    // Copy the old contents; both ranges are mapped in the active cell root.
    // SAFETY: trap context, cell address space active; `old_len` bytes of the
    // source are mapped and `new_len >= old_len` bytes of the destination are.
    unsafe {
        core::ptr::copy_nonoverlapping(old_addr as *const u8, base as *mut u8, old_len);
    }
    user::unmap_range(old_addr, old_len);
    base as i64
}

/// munmap(addr, len): unmap the pages and return their frames to the pool.
pub fn munmap(addr: u64, len: u64) -> i64 {
    user::unmap_range(addr as usize, len as usize);
    0
}

/// mprotect(addr, len, prot): change page permissions. Making a reserved
/// (uncommitted) range accessible commits fresh frames for it (the glibc
/// arena/stack growth path, docs/LINUX-COMPAT.md L4); PROT_NONE decommits the
/// range, returning its frames to the pool.
pub fn mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    if prot & PROT_ANY == 0 {
        user::unmap_range(addr as usize, len as usize);
    } else {
        if !user::commit_range(addr as usize, len as usize, perm_from_prot(prot)) {
            return -ENOMEM;
        }
    }
    0
}
