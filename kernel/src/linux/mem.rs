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
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
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
    user::map_anon_at(base, bytes, perm_from_prot(prot));
    st.mmap_cursor = base + bytes;
    base as i64
}

/// munmap(addr, len): unmap the pages and return their frames to the pool.
pub fn munmap(addr: u64, len: u64) -> i64 {
    user::unmap_range(addr as usize, len as usize);
    0
}

/// mprotect(addr, len, prot): rewrite page permissions in place.
pub fn mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    user::protect_range(addr as usize, len as usize, perm_from_prot(prot));
    0
}
