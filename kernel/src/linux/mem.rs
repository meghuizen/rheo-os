//! The Linux personality's memory syscalls (docs/LINUX-COMPAT.md L2): `brk`,
//! anonymous `mmap`, `munmap`, `mprotect`, `madvise`. These are pure
//! translation over the cell's own address space - the kernel mechanisms
//! (`user::map_anon_at`/`unmap_range`/`protect_range`, backed by
//! `AddressSpace::{map_user_frame,unmap,protect}` + `frames`) each pass the
//! ARCHITECTURE.md 6 admission rule as memory-grant mechanics, independent of
//! Linux. A Linux process's heap cannot exceed its hosting cell's grants.

use crate::arch::MapPerm;
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
        user::map_anon_at(st.brk_cur, new - st.brk_cur, MapPerm::UserRw);
    } else if new < st.brk_cur {
        user::unmap_range(new, st.brk_cur - new);
    }
    st.brk_cur = new;
    st.brk_cur as u64
}

/// mmap(addr, len, prot, flags, fd, off) - anonymous private only for L2.
/// fd-backed mappings are L7 (dynamic linking); MAP_FIXED is deferred. Both
/// return -ENOSYS so glibc sees an honest failure rather than a wrong mapping.
pub fn mmap(st: &mut LinuxState, len: u64, prot: u64, flags: u64) -> i64 {
    if flags & MAP_ANONYMOUS == 0 {
        return -ENOSYS; // fd-backed mmap is L7
    }
    if flags & MAP_FIXED != 0 {
        return -ENOSYS; // fixed placement is not modeled yet
    }
    if len == 0 {
        return -EINVAL;
    }
    let bytes = page_up(len as usize);
    let base = st.mmap_cursor;
    // Demand-commit (docs/LINUX-COMPAT.md L4): a PROT_NONE mapping only reserves
    // address space (no frames) - glibc reserves large PROT_NONE regions (a
    // 64 MiB malloc arena per thread, thread-stack guards) and commits
    // sub-ranges with `mprotect` as it grows. Backing them eagerly would
    // exhaust the frame pool the moment a second thread is created. An
    // accessible mapping is committed now.
    if prot & PROT_ANY != 0 {
        user::map_anon_at(base, bytes, perm_from_prot(prot));
    }
    st.mmap_cursor = base + bytes;
    base as i64
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
    user::map_anon_at(base, new_len, MapPerm::UserRw);
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
        user::commit_range(addr as usize, len as usize, perm_from_prot(prot));
    }
    0
}
