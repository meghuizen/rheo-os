//! Multi-context ("thread") support for a Linux-personality cell
//! (docs/LINUX-COMPAT.md L4). This is the CONCURRENCY.md vcore model made real
//! for a Linux cell: one cell holds up to `MAX_THREADS` execution contexts,
//! scheduled **cooperatively at syscall boundaries** on the single CPU. It adds
//! no kernel object - PIDs/TIDs/futex waiter lists are per-cell synthesized
//! state, exactly like the fd table (docs/LINUX-COMPAT.md 1).
//!
//! A context is a `TrapFrame` (the saved register state), a run state, and a
//! per-context FP/SIMD save area. Context 0 reuses the cell's installed frame
//! (`user::cell_frame`); `clone`-created contexts get kernel-owned frames from
//! `FRAMES`. Switching is a generalization of the native `SYS_SWITCH`: the
//! dispatcher returns a *different* context's frame to the arch trampoline,
//! which resumes it. All contexts of a cell share one kernel stack and one
//! address space, so a switch is cheap (no page-table reload); because the
//! two contexts time-share the vector registers, FP/SIMD state is saved and
//! restored eagerly on every switch, and the per-thread TLS base is reloaded
//! (x86-64 FS_BASE via `set_user_fs_base`; ARM64 TPIDR_EL0 / RISC-V `tp` ride
//! along in the frame).
//!
//! Cooperative, no preemption: a compute-bound thread that never issues a
//! syscall starves its siblings. This is accepted for L4 and documented
//! (docs/CONCURRENCY.md, docs/LINUX-COMPAT.md L4); the fix is timer preemption
//! (task #27). Priority inheritance for RT-reservation mutexes
//! (CONCURRENCY.md) is a documented TODO - L4 wakes futex waiters FIFO and no
//! reservation-holding threads exist in the test suite.

use crate::arch::{self, TrapFrame};
use crate::ktimer::{self, TimerClient};
use crate::linux::Ctl;
use crate::linux::errno::*;
use crate::linux::proc::Block;
use crate::mm::kmeta::{Funded, Owner};
use crate::user::{self, MAX_CELLS};
use core::ptr::addr_of_mut;

/// Contexts a cell starts with room for. **Not a ceiling** - the context tables
/// are [`Funded`] and grow on demand (docs/SUBSTRATE.md pillar 1), so what
/// actually bounds a cell's thread count is its own frame budget.
///
/// This used to be `MAX_THREADS = 8`, a fixed array dimension, and it was the
/// wrong shape for the target workloads: Node's libuv threadpool is 4 by default
/// *plus* V8's helper threads *plus* any `worker_threads` the program creates, so
/// a perfectly ordinary Node program exceeded it and got `-EAGAIN` from `clone`
/// (surfacing as `pthread_create` failing). 8 remains the *initial* reservation
/// because it covers the common case in one frame's worth of slots; growth past
/// it is ordinary and costs the cell frames it is charged for.
pub const INITIAL_CONTEXTS: usize = 8;

/// A hard sanity ceiling on contexts per cell.
///
/// Deliberately **not** the mechanism that limits anything in practice: a cell
/// runs out of frame budget long before this, and that refusal is the meaningful
/// one because it is attributable. This exists only so that a runaway
/// `clone` loop is bounded by something nameable rather than by whichever
/// allocation happens to fail first, and so the per-context tables that are
/// indexed by a small integer keep a stated upper bound. Linux has the same shape
/// in `RLIMIT_NPROC`.
pub const CONTEXT_CEILING: usize = 1024;

/// Per-context FP/SIMD save area. Sized above the largest per-ISA image
/// (x86 FXSAVE 512, ARM64 V-regs+FPSR/FPCR 528, RISC-V f-regs+fcsr 264) and
/// 16-aligned (FXSAVE/`stp q` require it).
// 64-byte aligned and sized for the widest per-ISA save format: x86 uses XSAVE
// when AVX/AVX-512 is enabled (an AVX-512 area is ~2.5 KiB and XSAVE requires
// 64-byte alignment), not just the 512-byte FXSAVE image (docs/TILES.md 4).
#[repr(C, align(64))]
#[derive(Copy, Clone)]
struct FpArea([u8; arch::FP_AREA_LEN]);

impl FpArea {
    const fn new() -> FpArea {
        FpArea([0; arch::FP_AREA_LEN])
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum TState {
    /// Slot unused.
    Free,
    /// Runnable (currently running, or waiting for its turn).
    Ready,
    /// Parked on a futex word.
    Blocked,
}

/// `Copy` because it lives in a [`Funded`] table, whose storage is raw frames
/// with no drop glue (docs/SUBSTRATE.md pillar 1). Every field already is: a raw
/// frame pointer, small integers, and two `Copy` records.
#[derive(Copy, Clone)]
struct Thread {
    /// Saved register state. Context 0 points at the cell's installed frame;
    /// others point into `FRAMES`.
    frame: *mut TrapFrame,
    state: TState,
    /// Thread id (gettid). Context 0 is the tgid (== getpid, 1000).
    tid: u32,
    /// CLONE_CHILD_CLEARTID address: on this context's exit, the word here is
    /// zeroed and a futex wake is issued on it (pthread join handshake).
    clear_child_tid: u64,
    /// x86-64 per-thread TLS base (reloaded into FS_BASE on switch); unused on
    /// ARM64/RISC-V, where the TLS register rides in the frame.
    fs_base: u64,
    /// Futex word this context is `Blocked` on (0 when not blocked).
    fut_addr: u64,
    /// Absolute deadline of this context's futex wait, in the **cell's own clock
    /// domain** ([`crate::linux::cell_clock_ns`]); 0 means "no timeout".
    ///
    /// The cell's domain, not the timer's, because a Linux program computes an
    /// absolute `FUTEX_WAIT_BITSET` deadline from its own `clock_gettime`, and
    /// comparing it against a different counter would make the same "50 ms" mean
    /// something different per ISA (docs/ENGINEERING.md 11).
    fut_deadline: u64,
    /// This context's **proc-level** block condition (`epoll_wait`/`poll`/
    /// `nanosleep`/pipe/eventfd/console/wait4) when `state == Blocked` for a
    /// non-futex reason - `Block::None` otherwise (per-context blocking,
    /// docs/LINUX-COMPAT.md L4). Distinct from a futex wait (`fut_addr`): a
    /// context is `Blocked` for exactly one of the two reasons at a time. The
    /// scheduler (`proc.rs`) judges its satisfiability and completes it.
    pblock: Block,
}

impl Thread {
    const fn new() -> Thread {
        Thread {
            frame: core::ptr::null_mut(),
            state: TState::Free,
            tid: 0,
            clear_child_tid: 0,
            fs_base: 0,
            fut_addr: 0,
            fut_deadline: 0,
            pblock: Block::None,
        }
    }
}

/// Per-cell context tables, **funded** rather than fixed arrays.
///
/// Two separate tables rather than one, because `TrapFrame` is large and its
/// slots are addressed by long-lived pointer (see [`child_frame_ptr`]), while
/// `Thread` is small and read by value. Keeping them apart means a cell with many
/// contexts pays for frames in the granularity each actually needs.
static mut THREADS: [ContextTable; MAX_CELLS] = [const { ContextTable::new() }; MAX_CELLS];
/// Kernel-owned frames for clone-created contexts (index 0 unused - context 0
/// reuses the cell's installed frame).
///
/// **Slot addresses are stable.** `child_frame_ptr` hands out a `*mut TrapFrame`
/// into this table that the arch layer keeps and dereferences later, so the
/// storage must never move under it. [`Funded`] guarantees that: growth allocates
/// new frames and files them in its directory, and never relocates the frames
/// already there (docs/SUBSTRATE.md pillar 1). A `Vec`-shaped container would
/// have made this migration unsound.
static mut FRAMES: [Funded<TrapFrame>; MAX_CELLS] = [const { Funded::new() }; MAX_CELLS];

/// Per-context FP/SIMD save areas, in their **own** funded table.
///
/// Split out of [`Thread`] rather than embedded in it, for two reasons that point
/// the same way. The forcing one: `arch::FP_AREA_LEN` is **4096 on x86-64** (an
/// XSAVE area sized for AVX-512), so a `Thread` carrying it inline is larger than
/// a frame - and a [`Funded`] element must fit in one, since the page directory
/// maps an element to a `(page, offset)` pair. Embedding it made every reservation
/// fail, and the const assert below now makes that a compile error rather than a
/// runtime one.
///
/// The design reason: this is bulk state touched only at a context switch, while
/// `Thread` is small metadata read on every scheduling decision. Keeping a 4 KiB
/// blob out of the hot struct is what a switch-heavy path wants anyway.
static mut FPAREAS: [Funded<FpArea>; MAX_CELLS] = [const { Funded::new() }; MAX_CELLS];

// Every element type stored in a `Funded` table must fit in one frame. Asserted
// here, per type, so exceeding it cannot compile - the failure mode it replaces was
// a reservation that silently returned false at run time and reported the wrong
// cause (an exhausted pool) for it.
const _: () = assert!(crate::mm::kmeta::elems_per_page::<Thread>() > 0);
const _: () = assert!(crate::mm::kmeta::elems_per_page::<TrapFrame>() > 0);
const _: () = assert!(crate::mm::kmeta::elems_per_page::<FpArea>() > 0);

/// Pointer to context `i`'s FP save area, or null when the slot is not reserved.
///
/// A raw pointer because that is what `arch::save_user_fp`/`restore_user_fp` take,
/// and because the address must stay valid across the switch - which it does, since
/// a `Funded` slot never moves while it is in capacity.
fn fp_ptr(cell: usize, i: usize) -> *mut u8 {
    // SAFETY: single CPU; the slot's address is stable (see `FRAMES`).
    unsafe {
        let t = &mut (*addr_of_mut!(FPAREAS))[cell];
        match t.get_mut(i) {
            Some(a) => a.0.as_mut_ptr(),
            None => {
                // Unreachable if `capacity` is respected, and worth saying rather
                // than returning null: the arch FP save/restore would dereference
                // it and take a *kernel-mode* page fault at address zero, which
                // reports as a bare TRAP with no hint at the cause. `FP_SCRATCH`
                // keeps that a wrong-but-contained value plus a diagnostic.
                crate::println!(
                    "linux: cell {cell} context {i} has no FP save area \
                     (capacity {}) - using scratch",
                    t.capacity()
                );
                (*addr_of_mut!(FP_SCRATCH)).0.as_mut_ptr()
            }
        }
    }
}

/// Fallback FP area for the unreachable case in [`fp_ptr`]. One shared area, since
/// it exists to keep a stray access in mapped memory rather than to preserve state.
static mut FP_SCRATCH: FpArea = FpArea::new();
static mut CUR_THREAD: [usize; MAX_CELLS] = [0; MAX_CELLS];
static mut NEXT_TID: [u32; MAX_CELLS] = [1001; MAX_CELLS];

/// A cell's context table: a [`Funded`] array of [`Thread`] that keeps array
/// indexing at its call sites.
///
/// `Index`/`IndexMut` are implemented so the ~40 existing `threads(cell)[i].field`
/// sites read exactly as they did over the fixed array - the migration changes
/// *where the storage comes from*, not how the personality talks about contexts.
/// Both panic on an out-of-capacity index, which is what array indexing already
/// did; callers bound their loops with [`ContextTable::capacity`] instead of a
/// constant, and the one site that creates a context calls
/// [`ContextTable::ensure`] first.
struct ContextTable {
    t: Funded<Thread>,
}

impl ContextTable {
    const fn new() -> ContextTable {
        ContextTable { t: Funded::new() }
    }

    /// Slots currently addressable.
    fn capacity(&self) -> usize {
        self.t.capacity()
    }

    /// Grow to at least `n` slots, charging `owner`. False when the owner is out
    /// of budget - the caller turns that into `-EAGAIN`, which is what Linux
    /// reports when a process cannot create another thread.
    fn ensure(&mut self, n: usize, owner: Owner) -> bool {
        if n > CONTEXT_CEILING {
            return false;
        }
        self.t.set_owner(owner);
        if n <= self.t.capacity() {
            return true;
        }
        let had = self.t.capacity();
        if !self.t.reserve(n) {
            return false;
        }
        // Freshly grown slots are zeroed frames, and an all-zero `Thread` is not a
        // valid free slot (`TState::Free` happens to be 0 today, but relying on
        // that would break silently if the enum were ever reordered). Initialise
        // them explicitly.
        for i in had..self.t.capacity() {
            self.t.set(i, Thread::new());
        }
        true
    }

    /// Release the table's frames (cell teardown).
    fn release(&mut self) {
        self.t.release();
    }

    /// Every slot, mutably - the `iter_mut()` the fixed array offered, kept so the
    /// reset/teardown paths read unchanged.
    fn for_each_mut(&mut self, mut f: impl FnMut(&mut Thread)) {
        for i in 0..self.t.capacity() {
            if let Some(r) = self.t.get_mut(i) {
                f(r);
            }
        }
    }

    /// Every slot, by value.
    fn iter_values(&self) -> impl Iterator<Item = Thread> + '_ {
        (0..self.t.capacity()).filter_map(move |i| self.t.get(i))
    }
}

impl core::ops::Index<usize> for ContextTable {
    type Output = Thread;
    fn index(&self, i: usize) -> &Thread {
        self.t
            .get_ref(i)
            .expect("context index past the cell's table capacity")
    }
}

impl core::ops::IndexMut<usize> for ContextTable {
    fn index_mut(&mut self, i: usize) -> &mut Thread {
        self.t
            .get_mut(i)
            .expect("context index past the cell's table capacity")
    }
}

fn threads(cell: usize) -> &'static mut ContextTable {
    // SAFETY: single CPU, synchronous traps; one context runs at a time.
    unsafe { &mut (*addr_of_mut!(THREADS))[cell] }
}

/// Contexts cell `cell` currently has slots for - the iteration bound that
/// replaced the old `MAX_THREADS` constant.
///
/// The **minimum** across the three per-context tables, which is the only safe
/// answer: `Funded` rounds a reservation up to whole frames, and the three element
/// types have very different sizes (a `Thread` is tens of bytes, a `TrapFrame`
/// hundreds, an `FpArea` exactly one frame on x86-64), so reserving `n` slots
/// leaves the three tables with *different* capacities. Taking the largest - or any
/// one table's - yields indices that are in range for that table and out of range
/// for another, which is precisely the bug this shape had first: `clone` picked a
/// slot from the `Thread` table, `fp_ptr` returned null for it, and the FP save
/// wrote to address zero from kernel mode.
pub fn capacity(cell: usize) -> usize {
    // SAFETY: single CPU; plain capacity reads.
    unsafe {
        let frames = (*addr_of_mut!(FRAMES))[cell].capacity();
        let fp = (*addr_of_mut!(FPAREAS))[cell].capacity();
        threads(cell).capacity().min(frames).min(fp)
    }
}

/// Ensure cell `cell` has at least `n` context slots, charged to that cell.
fn ensure_contexts(cell: usize, n: usize) -> bool {
    let owner = Owner::cell(cell);
    // SAFETY: single CPU; the two tables are distinct statics.
    let bulk_ok = unsafe {
        let f = &mut (*addr_of_mut!(FRAMES))[cell];
        f.set_owner(owner);
        let frames_ok = f.reserve(n);
        let a = &mut (*addr_of_mut!(FPAREAS))[cell];
        a.set_owner(owner);
        let fp_ok = a.reserve(n);
        if fp_ok {
            // A fresh FP area must be the ABI-default register state, not zeroes:
            // an all-zero x86-64 XSAVE header is not a valid state to restore.
            for i in 0..a.capacity() {
                if let Some(area) = a.get_mut(i) {
                    arch::fp_area_init(area.0.as_mut_ptr());
                }
            }
        }
        frames_ok && fp_ok
    };
    bulk_ok && threads(cell).ensure(n, owner)
}

fn child_frame_ptr(cell: usize, i: usize) -> *mut TrapFrame {
    // SAFETY: single CPU. The slot is in capacity (the caller grew the table
    // first), and a `Funded` slot's address is stable for as long as it is in
    // capacity - see the `FRAMES` docs.
    unsafe {
        let f = &mut (*addr_of_mut!(FRAMES))[cell];
        match f.get_mut(i) {
            Some(r) => r as *mut TrapFrame,
            None => core::ptr::null_mut(),
        }
    }
}

fn cur_thread(cell: usize) -> usize {
    // SAFETY: single CPU.
    unsafe { (*addr_of_mut!(CUR_THREAD))[cell] }
}

fn set_cur_thread(cell: usize, i: usize) {
    // SAFETY: single CPU.
    unsafe { (*addr_of_mut!(CUR_THREAD))[cell] = i };
}

fn next_tid(cell: usize) -> u32 {
    // SAFETY: single CPU.
    unsafe {
        let t = &mut (*addr_of_mut!(NEXT_TID))[cell];
        let v = *t;
        *t += 1;
        v
    }
}

/// Initialize cell `cell`'s thread table with a single running context
/// (context 0), reusing the cell's installed frame. Called from
/// `linux::install_cell`.
pub fn init_cell(cell: usize) {
    let f0 = user::cell_frame(cell);
    // The table is funded, so it starts empty: reserve before touching slot 0.
    // A cell that cannot get even one context slot cannot run at all, so say so
    // plainly rather than letting the index below carry the failure.
    if !ensure_contexts(cell, INITIAL_CONTEXTS) && !ensure_contexts(cell, 1) {
        crate::println!(
            "linux: cell {cell} could not reserve a single execution context - \
             the frame pool is exhausted below the metadata reserve"
        );
        return;
    }
    let t = threads(cell);
    t.for_each_mut(|th| *th = Thread::new());
    t[0].frame = f0;
    t[0].state = TState::Ready;
    t[0].tid = 1000; // main thread tid == tgid (getpid)
    set_cur_thread(cell, 0);
    // SAFETY: single CPU.
    unsafe { (*addr_of_mut!(NEXT_TID))[cell] = 1001 };
}

/// Clear every cell's thread table (called from `linux::reset`).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(DEADLOCK_WAITS) = 0;
        *addr_of_mut!(IMMEDIATE_TIMEOUTS) = 0;
    }
    for cell in 0..MAX_CELLS {
        // Release rather than clear: these tables hold frames now, and a reset that
        // only zeroed them would leak every context slot every run.
        threads(cell).release();
        // SAFETY: single CPU, between runs.
        unsafe {
            (*addr_of_mut!(FRAMES))[cell].release();
            (*addr_of_mut!(FPAREAS))[cell].release();
        }
        set_cur_thread(cell, 0);
    }
}

/// The tid of the currently running context (`gettid`).
pub fn current_tid(cell: usize) -> u32 {
    threads(cell)[cur_thread(cell)].tid
}

// ---- per-context proc-level blocking (docs/LINUX-COMPAT.md L4) ----
//
// A context can block on a proc-level condition (`epoll_wait`/`poll`/`nanosleep`/
// pipe/eventfd/console/wait4) while a *sibling* context of the same cell keeps
// running - which is what an event-loop program needs: Node's main thread blocks
// on `epoll_wait` for an eventfd a V8 worker thread must write. The condition
// itself lives here per-context; `proc.rs` judges its satisfiability and completes
// it (it owns the fd tables). The futex wait (`fut_addr`) is the other, separate
// reason a context is `Blocked`.

/// Record the current context's proc-level block condition and mark it `Blocked`.
pub(crate) fn set_current_pblock(cell: usize, b: Block) {
    let ci = cur_thread(cell);
    threads(cell)[ci].pblock = b;
    threads(cell)[ci].state = TState::Blocked;
}

/// A `Ready` sibling context to run instead of parking the whole cell (round-robin
/// from the current context), or `None` if the current context is the only ready
/// one - exactly the futex scheduler's `pick_next`.
pub(crate) fn pick_ready_sibling(cell: usize) -> Option<usize> {
    pick_next(cell, cur_thread(cell))
}

/// Switch from the current (now-blocked) context to ready sibling `to`, saving the
/// caller's FP and loading `to`'s. The cell stays runnable; `Ctl::Switch` resumes
/// on `to`'s frame.
pub(crate) fn switch_current_to(cell: usize, to: usize) -> Ctl {
    switch_to(cell, cur_thread(cell), to)
}

/// Context `idx`'s proc-level block condition (`Block::None` if it is not blocked
/// on one - e.g. free, running, or futex-blocked).
pub(crate) fn pblock_of(cell: usize, idx: usize) -> Block {
    threads(cell)[idx].pblock
}

/// True if context `idx` is live and parked on a proc-level condition (not free,
/// not running, not a futex wait) - a candidate for the scheduler to wake.
pub(crate) fn is_pblocked(cell: usize, idx: usize) -> bool {
    let th = &threads(cell)[idx];
    th.state == TState::Blocked && !matches!(th.pblock, Block::None)
}

/// Make proc-blocked context `idx` current for an **intra-cell** resume (a sibling
/// wrote the fd it waited on), saving the outgoing context's FP and loading `idx`'s.
/// Leaves `idx`'s `pblock`/`state` for [`complete_pblock`] to finish. Returns its
/// frame for `Ctl::Switch`.
pub(crate) fn resume_pblocked(cell: usize, idx: usize) -> *mut TrapFrame {
    let from = cur_thread(cell);
    if from != idx {
        let from_fp = fp_ptr(cell, from);
        // SAFETY: `from`'s user FP registers are still live (kernel is soft-float).
        unsafe { arch::save_user_fp(from_fp) };
        let (to_fp, to_fs) = {
            let th = &threads(cell)[idx];
            (fp_ptr(cell, idx) as *const u8, th.fs_base)
        };
        // SAFETY: `idx`'s FP image was saved when it switched away.
        unsafe { arch::restore_user_fp(to_fp) };
        arch::set_user_fs_base(to_fs);
    }
    set_cur_thread(cell, idx);
    threads(cell)[idx].frame
}

/// Set the current context (for the **cross-cell** resume path in `proc.rs`, before
/// `restore_current` reloads its FP).
pub(crate) fn set_current(cell: usize, idx: usize) {
    set_cur_thread(cell, idx);
}

/// Finish resuming proc-blocked context `idx`: clear its `pblock` and mark it
/// `Ready`. The block's syscall return is written by `proc.rs` into `idx`'s frame.
pub(crate) fn clear_pblock_ready(cell: usize, idx: usize) {
    threads(cell)[idx].pblock = Block::None;
    threads(cell)[idx].state = TState::Ready;
}

/// Diagnostic: print each live context's state for the deadlock reporter, so a
/// multi-threaded deadlock says which contexts exist and what each waits on
/// (docs/ARCHITECTURE-DEBT.md 2.4).
pub fn dump_contexts(cell: usize) {
    let cur = cur_thread(cell);
    for i in 0..capacity(cell) {
        let th = &threads(cell)[i];
        let s = match th.state {
            TState::Free => continue,
            TState::Ready => "ready",
            TState::Blocked => "blocked-on-futex",
        };
        crate::println!(
            "linux:     ctx {i}{} tid {} {s} fut={:#x} deadline={}",
            if i == cur { " (current)" } else { "" },
            th.tid,
            th.fut_addr,
            th.fut_deadline,
        );
    }
}

/// The index of the currently running context (for the signal module's
/// per-context state, docs/LINUX-COMPAT.md L5).
pub fn current_context(cell: usize) -> usize {
    cur_thread(cell)
}

/// The context index whose tid is `tid`, if it is a live context of `cell`
/// (for `tgkill`/`tkill` self-targeting, docs/LINUX-COMPAT.md L5).
pub fn index_of_tid(cell: usize, tid: u32) -> Option<usize> {
    let n = capacity(cell);
    let t = threads(cell);
    (0..n).find(|&i| t[i].state != TState::Free && t[i].tid == tid)
}

/// The saved `TrapFrame` of context `idx` in `cell` (for delivering a signal to
/// a context that is not the current one).
pub fn frame_ptr(cell: usize, idx: usize) -> *mut TrapFrame {
    threads(cell)[idx].frame
}

/// The saved `TrapFrame` of `cell`'s currently running context - the frame the
/// process scheduler resumes when it switches into `cell` (docs/LINUX-COMPAT.md
/// L6).
pub fn current_frame(cell: usize) -> *mut TrapFrame {
    threads(cell)[cur_thread(cell)].frame
}

/// Save the live U-mode FP/SIMD state into `cell`'s current context's save area
/// (the outgoing half of a cross-cell process switch). The registers still hold
/// the outgoing thread's values (the kernel is soft-float).
pub fn save_current_fp(cell: usize) {
    let fp = fp_ptr(cell, cur_thread(cell));
    // SAFETY: `fp` is a 16-aligned 1 KiB area; the FP registers are live.
    unsafe { arch::save_user_fp(fp) };
}

/// Load `cell`'s current context's FP/SIMD state and TLS base (the incoming
/// half of a cross-cell process switch, docs/LINUX-COMPAT.md L6).
pub fn restore_current(cell: usize) {
    let (fp, fs) = {
        let th = &threads(cell)[cur_thread(cell)];
        (fp_ptr(cell, cur_thread(cell)) as *const u8, th.fs_base)
    };
    // SAFETY: `fp` is a valid FP image seeded at fork/clone or saved on a prior
    // switch; reloading the TLS base is a no-op off x86-64.
    unsafe { arch::restore_user_fp(fp) };
    arch::set_user_fs_base(fs);
}

/// Set up cell `cell`'s thread table for a **forked** process: a single running
/// context 0 whose frame is `frame` (the eager copy of the parent's calling
/// thread, already primed to return 0), tid `tid`, TLS base `fs_base` inherited
/// from the parent, and FP state seeded from the parent's live registers.
/// Returns the stored context-0 frame pointer for `user::install_forked`
/// (docs/LINUX-COMPAT.md L6).
pub fn init_forked(
    cell: usize,
    tid: u32,
    fs_base: u64,
    clear_child_tid: u64,
    frame: TrapFrame,
) -> *mut TrapFrame {
    // Funded tables start empty, and `child_frame_ptr` hands out a pointer *into*
    // the frame table - so it must be grown before the pointer is taken, not after.
    if !ensure_contexts(cell, INITIAL_CONTEXTS) && !ensure_contexts(cell, 1) {
        crate::println!(
            "linux: forked cell {cell} could not reserve an execution context - \
             the frame pool is exhausted below the metadata reserve"
        );
        return core::ptr::null_mut();
    }
    let cf = child_frame_ptr(cell, 0);
    // SAFETY: `cf` is stable kernel storage (FRAMES[cell][0], unused for the
    // main thread of a test-installed cell but the owned store for a forked one).
    unsafe { cf.write(frame) };
    let t = threads(cell);
    t.for_each_mut(|th| *th = Thread::new());
    t[0].frame = cf;
    t[0].state = TState::Ready;
    t[0].tid = tid;
    t[0].fs_base = fs_base;
    t[0].clear_child_tid = clear_child_tid;
    // Seed the child's FP image from the parent's live registers (the parent is
    // the running cell at fork time).
    // SAFETY: the FP area is 16-aligned and large enough.
    unsafe { arch::save_user_fp(fp_ptr(cell, 0)) };
    set_cur_thread(cell, 0);
    // SAFETY: single CPU.
    unsafe { (*addr_of_mut!(NEXT_TID))[cell] = tid + 1 };
    cf
}

/// The current context's x86-64 TLS base (inherited by a forked child).
pub fn current_fs_base(cell: usize) -> u64 {
    threads(cell)[cur_thread(cell)].fs_base
}

/// The current context's `clear_child_tid` (inherited by a forked child so its
/// own later thread-exit handshake still works).
pub fn current_clear_child_tid(cell: usize) -> u64 {
    threads(cell)[cur_thread(cell)].clear_child_tid
}

/// Re-seed cell `cell`'s thread table to a single context 0 after `execve`
/// replaces the image (docs/LINUX-COMPAT.md L6): the new program starts
/// single-threaded. `frame` is the fresh entry frame; `tid` is kept (the pid
/// does not change across `execve`).
pub fn reset_after_exec(cell: usize, tid: u32, frame: TrapFrame) -> *mut TrapFrame {
    init_forked(cell, tid, 0, 0, frame)
}

/// Whether context `idx` in `cell` is a live (non-free) context.
pub fn is_active(cell: usize, idx: usize) -> bool {
    threads(cell)[idx].state != TState::Free
}

/// Record the current context's `clear_child_tid` (from `set_tid_address`) and
/// return its tid.
pub fn set_tid_address(cell: usize, addr: u64) -> u32 {
    let i = cur_thread(cell);
    threads(cell)[i].clear_child_tid = addr;
    threads(cell)[i].tid
}

/// Record the current context's x86-64 TLS base (from `arch_prctl(SET_FS)`), so
/// it is reloaded when this context is scheduled again.
pub fn set_current_fs_base(cell: usize, addr: u64) {
    let i = cur_thread(cell);
    threads(cell)[i].fs_base = addr;
}

// clone(2) flag bits used here (uapi/linux/sched.h).
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
const CLONE_CHILD_SETTID: u64 = 0x0100_0000;

/// clone(flags, child_stack, parent_tid, child_tid, tls) - the pthread-create
/// shape (docs/LINUX-COMPAT.md L4). Creates a new context in the SAME address
/// space, primed to return 0 in the child with its own stack and TLS; returns
/// the new tid to the parent (which keeps running - no switch). `-EAGAIN` if
/// the per-cell context cap is reached.
///
/// Reading `parent_frame` (the saved state to clone) is the point of the call;
/// it is a valid frame for the synchronous trap (matching `on_user_trap`).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn clone(
    cell: usize,
    parent_frame: *mut TrapFrame,
    flags: u64,
    child_stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
) -> i64 {
    // A free slot among those that exist, else grow. This is where a cell's thread
    // count stops being a constant: growth is charged to the cell, and `-EAGAIN` -
    // the errno `pthread_create` surfaces - now means "this cell is out of budget"
    // rather than "the kernel was built with room for eight".
    let slot = match (1..capacity(cell)).find(|&i| threads(cell)[i].state == TState::Free) {
        Some(i) => i,
        None => {
            // Grow by exactly one and take *that* index. Deriving it from the
            // post-growth capacity instead would pick a slot the widest table
            // happens to have and the narrowest does not (see `capacity`).
            let want = capacity(cell).max(1) + 1;
            if !ensure_contexts(cell, want) {
                return -EAGAIN;
            }
            let slot = want - 1;
            debug_assert!(slot < capacity(cell), "grown slot is not addressable");
            slot
        }
    };
    // The grown slot must actually be free; if the growth arithmetic ever picked an
    // occupied one, a `clone` would silently overwrite a live context.
    if threads(cell)[slot].state != TState::Free {
        return -EAGAIN;
    }
    let tid = next_tid(cell);
    let cf = child_frame_ptr(cell, slot);
    // SAFETY: `parent_frame` is the caller's saved frame; `cf` is stable
    // kernel storage. Both valid for the synchronous trap.
    unsafe {
        let child = arch::clone_child_frame(&*parent_frame, child_stack, tls);
        cf.write(child);
    }
    let th = &mut threads(cell)[slot];
    th.frame = cf;
    th.state = TState::Ready;
    th.tid = tid;
    th.fs_base = tls; // x86-64 SETTLS; ignored elsewhere
    th.clear_child_tid = if flags & CLONE_CHILD_CLEARTID != 0 {
        child_tid
    } else {
        0
    };
    th.fut_addr = 0;
    th.fut_deadline = 0;
    // Give the child a valid FP image (x86 FXRSTOR needs a well-formed MXCSR);
    // the parent's current FP state is a valid one to seed with.
    // SAFETY: the FP area is 16-aligned and large enough.
    unsafe { arch::save_user_fp(fp_ptr(cell, slot)) };

    if flags & CLONE_PARENT_SETTID != 0 && parent_tid != 0 {
        // SAFETY: trap context; `parent_tid` is a writable VA in the cell.
        unsafe { (parent_tid as *mut i32).write(tid as i32) };
    }
    if flags & CLONE_CHILD_SETTID != 0 && child_tid != 0 {
        // SAFETY: CLONE_VM - the child shares this address space.
        unsafe { (child_tid as *mut i32).write(tid as i32) };
    }
    tid as i64
}

/// gettid == current context's tid.
pub fn gettid(cell: usize) -> u64 {
    current_tid(cell) as u64
}

// futex op bits (uapi/linux/futex.h). PRIVATE (128) and CLOCK_REALTIME (256)
// are masked off; the low bits select the command.
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;

/// futex(uaddr, op, val, timeout, ...) - FUTEX_WAIT/WAKE (+ the _BITSET variants
/// treated as plain WAIT/WAKE; the PRIVATE flag is ignored). WAIT re-checks the
/// word and parks the caller if it still equals `val`, switching to another ready
/// context; WAKE moves up to `val` parked waiters back to ready
/// (docs/LINUX-COMPAT.md L4).
///
/// **The timeout is honoured.** It used to be ignored entirely, so
/// `pthread_cond_timedwait` could wait forever - a hang whose cause was nowhere
/// near its symptom. `timeout_va` is a `struct timespec`: **relative** for
/// `FUTEX_WAIT`, **absolute** for `FUTEX_WAIT_BITSET` (in CLOCK_MONOTONIC unless
/// `FUTEX_CLOCK_REALTIME` is set) - which is the shape glibc's condition variables
/// actually use. An elapsed deadline returns `-ETIMEDOUT`; when nothing else in the
/// cell is runnable the CPU **parks** on it through the kernel timer arbiter, never
/// `arch::timer_*` directly (docs/ENGINEERING.md 3).
pub fn futex(cell: usize, uaddr: u64, op: u64, val: u32, timeout_va: u64) -> Ctl {
    let cmd = op & 0x7f;
    // A sibling's deadline may already have elapsed while this context ran.
    expire_timeouts(cell);
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // Re-check under the "lock" (single CPU, synchronous): if the word
            // no longer holds the expected value the caller must not block.
            // SAFETY: `uaddr` is a readable 32-bit word in the active cell
            // (bounded at the dispatch point, docs/ENGINEERING.md 12).
            let cur = unsafe { (uaddr as *const u32).read() };
            if cur != val {
                return Ctl::Ret((-EAGAIN) as u64);
            }
            let deadline = match wait_deadline(cmd, op, timeout_va) {
                Deadline::None => 0,
                Deadline::At(d) => d,
                Deadline::Passed => {
                    // Deadline already elapsed: honour the timeout immediately. This
                    // registers no arbiter deadline, so it is counted separately as
                    // evidence the timeout was honoured this way (task #162).
                    // SAFETY: single CPU, synchronous trap.
                    unsafe {
                        let p = addr_of_mut!(IMMEDIATE_TIMEOUTS);
                        *p = (*p).wrapping_add(1);
                    }
                    return Ctl::Ret((-ETIMEDOUT) as u64);
                }
            };
            let ci = cur_thread(cell);
            threads(cell)[ci].state = TState::Blocked;
            threads(cell)[ci].fut_addr = uaddr;
            threads(cell)[ci].fut_deadline = deadline;
            match next_runnable_or_wait(cell) {
                Some(next) => switch_to(cell, ci, next),
                None => {
                    // No `Ready` sibling and no futex deadline. A sibling parked on a
                    // proc-level condition may be satisfiable *right now* (Node's
                    // teardown: main wrote the eventfd, then futex-waits for the
                    // worker parked on it). Resume that sibling; it will `FUTEX_WAKE`
                    // this context. `ci` stays `Blocked` on its futex word.
                    if let Some(ctl) = crate::linux::proc::resume_satisfiable_sibling(cell) {
                        return ctl;
                    }
                    // No runnable sibling and none satisfiable, no deadline: this wait
                    // can never be satisfied from inside the cell - a deadlock.
                    // There is no futex errno for that, and every real caller
                    // (glibc's low-level locks, Rust's parker) ignores the return
                    // and re-reads the word - so the survivable answer is still
                    // "recheck". What changes is that the kernel no longer claims a
                    // *wakeup* happened (it returned 0, a lie that read as "someone
                    // woke you"): it reports -EAGAIN, says so once on the console,
                    // and counts it, so the spin is visible at its cause.
                    // Remaining work: with timer preemption + a cross-cell futex
                    // this becomes a real block (docs/CONCURRENCY.md, task #27).
                    threads(cell)[ci].state = TState::Ready;
                    threads(cell)[ci].fut_addr = 0;
                    threads(cell)[ci].fut_deadline = 0;
                    note_deadlock(uaddr);
                    Ctl::Ret((-EAGAIN) as u64)
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => Ctl::Ret(wake(cell, uaddr, val) as u64),
        _ => {
            crate::println!("linux: futex op {cmd} unsupported");
            Ctl::Ret((-ENOSYS) as u64)
        }
    }
}

/// What a futex wait's `timeout` argument resolved to.
enum Deadline {
    /// No timeout supplied: wait indefinitely.
    None,
    /// An absolute deadline in the cell's clock domain.
    At(u64),
    /// The supplied deadline is already in the past.
    Passed,
}

/// `FUTEX_CLOCK_REALTIME`: the absolute deadline is in CLOCK_REALTIME, not
/// CLOCK_MONOTONIC (uapi/linux/futex.h).
const FUTEX_CLOCK_REALTIME: u64 = 256;

/// Resolve a futex wait's `timeout` pointer to an absolute deadline in the cell's
/// clock domain. `FUTEX_WAIT` takes a **relative** timespec; `FUTEX_WAIT_BITSET`
/// an **absolute** one, in the clock `FUTEX_CLOCK_REALTIME` selects.
fn wait_deadline(cmd: u64, op: u64, timeout_va: u64) -> Deadline {
    if timeout_va == 0 {
        return Deadline::None;
    }
    // SAFETY: a `struct timespec` (two 64-bit words) bounded against the calling
    // cell's user VA range at the dispatch point (`ptr_args_ok`'s FUTEX row, which
    // validates arg 3 exactly for the two WAIT commands that reach here - for the
    // WAKE commands arg 3 is a count and is not treated as a pointer).
    let (secs, nsecs) = unsafe {
        let p = timeout_va as *const i64;
        (p.read(), p.add(1).read())
    };
    if secs < 0 || !(0..1_000_000_000).contains(&nsecs) {
        // An invalid timespec: treat it as no timeout rather than as a deadline in
        // the past. Linux answers -EINVAL; the dispatcher has no path for that here
        // and no caller in the suite sends one, so this stays conservative.
        return Deadline::None;
    }
    let ts = (secs as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsecs as u64);
    let mono = crate::linux::cell_clock_ns(false);
    if cmd == FUTEX_WAIT_BITSET {
        // Absolute, in the caller's chosen clock. Convert to the monotonic domain
        // the table stores by taking the *remaining* interval.
        let now = crate::linux::cell_clock_ns(op & FUTEX_CLOCK_REALTIME != 0);
        if ts <= now {
            return Deadline::Passed;
        }
        Deadline::At(mono.saturating_add(ts - now).max(1))
    } else {
        if ts == 0 {
            return Deadline::Passed;
        }
        Deadline::At(mono.saturating_add(ts).max(1))
    }
}

/// The nearest outstanding futex deadline among `cell`'s blocked contexts.
fn nearest_deadline(cell: usize) -> Option<u64> {
    let t = threads(cell);
    t.iter_values()
        .filter(|th| th.state == TState::Blocked && th.fut_deadline != 0)
        .map(|th| th.fut_deadline)
        .min()
}

/// Move every blocked context whose futex deadline has passed back to ready, with
/// `-ETIMEDOUT` as its futex return value. Returns how many timed out.
fn expire_timeouts(cell: usize) -> usize {
    let now = crate::linux::cell_clock_ns(false);
    let mut n = 0;
    for i in 0..capacity(cell) {
        let (due, frame) = {
            let th = &threads(cell)[i];
            (
                th.state == TState::Blocked && th.fut_deadline != 0 && now >= th.fut_deadline,
                th.frame,
            )
        };
        if due {
            threads(cell)[i].state = TState::Ready;
            threads(cell)[i].fut_addr = 0;
            threads(cell)[i].fut_deadline = 0;
            // SAFETY: `frame` is this context's saved state.
            arch::set_syscall_ret(unsafe { &mut *frame }, (-ETIMEDOUT) as u64);
            n += 1;
        }
    }
    n
}

/// Park slice for a futex deadline, in the **timer arbiter's** domain. The
/// deadline itself lives in the cell's clock domain, and the two are different
/// counters on RISC-V, so the wait cannot be a single hardware arm: it is a
/// sequence of bounded parks, each followed by a re-read of the cell clock. 1 ms is
/// long enough that the halt is worth taking and short enough that the overshoot
/// past a deadline stays small.
const FUTEX_PARK_SLICE_NS: u64 = 1_000_000;

/// The next context of `cell` that can run, parking on the nearest futex deadline
/// while none can. `None` means nothing will ever be runnable again - every
/// context is blocked on a futex with no deadline.
fn next_runnable_or_wait(cell: usize) -> Option<usize> {
    let from = cur_thread(cell);
    loop {
        expire_timeouts(cell);
        if let Some(next) = pick_next(cell, from) {
            return Some(next);
        }
        let deadline = nearest_deadline(cell)?;
        wait_until(deadline);
    }
}

/// Halt until the cell-domain deadline `deadline` passes. The hardware one-shot is
/// reached only through the kernel timer arbiter's own slot
/// ([`TimerClient::FutexWait`]), so this can never cancel another subsystem's
/// deadline (docs/ENGINEERING.md 3, docs/NETSTACK.md 16). Where no timer interrupt
/// is wired the arbiter refuses to halt and the deadline is honoured by comparison,
/// which is the honest fallback rather than a claimed park.
fn wait_until(deadline: u64) {
    while crate::linux::cell_clock_ns(false) < deadline {
        ktimer::register(TimerClient::FutexWait, FUTEX_PARK_SLICE_NS);
        if !ktimer::park(false) {
            // Nothing to wake a halt (or the slice was below the one-shot's
            // resolution): spin out this slice rather than halt with no wake source.
            arch::spin_loop(256);
        }
    }
    ktimer::cancel(TimerClient::FutexWait);
}

/// Console note + count for a futex wait that can never be satisfied. One line per
/// cell run, so a spinning program says so once instead of flooding the log.
fn note_deadlock(uaddr: u64) {
    // SAFETY: single CPU, synchronous traps.
    let n = unsafe {
        let c = &mut *addr_of_mut!(DEADLOCK_WAITS);
        *c += 1;
        *c
    };
    if n == 1 {
        crate::println!(
            "linux: futex WAIT at {uaddr:#x} with no runnable sibling and no timeout - \
             cannot be satisfied; reporting EAGAIN so the caller re-checks (see \
             docs/LINUX-COMPAT.md, the futex row)"
        );
    }
}

/// Futex waits that could never be satisfied since the last [`reset`]. Evidence a
/// test can assert on rather than infer (docs/ENGINEERING.md 1).
pub fn deadlock_waits() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of_mut!(DEADLOCK_WAITS) }
}

static mut DEADLOCK_WAITS: u64 = 0;

/// Futex waits whose deadline was **already in the past** when the syscall ran, so
/// `-ETIMEDOUT` was returned immediately without ever parking on the timer arbiter
/// ([`futex`]'s `Deadline::Passed` fast path).
///
/// This is a genuine, correct honouring of the timeout - not the ignored-timeout
/// bug - but it registers no arbiter deadline, so a test asserting "the timeout was
/// honoured by a real deadline mechanism" must accept **either** this counter or the
/// arbiter's `FutexWait` registrations. It is reachable whenever the cell's clock
/// advances past a short deadline before the futex syscall is serviced, which under
/// heavy parallel test load is routine (docs/ENGINEERING.md 1, task #162).
pub fn immediate_timeouts() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of_mut!(IMMEDIATE_TIMEOUTS) }
}

static mut IMMEDIATE_TIMEOUTS: u64 = 0;

/// Move up to `max` contexts parked on `uaddr` back to ready, setting each one's
/// futex return value to 0. Returns the number woken.
fn wake(cell: usize, uaddr: u64, max: u32) -> u32 {
    let mut woken = 0u32;
    for i in 0..capacity(cell) {
        if woken >= max {
            break;
        }
        let (blocked, frame) = {
            let th = &threads(cell)[i];
            (
                th.state == TState::Blocked && th.fut_addr == uaddr,
                th.frame,
            )
        };
        if blocked {
            threads(cell)[i].state = TState::Ready;
            threads(cell)[i].fut_addr = 0;
            threads(cell)[i].fut_deadline = 0;
            // SAFETY: `frame` is this context's saved state.
            arch::set_syscall_ret(unsafe { &mut *frame }, 0);
            woken += 1;
        }
    }
    woken
}

/// sched_yield: hand the CPU to the next ready context (if any), leaving the
/// caller ready. Returns 0.
///
/// A sibling **context** of this cell wins first (that is the L4 thread
/// scheduler). With no ready sibling the yield crosses to the next runnable
/// **process** instead of returning immediately: a yield that keeps running is
/// not a yield, and in a cooperative scheduler it left a forked child able to
/// starve its parent (docs/ARCHITECTURE-DEBT.md 4,
/// [`crate::linux::proc::yield_cell`]).
pub fn sched_yield(cell: usize) -> Ctl {
    expire_timeouts(cell);
    let ci = cur_thread(cell);
    // `pick_next` scans a full lap, so its last candidate is `ci` itself - the
    // running context is `Ready`, "currently running or waiting for its turn".
    // Every other caller reaches it with `ci` already `Blocked` or `Free`, so
    // only a yield can pick itself, and picking itself made the whole call a
    // no-op: `switch_to(cell, ci, ci)` saves and reloads one FP image and
    // returns the same frame. That is what hid the missing cross-cell yield
    // below - a single-threaded process never reached the `None` arm at all.
    match pick_next(cell, ci).filter(|&next| next != ci) {
        Some(next) => {
            // The caller stays ready and returns 0 when resumed.
            let frame = threads(cell)[ci].frame;
            arch::set_syscall_ret(unsafe { &mut *frame }, 0);
            switch_to(cell, ci, next)
        }
        None => crate::linux::proc::yield_cell(cell),
    }
}

/// exit(code): end the calling context. Runs the CHILD_CLEARTID handshake
/// (zero the tid word + futex-wake it, so a joiner wakes), frees the slot, and
/// switches to the next ready context. If it was the last context, the cell
/// ends with `code` (docs/LINUX-COMPAT.md L4).
pub fn exit_thread(cell: usize, code: u64) -> Ctl {
    let ci = cur_thread(cell);
    let cct = threads(cell)[ci].clear_child_tid;
    if cct != 0 {
        // SAFETY: trap context; `cct` is a writable word in the cell.
        unsafe { (cct as *mut u32).write(0) };
        wake(cell, cct, 1);
    }
    threads(cell)[ci].state = TState::Free;
    threads(cell)[ci].fut_addr = 0;
    threads(cell)[ci].fut_deadline = 0;
    // A sibling parked on a futex *with a deadline* is not gone - the process is
    // over only when nothing can become runnable again.
    match next_runnable_or_wait(cell) {
        // The exiting context's FP state is gone; just load the successor's.
        Some(next) => resume(cell, next),
        // Last thread of the cell: this is a whole-process exit. Route through
        // the process scheduler (zombie + reap by the parent, or unwind if this
        // is the top cell) - docs/LINUX-COMPAT.md L6.
        None => crate::linux::proc::exit_group(cell, code),
    }
}

/// Round-robin: the next `Ready` context after `from`, or None.
fn pick_next(cell: usize, from: usize) -> Option<usize> {
    let n = capacity(cell);
    if n == 0 {
        return None;
    }
    let t = threads(cell);
    (1..=n).find_map(|k| {
        let i = (from + k) % n;
        (t[i].state == TState::Ready).then_some(i)
    })
}

/// Switch from context `from` to `to`: save `from`'s FP, load `to`'s FP and TLS
/// base, and hand the trampoline `to`'s frame.
fn switch_to(cell: usize, from: usize, to: usize) -> Ctl {
    let from_fp = fp_ptr(cell, from);
    // SAFETY: `from`'s user FP registers are still live (kernel is soft-float).
    unsafe { arch::save_user_fp(from_fp) };
    resume(cell, to)
}

/// Make context `to` current: load its FP state and TLS base and return its
/// frame. Used both by `switch_to` (after saving the outgoing FP) and on a
/// context exit (nothing to save).
fn resume(cell: usize, to: usize) -> Ctl {
    let (to_fp, to_fs, to_frame) = {
        let th = &threads(cell)[to];
        (fp_ptr(cell, to) as *const u8, th.fs_base, th.frame)
    };
    // SAFETY: `to_fp` is a valid FP image (seeded at clone or saved on a prior
    // switch). Reloading the TLS base is a no-op off x86-64.
    unsafe { arch::restore_user_fp(to_fp) };
    arch::set_user_fs_base(to_fs);
    set_cur_thread(cell, to);
    Ctl::Switch(to_frame)
}
