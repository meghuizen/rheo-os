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
use crate::linux::filemap;
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

/// The base of the per-cell mmap region, for callers that need to name it.
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
        // A caller-chosen address is the one case placement cannot protect
        // against: refuse the kernel-owned rings rather than let the cell replace
        // their frames (docs/ARCHITECTURE-DEBT.md 4).
        if let Some(what) = reserved_overlap(b, bytes) {
            crate::println!("linux: mmap MAP_FIXED at {b:#x} refused - overlaps {what}");
            return -EINVAL;
        }
        b
    } else {
        // **First fit over the VMA list**, from the bottom of the region every
        // time (docs/ARCHITECTURE-DEBT.md 4, blocker 2). The old bump cursor only
        // moved forward, so a program that mapped and unmapped in a loop walked it
        // to the region's end and then failed with the whole region free behind it
        // - for a long-running process the normal outcome, not a corner case.
        //
        // Scanning from the bottom is not an oversight. The first version of this
        // kept the cursor as a *hint* to start from, on the reasoning that an
        // allocation-heavy program should not rescan the low end on every call -
        // and that silently restored the exact behaviour being removed, because a
        // search that starts past every existing mapping can never find a hole
        // behind it. The fixture caught it (it asked for the freed address and got
        // a fresh one). An optimisation that defeats the property it decorates is
        // not an optimisation; if the scan ever measures as hot, the answer is a
        // sorted list, not a cursor (docs/ENGINEERING.md 11).
        let Some(b) = st.vmas.find_free(MMAP_BASE, MMAP_END, bytes) else {
            crate::println!(
                "linux: mmap of {bytes:#x} refused - no free span in the \
                 {MMAP_BASE:#x}..{MMAP_END:#x} mmap region ({} mappings live)",
                st.vmas.count()
            );
            return -ENOMEM;
        };
        b
    };

    // A file-backed mapping needs its own VFS handle, opened before the record goes
    // in: `ld.so` closes the caller's fd immediately after `mmap`, so keeping that
    // descriptor would leave the mapping pointing at a closed - and soon reused -
    // one (`linux::filemap`). Open first, so a failure refuses the call before the
    // list has been touched.
    let backing = if anon {
        None
    } else {
        if fd < 0 {
            return -EBADF;
        }
        let mut path = [0u8; 256];
        let Some(n) = st.fds.vfs_path(fd, &mut path) else {
            // Not a VFS file: there is nothing a fault could re-read.
            return -EBADF;
        };
        match filemap::open(path.as_ptr() as u64, n as u64) {
            Some(h) => Some(h),
            None => {
                crate::println!(
                    "linux: mmap of a file refused - the mapped-file table is full \
                     ({} entries) or the path could not be reopened",
                    crate::linux::filemap::MAX_MAPPED_FILES
                );
                return -ENOMEM;
            }
        }
    };

    // Record the mapping before touching pages, so a full table refuses the call
    // rather than leaving the list disagreeing with the page tables. `insert`
    // replaces whatever was recorded at `base`, which is what `MAP_FIXED` over an
    // `ld.so` reservation means.
    // A file mapping is backed for its whole length: a read past end of file
    // short-reads, which leaves the rest of the frame zero - the same answer Linux
    // gives. (An ELF *segment* is the case that is backed only part way; see
    // `Vma::file_len`.)
    let backing = backing.map(|h| crate::linux::vma::Backing {
        file: h,
        off: offset,
        len: bytes,
    });
    if !st.vmas.insert_backed(base, bytes, prot, flags, backing) {
        crate::println!(
            "linux: mmap of {bytes:#x} at {base:#x} refused - the per-cell VMA table \
             is full ({} records)",
            crate::linux::vma::MAX_VMAS
        );
        return -ENOMEM;
    }

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
        // **File-backed private mapping: demand-paged** (docs/ARCHITECTURE-DEBT.md
        // 4.0, blocker 2). Nothing is read and no frame is allocated here - the
        // record above is the whole mapping, and `fault` below fills each page from
        // the file the first time it is touched.
        //
        // This used to read every page eagerly. That is not a size problem to be
        // solved with a bigger pool: it is the wrong design at any size, because a
        // mapping's *cost* should follow what the program touches rather than what
        // it reserved. Measured on the binary this is aimed at, all three PT_LOADs
        // have `filesz == memsz` - there is no bss - so the whole image is
        // file-backed and this path is the one that decides whether it fits at all.
        //
        // MAP_FIXED over existing pages still has to drop them: ld.so overlays
        // segments onto a reservation it already made, and leaving the old frames
        // mapped would serve the previous mapping's bytes.
        if fixed {
            user::unmap_range(base, bytes);
        }
    }
    base as i64
}

/// Fill a missing page for a Linux cell, returning true if the instruction should
/// be **retried** (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
///
/// This is the demand-paging half of a resumable fault. The order of the checks is
/// the whole correctness argument:
///
/// 1. **Is there a mapping here?** No record means the address was never mapped -
///    a genuine SIGSEGV, and the commonest one (a null dereference).
/// 2. **Is the page already present?** If it is, this fault was a *permission*
///    refusal, not a missing page. Treating it as missing would repopulate and
///    re-fault forever, with no diagnostic - so the page tables are consulted
///    (`AddressSpace::is_mapped`) rather than guessed at from `FaultCause`, which
///    carries no read/write bit.
/// 3. **Does the mapping permit any access?** A `PROT_NONE` record is a *
///    reservation* - glibc reserves large arenas that way and commits sub-ranges
///    with `mprotect`. Populating one would hand out memory the program deliberately
///    made inaccessible.
///
/// Only then is the page filled: from the mapping's file if it has one, else zeroed.
/// The frame is charged to the cell exactly as an eager mapping was, so demand
/// paging changes *when* a page is paid for, never *whether*.
pub fn fault(st: &mut LinuxState, addr: usize) -> bool {
    let page = addr & !(FRAME_SIZE - 1);
    // (0) A **copy-on-write** write, asked before anything else and answered from the
    // page table rather than from the VMA list. That is deliberate: a fork shares the
    // stack and the `brk` heap too, and neither has a VMA record, so a COW test that
    // went through `st.vmas` would refuse the first stack write after every fork.
    if user::with_current_aspace(|aspace| aspace.cow_fault(page)) {
        return true;
    }
    let Some(m) = st.vmas.find(page) else {
        return false; // (1) nothing mapped here
    };
    // (2) present already => a permission fault, not a missing page.
    if user::with_current_aspace(|aspace| aspace.is_mapped(page)) {
        return false;
    }
    // (3) a bare reservation stays inaccessible.
    if m.prot & PROT_ANY == 0 {
        return false;
    }
    let Some(pa) = user::alloc_user_frame() else {
        // Out of frames is a refusal, not a panic (docs/ENGINEERING.md 12); the
        // caller turns it into the SIGSEGV the program would get from a Linux OOM.
        crate::println!("linux: page fault at {addr:#x} could not be filled - no frames");
        return false;
    };
    // A file-backed page is read through the kernel's linear map *before* the frame
    // is user-mapped, so the read cannot alias the cell's memory. A short read (past
    // end of file) leaves the tail zero, which is what the frame already is.
    if let Some((h, off, avail)) = st.vmas.file_at(page) {
        let kva = arch::phys_to_virt(pa) as u64;
        // `avail` bytes at most: an ELF segment's file content can end mid-page, and
        // reading the whole page would serve the *next* segment's bytes in the tail
        // instead of the zeros the program is entitled to.
        filemap::read_at(h, kva, avail as u64, off as i64);
    }
    user::with_current_aspace(|aspace| {
        aspace.map_user_frame(page, pa, perm_from_prot(m.prot));
    });
    bump_faults(page);
    true
}

/// Pages filled by [`fault`] since boot - the witness that demand paging is what
/// populated a mapping, rather than the eager path having done it earlier.
static mut FAULTS: u64 = 0;
/// Of those, the ones inside the `mmap` region.
///
/// Split out because the total stops being a usable oracle as soon as anything else is
/// demand-paged: a fixture proving "my 64-page mapping cost 4 pages" would otherwise
/// be counting the program's own text and its syscall buffers too. Which region a page
/// lies in is a property of the kernel's layout, so this stays evidence the cell
/// cannot fake.
static mut FAULTS_MMAP: u64 = 0;

fn bump_faults(page: usize) {
    // SAFETY: single CPU, synchronous trap.
    unsafe {
        let p = core::ptr::addr_of_mut!(FAULTS);
        *p = (*p).wrapping_add(1);
        if (MMAP_BASE..MMAP_END).contains(&page) {
            let m = core::ptr::addr_of_mut!(FAULTS_MMAP);
            *m = (*m).wrapping_add(1);
        }
    }
}

/// How many pages demand paging has filled, anywhere.
pub fn faults() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(FAULTS) }
}

/// How many of them were in the `mmap` region.
pub fn faults_mmap() -> u64 {
    // SAFETY: single CPU.
    unsafe { *core::ptr::addr_of!(FAULTS_MMAP) }
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
            st.vmas.remove(old_addr + new_len, old_len - new_len);
        }
        return old_addr as i64;
    }
    // Grow: only by moving (the region is a forward bump allocator).
    if flags & MREMAP_MAYMOVE == 0 {
        return -ENOMEM;
    }
    // Placed by the same first fit as `mmap`, over the same VMA list, so a grow
    // can land in a span something else freed (docs/ARCHITECTURE-DEBT.md 4,
    // blocker 2). The old range is still recorded here, so first fit will not
    // pick it - the copy below reads from it.
    let Some(base) = st.vmas.find_free(MMAP_BASE, MMAP_END, new_len) else {
        crate::println!("linux: mremap of {new_len:#x} refused - no free span in the mmap region");
        return -ENOMEM;
    };
    if !st
        .vmas
        .insert(base, new_len, PROT_READ | PROT_WRITE, MAP_ANONYMOUS)
    {
        return -ENOMEM;
    }
    if !user::map_anon_at(base, new_len, MapPerm::UserRw) {
        st.vmas.remove(base, new_len);
        return -ENOMEM;
    }
    // Copy the old contents; both ranges are mapped in the active cell root.
    // SAFETY: trap context, cell address space active; `old_len` bytes of the
    // source are mapped and `new_len >= old_len` bytes of the destination are.
    unsafe {
        core::ptr::copy_nonoverlapping(old_addr as *const u8, base as *mut u8, old_len);
    }
    user::unmap_range(old_addr, old_len);
    st.vmas.remove(old_addr, old_len);
    base as i64
}

/// munmap(addr, len): unmap the pages, return their frames to the pool, and
/// punch the range out of the VMA list so the span becomes reusable.
///
/// The list update is what makes a freed span *findable* again by first fit
/// (docs/ARCHITECTURE-DEBT.md 4, blocker 2). A partial unmap in the middle of a
/// mapping splits its record in two, so nothing is left claiming to own a hole.
pub fn munmap(st: &mut LinuxState, addr: u64, len: u64) -> i64 {
    let base = (addr as usize) & !(FRAME_SIZE - 1);
    let bytes = page_up(len as usize);
    user::unmap_range(base, bytes);
    st.vmas.remove(base, bytes);
    0
}

/// mprotect(addr, len, prot): change page permissions. Making a reserved
/// (uncommitted) range accessible commits fresh frames for it (the glibc
/// arena/stack growth path, docs/LINUX-COMPAT.md L4); PROT_NONE decommits the
/// range, returning its frames to the pool.
pub fn mprotect(st: &mut LinuxState, addr: u64, len: u64, prot: u64) -> i64 {
    if wx_refused(prot, "mprotect") {
        return -EPERM;
    }
    let base = (addr as usize) & !(FRAME_SIZE - 1);
    let bytes = page_up(len as usize);
    if prot & PROT_ANY == 0 {
        user::unmap_range(base, bytes);
    } else if !user::commit_range(base, bytes, perm_from_prot(prot)) {
        return -ENOMEM;
    }
    // The pages keep their record either way - PROT_NONE is still a *reservation*
    // this cell owns, which is exactly why `mprotect`ing it back to RW must find
    // it and not have first fit hand the span to something else. Only `munmap`
    // releases a span.
    st.vmas.set_prot(base, bytes, prot);
    0
}
