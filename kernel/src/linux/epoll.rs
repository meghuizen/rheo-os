//! A **minimal, level-triggered** epoll for the Linux personality
//! (docs/LINUX-COMPAT.md L8-INET). Many networked binaries multiplex sockets with
//! epoll; this provides the common subset - `epoll_create1`, `epoll_ctl`
//! (ADD/MOD/DEL), and `epoll_wait`/`epoll_pwait` reporting `EPOLLIN`/`EPOLLOUT`
//! readiness. It adds **no kernel object**: an epoll instance is a per-cell fd
//! (`FdKind::Epoll`) indexing this per-personality registry, and readiness is
//! queried from the owning cell's fd table at `epoll_wait` time.
//!
//! ## Scope (honest)
//! - **Level-triggered only.** `EPOLLET` (edge-triggered), `EPOLLONESHOT`,
//!   `EPOLLEXCLUSIVE`, and `EPOLLRDHUP`/`EPOLLPRI` are **not** implemented - the
//!   requested `events` are masked to `EPOLLIN|EPOLLOUT` and readiness is
//!   recomputed each `epoll_wait`.
//! - **Non-blocking `epoll_wait`.** Readiness is checked at call time; a wait with
//!   nothing ready returns 0 immediately rather than parking (a blocking
//!   cross-cell `epoll_wait` is a later refinement, like the AF_UNIX blocking
//!   `accept`). The loopback proof makes the watched fds ready before it waits.

use core::ptr::addr_of_mut;

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLOUT: u32 = 0x004;

/// `epoll_ctl` ops.
pub const EPOLL_CTL_ADD: u64 = 1;
pub const EPOLL_CTL_DEL: u64 = 2;
pub const EPOLL_CTL_MOD: u64 = 3;

/// Max concurrent epoll instances across all cells.
const EPOLLS: usize = 8;
/// Watched fds per instance.
const WATCH: usize = 32;

#[derive(Copy, Clone)]
struct Watch {
    active: bool,
    fd: i32,
    events: u32,
    data: u64,
}

impl Watch {
    const fn new() -> Watch {
        Watch {
            active: false,
            fd: 0,
            events: 0,
            data: 0,
        }
    }
}

struct Epoll {
    used: bool,
    refs: u16,
    w: [Watch; WATCH],
}

impl Epoll {
    const fn new() -> Epoll {
        Epoll {
            used: false,
            refs: 0,
            w: [const { Watch::new() }; WATCH],
        }
    }
}

static mut EPOLLS_TBL: [Epoll; EPOLLS] = [const { Epoll::new() }; EPOLLS];

fn epolls() -> &'static mut [Epoll; EPOLLS] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut *addr_of_mut!(EPOLLS_TBL) }
}

/// Clear every epoll instance (called from `linux::reset`).
pub fn reset() {
    for e in epolls().iter_mut() {
        *e = Epoll::new();
    }
}

/// Create an epoll instance; returns its index, or `None` if the table is full.
pub fn create() -> Option<u8> {
    let es = epolls();
    let idx = (0..EPOLLS).find(|&i| !es[i].used)?;
    es[idx] = Epoll::new();
    es[idx].used = true;
    es[idx].refs = 1;
    Some(idx as u8)
}

/// A new fd references instance `ep` (dup/fork inheritance).
pub fn addref(ep: u8) {
    let e = &mut epolls()[ep as usize];
    e.refs = e.refs.saturating_add(1);
}

/// Drop an instance fd reference; free the slot at zero.
pub fn close(ep: u8) {
    let e = &mut epolls()[ep as usize];
    e.refs = e.refs.saturating_sub(1);
    if e.refs == 0 {
        *e = Epoll::new();
    }
}

/// `epoll_ctl(op, fd, events, data)` on instance `ep`. Only `EPOLLIN|EPOLLOUT`
/// are honored (level-triggered). Returns 0 or a negative errno.
pub fn ctl(ep: u8, op: u64, fd: i32, events: u32, data: u64) -> i64 {
    use crate::linux::errno::*;
    let e = &mut epolls()[ep as usize];
    let masked = events & (EPOLLIN | EPOLLOUT);
    match op {
        EPOLL_CTL_ADD => {
            if e.w.iter().any(|w| w.active && w.fd == fd) {
                return -EEXIST;
            }
            let Some(slot) = e.w.iter().position(|w| !w.active) else {
                return -ENOMEM;
            };
            e.w[slot] = Watch {
                active: true,
                fd,
                events: masked,
                data,
            };
            0
        }
        EPOLL_CTL_MOD => match e.w.iter_mut().find(|w| w.active && w.fd == fd) {
            Some(w) => {
                w.events = masked;
                w.data = data;
                0
            }
            None => -ENOENT,
        },
        EPOLL_CTL_DEL => match e.w.iter_mut().find(|w| w.active && w.fd == fd) {
            Some(w) => {
                *w = Watch::new();
                0
            }
            None => -ENOENT,
        },
        _ => -EINVAL,
    }
}

/// The active `(fd, events, data)` watches of instance `ep`, copied into `out`;
/// returns the count. Lets `fd::epoll_wait` compute readiness against the cell's
/// fd table without borrowing this module's statics across that call.
pub fn snapshot(ep: u8, out: &mut [(i32, u32, u64); WATCH]) -> usize {
    let e = &epolls()[ep as usize];
    let mut n = 0;
    for w in e.w.iter() {
        if w.active {
            out[n] = (w.fd, w.events, w.data);
            n += 1;
        }
    }
    n
}

/// The watch-list capacity (the caller sizes its snapshot buffer to this).
pub const MAX_WATCH: usize = WATCH;
