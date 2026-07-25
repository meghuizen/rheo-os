//! The async engine (docs/LIBRHEO.md, docs/CONCURRENCY.md). Two pieces:
//!
//! - the **strand executor** (re-exported from `runtime::strand`): a strand is
//!   a stackless `Future` that "blocks" by parking on a token and is woken when
//!   a completion carries that token in `user_data`. Blocking exists only here,
//!   as a park - never a syscall that idles the vcore.
//! - the **reactor**: the cell's side of its queue pair. It owns the mapped
//!   ring + the capability id, submits ops tagged with a strand's token, and on
//!   `run` rings `SYS_DOORBELL`, drains the completion ring, and wakes each
//!   parked strand by the token its completion carries. This closes the
//!   CONCURRENCY.md 1 loop - "one wakeup, N strands resumed" - from userspace.

use alloc::collections::BTreeMap;
use core::future::Future;

use crate::cap::CapSet;
use crate::sys::{self, CqEntry, Qp};

pub use runtime::strand::{
    JoinHandle, StrandId, complete, has_pending, next_token, park_on, spawn, stats, yield_now,
};

/// The cell's reactor: its queue pair + the capability that authorises it.
pub struct Reactor {
    qp: Qp,
    cap_id: u32,
    /// Completions drained but not yet claimed by their awaiting strand,
    /// keyed by the token (`user_data`) the strand parked on.
    results: BTreeMap<u64, CqEntry>,
}

impl Reactor {
    /// Submit `op` with `args` (up to 24 bytes) and `flags`, tagged with
    /// `token`. Spins through a doorbell drain if the ring is momentarily full.
    fn submit(&mut self, op: u8, flags: u8, args: &[u8], token: u64) {
        while !self.qp.submit(op, flags, self.cap_id, 0, token, args) {
            self.pump();
        }
    }

    /// Ring the doorbell, then drain every completion into `results` and wake
    /// the strand each one belongs to. Returns the number of completions.
    fn pump(&mut self) -> usize {
        sys::doorbell();
        let mut n = 0;
        while let Some(cqe) = self.qp.reap() {
            let token = cqe.user_data;
            self.results.insert(token, cqe);
            complete(token);
            n += 1;
        }
        n
    }

    fn take(&mut self, token: u64) -> Option<CqEntry> {
        self.results.remove(&token)
    }
}

static mut REACTOR: Option<Reactor> = None;

/// Build the reactor from the cell's queue capability and mapped ring VA
/// (called by `_start`).
pub fn init(caps: &CapSet, qp_va: u64) {
    // SAFETY: `qp_va` is this cell's mapped, kernel-initialised ring region.
    let qp = unsafe { Qp::attach(qp_va as *mut u8) };
    let reactor = Reactor {
        qp,
        cap_id: caps.queue_cap_id(),
        results: BTreeMap::new(),
    };
    // SAFETY: single-CPU cooperative cell; init runs once before any strand.
    unsafe {
        *core::ptr::addr_of_mut!(REACTOR) = Some(reactor);
    }
}

#[inline]
fn with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> R {
    // SAFETY: single CPU; the reactor is never borrowed across an `.await`
    // (submit and take are separate, synchronous sections).
    unsafe {
        let r = (*core::ptr::addr_of_mut!(REACTOR))
            .as_mut()
            .expect("librheo: reactor used before init");
        f(r)
    }
}

/// Submit `op` with `args`, park until it completes, and return the completion
/// (`status`, `result`, ...). The async replacement for a blocking syscall:
/// the vcore runs other strands while this one is parked.
pub async fn submit_and_await(op: u8, args: [u8; 24]) -> CqEntry {
    submit_and_await_flags(op, 0, args).await
}

/// Like [`submit_and_await`] but carrying op `flags` (e.g.
/// [`sys::FLAG_INLINE`](crate::sys::FLAG_INLINE) for a sub-threshold write).
pub async fn submit_and_await_flags(op: u8, flags: u8, args: [u8; 24]) -> CqEntry {
    let token = next_token();
    with_reactor(|r| r.submit(op, flags, &args, token));
    park_on(token).await;
    with_reactor(|r| r.take(token)).expect("librheo: completion missing after wake")
}

/// Drive `root` (and every strand it spawns) to completion, servicing the
/// queue whenever no strand is ready. The userland event loop: run ready
/// strands, and when they have all parked, ring the doorbell + drain + wake,
/// then run again.
pub fn block_on<F: Future<Output = ()> + 'static>(root: F) {
    spawn(root);
    let mut guard: u32 = 0;
    loop {
        runtime::strand::run();
        if !has_pending() {
            break;
        }
        let woke = with_reactor(|r| r.pump());
        guard += 1;
        assert!(woke > 0 || guard < 4, "librheo: reactor made no progress");
        assert!(guard < 100_000, "librheo: reactor ran away (deadlock)");
    }
}
