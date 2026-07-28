// The strand executor: native async on the OS's terms (docs/CONCURRENCY.md).
//
// A strand is a user-level task - a `Future`. It is stackless (an async state
// machine), so it costs bytes, not a stack, and spawn/teardown is a slab slot
// plus a `Box` - no `clone` syscall, no kernel stack, no global thread table.
// "Blocking" does not exist: a strand that needs I/O parks on a token and the
// runtime runs the next ready strand; the vcore never idles into the kernel.
// The token is the queue-pair completion's `user_data`, so one drained
// completion ring unparks exactly the strands whose ops finished - one
// wakeup, N strands resumed (CONCURRENCY.md 1, 8).
//
// **One executor per vcore, plus a shared injector.** Each vcore's run queue,
// slab and wake tables are its own, so nothing on the hot path is locked and
// `spawn` keeps its `Rc` join handles - a strand spawned on a vcore stays on
// that vcore, which is what makes `!Send` futures sound. Work that any vcore may
// take goes through `spawn_shared`, which is `Send`-bounded and has no join
// handle: that is the whole API split, and it is the split a `Send` bound forces
// rather than a style choice (docs/CONCURRENCY.md).
//
// The runtime cannot know which vcore it is running on - it is a userspace
// library with no CPU register to read - so the embedder supplies an accessor
// once with `set_vcore_hook`. Unset, it is a constant 0 and every pre-vcore
// caller resolves to slot 0 exactly as before.
//
// No real `Waker` machinery either: readiness is the executor's own run queue,
// and external completions arrive through `complete(token)`. The
// `core::task::Waker` handed to `poll` is a no-op. Regular // comments (not
// //! module docs) keep this file includable by the host comparison bench.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::rc::Rc;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// A strand's slot index in the executor slab.
pub type StrandId = usize;

struct Strand {
    future: Pin<Box<dyn Future<Output = ()> + 'static>>,
}

struct Executor {
    /// Slab of strands; `None` is a free slot. Spawn/teardown is O(1) with no
    /// tree rebalancing - the "light thread" cost.
    slab: alloc::vec::Vec<Option<Strand>>,
    free: alloc::vec::Vec<StrandId>,
    ready: VecDeque<StrandId>,
    /// token -> the strand parked on it.
    waiting: BTreeMap<u64, StrandId>,
    /// tokens completed (possibly before the strand parked).
    completed: BTreeSet<u64>,
    current: StrandId,
    live: u64,
    spawned: u64,
    finished: u64,
}

impl Executor {
    fn new() -> Executor {
        Executor {
            slab: alloc::vec::Vec::new(),
            free: alloc::vec::Vec::new(),
            ready: VecDeque::new(),
            waiting: BTreeMap::new(),
            completed: BTreeSet::new(),
            current: 0,
            live: 0,
            spawned: 0,
            finished: 0,
        }
    }

    fn insert(&mut self, strand: Strand) -> StrandId {
        let id = match self.free.pop() {
            Some(id) => {
                self.slab[id] = Some(strand);
                id
            }
            None => {
                self.slab.push(Some(strand));
                self.slab.len() - 1
            }
        };
        self.ready.push_back(id);
        self.live += 1;
        self.spawned += 1;
        id
    }
}

/// How many vcores one cell's runtime supports. Matches the kernel's `MAX_VCORES`
/// by value, not by dependency: `runtime` is a userspace library and must not
/// link the kernel, so the two are kept equal by the fact that a cell cannot be
/// given more vcores than the kernel will install.
pub const MAX_VCORES: usize = 4;

static mut EXECS: [Option<Executor>; MAX_VCORES] = [const { None }; MAX_VCORES];

/// The embedder's "which vcore am I on" accessor, as a raw address (0 = unset).
///
/// A hook rather than something the runtime works out for itself, because there is
/// nothing in userspace to work it out *from*: in a cell it is a syscall, in a test
/// kernel it is the per-CPU registry. The runtime is told, and says so.
static VCORE_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Tell the runtime how to find the calling vcore's index. Call once, before any
/// `spawn`. Unset, every access resolves to vcore 0 - which is exactly the
/// single-vcore behaviour, so a caller that never sets it is unchanged.
pub fn set_vcore_hook(f: fn() -> usize) {
    VCORE_HOOK.store(f as usize, core::sync::atomic::Ordering::Release);
}

/// The calling vcore's index, or 0 when no hook is set.
#[inline]
fn cur_vcore() -> usize {
    let h = VCORE_HOOK.load(core::sync::atomic::Ordering::Acquire);
    if h == 0 {
        return 0;
    }
    // SAFETY: `h` was stored by `set_vcore_hook` from a `fn() -> usize`.
    let f: fn() -> usize = unsafe { core::mem::transmute::<usize, fn() -> usize>(h) };
    let v = f();
    if v < MAX_VCORES { v } else { 0 }
}

/// Short-lived access to **this vcore's** executor. Cooperative within a vcore, so
/// no borrow is ever held across a `poll` (see `run`), which makes the re-entrant
/// access from inside a strand's `poll` sound.
#[inline]
fn with_exec<R>(f: impl FnOnce(&mut Executor) -> R) -> R {
    with_exec_on(cur_vcore(), f)
}

#[inline]
fn with_exec_on<R>(v: usize, f: impl FnOnce(&mut Executor) -> R) -> R {
    // SAFETY: each vcore touches only its own slot, and a vcore belongs to one core
    // at a time (the kernel's claim, docs/SMP.md 10.0a) - so two cores here are two
    // disjoint elements of the array, never one. Safe by partitioning, which is the
    // same argument the kernel's `PerCpu` rests on rather than a lock. `run` never
    // holds the borrow across `poll`.
    unsafe {
        let slot = &mut (*core::ptr::addr_of_mut!(EXECS))[v];
        if slot.is_none() {
            *slot = Some(Executor::new());
        }
        f(slot.as_mut().unwrap())
    }
}

/// Work any vcore may take, and the counter for how much each one took.
///
/// A single shared deque rather than per-vcore deques with stealing: with a handful
/// of vcores the injector is not the contended structure a many-core stealing deque
/// solves, and a `TicketLock` around a `VecDeque` is honest about what it is. Whether
/// that becomes the bottleneck is a measurement, and there is no hardware here to
/// take it on.
type SharedWork = alloc::boxed::Box<dyn Future<Output = ()> + Send + 'static>;
static INJECTOR: crate::lock::TicketLock<Option<VecDeque<SharedWork>>> =
    crate::lock::TicketLock::new(None);
static SHARED_TAKEN: [AtomicU64; MAX_VCORES] = [const { AtomicU64::new(0) }; MAX_VCORES];

/// Spawn a strand **any vcore may run**. `Send`, and no join handle.
///
/// Both restrictions are the same fact: work that can cross cores cannot carry an
/// `Rc`, so it cannot carry this runtime's join handle either. A caller that needs a
/// result uses a channel or an atomic - which is what a work-stealing pool does
/// anyway. `spawn` remains vcore-local and keeps its handle (docs/CONCURRENCY.md).
pub fn spawn_shared<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut g = INJECTOR.lock();
    g.get_or_insert_with(VecDeque::new)
        .push_back(alloc::boxed::Box::new(future));
}

/// How many shared strands vcore `v` has taken off the injector.
pub fn shared_taken(v: usize) -> u64 {
    SHARED_TAKEN[v].load(Ordering::Acquire)
}

/// Take one shared strand for this vcore, or `None` when the injector is empty.
fn take_shared() -> Option<SharedWork> {
    INJECTOR.lock().as_mut().and_then(|q| q.pop_front())
}

/// Reset the runtime (drops all strands and state). For tests that run the
/// executor more than once in one boot.
pub fn reset() {
    // SAFETY: called between runs, with no vcore inside `run`.
    unsafe {
        for slot in (*core::ptr::addr_of_mut!(EXECS)).iter_mut() {
            *slot = None;
        }
    }
    *INJECTOR.lock() = None;
    for c in SHARED_TAKEN.iter() {
        c.store(0, Ordering::Release);
    }
}

struct JoinState<T> {
    result: Option<T>,
    done: bool,
    waiter: Option<StrandId>,
}

/// A handle to a spawned strand's result. Awaiting it yields the value the
/// strand returned (structured concurrency; async "join"). Join uses per-
/// handle shared state, so a fire-and-forget strand costs nothing extra - it
/// does not accumulate anything in the executor's wake tables.
pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    /// Wait for the strand to finish and take its result.
    pub async fn join(self) -> T {
        JoinFut {
            state: self.state.clone(),
        }
        .await;
        self.state
            .borrow_mut()
            .result
            .take()
            .expect("joined strand produced no result")
    }
}

struct JoinFut<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> Future for JoinFut<T> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let mut st = self.state.borrow_mut();
        if st.done {
            Poll::Ready(())
        } else {
            st.waiter = Some(with_exec(|e| e.current));
            Poll::Pending
        }
    }
}

/// Spawn a strand running `future`; returns a handle to await its result.
/// This is the "thread create" of the OS - a slab slot plus a boxed state
/// machine, no syscall, no kernel stack.
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    let state = Rc::new(RefCell::new(JoinState {
        result: None,
        done: false,
        waiter: None,
    }));
    let sink = state.clone();
    let wrapped = async move {
        let out = future.await;
        let waiter = {
            let mut st = sink.borrow_mut();
            st.result = Some(out);
            st.done = true;
            st.waiter.take()
        };
        if let Some(id) = waiter {
            wake_strand(id);
        }
    };
    with_exec(|e| {
        e.insert(Strand {
            future: Box::pin(wrapped),
        })
    });
    JoinHandle { state }
}

/// Re-queue a specific strand (used to wake a joiner when its target finishes).
fn wake_strand(id: StrandId) {
    with_exec(|e| e.ready.push_back(id));
}

/// Run every ready strand until none is ready (the vcore would otherwise
/// idle). Strands that parked are resumed later by `complete`.
pub fn run() {
    let me = cur_vcore();
    loop {
        // This vcore's own ready queue first - local work is cheaper, and taking from
        // the injector while local strands wait would only add lock traffic.
        let Some(id) = with_exec_on(me, |e| e.ready.pop_front()) else {
            // Local queue dry: take shared work, if any is left.
            let Some(work) = take_shared() else { break };
            SHARED_TAKEN[me].fetch_add(1, Ordering::AcqRel);
            with_exec_on(me, |e| {
                e.insert(Strand {
                    future: Box::into_pin(work),
                })
            });
            continue;
        };
        run_one(me, id);
    }
}

/// Poll strand `id` on vcore `v` once, retiring or re-parking it.
fn run_one(v: usize, id: StrandId) {
    {
        // Take the strand out so its `poll` can freely re-enter the executor
        // (spawn/park/complete) without an aliasing borrow.
        let mut strand = match with_exec_on(v, |e| e.slab.get_mut(id).and_then(|s| s.take())) {
            Some(s) => s,
            None => return,
        };
        with_exec_on(v, |e| e.current = id);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match strand.future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                with_exec_on(v, |e| {
                    e.free.push(id);
                    e.live -= 1;
                    e.finished += 1;
                });
                // strand dropped here
            }
            Poll::Pending => {
                with_exec_on(v, |e| e.slab[id] = Some(strand));
            }
        }
    }
}

/// True while any strand is still alive (parked or ready).
pub fn has_pending() -> bool {
    with_exec(|e| e.live > 0)
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

/// A monotonic source of unique tokens (queue `user_data`, channel/lock/join
/// wakeups).
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

/// Cooperatively yield the vcore: re-queue this strand behind the others and
/// let them run before continuing. The compiler-yield / fair-scheduling point.
pub async fn yield_now() {
    YieldNow { yielded: false }.await
}

struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.yielded {
            Poll::Ready(())
        } else {
            this.yielded = true;
            with_exec(|e| {
                let id = e.current;
                e.ready.push_back(id);
            });
            Poll::Pending
        }
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
