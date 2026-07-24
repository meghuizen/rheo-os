//! The strand executor: native async on the OS's terms (docs/CONCURRENCY.md).
//!
//! A **strand** is a user-level task - a `Future`. It is *stackless* (an
//! async state machine), so it costs bytes, not a stack. "Blocking" does not
//! exist: a strand that needs I/O parks on a token and the runtime runs the
//! next ready strand; the vcore never idles into the kernel. The token is the
//! queue-pair completion's `user_data`, so one kernel notification (a drained
//! completion ring) unparks exactly the strands whose ops finished - one
//! wakeup, N strands resumed (CONCURRENCY.md 1, 8).
//!
//! This executor is single-vcore and cooperative (the kernel is single-CPU
//! today), so it needs no locks and no real `Waker` machinery: readiness is
//! the executor's own run queue, and external completions arrive through
//! `complete(token)`. The `core::task::Waker` handed to `poll` is a no-op.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

pub type StrandId = u64;

struct Strand {
    future: Pin<Box<dyn Future<Output = ()> + 'static>>,
}

struct Executor {
    strands: BTreeMap<StrandId, Strand>,
    ready: VecDeque<StrandId>,
    /// token -> the strand parked on it.
    waiting: BTreeMap<u64, StrandId>,
    /// tokens completed (possibly before the strand parked).
    completed: BTreeSet<u64>,
    current: StrandId,
    next_id: StrandId,
    spawned: u64,
    finished: u64,
}

impl Executor {
    fn new() -> Executor {
        Executor {
            strands: BTreeMap::new(),
            ready: VecDeque::new(),
            waiting: BTreeMap::new(),
            completed: BTreeSet::new(),
            current: 0,
            next_id: 1,
            spawned: 0,
            finished: 0,
        }
    }
}

static mut EXEC: Option<Executor> = None;

/// Short-lived access to the global executor. Single-CPU cooperative use, so
/// no borrow is ever held across a `poll` (see `run`), which makes the
/// re-entrant access from inside a strand's `poll` sound.
#[inline]
fn with_exec<R>(f: impl FnOnce(&mut Executor) -> R) -> R {
    // SAFETY: single CPU; `run` never holds this borrow across `poll`.
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(EXEC);
        if slot.is_none() {
            *slot = Some(Executor::new());
        }
        f(slot.as_mut().unwrap())
    }
}

/// Reset the runtime (drops all strands and state). For tests that run the
/// executor more than once in one boot.
pub fn reset() {
    with_exec(|e| *e = Executor::new());
}

/// Spawn a strand. Returns its id.
pub fn spawn<F: Future<Output = ()> + 'static>(future: F) -> StrandId {
    with_exec(|e| {
        let id = e.next_id;
        e.next_id += 1;
        e.strands.insert(
            id,
            Strand {
                future: Box::pin(future),
            },
        );
        e.ready.push_back(id);
        e.spawned += 1;
        id
    })
}

/// Run every ready strand until none is ready (the vcore would otherwise
/// idle). Strands that parked are resumed later by `complete`.
pub fn run() {
    while let Some(id) = with_exec(|e| e.ready.pop_front()) {
        // Take the strand out so its `poll` can freely re-enter the executor
        // (spawn/park/complete) without an aliasing borrow.
        let mut strand = match with_exec(|e| e.strands.remove(&id)) {
            Some(s) => s,
            None => continue,
        };
        with_exec(|e| e.current = id);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match strand.future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                with_exec(|e| e.finished += 1);
                // strand dropped here
            }
            Poll::Pending => {
                with_exec(|e| {
                    e.strands.insert(id, strand);
                });
            }
        }
    }
}

/// True while any strand is still alive (parked or ready).
pub fn has_pending() -> bool {
    with_exec(|e| !e.strands.is_empty())
}

/// (spawned, finished) strand counts, for tests/introspection.
pub fn stats() -> (u64, u64) {
    with_exec(|e| (e.spawned, e.finished))
}

/// Complete a token: wake the strand parked on it (or record the completion
/// so a strand that parks later returns immediately). This is what the
/// queue-pair reactor calls for each drained completion, passing the
/// completion's `user_data`.
pub fn complete(token: u64) {
    with_exec(|e| {
        if let Some(id) = e.waiting.remove(&token) {
            e.completed.insert(token);
            e.ready.push_back(id);
        } else {
            e.completed.insert(token);
        }
    });
}

/// A monotonic source of unique tokens (queue `user_data`, channel wakeups).
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
pub fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Park the current strand until `complete(token)` is called. The core of the
/// "blocking is structural" model: awaiting this yields the vcore to the next
/// ready strand instead of spinning or trapping into the kernel.
pub async fn park_on(token: u64) {
    Park { token }.await
}

struct Park {
    token: u64,
}

impl Future for Park {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let token = self.token;
        with_exec(|e| {
            if e.completed.remove(&token) {
                Poll::Ready(())
            } else {
                e.waiting.insert(token, e.current);
                Poll::Pending
            }
        })
    }
}

// A no-op waker: this cooperative executor tracks readiness itself (through
// its run queue and `complete`), so it does not use the standard wake path.
fn noop_waker() -> Waker {
    fn raw() -> RawWaker {
        RawWaker::new(core::ptr::null(), &VT)
    }
    static VT: RawWakerVTable = RawWakerVTable::new(|_| raw(), |_| {}, |_| {}, |_| {});
    // SAFETY: the vtable functions are all valid and the data pointer is
    // never dereferenced.
    unsafe { Waker::from_raw(raw()) }
}
