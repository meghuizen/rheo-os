//! `timerfd` for the Linux personality (docs/LINUX-COMPAT.md L8-TIMERFD).
//!
//! A timerfd is a timer you `read` and, crucially, `poll`/`epoll`. It is how an
//! event loop gives itself a **timer wakeup**: the loop arms a timerfd for its
//! nearest deadline, adds it to its epoll set, and `epoll_wait` returns when the
//! timer fires. libuv - the runtime under Node.js and much of the JS/async world -
//! uses `timerfd_create(CLOCK_MONOTONIC)` exactly this way, so the absence of
//! timerfd removes a program's timer source rather than merely degrading it.
//!
//! It adds **no kernel object**. A timerfd is a per-cell fd
//! (`fd::FdKind::TimerFd`) indexing this per-personality registry - the `eventfd`
//! / `epoll` / `pipe` pattern - and its expiry is an ordinary **cell-clock
//! deadline**, the same kind `nanosleep` (`proc::Block::Timer`) already parks on
//! and the same kind the scheduler already halts for through the timer arbiter's
//! `CellSleep` slice (docs/ARCHITECTURE-DEBT.md 2.4). So timerfd composes the
//! existing time machinery; it introduces no new wake source and no change to the
//! deadline arithmetic.
//!
//! ## Semantics implemented (Linux `fs/timerfd.c`)
//! - `timerfd_settime(fd, flags, new, old)`: arm `new.it_value` (relative, or
//!   absolute with `TFD_TIMER_ABSTIME`) with optional `new.it_interval` for a
//!   periodic timer; an all-zero `it_value` **disarms**. `old` receives the prior
//!   setting (a `timerfd_gettime` snapshot).
//! - `timerfd_gettime(fd, cur)`: `cur.it_value` is the time until the next expiry
//!   (0 if disarmed), `cur.it_interval` the period.
//! - `read`: returns the number of expirations since the last read as a host-endian
//!   `u64` and consumes them (a periodic timer advances to its next future expiry).
//!   An unexpired timer is not readable: a blocking `read` parks on the deadline, a
//!   non-blocking one reports `-EAGAIN`.
//! - `POLLIN` once the timer has expired; never `POLLOUT` (a timerfd is not
//!   writable) - the readiness `epoll`/`poll` need.
//!
//! ## Clock domain
//! The deadline is stored and compared in the **cell's own clock domain**
//! (`crate::linux::cell_clock_ns`), the domain the program's `clock_gettime`
//! reports, exactly as `nanosleep` does - a timer of N ns fires N ns as the program
//! measures it (docs/ENGINEERING.md 11: clock domains are not interchangeable).
//! `CLOCK_MONOTONIC` and `CLOCK_REALTIME` are both accepted; the stored `realtime`
//! bit selects the domain each expiry check reads.
//!
//! ## Scope (honest)
//! - `TFD_NONBLOCK`/`TFD_CLOEXEC` are descriptor flags (`fd::set_nonblock` /
//!   `FD_CLOEXEC`), handled on the fd like `eventfd`'s.
//! - A blocking read parks the **cell** (the `nanosleep`/eventfd rule), so a
//!   *thread* in the same cell does not independently wait - the documented
//!   cell-level block limit of the L4 thread model (task #27). Within one context
//!   and across processes it is exact.
//! - `TFD_TIMER_CANCEL_ON_SET` (a realtime-clock-step cancellation) is not modelled;
//!   the cell clock does not step, so there is nothing to cancel on.

use core::ptr::addr_of_mut;

/// `timerfd_settime` flags (Linux `include/uapi/linux/timerfd.h`).
pub const TFD_TIMER_ABSTIME: u64 = 1;
/// `timerfd_create` flags. `TFD_CLOEXEC`/`TFD_NONBLOCK` share the `O_*` values and
/// are handled on the descriptor, like `eventfd`'s.
pub const TFD_NONBLOCK: u64 = 0o4000;
pub const TFD_CLOEXEC: u64 = 0o2000000;

/// `clockid_t` values a timerfd accepts.
pub const CLOCK_REALTIME: u64 = 0;
pub const CLOCK_MONOTONIC: u64 = 1;

/// Max concurrent timerfd objects across all cells. Fixed, like every other
/// registry here - the kernel is allocation-free. A libuv loop uses one.
const TIMERFDS: usize = 16;

#[derive(Copy, Clone)]
struct TimerFd {
    used: bool,
    refs: u16,
    /// `true` = the deadline is in the CLOCK_REALTIME domain, else CLOCK_MONOTONIC.
    realtime: bool,
    /// Absolute deadline of the next expiry, cell-clock ns. 0 = disarmed.
    deadline_ns: u64,
    /// Period for a repeating timer, ns. 0 = one-shot.
    interval_ns: u64,
}

impl TimerFd {
    const fn new() -> TimerFd {
        TimerFd {
            used: false,
            refs: 0,
            realtime: false,
            deadline_ns: 0,
            interval_ns: 0,
        }
    }

    /// `now` in this timer's clock domain.
    fn now(&self) -> u64 {
        crate::linux::cell_clock_ns(self.realtime)
    }

    /// Expirations pending at `now` without consuming them.
    fn count_at(&self, now: u64) -> u64 {
        if self.deadline_ns == 0 || now < self.deadline_ns {
            return 0;
        }
        // One expiry, plus one per whole interval elapsed since. `checked_div`
        // folds the one-shot case (`interval_ns == 0`) into "no extra intervals".
        match (now - self.deadline_ns).checked_div(self.interval_ns) {
            Some(extra) => 1 + extra,
            None => 1,
        }
    }
}

static mut TBL: [TimerFd; TIMERFDS] = [const { TimerFd::new() }; TIMERFDS];

fn tbl() -> &'static mut [TimerFd; TIMERFDS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(TBL) }
}

/// Clear every timerfd (called from `linux::reset`).
pub fn reset() {
    for e in tbl().iter_mut() {
        *e = TimerFd::new();
    }
}

/// Create a disarmed timerfd on `clockid`; returns its index, or `None` if full or
/// the clock is unsupported.
pub fn create(clockid: u64) -> Option<u8> {
    let realtime = match clockid {
        CLOCK_MONOTONIC => false,
        CLOCK_REALTIME => true,
        _ => return None,
    };
    let t = tbl();
    let idx = (0..TIMERFDS).find(|&i| !t[i].used)?;
    t[idx] = TimerFd {
        used: true,
        refs: 1,
        realtime,
        deadline_ns: 0,
        interval_ns: 0,
    };
    Some(idx as u8)
}

/// A new fd references object `tf` (dup/fork inheritance).
pub fn addref(tf: u8) {
    let e = &mut tbl()[tf as usize];
    e.refs = e.refs.saturating_add(1);
}

/// Drop a descriptor reference; free the slot at zero.
pub fn close(tf: u8) {
    let e = &mut tbl()[tf as usize];
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        *e = TimerFd::new();
    }
}

/// The prior setting a `settime`/`gettime` reports: `(value_ns, interval_ns)` where
/// `value_ns` is the time until the next expiry (0 = disarmed).
#[derive(Copy, Clone)]
pub struct ItSpec {
    pub value_ns: u64,
    pub interval_ns: u64,
}

/// `timerfd_gettime`: time until next expiry + the period.
pub fn gettime(tf: u8) -> ItSpec {
    let e = tbl()[tf as usize];
    if e.deadline_ns == 0 {
        return ItSpec {
            value_ns: 0,
            interval_ns: e.interval_ns,
        };
    }
    let now = e.now();
    let value_ns = if now < e.deadline_ns {
        e.deadline_ns - now
    } else if e.interval_ns != 0 {
        // Expired-and-unread periodic timer: time until the *next* tick.
        e.interval_ns - (now - e.deadline_ns) % e.interval_ns
    } else {
        0
    };
    ItSpec {
        value_ns,
        interval_ns: e.interval_ns,
    }
}

/// `timerfd_settime`: arm (or, with an all-zero `value_ns`, disarm) the timer.
/// `abstime` selects an absolute deadline. Returns the prior setting for `old`.
pub fn settime(tf: u8, abstime: bool, value_ns: u64, interval_ns: u64) -> ItSpec {
    let prev = gettime(tf);
    let e = &mut tbl()[tf as usize];
    if value_ns == 0 {
        // Disarm: it_value all-zero, regardless of it_interval (Linux fs/timerfd.c).
        e.deadline_ns = 0;
        e.interval_ns = 0;
    } else {
        let now = crate::linux::cell_clock_ns(e.realtime);
        e.deadline_ns = if abstime {
            value_ns
        } else {
            now.saturating_add(value_ns)
        };
        e.interval_ns = interval_ns;
    }
    prev
}

/// Readable now? (`POLLIN`.) True once the timer has expired at least once. Reads
/// only kernel state (the registry + the cell clock), so the scheduler can judge it
/// with the waiting cell's address space inactive - the `eventfd::readable` contract.
pub fn readable(tf: u8) -> bool {
    let e = tbl()[tf as usize];
    e.count_at(e.now()) > 0
}

/// Outcome of a non-blocking read.
pub enum ReadNb {
    /// The 8-byte expiration count was written to the caller's buffer.
    Done,
    /// The timer has not expired yet.
    WouldBlock,
}

/// `read(tfd, buf, 8)`: the expiration count as a host-endian `u64`, consumed. A
/// count under 8 bytes is `-EINVAL`; an unexpired timer is `WouldBlock`. A periodic
/// timer advances its deadline past `now` so the next read waits for the next tick.
pub fn read(tf: u8, buf_va: u64, count: u64) -> Result<ReadNb, i64> {
    if count < 8 {
        return Err(-crate::linux::errno::EINVAL);
    }
    let e = tbl()[tf as usize];
    let now = e.now();
    let expirations = e.count_at(now);
    if expirations == 0 {
        return Ok(ReadNb::WouldBlock);
    }
    // Write before consuming: a failed store (unmapped/unwritable buffer) must not
    // lose the expirations - the same peek-then-take discipline as `eventfd::read`.
    if !crate::uaccess::write_unaligned::<u64>(buf_va, expirations) {
        return Err(-crate::linux::errno::EFAULT);
    }
    let e = &mut tbl()[tf as usize];
    if e.interval_ns == 0 {
        e.deadline_ns = 0; // one-shot: disarm
    } else {
        // Advance to the first expiry strictly after `now`.
        e.deadline_ns = e.deadline_ns.saturating_add(expirations * e.interval_ns);
    }
    Ok(ReadNb::Done)
}
