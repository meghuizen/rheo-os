//! **The one place kernel code touches a cell's memory.**
//!
//! ## Why this module exists
//!
//! A cell hands the kernel raw virtual addresses - a `write` buffer, a `stat` out-
//! parameter, an `iovec` array. Servicing that means the kernel dereferences an
//! address the *cell* chose, in kernel mode, through the cell's own page tables. Two
//! separate obligations follow, and conflating them has cost real defects:
//!
//! 1. **Is the kernel allowed to address this?** A pure bounds question
//!    (docs/ENGINEERING.md 12): null, alignment, overflow, and a range test against
//!    `USER_VA_MAX` plus the shared `.user` window. [`readable`]/[`writable`].
//! 2. **Is the mapping ready for what the kernel is about to do?** Lazy mapping makes
//!    this a moving target. Demand paging made a page's *presence* lazy; copy-on-write
//!    `fork` makes its *writability* lazy on top of that - a page can be present and
//!    still not writable. A fault taken in **kernel** mode is not resumable here, so
//!    every strength has to be resolved *before* the access, which is exactly why
//!    Linux has `copy_from_user`/`copy_to_user` and a fault fixup table.
//!
//! Keeping (1) and (2) separate is load-bearing and was learned the expensive way:
//! folding presence into the bare range predicates cost a measured **~2,900x**
//! amplification, because `unmap_range` calls them purely to *bound* a range and so
//! materialised every page immediately before freeing it.
//!
//! ## The shape, and why it is a module rather than a few helpers
//!
//! Before this module, 47 sites went through validating helpers and **51
//! dereferenced the raw user VA** with only a bounds check done in some other
//! function. Every new lazy-mapping feature therefore re-opened a 98-site audit, and
//! half the sites had no guard to extend. The symptom was a run of one-site fixes -
//! the last being `linux::proc::reap` storing a wait status onto the parent's stack,
//! which is copy-on-write after a fork.
//!
//! So this module offers three tiers, and the third is the one that did not exist:
//!
//! - **Bounds only** - [`readable`], [`writable`]. For callers that need the limit of
//!   a range and will not touch it.
//! - **Resolve and hand back a pointer** - [`out_ptr`], [`in_ptr`], [`buf`],
//!   [`buf_mut`], [`slice`], [`read_span`]. For bulk work that then does its own
//!   copying, and for forwarding a buffer to a `svc::FileOps` handler.
//! - **Resolve and perform the access** - [`read`], [`write`], [`read_unaligned`],
//!   [`write_unaligned`], [`copy_in`], [`copy_out`], [`fill`]. A site written this way
//!   *cannot* forget to resolve, because resolution and access are one call.
//!
//! New lazy-mapping work - the grow-on-fault stack, later swap or shared file
//! mappings - changes [`present_prefix`] here and nothing else.
//!
//! ## Scope (honest)
//!
//! This is pre-resolution, not a fixup table: the kernel makes the mapping ready and
//! then accesses it, rather than taking the fault and recovering. That works because
//! the kernel is the only thing running (single CPU, synchronous traps) so nothing can
//! un-ready the mapping in between. It would not survive preemption on another core;
//! SMP (task #27) is where a real fixup path becomes necessary.

use crate::mm::frames;
use crate::user::{
    Personality, USER_VA_MAX, cell_personality, cell_present, current_cell, with_current_aspace,
};

/// Pages the kernel has faulted in on a cell's behalf.
static mut PREFAULTS: u64 = 0;

/// How many pages [`ensure_present`] has filled since boot - the witness that the
/// pre-fault path is what made a buffer usable, and the number to watch when judging
/// its cost (docs/ENGINEERING.md 1).
pub fn prefaults() -> u64 {
    // SAFETY: single CPU, synchronous traps.
    unsafe { *core::ptr::addr_of!(PREFAULTS) }
}

/// Make every page of `[va, va+len)` **present** in the calling cell, filling any
/// that demand paging has not materialised yet. `false` if a page is not there and
/// cannot be made so - the caller then refuses the syscall rather than touching it.
///
/// ## Why this exists
///
/// Demand paging (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2) makes a page's presence
/// lazy, and that turns **every kernel access to a user buffer into a fault site**. A
/// user-mode fault is resumable here - the handler fills the page and the instruction
/// re-executes - but a fault taken in *kernel* mode is not. A cell that hands `write`
/// a pointer into a page it has never touched itself would take the kernel down,
/// which is exactly why Linux has `copy_from_user` and a fixup table.
///
/// ## Why it is *here* and not in `user_read_ok`/`user_write_ok`
///
/// This distinction is load-bearing and was learned the expensive way. Putting the
/// presence guard in the bare range checks looked equivalent and cost a **~2,900x**
/// amplification: `unmap_range` calls `user_write_ok` purely to *bound* a range, so
/// every `munmap` and every `MAP_FIXED` overlay materialised each page in the range
/// immediately before freeing it - 11,516 of 11,520 demand fills in one test run came
/// from the kernel, against 4 from the program.
///
/// So presence belongs on the helpers that hand back something to **dereference**
/// (`user_out`, `user_in`, `user_buf`, `user_buf_mut`, `user_slice`,
/// `user_read_span`), and the bare range checks stay pure predicates for callers that
/// only need the bound. "Am I allowed to address this?" and "am I about to touch it?"
/// are different questions.
///
/// ## Readable is not writable
///
/// Once a COW `fork` shares pages, a page can be **present and still not writable** -
/// and a kernel store to one faults at a kernel PC exactly as an absent page does. So
/// the guard has two strengths: `Access::Read` only fills, `Access::Write` also
/// resolves copy-on-write. The write helpers pass `Write`; nothing else has to know.
#[inline]
fn ensure_present(va: u64, len: usize, how: Access) -> bool {
    present_prefix(va, len, how) == len
}

/// What the kernel is about to do with a user range - and therefore how far the
/// mapping has to be brought along before it does it.
#[derive(Copy, Clone, PartialEq)]
enum Access {
    Read,
    Write,
}

/// The number of leading bytes of `[va, va+len)` that are usable for `how` after
/// filling (and, for a write, un-sharing) what can be. `len` means the whole range.
fn present_prefix(va: u64, len: usize, how: Access) -> usize {
    if len == 0 || va == 0 {
        return 0;
    }
    let cur = current_cell();
    // Native cells map eagerly: nothing to fill, and their cost stays exactly zero.
    if !cell_present(cur) || cell_personality(cur) != Personality::Linux {
        return len;
    }
    const PAGE: u64 = frames::FRAME_SIZE as u64;
    let first = va & !(PAGE - 1);
    let Some(last_byte) = va.checked_add(len as u64 - 1) else {
        return 0;
    };
    let last = last_byte & !(PAGE - 1);
    let mut page = first;
    while page <= last {
        // The common case by far: already there, one page-table probe.
        // A write to a shared copy-on-write page has to private it first, or the
        // kernel's own store takes an unresumable fault at a kernel PC. Checked before
        // presence because a COW page *is* present, so the presence probe below would
        // pass it straight through.
        if how == Access::Write
            && with_current_aspace(|aspace| aspace.is_cow(page as usize))
            && !with_current_aspace(|aspace| aspace.cow_fault(page as usize))
        {
            return page.saturating_sub(va) as usize;
        }
        if !with_current_aspace(|aspace| aspace.is_mapped(page as usize)) {
            if !crate::linux::fill_fault(cur, page as usize) {
                // Not fillable: report the prefix that is. A caller needing the whole
                // range refuses; a bounded string scan simply stops here.
                return page.saturating_sub(va) as usize;
            }
            // SAFETY: single CPU, synchronous trap.
            unsafe {
                let p = core::ptr::addr_of_mut!(PREFAULTS);
                *p = (*p).wrapping_add(1);
            }
        }
        page += PAGE;
    }
    len
}

/// Whether `[va, va+len)` is an address range the kernel may **write** on this
/// cell's behalf: non-null, no overflow, and either wholly inside the low-half
/// user range (`< `[`USER_VA_MAX`]) or wholly inside the writable part of the
/// shared `.user` window.
///
/// The window exception exists because the hand-written U-mode programs
/// (`kernel/src/user_progs.rs` - the `lsh` shell, the benchmark workers, the
/// isolation prober) run from the linker's 2 MiB `.user` span, which on
/// riscv64 and x86-64 is linked *high*, beside the kernel (kernel/link/*.ld),
/// yet is genuinely mapped U into every cell root. Only `[__user_data_start,
/// __user_end)` - the per-cell `.user.data`/`.user.bss` pages - is accepted for
/// a write: `.user.text`/`.user.rodata` are shared read-only by every cell, so
/// letting the kernel write there on one cell's request would corrupt all of
/// them.
///
/// An **empty** range (`len == 0`) is accepted whatever `va` is: nothing is
/// dereferenced, and POSIX callers legitimately pass `read(fd, NULL, 0)`. This
/// is the same rule as Linux's `access_ok`.
#[inline]
pub fn writable(va: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if va == 0 {
        return false;
    }
    let Some(end) = va.checked_add(len as u64) else {
        return false;
    };
    if end <= USER_VA_MAX {
        return true;
    }
    // Link-time constants, not memory loads; reached only for a `.user`-window
    // cell, never on a loaded cell's hot path.
    let data_start = crate::mm::user_rodata_range().1 as u64;
    let window_end = crate::mm::user_window().1 as u64;
    va >= data_start && end <= window_end
}

/// Whether `[va, va+len)` is an address range the kernel may **read** on this
/// cell's behalf. As [`user_write_ok`], but the whole `.user` window is
/// accepted (a U-mode program legitimately passes `.user.rodata` string
/// constants to `SYS_DEBUG_WRITE`).
#[inline]
pub fn readable(va: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if va == 0 {
        return false;
    }
    let Some(end) = va.checked_add(len as u64) else {
        return false;
    };
    if end <= USER_VA_MAX {
        return true;
    }
    let (window_start, window_end) = crate::mm::user_window();
    va >= window_start as u64 && end <= window_end as u64
}

/// Validate a cell-supplied out-parameter address for a `T`-sized, `T`-aligned
/// **write**, returning the pointer to write through. `None` = refuse the
/// syscall (never write, never panic).
#[inline]
pub fn out_ptr<T>(va: u64) -> Option<*mut T> {
    if !va.is_multiple_of(core::mem::align_of::<T>() as u64) {
        return None;
    }
    if !writable(va, core::mem::size_of::<T>()) {
        return None;
    }
    if !ensure_present(va, core::mem::size_of::<T>(), Access::Write) {
        return None;
    }
    Some(va as *mut T)
}

/// Validate a cell-supplied in-parameter address for a `T`-sized, `T`-aligned
/// **read**.
#[inline]
pub fn in_ptr<T>(va: u64) -> Option<*const T> {
    if !va.is_multiple_of(core::mem::align_of::<T>() as u64) {
        return None;
    }
    if !readable(va, core::mem::size_of::<T>()) {
        return None;
    }
    if !ensure_present(va, core::mem::size_of::<T>(), Access::Read) {
        return None;
    }
    Some(va as *const T)
}

/// Validate a cell-supplied **readable** byte buffer of `len` bytes, returning
/// the address unchanged. No alignment requirement (a byte buffer, or a
/// structure read with `read_unaligned`).
#[inline]
pub fn buf(va: u64, len: usize) -> Option<u64> {
    if readable(va, len) && ensure_present(va, len, Access::Read) {
        Some(va)
    } else {
        None
    }
}

/// Validate a cell-supplied **writable** byte buffer of `len` bytes.
#[inline]
pub fn buf_mut(va: u64, len: usize) -> Option<u64> {
    if writable(va, len) && ensure_present(va, len, Access::Write) {
        Some(va)
    } else {
        None
    }
}

/// The largest `n <= max` for which `[va, va+n)` is a **readable** range in the
/// calling cell (0 if `va` is in no such range).
///
/// For the one argument shape whose length the caller never states - a
/// NUL-terminated string - so the scan for its terminator carries a bound and
/// cannot walk out of the cell's range (docs/ENGINEERING.md 12).
#[inline]
pub fn read_span(va: u64, max: usize) -> usize {
    if va == 0 {
        return 0;
    }
    if va < USER_VA_MAX {
        // Bound by the range, then by what is actually there: a scan must not walk
        // into a page demand paging cannot fill.
        let span = ((USER_VA_MAX - va) as usize).min(max);
        return present_prefix(va, span, Access::Read);
    }
    let (window_start, window_end) = crate::mm::user_window();
    if va >= window_start as u64 && va < window_end as u64 {
        return ((window_end as u64 - va) as usize).min(max);
    }
    0
}

/// A validated writable byte slice over `[va, va+len)` in the calling cell.
///
/// # Safety
/// The caller must be servicing that cell's synchronous trap (so the range is
/// mapped and no other reference to it is live).
#[inline]
pub unsafe fn slice(va: u64, len: usize) -> Option<&'static mut [u8]> {
    buf_mut(va, len)?;
    // SAFETY: range validated above; the caller guarantees the trap context.
    Some(unsafe { core::slice::from_raw_parts_mut(va as *mut u8, len) })
}

// ------------------------------------------------------- resolve and access
//
// The tier that did not exist. Each of these resolves the mapping to the strength the
// operation needs and *then* performs it, so a caller cannot express the access
// without the resolution - which is what the 51 raw dereferences this module replaced
// each got wrong in their own way.

/// Read a `T` out of the cell at `va` (aligned). `None` if the range is not readable.
pub fn read<T: Copy>(va: u64) -> Option<T> {
    let p = in_ptr::<T>(va)?;
    // SAFETY: `in_ptr` checked alignment, bounds and presence in the active cell.
    Some(unsafe { p.read() })
}

/// Write a `T` into the cell at `va` (aligned). False if the range is not writable.
pub fn write<T: Copy>(va: u64, val: T) -> bool {
    match out_ptr::<T>(va) {
        // SAFETY: `out_ptr` checked alignment, bounds, presence and writability.
        Some(p) => {
            unsafe { p.write(val) };
            true
        }
        None => false,
    }
}

/// [`read`] without the alignment requirement - for the ABI shapes that genuinely do
/// not promise it (an `eventfd` counter buffer, a `pollfd` field at an odd offset).
/// Refusing those with `-EFAULT` would be a bug of our own making.
pub fn read_unaligned<T: Copy>(va: u64) -> Option<T> {
    buf(va, core::mem::size_of::<T>())?;
    // SAFETY: `buf` checked bounds and presence; the read is explicitly unaligned.
    Some(unsafe { (va as *const T).read_unaligned() })
}

/// [`write`] without the alignment requirement.
pub fn write_unaligned<T: Copy>(va: u64, val: T) -> bool {
    if buf_mut(va, core::mem::size_of::<T>()).is_none() {
        return false;
    }
    // SAFETY: `buf_mut` checked bounds, presence and writability; write is unaligned.
    unsafe { (va as *mut T).write_unaligned(val) };
    true
}

/// Copy `dst.len()` bytes **from** the cell at `va` into kernel memory.
pub fn copy_in(dst: &mut [u8], va: u64) -> bool {
    if dst.is_empty() {
        return true;
    }
    if buf(va, dst.len()).is_none() {
        return false;
    }
    // SAFETY: `buf` checked bounds and presence for the whole length; `dst` is kernel
    // memory the caller owns, and the two cannot overlap (one is a user VA below
    // `USER_VA_MAX` or in the `.user` window, the other a kernel object).
    unsafe { core::ptr::copy_nonoverlapping(va as *const u8, dst.as_mut_ptr(), dst.len()) };
    true
}

/// Copy `src` **into** the cell at `va`.
pub fn copy_out(va: u64, src: &[u8]) -> bool {
    if src.is_empty() {
        return true;
    }
    if buf_mut(va, src.len()).is_none() {
        return false;
    }
    // SAFETY: as `copy_in`, with writability also resolved.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), va as *mut u8, src.len()) };
    true
}

/// Set `len` bytes at `va` in the cell to `byte` - the `memset` half, for the
/// out-parameters an ABI says to zero.
pub fn fill(va: u64, byte: u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if buf_mut(va, len).is_none() {
        return false;
    }
    // SAFETY: `buf_mut` checked bounds, presence and writability for `len` bytes.
    unsafe { core::ptr::write_bytes(va as *mut u8, byte, len) };
    true
}
