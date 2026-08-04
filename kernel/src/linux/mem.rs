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
use crate::linux::vma;
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
/// `MAP_NORESERVE`: do not commit (reserve) backing store up front - the mapping
/// is demand-zero-filled on first touch. JavaScriptCore's Gigacage reserves a
/// single **128 GiB** `MAP_NORESERVE` region this way (GOAL-BUN); committing it
/// eagerly would try to allocate ~33M frames and fail.
const MAP_NORESERVE: u64 = 0x4000;

/// Base of the per-cell anonymous mmap region: **80 GiB**, in the large free span
/// **above** every fixed region - the image (4 GiB), stack (8 GiB), queue-pair
/// (16 GiB), channels (24 GiB) and the ELF interpreter at
/// [`load::LINUX_INTERP_BASE`] (64 GiB, `ld.so` + its libraries) all sit below it.
/// Each cell has its own address space, so the same VA in two cells is isolated.
///
/// It used to be 12 GiB, boxed into the 4 GiB gap below the queue region. That is
/// enough for glibc/V8 arenas but **not** for JavaScriptCore's Gigacage: Bun
/// reserves a single **128 GiB** `PROT_NONE` region (plus 8 + 4 GiB) up front, and
/// no 4 GiB window can hold it. The reservations are demand-committed (a 128 GiB
/// cage costs one VMA record and zero frames until touched), so the fix is purely
/// address space: place the region high, where 172 GiB is free below
/// [`crate::user::USER_VA_MAX`] (docs/LINUX-COMPAT.md, GOAL-BUN).
const MMAP_BASE: usize = 0x14_0000_0000;

/// **End** of the per-cell mmap region - the first address a mapping may not reach.
/// Four GiB below [`crate::user::USER_VA_MAX`], which is now **each ISA's own** user
/// half rather than the RISC-V Sv39 floor imposed on all three: 252 GiB on riscv64
/// (unchanged), ~128 TiB on x86-64, ~256 TiB on ARM64. Placement is a first-fit VMA
/// search in `[MMAP_BASE, MMAP_END)`; past it `mmap` reports `-ENOMEM`, an answer a
/// caller can act on.
///
/// The 4 GiB of headroom is deliberate: the F1 pointer bounds check refuses a span
/// that *reaches* `USER_VA_MAX`, so a mapping placed hard against it could not then
/// be read or written through a syscall argument. Leaving the gap means every
/// address this window hands out is one the kernel can also accept back.
const MMAP_END: usize = crate::user::USER_VA_MAX as usize - 0x1_0000_0000;

/// The window `[base, end)` a cell's anonymous `mmap` is placed in.
///
/// Exposed so a test can compute the oracle for "how large a reservation fits" from
/// the same two numbers the placement uses, rather than restating them. The answer
/// is per-ISA now, so a hardcoded one in a fixture would be wrong on two of three.
pub fn mmap_window() -> (usize, usize) {
    (MMAP_BASE, MMAP_END)
}
const _: () = assert!(MMAP_BASE < MMAP_END);
const _: () = assert!(MMAP_END as u64 <= crate::user::USER_VA_MAX);
// Above every fixed region, so the window and the queue/channel/interp cannot alias.
const _: () = assert!(MMAP_BASE as u64 > crate::load::LINUX_INTERP_BASE as u64);

/// Spans a cell's `mmap` must never place a mapping over, because the kernel owns
/// them. Checked for `MAP_FIXED` (a caller-chosen address), which placement cannot
/// protect against on its own.
///
/// Delegated to the cell's **recorded** layout (`user::kernel_owned_overlap`)
/// rather than restated here as constants. The refusals are the same two today - a
/// Linux cell holds nothing else kernel-owned - so this is a change of authority,
/// not of behaviour: one place decides what the kernel owns, and it decides from a
/// record instead of from a copy of `load.rs`'s constants that has to be kept in
/// step by hand.
fn reserved_overlap(base: usize, bytes: usize) -> Option<&'static str> {
    crate::user::kernel_owned_overlap(crate::user::current_cell(), base, bytes)
}

fn page_up(x: usize) -> usize {
    (x + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

/// The base of the per-cell mmap region, for callers that need to name it.
pub const fn mmap_base() -> usize {
    MMAP_BASE
}

/// Register an anonymous read-write **reservation** the fault handler fills on first
/// touch - the grow-on-fault stack (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
///
/// Unlike a `PROT_NONE` reservation this is immediately fillable (a stack page a
/// program touches must appear); unlike an eager mapping it costs nothing until
/// touched. Only the top page is mapped eagerly by `stack::setup_stack`; this record
/// is what lets `fault` fill the rest, and its lower bound is what makes an overflow a
/// SIGSEGV rather than silent growth into whatever lies below.
pub fn reserve_stack(st: &mut LinuxState, base: usize, bytes: usize) {
    st.vmas
        .insert(base, bytes, PROT_READ | PROT_WRITE, MAP_ANONYMOUS);
}

/// Map mmap `prot` bits onto a `MapPerm`. PROT_EXEC without PROT_WRITE is
/// executable-read; PROT_WRITE is read-write; both together is the
/// capability-gated `UserRwx` (docs/ARCHITECTURE.md 5.1); anything else
/// (PROT_READ, PROT_NONE) is read-only.
///
/// Callers must have run [`wx_refused`] first, which is what verifies the calling
/// cell's authority for the `UserRwx` case. This function only translates bits; it
/// checks no capability, and a caller that skips the gate would get an RWX mapping,
/// which is why the gate is a separate, loudly-named function that both call sites
/// invoke on the line before.
fn perm_from_prot(prot: u64) -> MapPerm {
    if prot & PROT_EXEC != 0 && prot & PROT_WRITE == 0 {
        MapPerm::UserRx
    } else if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        MapPerm::UserRwx
    } else if prot & PROT_WRITE != 0 {
        MapPerm::UserRw
    } else {
        MapPerm::UserRo
    }
}

/// Whether the calling cell holds the **W^X exception authority**: a `MemoryGrant`
/// capability carrying `RIGHT_WRITE | RIGHT_EXECUTE` (docs/ARCHITECTURE.md 5.1).
///
/// The `SYS_SPAWN` precedent exactly - "the caller must hold an `ObjectKind::Cell`
/// capability with WRITE, no ambient authority" (docs/LIBRHEO.md Phase F) - applied
/// to a different object and a different pair of rights. Nothing new is invented:
/// "may hold memory that is simultaneously writable and executable" is already
/// expressible in the rights vocabulary, and this reads it.
///
/// A cell with no capability table (a bare test cell) holds nothing, so it is
/// refused: absence of a table is absence of authority, never a bypass.
fn holds_wx_authority(cell: usize) -> bool {
    let caps = user::cell_caps(cell);
    let objs = user::cell_objects(cell);
    if caps.is_null() || objs.is_null() {
        return false;
    }
    // SAFETY: single CPU, synchronous trap; the cell's tables are uniquely owned for
    // the duration of the trap, exactly as `nproc::spawn` reads them.
    unsafe {
        (*caps).holds(
            &*objs,
            crate::capability::ObjectKind::MemoryGrant,
            crate::capability::WRITE | crate::capability::EXECUTE,
        )
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
fn wx_refused(cell: usize, prot: u64, what: &str) -> bool {
    if prot & PROT_WRITE == 0 || prot & PROT_EXEC == 0 {
        return false;
    }
    if holds_wx_authority(cell) {
        // Granted, and said out loud once per call: a cell running with the W^X
        // exception is the one thing about this kernel's memory model an operator
        // would want to see in a log, and a silently-granted exception is
        // indistinguishable from a missing check.
        crate::println!(
            "linux: {what} PROT_WRITE|PROT_EXEC granted - cell {cell} holds the W^X \
             exception capability (MemoryGrant + WRITE|EXECUTE, \
             docs/ARCHITECTURE.md 5.1)"
        );
        return false;
    }
    crate::println!(
        "linux: {what} PROT_WRITE|PROT_EXEC refused - cell {cell} holds no W^X \
         exception capability (MemoryGrant + WRITE|EXECUTE). W^X is the default; \
         use mprotect RW then RX, or be granted the authority"
    );
    true
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
    if wx_refused(user::current_index(), prot, "mmap") {
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
                    "linux: mmap of a file refused - the mapped-file registry could \
                     not be funded ({} of {} entries in use) or the path could not \
                     be reopened",
                    filemap::in_use(),
                    filemap::slots()
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
        // The table grows on demand now (docs/SUBSTRATE.md pillar 1), so this is a
        // *resource* refusal - the cell's frame budget, or the pool's metadata
        // reserve - and the diagnostic reports the table's real size rather than a
        // constant that no longer bounds anything.
        crate::println!(
            "linux: mmap of {bytes:#x} at {base:#x} refused - no funded VMA slot \
             ({} records live in {} slots, {} frames)",
            st.vmas.count(),
            st.vmas.slots(),
            st.vmas.frames_held()
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
        // A `MAP_NORESERVE` mapping reserves address space that `fault` demand-zero-
        // fills on first touch (the record's prot has PROT_ANY, so `fault` fills it,
        // unlike a PROT_NONE reservation which stays inaccessible). Committing every
        // page here instead is what made JSC's 128 GiB Gigacage try to allocate ~33M
        // frames, fail, and - the real defect - leave a phantom VMA occupying the low
        // window so every later mapping was pushed high or refused (GOAL-BUN). This is
        // also what Linux does for anonymous memory; only the eager path is narrowed.
        let lazy = flags & MAP_NORESERVE != 0;
        if prot & PROT_ANY != 0 && !lazy && !user::map_anon_at(base, bytes, perm_from_prot(prot)) {
            // A failed eager commit must not leave the record behind, or the span is
            // lost until the cell exits (the leak the Gigacage exposed).
            st.vmas.remove(base, bytes);
            return -ENOMEM;
        }
        // PROT_NONE (or MAP_NORESERVE): leave it a bare reservation (no frames).
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
        // A copy-on-write private, which is a `Transfer` rather than an `Acquire`: the
        // page changed hands between two cells that were sharing it. Recording it apart
        // from a fill is what lets "this process is paying for its parent's pages" and
        // "this process is touching new ones" be different answers.
        crate::obs_event!(
            crate::obs::Window::Mem,
            crate::obs::Kind::Transfer,
            user::current_index() as u16,
            page as u64,
            0
        );
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
    // Asked of the record `find` already returned, not of the list again - the
    // list-level lookup made every file-backed fault walk the whole table twice.
    if let Some((h, off, avail)) = m.file_page(page) {
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
    // The memory window (docs/OBSERVABILITY.md 11.4): a page was filled on demand.
    // Here rather than at `fault`'s several exits, because this function *is* "the fill
    // happened" - a frame was taken and a mapping now exists, which is what makes this
    // an `Acquire` with a resource attached rather than a note.
    crate::obs_event!(
        crate::obs::Window::Mem,
        crate::obs::Kind::Acquire,
        crate::user::current_index() as u16,
        page as u64,
        1
    );
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

// --------------------------------------------------------------------- madvise
//
// `madvise` advice values (asm-generic; identical on x86-64).

/// No advice.
const MADV_NORMAL: u64 = 0;
/// Access will be random / sequential / is expected soon. Genuinely advisory
/// here: there is no read-ahead machinery to inform, so these are accepted and
/// do nothing - which is what "advisory" means and is not a stub.
const MADV_RANDOM: u64 = 1;
const MADV_SEQUENTIAL: u64 = 2;
const MADV_WILLNEED: u64 = 3;
/// The caller is done with these pages: **free them now**.
const MADV_DONTNEED: u64 = 4;
/// Punch a hole in the backing store. Needs a writable file mapping this kernel
/// does not have.
const MADV_REMOVE: u64 = 9;
/// Do not inherit / do inherit this range across `fork`.
const MADV_DONTFORK: u64 = 10;
const MADV_DOFORK: u64 = 11;
/// The caller is done with these pages, but the kernel may free them lazily.
const MADV_FREE: u64 = 8;
/// Transparent-hugepage hints. Accepted and advisory: page size is chosen by the
/// mapping's own alignment here, not by a hint.
const MADV_HUGEPAGE: u64 = 14;
const MADV_NOHUGEPAGE: u64 = 15;
/// Zero this range in the child on `fork` / stop doing so.
/// `MADV_DONTDUMP` / `MADV_DODUMP`: exclude a range from, or include it in, a core
/// dump. Accepted as genuinely advisory - this OS produces no core dumps at all, so
/// the request's whole observable effect is provided vacuously, which is a different
/// thing from dropping a request that has one. Refusing them was the previous
/// behaviour and it is what a runtime marking its large reservations (JSC marks the
/// Gigacage `MADV_DONTDUMP`, which is the sane thing to do with 128 GiB of mostly
/// untouched address space) sees as an unexplained failure.
const MADV_DONTDUMP: u64 = 16;
const MADV_DODUMP: u64 = 17;
const MADV_WIPEONFORK: u64 = 18;
const MADV_KEEPONFORK: u64 = 19;

/// madvise(addr, len, advice).
///
/// This used to be `Ctl::Ret(0)` with the comment "advisory by specification",
/// which is true of *some* advice values and false of the ones that matter. Two
/// of them are requests for action, and reporting success without acting is the
/// docs/ENGINEERING.md 7 shape - a stub that claims to have done something:
///
/// - **`MADV_DONTNEED`/`MADV_FREE`** is how every serious allocator returns
///   memory. V8 and JavaScriptCore trim their heaps with it; glibc's malloc uses
///   it on arena teardown. Answering 0 without freeing means a long-running
///   program's resident set only ever grows, and the program has no way to tell -
///   it asked, and was told yes. With demand paging landed, honouring it is
///   cheap: drop the frames and let the next touch re-fault.
/// - **`MADV_WIPEONFORK`** is a *security* request. A userspace CSPRNG that is
///   `fork`ed without it produces identical streams in parent and child
///   (docs/SUBSTRATE.md 10a); OpenSSL asks for this precisely so that cannot
///   happen. Accepting and ignoring it leaves the caller believing it is safe.
///
/// Everything genuinely advisory is accepted and does nothing, and everything
/// unimplemented is **refused with a reason** rather than absorbed - the
/// `fcntl`/`sched_setscheduler` discipline.
pub fn madvise(st: &mut LinuxState, addr: u64, len: u64, advice: u64) -> i64 {
    let base = (addr as usize) & !(FRAME_SIZE - 1);
    let bytes = page_up(len as usize);
    if bytes == 0 {
        return 0;
    }
    // Linux requires the range to be mapped for the advice values that act on
    // pages; an unmapped range is ENOMEM.
    let mapped = st.vmas.overlapping(base, bytes).next().is_some();

    match advice {
        // Genuinely advisory: no read-ahead or page-size machinery to inform.
        MADV_NORMAL | MADV_RANDOM | MADV_SEQUENTIAL | MADV_WILLNEED | MADV_HUGEPAGE
        | MADV_NOHUGEPAGE | MADV_DONTDUMP | MADV_DODUMP => 0,

        // Free the pages now. The mapping's *record* stays - this is a decommit,
        // not an unmap, so the next touch re-faults (anonymous pages come back
        // zeroed, file-backed pages come back from the file). That is exactly the
        // `mprotect(PROT_NONE)` decommit path, which is why it reuses it.
        MADV_DONTNEED | MADV_FREE => {
            if !mapped {
                return -ENOMEM;
            }
            user::unmap_range(base, bytes);
            0
        }

        // Zero in the child on fork. Recorded per range; applied by `proc::fork`.
        MADV_WIPEONFORK => {
            if !mapped {
                return -ENOMEM;
            }
            st.vmas
                .set_advice(base, bytes, vma::ADV_WIPEONFORK, vma::ADV_DONTFORK);
            0
        }
        MADV_KEEPONFORK => {
            if !mapped {
                return -ENOMEM;
            }
            st.vmas.set_advice(base, bytes, 0, vma::ADV_WIPEONFORK);
            0
        }

        // Do not inherit across fork. Refused when it would have to be applied to
        // more than the caller asked for: widening this one is observable in the
        // child as a mapping that should exist and does not, so an honest refusal
        // beats a silent over-application (see `VmaList::set_advice`).
        MADV_DONTFORK => {
            if !mapped {
                return -ENOMEM;
            }
            if st.vmas.advice_would_widen(base, bytes) {
                crate::println!(
                    "linux: madvise(MADV_DONTFORK) refused for {base:#x}..{:#x} - it \
                     covers part of a larger mapping, and this VMA list records advice \
                     per mapping, so honouring it would withhold pages the caller did \
                     not name (docs/SUBSTRATE.md 10a)",
                    base + bytes
                );
                return -EINVAL;
            }
            st.vmas
                .set_advice(base, bytes, vma::ADV_DONTFORK, vma::ADV_WIPEONFORK);
            0
        }
        MADV_DOFORK => {
            if !mapped {
                return -ENOMEM;
            }
            st.vmas.set_advice(base, bytes, 0, vma::ADV_DONTFORK);
            0
        }

        // Punching a hole in the backing store needs a writable file mapping,
        // which this kernel's file mappings are not (MAP_PRIVATE only).
        MADV_REMOVE => {
            crate::println!(
                "linux: madvise(MADV_REMOVE) unsupported - file mappings here are \
                 MAP_PRIVATE, so there is no shared backing store to punch"
            );
            -EOPNOTSUPP
        }

        other => {
            crate::println!("linux: madvise advice {other} not implemented - refused EINVAL");
            -EINVAL
        }
    }
}

/// mprotect(addr, len, prot): change page permissions. Making a reserved
/// (uncommitted) range accessible commits fresh frames for it (the glibc
/// arena/stack growth path, docs/LINUX-COMPAT.md L4); PROT_NONE decommits the
/// range, returning its frames to the pool.
pub fn mprotect(st: &mut LinuxState, addr: u64, len: u64, prot: u64) -> i64 {
    if wx_refused(user::current_index(), prot, "mprotect") {
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
