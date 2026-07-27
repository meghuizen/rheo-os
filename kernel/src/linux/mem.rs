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

/// **End** of the per-cell mmap region - the first address the bump cursor may
/// not reach (docs/ARCHITECTURE-DEBT.md 4, blocker 2).
///
/// The cursor used to be unbounded. `mmap` is a forward bump with no accounting,
/// so a large enough run of allocations walked straight through the cell's
/// **queue-pair** region (16 GiB), its **channel** regions (24 GiB) and then the
/// **ELF interpreter** at [`load::LINUX_INTERP_BASE`] (64 GiB), where `ld.so` and
/// `libc.so.6` live - handing a program addresses that alias its own dynamic
/// linker. Silent corruption, and against a ~100 MB binary not a remote
/// possibility: 4 GiB of mappings is enough to reach the queue.
///
/// 16 GiB is the queue region, so that is the ceiling: the whole 4 GiB from 12 to
/// 16 GiB is the mmap region, and past it `mmap` reports `-ENOMEM`, which is an
/// answer a caller can act on. A real VMA list with first-fit placement and reuse
/// of freed spans is the proper fix and is still open; this is the bound that
/// makes the *failure mode* correct in the meantime.
const MMAP_END: usize = crate::load::USER_QUEUE_VA;
const _: () = assert!(MMAP_BASE < MMAP_END);

/// Spans a cell's `mmap` must never place a mapping over, because something else
/// already owns them. Checked for `MAP_FIXED` (a caller-chosen address), which the
/// bump cursor cannot protect against.
///
/// The ELF interpreter's span is deliberately **not** here: `ld.so` legitimately
/// maps within its own region, and refusing that would break every dynamically
/// linked binary (docs/LINUX-COMPAT.md L7). The queue and channel regions are
/// kernel-owned rings mapped into the cell; a program targeting one is either
/// confused or hostile, and either way must be refused rather than allowed to
/// replace the kernel's frames.
fn reserved_overlap(base: usize, bytes: usize) -> Option<&'static str> {
    let end = base.saturating_add(bytes);
    let hits = |s: usize, n: usize| base < s + n && s < end;
    if hits(
        crate::load::USER_QUEUE_VA,
        crate::queue::QueuePair::REGION_SIZE,
    ) {
        return Some("the cell's queue-pair region");
    }
    let chan = crate::load::channel_slot_va(0);
    let chan_span = crate::abi::MAX_CELL_CHANNELS * crate::queue::QueuePair::REGION_SIZE;
    if hits(chan, chan_span) {
        return Some("the cell's cross-cell channel region");
    }
    None
}

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
///
/// Callers must reject `PROT_WRITE | PROT_EXEC` **before** calling this (see
/// [`wx_refused`]): there is no RWX `MapPerm`, and this function would quietly
/// return `UserRw`.
fn perm_from_prot(prot: u64) -> MapPerm {
    if prot & PROT_EXEC != 0 && prot & PROT_WRITE == 0 {
        MapPerm::UserRx
    } else if prot & PROT_WRITE != 0 {
        MapPerm::UserRw
    } else {
        MapPerm::UserRo
    }
}

/// Refuse a simultaneously writable and executable mapping - and **say so** -
/// rather than granting it and dropping EXEC (docs/ARCHITECTURE-DEBT.md 4,
/// blocker 1).
///
/// W^X is structural here: [`MapPerm`] has three variants and no RWX, by design
/// (ARCHITECTURE.md 5). But `mmap`/`mprotect` used to *accept* `PROT_WRITE |
/// PROT_EXEC`, run it through [`perm_from_prot`], and hand back a plain
/// read-write mapping while **reporting success**. A JIT that maps its code pool
/// RWX - which is what JavaScriptCore does on Linux - would then fault on its
/// first jump into generated code, with no diagnostic anywhere near the cause.
///
/// `-EPERM` is the answer that lets a caller act: the W->X *flip* path
/// (`mprotect` RW then RX) works here, and a JIT that checks its `mmap` result
/// can take it. Silently dropping the bit removes that choice.
///
/// Whether to add a `UserRwx` variant is a **doctrine** question - it needs the
/// ARCHITECTURE.md 6 admission pass, not a patch - and is deliberately left open.
/// This only makes the current answer honest.
fn wx_refused(prot: u64, what: &str) -> bool {
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        crate::println!(
            "linux: {what} PROT_WRITE|PROT_EXEC refused (W^X is structural; \
             use mprotect RW then RX)"
        );
        return true;
    }
    false
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
        // (docs/ENGINEERING.md 12).
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
    if wx_refused(prot, "mmap") {
        return -EPERM;
    }
    let bytes = page_up(len as usize);
    let fixed = flags & MAP_FIXED != 0;
    let anon = flags & MAP_ANONYMOUS != 0;

    let base = if fixed {
        let b = (addr as usize) & !(FRAME_SIZE - 1);
        // A caller-chosen address is the one case the bump cursor cannot protect
        // against: refuse the kernel-owned rings rather than let the cell replace
        // their frames (docs/ARCHITECTURE-DEBT.md 4).
        if let Some(what) = reserved_overlap(b, bytes) {
            crate::println!("linux: mmap MAP_FIXED at {b:#x} refused - overlaps {what}");
            return -EINVAL;
        }
        b
    } else {
        let b = st.mmap_cursor;
        // Bounded (docs/ARCHITECTURE-DEBT.md 4, blocker 2): the cursor used to run
        // forward without limit, through the queue region, the channel regions and
        // then ld.so. `-ENOMEM` is an answer glibc acts on; silently aliasing the
        // dynamic linker is not.
        let Some(next) = b.checked_add(bytes) else {
            return -ENOMEM;
        };
        if next > MMAP_END {
            crate::println!(
                "linux: mmap of {bytes:#x} refused - the {:#x}..{MMAP_END:#x} mmap \
                 region is exhausted at {b:#x}",
                MMAP_BASE
            );
            return -ENOMEM;
        }
        st.mmap_cursor = next;
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
        // refusal maps nothing further (docs/ENGINEERING.md 12).
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
    // Same bound as `mmap`: the cursor is shared, so an unbounded `mremap` would
    // walk into the queue region and ld.so exactly as an unbounded `mmap` did
    // (docs/ARCHITECTURE-DEBT.md 4, blocker 2).
    let base = st.mmap_cursor;
    let Some(next) = base.checked_add(new_len) else {
        return -ENOMEM;
    };
    if next > MMAP_END {
        crate::println!(
            "linux: mremap of {new_len:#x} refused - the mmap region is exhausted at {base:#x}"
        );
        return -ENOMEM;
    }
    if !user::map_anon_at(base, new_len, MapPerm::UserRw) {
        return -ENOMEM;
    }
    st.mmap_cursor = next;
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
    if wx_refused(prot, "mprotect") {
        return -EPERM;
    }
    if prot & PROT_ANY == 0 {
        user::unmap_range(addr as usize, len as usize);
    } else {
        if !user::commit_range(addr as usize, len as usize, perm_from_prot(prot)) {
            return -ENOMEM;
        }
    }
    0
}
