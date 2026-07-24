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
use crate::linux::Ctl;
use crate::linux::errno::*;
use crate::user::{self, MAX_CELLS};
use core::ptr::addr_of_mut;

/// Maximum execution contexts per cell (docs/LINUX-COMPAT.md L4). A program
/// spawning more than this gets `-EAGAIN` from `clone` (glibc surfaces it as
/// `pthread_create` returning `EAGAIN`). Small and fixed so the kernel stays
/// allocation-free.
pub const MAX_THREADS: usize = 8;

/// Per-context FP/SIMD save area. Sized above the largest per-ISA image
/// (x86 FXSAVE 512, ARM64 V-regs+FPSR/FPCR 528, RISC-V f-regs+fcsr 264) and
/// 16-aligned (FXSAVE/`stp q` require it).
#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct FpArea([u8; 1024]);

impl FpArea {
    const fn new() -> FpArea {
        FpArea([0; 1024])
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
    fp: FpArea,
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
            fp: FpArea::new(),
        }
    }
}

static mut THREADS: [[Thread; MAX_THREADS]; MAX_CELLS] =
    [const { [const { Thread::new() }; MAX_THREADS] }; MAX_CELLS];
/// Kernel-owned frames for clone-created contexts (index 0 unused - context 0
/// reuses the cell's installed frame).
static mut FRAMES: [[TrapFrame; MAX_THREADS]; MAX_CELLS] =
    [const { [const { arch::trapframe_zeroed() }; MAX_THREADS] }; MAX_CELLS];
static mut CUR_THREAD: [usize; MAX_CELLS] = [0; MAX_CELLS];
static mut NEXT_TID: [u32; MAX_CELLS] = [1001; MAX_CELLS];

fn threads(cell: usize) -> &'static mut [Thread; MAX_THREADS] {
    // SAFETY: single CPU, synchronous traps; one context runs at a time.
    unsafe { &mut (*addr_of_mut!(THREADS))[cell] }
}

fn child_frame_ptr(cell: usize, i: usize) -> *mut TrapFrame {
    // SAFETY: stable address of static storage; single CPU.
    unsafe { addr_of_mut!((*addr_of_mut!(FRAMES))[cell][i]) }
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
    let t = threads(cell);
    for th in t.iter_mut() {
        *th = Thread::new();
    }
    t[0].frame = f0;
    t[0].state = TState::Ready;
    t[0].tid = 1000; // main thread tid == tgid (getpid)
    set_cur_thread(cell, 0);
    // SAFETY: single CPU.
    unsafe { (*addr_of_mut!(NEXT_TID))[cell] = 1001 };
}

/// Clear every cell's thread table (called from `linux::reset`).
pub fn reset() {
    for cell in 0..MAX_CELLS {
        for th in threads(cell).iter_mut() {
            *th = Thread::new();
        }
        set_cur_thread(cell, 0);
    }
}

/// The tid of the currently running context (`gettid`).
pub fn current_tid(cell: usize) -> u32 {
    threads(cell)[cur_thread(cell)].tid
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
    let Some(slot) = (1..MAX_THREADS).find(|&i| threads(cell)[i].state == TState::Free) else {
        return -EAGAIN;
    };
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
    // Give the child a valid FP image (x86 FXRSTOR needs a well-formed MXCSR);
    // the parent's current FP state is a valid one to seed with.
    // SAFETY: the FP area is 16-aligned and large enough.
    unsafe { arch::save_user_fp(th.fp.0.as_mut_ptr()) };

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

/// futex(uaddr, op, val, ...) - FUTEX_WAIT/WAKE (+ the _BITSET variants treated
/// as plain WAIT/WAKE; the PRIVATE flag is ignored). WAIT re-checks the word
/// and parks the caller if it still equals `val`, switching to another ready
/// context; WAKE moves up to `val` parked waiters back to ready
/// (docs/LINUX-COMPAT.md L4). Any timeout is treated as infinite (cooperative
/// model; documented).
pub fn futex(cell: usize, uaddr: u64, op: u64, val: u32) -> Ctl {
    let cmd = op & 0x7f;
    match cmd {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // Re-check under the "lock" (single CPU, synchronous): if the word
            // no longer holds the expected value the caller must not block.
            // SAFETY: `uaddr` is a readable 32-bit word in the active cell.
            let cur = unsafe { (uaddr as *const u32).read() };
            if cur != val {
                return Ctl::Ret((-EAGAIN) as u64);
            }
            let ci = cur_thread(cell);
            match pick_next(cell, ci) {
                Some(next) => {
                    threads(cell)[ci].state = TState::Blocked;
                    threads(cell)[ci].fut_addr = uaddr;
                    // The caller's return value (0) is written when it is woken.
                    switch_to(cell, ci, next)
                }
                // Nobody else can run: blocking would be a deadlock, so treat
                // this as a spurious wake (glibc re-checks and loops).
                None => Ctl::Ret(0),
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => Ctl::Ret(wake(cell, uaddr, val) as u64),
        _ => {
            crate::println!("linux: futex op {cmd} unsupported");
            Ctl::Ret((-ENOSYS) as u64)
        }
    }
}

/// Move up to `max` contexts parked on `uaddr` back to ready, setting each one's
/// futex return value to 0. Returns the number woken.
fn wake(cell: usize, uaddr: u64, max: u32) -> u32 {
    let mut woken = 0u32;
    for i in 0..MAX_THREADS {
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
            // SAFETY: `frame` is this context's saved state.
            arch::set_syscall_ret(unsafe { &mut *frame }, 0);
            woken += 1;
        }
    }
    woken
}

/// sched_yield: hand the CPU to the next ready context (if any), leaving the
/// caller ready. Returns 0.
pub fn sched_yield(cell: usize) -> Ctl {
    let ci = cur_thread(cell);
    match pick_next(cell, ci) {
        Some(next) => {
            // The caller stays ready and returns 0 when resumed.
            let frame = threads(cell)[ci].frame;
            arch::set_syscall_ret(unsafe { &mut *frame }, 0);
            switch_to(cell, ci, next)
        }
        None => Ctl::Ret(0),
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
    match pick_next(cell, ci) {
        // The exiting context's FP state is gone; just load the successor's.
        Some(next) => resume(cell, next),
        None => Ctl::Exit(code),
    }
}

/// Round-robin: the next `Ready` context after `from`, or None.
fn pick_next(cell: usize, from: usize) -> Option<usize> {
    let t = threads(cell);
    (1..=MAX_THREADS).find_map(|k| {
        let i = (from + k) % MAX_THREADS;
        (t[i].state == TState::Ready).then_some(i)
    })
}

/// Switch from context `from` to `to`: save `from`'s FP, load `to`'s FP and TLS
/// base, and hand the trampoline `to`'s frame.
fn switch_to(cell: usize, from: usize, to: usize) -> Ctl {
    let from_fp = threads(cell)[from].fp.0.as_mut_ptr();
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
        (th.fp.0.as_ptr(), th.fs_base, th.frame)
    };
    // SAFETY: `to_fp` is a valid FP image (seeded at clone or saved on a prior
    // switch). Reloading the TLS base is a no-op off x86-64.
    unsafe { arch::restore_user_fp(to_fp) };
    arch::set_user_fs_base(to_fs);
    set_cur_thread(cell, to);
    Ctl::Switch(to_frame)
}
