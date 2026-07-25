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
    /// A pending console read: `(buf_va, len, token)`. One reader at a time - a
    /// terminal has a single input stream (docs/LIBRHEO.md Phase D). Serviced by
    /// `block_on` when no queue completion is ready, by blocking in the kernel
    /// (`SYS_WAIT_INPUT`) until input arrives - the terminal idle path.
    console_req: Option<(u64, usize, u64)>,
    /// Byte count the last serviced console read returned.
    console_n: usize,
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

    /// Register a pending console read (the strand parks on `token`).
    fn set_console_read(&mut self, buf: u64, len: usize, token: u64) {
        self.console_req = Some((buf, len, token));
    }

    /// Service a pending console read by **blocking in the kernel** until input
    /// arrives, then wake its strand. Returns false if none was pending. This is
    /// where the terminal idles: the kernel halts (or polls) inside
    /// `SYS_WAIT_INPUT` while every strand is parked.
    fn service_console(&mut self) -> bool {
        if let Some((buf, len, token)) = self.console_req.take() {
            self.console_n = sys::wait_input(buf as *mut u8, len);
            complete(token);
            true
        } else {
            false
        }
    }

    fn console_result(&self) -> usize {
        self.console_n
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
        console_req: None,
        console_n: 0,
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

/// Block-and-wake console read: register a request, park until the reactor
/// services it (the kernel idles until input where the UART RX interrupt is
/// wired, polls otherwise), and return the byte count (0 = end of input). The
/// terminal's async input substrate (`term`, docs/LIBRHEO.md Phase D): while
/// this strand is parked the vcore runs the others, and only when they have all
/// parked does the reactor block in the kernel for a byte.
///
/// # Safety
/// `buf` must point at `len` writable bytes that outlive the await (the kernel
/// writes them during `SYS_WAIT_INPUT`).
pub async fn read_console(buf: *mut u8, len: usize) -> usize {
    let token = next_token();
    with_reactor(|r| r.set_console_read(buf as u64, len, token));
    park_on(token).await;
    with_reactor(|r| r.console_result())
}

/// Drive `root` (and every strand it spawns) to completion, servicing the
/// queue whenever no strand is ready. The userland event loop: run ready
/// strands; when they have all parked, ring the doorbell + drain + wake; and if
/// nothing was ready there, block for console input (the terminal idle path).
pub fn block_on<F: Future<Output = ()> + 'static>(root: F) {
    spawn(root);
    let mut guard: u32 = 0;
    loop {
        runtime::strand::run();
        if !has_pending() {
            break;
        }
        let woke = with_reactor(|r| r.pump());
        if woke > 0 {
            guard = 0; // queue completions woke strands: progress
        } else if with_reactor(|r| r.service_console()) {
            guard = 0; // blocked for console input and woke its strand: progress
        } else {
            // No completion, no console read: allow a few settling iterations
            // (join hand-offs), then declare no progress.
            guard += 1;
            assert!(guard < 4, "librheo: reactor made no progress");
        }
        assert!(guard < 100_000, "librheo: reactor ran away (deadlock)");
    }
}
