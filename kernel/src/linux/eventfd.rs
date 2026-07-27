//! `eventfd2` for the Linux personality (docs/LINUX-COMPAT.md L8-EVENTFD).
//!
//! An eventfd is a 64-bit counter you can `read` and `write` and, crucially,
//! `poll`. It is how an epoll event loop wakes **itself**: the loop parks in
//! `epoll_wait` on a set that includes the eventfd, and any other context that
//! wants the loop to run again writes 1 to it. Bun's JavaScriptCore event loop
//! does exactly this, which is the one syscall of the seven measured in
//! docs/ARCHITECTURE-DEBT.md 4.0 that is **load-bearing** rather than advisory:
//! refusing it does not degrade the program, it removes its only wakeup path.
//!
//! It adds **no kernel object**. An eventfd is a per-cell fd
//! (`fd::FdKind::EventFd`) indexing this per-personality registry, exactly as an
//! epoll instance is (`linux::epoll`) and a pipe is (`linux::pipe`). The counter
//! lives **here** rather than inside the `FdKind` variant for a reason: `dup` and
//! `fork` produce a second descriptor for the *same* object, and a counter copied
//! into each descriptor would give two independent counters that silently stop
//! waking each other (docs/ENGINEERING.md 11, "two encodings that are usually
//! equal"). Sharing is what an eventfd is for, so the shared state is the object.
//!
//! ## Semantics implemented (Linux `fs/eventfd.c`)
//! - `write` adds an 8-byte host-endian `u64` to the counter, refusing
//!   `0xffff_ffff_ffff_ffff` (`-EINVAL`) and refusing to overflow (`-EAGAIN`,
//!   the counter saturates at `u64::MAX - 1`).
//! - `read` in **counter** mode returns the whole counter and zeroes it; in
//!   **semaphore** mode (`EFD_SEMAPHORE`) it returns 1 and decrements by 1.
//! - A zero counter is not readable: a blocking `read` parks, a non-blocking one
//!   reports `-EAGAIN`.
//! - `POLLIN` when the counter is non-zero; `POLLOUT` while a write of 1 would
//!   fit (always, short of saturation) - the readiness `epoll`/`poll` need.
//! - Reads and writes shorter than 8 bytes are `-EINVAL`, as Linux does.
//!
//! ## Scope (honest)
//! - **`EFD_NONBLOCK` is recorded on the descriptor**, not here, because that is
//!   an fd flag (`fd::set_nonblock`) shared with `pipe2`/`fcntl`.
//! - `EFD_CLOEXEC` is likewise the descriptor's flag.
//! - A blocking read parks the **cell** through the same
//!   `proc::runnable_peer_exists` rule a pipe read uses, so a *thread* in the same
//!   cell writing the eventfd does not wake a sibling context that parked on it -
//!   that shares the documented cell-level block limitation of the whole L4
//!   thread model (docs/LINUX-COMPAT.md L4, task #27). Within one context, and
//!   across processes, it is exact.

use core::ptr::addr_of_mut;

/// `eventfd2` flags (Linux `include/uapi/linux/eventfd.h`). `EFD_CLOEXEC` and
/// `EFD_NONBLOCK` share the `O_*` values and are handled on the descriptor.
pub const EFD_SEMAPHORE: u64 = 1;
pub const EFD_CLOEXEC: u64 = 0o2000000;
pub const EFD_NONBLOCK: u64 = 0o4000;

/// Max concurrent eventfd objects across all cells. Fixed, like every other
/// registry here - the kernel is allocation-free.
const EVENTFDS: usize = 16;

/// The counter's ceiling. Linux caps at `ULLONG_MAX - 1` so that
/// `0xffff_ffff_ffff_ffff` stays the one value a write may never carry.
const MAX_COUNT: u64 = u64::MAX - 1;

#[derive(Copy, Clone)]
struct EventFd {
    used: bool,
    refs: u16,
    /// `EFD_SEMAPHORE`: a read yields 1 and decrements, instead of draining.
    semaphore: bool,
    count: u64,
}

impl EventFd {
    const fn new() -> EventFd {
        EventFd {
            used: false,
            refs: 0,
            semaphore: false,
            count: 0,
        }
    }
}

static mut TBL: [EventFd; EVENTFDS] = [const { EventFd::new() }; EVENTFDS];

fn tbl() -> &'static mut [EventFd; EVENTFDS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(TBL) }
}

/// Clear every eventfd (called from `linux::reset`).
pub fn reset() {
    for e in tbl().iter_mut() {
        *e = EventFd::new();
    }
}

/// Create an eventfd with initial counter `initval`; returns its index, or `None`
/// if the table is full.
pub fn create(initval: u64, semaphore: bool) -> Option<u8> {
    let t = tbl();
    let idx = (0..EVENTFDS).find(|&i| !t[i].used)?;
    t[idx] = EventFd {
        used: true,
        refs: 1,
        semaphore,
        count: initval.min(MAX_COUNT),
    };
    Some(idx as u8)
}

/// A new fd references object `ev` (dup/fork inheritance).
pub fn addref(ev: u8) {
    let e = &mut tbl()[ev as usize];
    e.refs = e.refs.saturating_add(1);
}

/// Drop a descriptor reference; free the slot at zero.
pub fn close(ev: u8) {
    let e = &mut tbl()[ev as usize];
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        *e = EventFd::new();
    }
}

/// The current counter - the scheduler's readiness question
/// (`proc::satisfiable`), asked with the *waiting* cell's address space inactive,
/// which is why it reads kernel state only.
pub fn count(ev: u8) -> u64 {
    tbl()[ev as usize].count
}

/// Readable now? (`POLLIN`.) A zero counter is not readable.
pub fn readable(ev: u8) -> bool {
    tbl()[ev as usize].count > 0
}

/// Writable now? (`POLLOUT`.) True unless the counter is saturated, i.e. unless
/// even an add of 1 would overflow.
pub fn writable(ev: u8) -> bool {
    tbl()[ev as usize].count < MAX_COUNT
}

/// Outcome of a non-blocking read - the caller decides whether to park
/// (`proc::block_eventfd_read`) or report `-EAGAIN`, exactly as for a pipe.
pub enum ReadNb {
    /// The 8-byte value was written to the caller's buffer.
    Done,
    /// The counter is zero.
    WouldBlock,
}

/// `read(evfd, buf, count)`: the counter (or 1 in semaphore mode) as a host-endian
/// `u64`. `count` under 8 is `-EINVAL`; a zero counter is `WouldBlock`.
///
/// Writes through `buf_va`, so the calling cell's address space must be active -
/// true in trap context, and true again when the scheduler completes a parked read
/// after switching back into the waiter.
pub fn read(ev: u8, buf_va: u64, count: u64) -> Result<ReadNb, i64> {
    if count < 8 {
        return Err(-crate::linux::errno::EINVAL);
    }
    // Peek before taking: the write can still fail (an unmapped buffer, or one the
    // cell has no writable mapping for), and draining a counter into a write that
    // then fails would lose the wakeup the eventfd exists to carry.
    let e = &mut tbl()[ev as usize];
    if e.count == 0 {
        return Ok(ReadNb::WouldBlock);
    }
    let val = if e.semaphore { 1 } else { e.count };
    // Unaligned on purpose: Linux does not require an 8-aligned buffer here, and
    // refusing one with -EFAULT would be a bug of our own making. `uaccess` bounds it,
    // makes it present, and resolves copy-on-write before the store.
    if !crate::uaccess::write_unaligned::<u64>(buf_va, val) {
        return Err(-crate::linux::errno::EFAULT);
    }
    let e = &mut tbl()[ev as usize];
    if e.semaphore {
        e.count -= 1;
    } else {
        e.count = 0;
    }
    Ok(ReadNb::Done)
}

/// `write(evfd, buf, count)`: add the 8-byte host-endian `u64` at `buf_va`.
/// Returns 8, or a negative errno: `-EINVAL` for a short buffer or the reserved
/// `u64::MAX`, `-EAGAIN` when the add would overflow (Linux blocks there; we
/// report it, because the counter only drains from within this cell tree and a
/// saturated eventfd means a reader has stopped reading).
pub fn write(ev: u8, buf_va: u64, count: u64) -> i64 {
    use crate::linux::errno::*;
    if count < 8 {
        return -EINVAL;
    }
    // Unaligned for the same reason as `read` above.
    let Some(add) = crate::uaccess::read_unaligned::<u64>(buf_va) else {
        return -EFAULT;
    };
    if add == u64::MAX {
        return -EINVAL; // the one value a write may never carry
    }
    let e = &mut tbl()[ev as usize];
    match e.count.checked_add(add) {
        Some(n) if n <= MAX_COUNT => {
            e.count = n;
            8
        }
        _ => -EAGAIN,
    }
}
