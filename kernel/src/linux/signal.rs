//! Synthesized POSIX signal delivery for a Linux-personality cell
//! (docs/LINUX-COMPAT.md L5). Like the fd table and thread state, this adds **no
//! kernel object** (docs/LINUX-COMPAT.md 1): dispositions, masks, pending sets,
//! and altstacks are per-cell / per-context synthesized state in this module.
//!
//! Delivery is a **saved-`TrapFrame` rewrite** in trap context - the mechanism
//! the personality already has (it runs where the cell's registers and user
//! memory are live). A signal builds a Linux `rt_sigframe` on the user stack
//! (via `arch::setup_rt_frame`), points the frame's PC at the handler and its
//! return address at the restorer (glibc's `SA_RESTORER` on x86-64, or the
//! injected 2-instruction trampoline page on ARM64/RISC-V), and applies the
//! handler's mask; `rt_sigreturn` restores the frame and mask. Synchronous
//! faults (SIGSEGV/SIGBUS/SIGILL/SIGFPE) route through the same path from
//! `user::on_user_trap` when a Linux cell has an installed, unblocked handler;
//! otherwise the default disposition terminates the cell reporting 128+signo.
//!
//! Scope: delivery is to self (`raise`/`tgkill`/`tkill`), to synchronous
//! faults, and - since docs/ARCHITECTURE-DEBT.md 4 - to **another process** via
//! `kill`. A cross-process signal cannot be delivered where it is sent: the
//! rewrite needs the *target's* saved frame and the target's user stack, so the
//! target's address space must be active. It is therefore recorded pending and
//! delivered by [`on_resume`], which the process scheduler calls at the one
//! moment those conditions hold - just after it switches into the target.
//! Cross-*thread* targeting of a non-running context is still recorded pending
//! only (no fixture needs it; documented in LINUX-COMPAT.md).

use crate::arch::{self, SigFrameSpec, TrapFrame};
use crate::linux::errno::*;
use crate::linux::thread::{self, MAX_THREADS};
use crate::linux::{Ctl, err, ret};
use crate::user::MAX_CELLS;
use core::ptr::addr_of_mut;

/// Maximum execution contexts per cell (mirrors `thread::MAX_THREADS`), the
/// dimension of the per-context signal state.
const MAX_CONTEXTS: usize = MAX_THREADS;

/// Number of signals (1..=NSIG). Masks are `u64` (signal n uses bit n-1).
const NSIG: usize = 64;

// Well-known signal numbers (asm-generic / x86 agree for these).
const SIGILL: u32 = 4;
const SIGBUS: u32 = 7;
const SIGFPE: u32 = 8;
const SIGKILL: u32 = 9;
const SIGSEGV: u32 = 11;
const SIGCHLD: u32 = 17;
const SIGSTOP: u32 = 19;
const SIGURG: u32 = 23;
const SIGWINCH: u32 = 28;

// Handler sentinels and sa_flags bits (uapi/asm-generic/signal.h).
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_NODEFER: u64 = 0x4000_0000;

// rt_sigprocmask `how`.
const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

// sigaltstack ss_flags.
const SS_ONSTACK: i32 = 1;
const SS_DISABLE: i32 = 2;

// siginfo si_code for a synchronous SIGSEGV (address not mapped).
const SEGV_MAPERR: i32 = 1;

/// A process-wide signal disposition (docs/LINUX-COMPAT.md L5). SIG_DFL is
/// handler 0, SIG_IGN is handler 1; anything else is a user handler VA.
#[derive(Copy, Clone)]
struct SigAction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

impl SigAction {
    const fn dfl() -> SigAction {
        SigAction {
            handler: SIG_DFL,
            flags: 0,
            restorer: 0,
            mask: 0,
        }
    }
}

/// Per-context signal state: the blocked mask, the pending set, and the
/// alternate signal stack (sigaltstack).
#[derive(Copy, Clone)]
struct SigCtx {
    blocked: u64,
    pending: u64,
    alt_sp: u64,
    alt_size: u64,
    alt_active: bool,
}

impl SigCtx {
    const fn new() -> SigCtx {
        SigCtx {
            blocked: 0,
            pending: 0,
            alt_sp: 0,
            alt_size: 0,
            alt_active: false,
        }
    }
}

/// Dispositions are process-wide (per cell); masks/pending/altstack are
/// per-context. Fixed-size so the kernel stays allocation-free.
static mut ACTIONS: [[SigAction; NSIG + 1]; MAX_CELLS] =
    [const { [const { SigAction::dfl() }; NSIG + 1] }; MAX_CELLS];
static mut CTXS: [[SigCtx; MAX_CONTEXTS]; MAX_CELLS] =
    [const { [const { SigCtx::new() }; MAX_CONTEXTS] }; MAX_CELLS];

fn actions(cell: usize) -> &'static mut [SigAction; NSIG + 1] {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut (*addr_of_mut!(ACTIONS))[cell] }
}

fn ctx(cell: usize, idx: usize) -> &'static mut SigCtx {
    // SAFETY: single CPU, synchronous traps.
    unsafe { &mut (*addr_of_mut!(CTXS))[cell][idx] }
}

/// Clear all per-cell signal state (called from `linux::reset`).
pub fn reset() {
    for c in 0..MAX_CELLS {
        *actions(c) = [SigAction::dfl(); NSIG + 1];
        for i in 0..MAX_CONTEXTS {
            *ctx(c, i) = SigCtx::new();
        }
    }
}

fn bit(signo: u32) -> u64 {
    1u64 << ((signo - 1) as u64)
}

/// Copy cell `from`'s signal dispositions and context-0 mask/altstack into cell
/// `to` - the `fork` inheritance step (docs/LINUX-COMPAT.md L6): a child inherits
/// the parent's handlers and blocked mask.
pub fn fork_copy(from: usize, to: usize) {
    *actions(to) = *actions(from);
    *ctx(to, 0) = *ctx(from, 0);
    for i in 1..MAX_CONTEXTS {
        *ctx(to, i) = SigCtx::new();
    }
}

/// Reset cell `cell`'s dispositions across `execve` (docs/LINUX-COMPAT.md L6):
/// caught signals revert to the default; ignored/default dispositions are kept
/// (POSIX). The blocked mask is preserved (context 0), the pending set cleared.
pub fn exec_reset(cell: usize) {
    let a = actions(cell);
    for act in a.iter_mut() {
        if act.handler != SIG_DFL && act.handler != SIG_IGN {
            *act = SigAction::dfl();
        }
    }
    ctx(cell, 0).pending = 0;
    ctx(cell, 0).alt_sp = 0;
    ctx(cell, 0).alt_size = 0;
    ctx(cell, 0).alt_active = false;
    for i in 1..MAX_CONTEXTS {
        *ctx(cell, i) = SigCtx::new();
    }
}

/// Is `signo`'s default disposition to terminate the process? (Everything
/// except the "ignore by default" set: SIGCHLD, SIGURG, SIGWINCH.)
fn default_terminates(signo: u32) -> bool {
    !matches!(signo, SIGCHLD | SIGURG | SIGWINCH)
}

// ------------------------------------------------------------- rt_sigaction

/// rt_sigaction(signo, act, oldact, sigsetsize): store a disposition, return the
/// old one. SIGKILL/SIGSTOP cannot be caught or ignored (-EINVAL). The kernel
/// `struct sigaction` layout is ISA-specific: x86-64 has an `sa_restorer` field
/// (`arch::SIGACTION_HAS_RESTORER`), the asm-generic ISAs do not.
pub fn rt_sigaction(cell: usize, signo: u64, act: u64, oldact: u64, _sigsetsize: u64) -> i64 {
    if signo == 0 || signo as usize > NSIG {
        return -EINVAL;
    }
    let s = signo as u32;
    if act != 0 && (s == SIGKILL || s == SIGSTOP) {
        return -EINVAL;
    }
    // Field offsets: handler@0, flags@8; then restorer@16 + mask@24 (x86-64) or
    // mask@16 (asm-generic).
    let (mask_off, has_restorer) = if arch::SIGACTION_HAS_RESTORER {
        (24u64, true)
    } else {
        (16u64, false)
    };
    if oldact != 0 {
        let a = actions(cell)[signo as usize];
        // SAFETY: `oldact` is a writable `struct sigaction` in the active cell.
        unsafe {
            (oldact as *mut u64).write(a.handler);
            ((oldact + 8) as *mut u64).write(a.flags);
            if has_restorer {
                ((oldact + 16) as *mut u64).write(a.restorer);
            }
            ((oldact + mask_off) as *mut u64).write(a.mask);
        }
    }
    if act != 0 {
        // SAFETY: `act` is a readable `struct sigaction` in the active cell.
        let (handler, flags, restorer, mask) = unsafe {
            let handler = (act as *const u64).read();
            let flags = ((act + 8) as *const u64).read();
            let restorer = if has_restorer {
                ((act + 16) as *const u64).read()
            } else {
                0
            };
            let mask = ((act + mask_off) as *const u64).read();
            (handler, flags, restorer, mask)
        };
        actions(cell)[signo as usize] = SigAction {
            handler,
            flags,
            restorer,
            mask,
        };
    }
    0
}

// ----------------------------------------------------------- rt_sigprocmask

/// rt_sigprocmask(how, set, oldset, sigsetsize): read/modify the current
/// context's blocked mask. Unblocking may make a pending signal deliverable, so
/// this returns a `Ctl` (it can deliver or terminate before returning).
pub fn rt_sigprocmask(cell: usize, how: u64, set: u64, oldset: u64, frame: *mut TrapFrame) -> Ctl {
    let idx = thread::current_context(cell);
    if oldset != 0 {
        let cur = ctx(cell, idx).blocked;
        // SAFETY: `oldset` is a writable sigset_t in the active cell.
        unsafe { (oldset as *mut u64).write(cur) };
    }
    if set != 0 {
        // SAFETY: `set` is a readable sigset_t in the active cell.
        let new = unsafe { (set as *const u64).read() };
        let c = ctx(cell, idx);
        c.blocked = match how {
            SIG_BLOCK => c.blocked | new,
            SIG_UNBLOCK => c.blocked & !new,
            SIG_SETMASK => new,
            _ => return err(EINVAL),
        };
        // SIGKILL/SIGSTOP are never blockable.
        c.blocked &= !(bit(SIGKILL) | bit(SIGSTOP));
        if let Some(ctl) = check_pending_current(cell, frame) {
            return ctl;
        }
    }
    ret(0)
}

// ------------------------------------------------------------- sigaltstack

/// sigaltstack(ss, old): set/query the current context's alternate signal
/// stack. Honored for SA_ONSTACK handlers (docs/LINUX-COMPAT.md L5).
pub fn sigaltstack(cell: usize, ss: u64, old: u64) -> i64 {
    let idx = thread::current_context(cell);
    if old != 0 {
        let c = *ctx(cell, idx);
        let flags = if c.alt_active {
            SS_ONSTACK
        } else if c.alt_size == 0 {
            SS_DISABLE
        } else {
            0
        };
        // SAFETY: `old` is a writable `stack_t` { ss_sp, ss_flags, ss_size }.
        unsafe {
            (old as *mut u64).write(c.alt_sp);
            ((old + 8) as *mut i32).write(flags);
            ((old + 16) as *mut u64).write(c.alt_size);
        }
    }
    if ss != 0 {
        // SAFETY: `ss` is a readable `stack_t`.
        let (sp, flags, size) = unsafe {
            (
                (ss as *const u64).read(),
                ((ss + 8) as *const i32).read(),
                ((ss + 16) as *const u64).read(),
            )
        };
        let c = ctx(cell, idx);
        if c.alt_active {
            return -EPERM; // cannot change the stack while executing on it
        }
        if flags & SS_DISABLE != 0 {
            c.alt_sp = 0;
            c.alt_size = 0;
        } else {
            c.alt_sp = sp;
            c.alt_size = size;
        }
    }
    0
}

// ------------------------------------------------------------- rt_sigreturn

/// rt_sigreturn: restore the interrupted `TrapFrame` and signal mask saved on
/// the user stack by delivery, then resume where the signal interrupted.
/// Dereferencing `frame` (the current context's saved state) is the point.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn rt_sigreturn(cell: usize, frame: *mut TrapFrame) -> Ctl {
    let idx = thread::current_context(cell);
    // SAFETY: `frame` is the current context's saved state; the ucontext it
    // points at was written by `arch::setup_rt_frame`.
    let mask = unsafe { arch::restore_rt_frame(&mut *frame) };
    let c = ctx(cell, idx);
    c.blocked = mask;
    c.alt_active = false;
    Ctl::Switch(frame)
}

// ------------------------------------------------------------- kill family

/// Map a `FaultCause` to the fatal signal it raises (docs/LINUX-COMPAT.md L5).
fn cause_signo(cause: arch::FaultCause) -> u32 {
    match cause {
        arch::FaultCause::Segv => SIGSEGV,
        arch::FaultCause::Bus => SIGBUS,
        arch::FaultCause::Ill => SIGILL,
        arch::FaultCause::Fpe => SIGFPE,
    }
}

/// kill(pid, sig): signal a process (docs/ARCHITECTURE-DEBT.md 4).
///
/// This used to refuse any pid but the caller's own with `-ESRCH`, and to answer
/// `kill(0, sig)` / `kill(-1, sig)` by **silently delivering to the caller** -
/// the "signal my children" a supervisor is actually asking for, reported as
/// done and delivered to the wrong process. Four target forms now:
///
/// - `pid > 0` - that process. Itself is delivered inline (the frame is right
///   here); another is posted and delivered on its next resume ([`on_resume`]).
/// - `pid == 0` - the caller's process group. There is no `setpgid` here, so
///   every live process genuinely *is* in the initial group, the caller
///   included.
/// - `pid == -1` - every process the caller may signal **except init**, per
///   `kill(2)`. The top of the process tree is the only process with no parent,
///   so that is what stands in for init.
/// - any other negative - a process group that does not exist here. Refused
///   `-ESRCH` rather than quietly redirected to the caller.
///
/// `sig == 0` is an existence probe in every form: no delivery, but a real
/// answer (`0` or `-ESRCH`), which is how `kill(pid, 0)` is used to tell a live
/// child from a reaped one.
pub fn kill(cell: usize, pid: i64, sig: u64, frame: *mut TrapFrame) -> Ctl {
    use crate::linux::proc;
    if sig as usize > NSIG {
        return err(EINVAL);
    }
    let mypid = proc::pid(cell) as i64;

    // A single named process.
    if pid > 0 {
        if pid == mypid {
            return deliver_or_signal(cell, thread::current_context(cell), sig, frame, 0, 0);
        }
        if sig == 0 {
            return if proc::pid_exists(pid as u32) {
                ret(0)
            } else {
                err(ESRCH)
            };
        }
        let Some(target) = proc::cell_of_pid(pid as u32) else {
            return err(ESRCH);
        };
        return post_remote(target, sig as u32);
    }

    // A negative pid other than -1 names a process group. Groups do not exist
    // here (no `setpgid`), so there is nothing to signal - and answering the
    // caller's own process instead, as this used to, is the worst possible lie.
    if pid < -1 {
        return err(ESRCH);
    }

    // 0 = the caller's group (every live process, caller included);
    // -1 = every live process except init (the top of the tree).
    let skip_top = pid == -1;
    let mut targets = 0usize;
    let mut self_targeted = false;
    proc::for_each_live(skip_top, |i| {
        targets += 1;
        if i == cell {
            self_targeted = true;
        } else if sig != 0 {
            // Ignore the per-target status: a fan-out reports the *set*, and a
            // single unsignalable member is not the caller's error.
            let _ = post_remote(i, sig as u32);
        }
    });
    if targets == 0 {
        return err(ESRCH);
    }
    if self_targeted && sig != 0 {
        return deliver_or_signal(cell, thread::current_context(cell), sig, frame, 0, 0);
    }
    ret(0)
}

/// Post `signo` to another live process, resolving the disposition against
/// **that process's** table (dispositions are per-cell), and return the `kill`
/// result.
///
/// Delivery itself cannot happen here - the target's address space is not
/// active, so its stack cannot be written and its frame must not be rewritten
/// against the wrong page tables. What happens here is the decision; the frame
/// rewrite happens in [`on_resume`].
fn post_remote(target: usize, signo: u32) -> Ctl {
    let act = actions(target)[signo as usize];
    // Ignored outright: nothing is recorded, and that is a success.
    if act.handler == SIG_IGN || (act.handler == SIG_DFL && !default_terminates(signo)) {
        return ret(0);
    }
    // A fatal default kills the target now. It is not running, so there is no
    // frame to rewrite and nothing to reschedule - the process simply becomes a
    // zombie its parent can reap. A dead *top* cell cannot be a zombie; that is
    // reported and the run ends when the scheduler next reaches it.
    if act.handler == SIG_DFL {
        if crate::linux::proc::mark_signaled_remote(target, signo) {
            crate::println!("linux: kill: signal {signo} terminated the top process");
        }
        return ret(0);
    }
    // A real handler: record it and let the scheduler deliver it. Context 0 is
    // the process's main context - the one a cross-*process* signal targets.
    ctx(target, 0).pending |= bit(signo);
    ret(0)
}

/// What [`on_resume`] did.
pub enum Resumed {
    /// Nothing pending, or a handler frame was built - resume the frame.
    Ran,
    /// A pending signal whose disposition is a fatal default: the process must
    /// terminate with this signal instead of resuming.
    Fatal(u32),
}

/// Deliver a signal that arrived while `cell` was not running, called by the
/// process scheduler immediately after it switches into `cell`
/// (docs/ARCHITECTURE-DEBT.md 4).
///
/// This is the only place a cross-process signal *can* be delivered: building a
/// `rt_sigframe` writes the target's user stack and rewrites the target's saved
/// frame, both of which need the target's address space active.
///
/// Unlike [`check_pending_current`] this does **not** overwrite the syscall
/// return register. The caller has just completed the interrupted syscall's
/// block, so the frame already carries the value the program must see when
/// `rt_sigreturn` restores it; clobbering it with 0 would turn a completed read
/// into a spurious end-of-file.
pub fn on_resume(cell: usize) -> Resumed {
    let idx = thread::current_context(cell);
    let deliverable = ctx(cell, idx).pending & !ctx(cell, idx).blocked;
    if deliverable == 0 {
        return Resumed::Ran;
    }
    for s in 1..=NSIG as u32 {
        if deliverable & bit(s) == 0 {
            continue;
        }
        let act = actions(cell)[s as usize];
        if act.handler == SIG_IGN || (act.handler == SIG_DFL && !default_terminates(s)) {
            ctx(cell, idx).pending &= !bit(s);
            continue;
        }
        if act.handler == SIG_DFL {
            ctx(cell, idx).pending &= !bit(s);
            return Resumed::Fatal(s);
        }
        let frame = thread::frame_ptr(cell, idx);
        // SAFETY: `frame` is `cell`'s current-context saved state and `cell`'s
        // address space is active (the scheduler just switched into it), so the
        // user stack `build_frame` writes is mapped.
        unsafe { build_frame(cell, idx, s, &act, &mut *frame, 0, 0) };
        return Resumed::Ran;
    }
    Resumed::Ran
}

/// tgkill(tgid, tid, sig): deliver to a thread of this process (self only).
pub fn tgkill(cell: usize, tgid: i64, tid: i64, sig: u64, frame: *mut TrapFrame) -> Ctl {
    if tgid != crate::linux::proc::pid(cell) as i64 {
        return err(ESRCH);
    }
    tkill(cell, tid, sig, frame)
}

/// tkill(tid, sig): deliver to a thread of this process by tid.
pub fn tkill(cell: usize, tid: i64, sig: u64, frame: *mut TrapFrame) -> Ctl {
    let Some(idx) = thread::index_of_tid(cell, tid as u32) else {
        return err(ESRCH);
    };
    deliver_or_signal(cell, idx, sig, frame, 0, 0)
}

/// rt_sigqueueinfo(tgid, sig, uinfo): deliver to self, carrying the caller's
/// si_code (the rest of the queued siginfo is not reconstructed at L5).
pub fn rt_sigqueueinfo(cell: usize, tgid: i64, sig: u64, uinfo: u64, frame: *mut TrapFrame) -> Ctl {
    if tgid != crate::linux::proc::pid(cell) as i64 {
        return err(ESRCH);
    }
    // SAFETY: `uinfo` (if given) is a readable siginfo; si_code is at +8.
    let si_code = if uinfo != 0 {
        unsafe { ((uinfo + 8) as *const i32).read() }
    } else {
        0
    };
    deliver_or_signal(cell, thread::current_context(cell), sig, frame, si_code, 0)
}

/// rt_sigtimedwait: no signal is ever pending-and-waited here (the fixtures do
/// not block in it); report the wait as interrupted so callers loop or bail
/// rather than hang. Documented in LINUX-COMPAT.md.
pub fn rt_sigtimedwait() -> i64 {
    -EAGAIN
}

// --------------------------------------------------------------- delivery

/// The outcome of a synchronous-fault signal decision (docs/LINUX-COMPAT.md L5).
pub enum FaultOutcome {
    /// Resume the (rewritten) frame at the handler.
    Resume(*mut TrapFrame),
    /// No catchable handler: terminate the process with this signal (the caller
    /// routes it through `proc::exit_signaled` - a zombie for a forked child,
    /// or an unwind reporting 128+signo for the top cell, docs/LINUX-COMPAT.md
    /// L6).
    Terminate(u32),
}

/// The fault hook called from `user::on_user_trap` for a Linux cell. Delivers a
/// synchronous signal to an installed, unblocked handler by frame rewrite;
/// otherwise terminates (SIG_DFL / SIG_IGN / blocked synchronous signal all
/// force termination, matching Linux). Dereferencing `frame` (the faulting
/// context's saved state) is the point of the call.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn deliver_fault(
    cell: usize,
    cause: arch::FaultCause,
    fault_addr: usize,
    frame: *mut TrapFrame,
) -> FaultOutcome {
    let signo = cause_signo(cause);
    let idx = thread::current_context(cell);
    let act = actions(cell)[signo as usize];
    let catchable = act.handler != SIG_DFL && act.handler != SIG_IGN;
    let blocked = ctx(cell, idx).blocked & bit(signo) != 0;
    if catchable && !blocked {
        // SAFETY: `frame` is the faulting context's saved state; the cell root
        // is active, so the user stack is writable.
        unsafe {
            build_frame(
                cell,
                idx,
                signo,
                &act,
                &mut *frame,
                SEGV_MAPERR,
                fault_addr as u64,
            )
        };
        FaultOutcome::Resume(frame)
    } else {
        FaultOutcome::Terminate(signo)
    }
}

/// The shared kill/raise path: decide the disposition for `signo` delivered to
/// context `idx`, and either deliver it now (frame rewrite of the current
/// context), record it pending, ignore it, or terminate the cell.
fn deliver_or_signal(
    cell: usize,
    idx: usize,
    sig: u64,
    frame: *mut TrapFrame,
    si_code: i32,
    si_addr: u64,
) -> Ctl {
    if sig == 0 {
        return ret(0); // sig 0: existence probe, no delivery
    }
    if sig as usize > NSIG {
        return err(EINVAL);
    }
    let signo = sig as u32;
    let act = actions(cell)[signo as usize];
    let cur = thread::current_context(cell);

    // SIG_IGN, or SIG_DFL with an "ignore" default: nothing happens.
    if act.handler == SIG_IGN || (act.handler == SIG_DFL && !default_terminates(signo)) {
        return ret(0);
    }
    // SIG_DFL with a "terminate" default: the process dies now (a zombie for a
    // forked child, an unwind for the top cell, docs/LINUX-COMPAT.md L6).
    if act.handler == SIG_DFL {
        return crate::linux::proc::exit_signaled(cell, signo);
    }
    // A real handler. If the target is not the running context, or the signal
    // is blocked there, record it pending (delivered when next runnable/
    // unblocked). Otherwise deliver now by rewriting the current frame.
    let blocked = ctx(cell, idx).blocked & bit(signo) != 0;
    if idx != cur || blocked {
        ctx(cell, idx).pending |= bit(signo);
        return ret(0);
    }
    // Deliver on the current syscall frame: the raising syscall returns 0 once
    // the handler completes (saved into the ucontext before capture).
    // SAFETY: `frame` is the current context's saved state; cell root active.
    unsafe {
        arch::set_syscall_ret(&mut *frame, 0);
        build_frame(cell, idx, signo, &act, &mut *frame, si_code, si_addr);
    }
    Ctl::Switch(frame)
}

/// Build the signal frame and apply the handler's mask (the shared tail of
/// fault and kill delivery). `frame` is rewritten to enter the handler.
///
/// # Safety
/// `frame` is the target context's saved state and the cell root is active, so
/// the user stack it selects is writable.
unsafe fn build_frame(
    cell: usize,
    idx: usize,
    signo: u32,
    act: &SigAction,
    frame: &mut TrapFrame,
    si_code: i32,
    si_addr: u64,
) {
    let c = ctx(cell, idx);
    let saved_mask = c.blocked;
    // Block the handler's mask + (unless SA_NODEFER) the signal itself.
    let mut new_blocked = c.blocked | act.mask;
    if act.flags & SA_NODEFER == 0 {
        new_blocked |= bit(signo);
    }
    c.blocked = new_blocked;
    c.pending &= !bit(signo);

    // Pick the stack: the alternate one if requested and available, else the
    // interrupted SP.
    let on_alt = act.flags & SA_ONSTACK != 0 && c.alt_size != 0 && !c.alt_active;
    let stack_top = if on_alt {
        c.alt_active = true;
        c.alt_sp + c.alt_size
    } else {
        arch::user_sp(frame)
    };

    let spec = SigFrameSpec {
        signo,
        handler: act.handler,
        restorer: act.restorer,
        saved_mask,
        si_code,
        si_addr,
        stack_top,
    };
    arch::setup_rt_frame(frame, &spec);
}

/// After unblocking, deliver the lowest pending signal that is now unblocked and
/// catchable (or terminate on a pending unblocked fatal default). Returns None
/// if nothing is deliverable.
fn check_pending_current(cell: usize, frame: *mut TrapFrame) -> Option<Ctl> {
    let idx = thread::current_context(cell);
    let c = ctx(cell, idx);
    let deliverable = c.pending & !c.blocked;
    if deliverable == 0 {
        return None;
    }
    for s in 1..=NSIG as u32 {
        if deliverable & bit(s) == 0 {
            continue;
        }
        let act = actions(cell)[s as usize];
        if act.handler == SIG_IGN || (act.handler == SIG_DFL && !default_terminates(s)) {
            ctx(cell, idx).pending &= !bit(s);
            continue;
        }
        if act.handler == SIG_DFL {
            return Some(crate::linux::proc::exit_signaled(cell, s));
        }
        // SAFETY: current context's frame; cell root active.
        unsafe {
            arch::set_syscall_ret(&mut *frame, 0);
            build_frame(cell, idx, s, &act, &mut *frame, 0, 0);
        }
        return Some(Ctl::Switch(frame));
    }
    None
}
