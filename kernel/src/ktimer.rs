//! Kernel **timer arbiter**: the single owner of the hardware one-shot timer
//! (docs/NETSTACK.md 16, rheo-net N2h).
//!
//! ## The defect this exists to remove
//!
//! Every ISA has exactly **one** programmable one-shot deadline behind
//! `arch::timer_arm` / `timer_expired` / `timer_disarm` (riscv Sstc `stimecmp`,
//! aarch64 CNTV `cntv_cval_el0`, x86-64 the LAPIC one-shot). Before N2h two
//! independent subsystems armed it *directly*:
//!
//! - `net_rx::wait_frame` - the receive deadline, and (where no NIC interrupt
//!   exists) the poll slices between receive-queue polls;
//! - `time::arm_timer` - `SYS_ARM_TIMER`, i.e. every cell's `sleep`/`timeout`/
//!   `interval`.
//!
//! Last-armer-wins, and worse: each one **disarmed the timer on its way out**, so
//! the inner requester's completion silently cancelled the outer requester's
//! pending deadline. The outer waiter was then told two lies at once - the
//! hardware had no deadline left to wake it, *and* `arch::timer_expired()` (which
//! compares against the last-armed target, or on x86-64 reads a zeroed one-shot
//! count) reported "your deadline elapsed" long before it had. A lost deadline
//! **and** a false expiry.
//!
//! It is latent today only because the OS is single-CPU cooperative and no code
//! path yet has two deadlines outstanding at the same instant. It stops being
//! latent the moment a transport paces continuously (the BBR pacer) while a TCP
//! RTO and a receive poll slice are also outstanding - which is why the arbiter
//! lands before that, and why [`TimerClient::Pacer`] already has its slot.
//!
//! ## The single-owner invariant
//!
//! > **This module is the only caller of `arch::timer_arm` / `arch::timer_expired`
//! > / `arch::timer_disarm` / `arch::timer_park` in the kernel.**
//!
//! A subsystem that wants a deadline [`register`]s one under its own
//! [`TimerClient`] slot and asks [`expired`] whether *its* deadline passed. The
//! arbiter keeps every outstanding deadline in a fixed-size table, arms the
//! hardware for the **nearest** one only, and on every state change re-arms the
//! nearest **remaining** deadline instead of disarming (that re-arm is the fix -
//! see [`preserved`], which counts exactly the deadlines the old pattern would
//! have thrown away). A [`cancel`] therefore can never cancel anybody else's
//! deadline.
//!
//! The invariant is enforced by construction rather than by a lint: `arch` no
//! longer exposes an arm-wait-disarm helper (the old `arch::timer_wait`), so there
//! is no per-ISA path that can quietly own the timer behind the arbiter's back.
//! The remaining raw `arch::timer_*` primitives stay `pub` because everything in
//! `arch` is (the portability rule keeps per-ISA code there), and one test kernel
//! calls them deliberately to reproduce the pre-N2h conflict.
//!
//! ## Time domain
//!
//! Deadlines are **monotonic nanoseconds in the hardware timer's own domain**
//! ([`now_ns`] = `arch::timer_now_ns()`), which is the domain `arch::timer_arm`'s
//! relative delta is expressed in. That matters on RISC-V, where the timer runs on
//! the `time` CSR (a 10 MHz wall counter) while `arch::cycles()` is the retired-
//! instruction counter: comparing a `time`-domain deadline against a `cycles`-
//! domain reading would make a "20 ms" wait mean something different per ISA. A
//! deadline is always a **deadline, never a spin count** (the N4c rule).
//!
//! The arbiter works with **no timer interrupt at all**: where
//! `arch::timer_irq_enabled()` is false it touches no hardware and every deadline
//! is honoured by comparison against [`now_ns`], which is what the honest
//! bounded-poll receive path needs.
//!
//! ## SMP
//!
//! Single-CPU today (task #27, docs/SMP.md). The table is a plain fixed array of
//! `static mut` like the rest of the kernel's single-CPU state. The natural SMP
//! shape is one arbiter **per CPU** (each CPU has its own one-shot): the table
//! becomes per-CPU state in `smp.rs` and `this_cpu()` selects it, with a
//! cross-CPU deadline delivered as an IPI. Nothing here assumes a global table
//! beyond the statics themselves.

use crate::arch;
use core::ptr::{addr_of, addr_of_mut};

/// Who a deadline belongs to. One fixed slot each - the kernel stays
/// allocation-free, so the client set is closed and named here.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TimerClient {
    /// `net_rx`'s receive **poll slice**: the short halt between receive-queue
    /// polls on an ISA with no NIC RX interrupt (docs/NETSTACK.md 16).
    RxPoll = 0,
    /// `net_rx`'s receive **deadline**: the caller's `timeout_ns` on
    /// `SYS_WAIT_NET` ("a frame, or the deadline, whichever comes first").
    RxDeadline = 1,
    /// A cell's `SYS_ARM_TIMER` sleep (`librheo::time::sleep`/`timeout`/
    /// `interval`).
    CellSleep = 2,
    /// The network stack's timer wheel / TCP retransmission timeout.
    NetTimer = 3,
    /// Reserved for the **BBR pacer** (the next phase): a continuously re-armed
    /// send-pacing deadline, the requester that makes the conflict above fatal
    /// rather than latent. Registered here now so the pacer is a `register` call
    /// and not another subsystem reaching for the hardware.
    Pacer = 4,
}

/// Number of deadline slots (one per [`TimerClient`]).
pub const CLIENTS: usize = 5;

#[derive(Copy, Clone)]
struct Slot {
    /// Absolute deadline, monotonic ns in the timer domain ([`now_ns`]).
    deadline_ns: u64,
    /// Outstanding: registered and not yet due or cancelled.
    armed: bool,
    /// Became due and has not been cancelled/re-registered since.
    fired: bool,
}

const EMPTY: Slot = Slot {
    deadline_ns: 0,
    armed: false,
    fired: false,
};

static mut SLOTS: [Slot; CLIENTS] = [EMPTY; CLIENTS];
/// Hardware arm operations performed (an arm of the nearest deadline).
static mut ARMS: u64 = 0;
/// Client deadlines marked due.
static mut FIRINGS: u64 = 0;
/// Halts performed by [`park`].
static mut PARKS: u64 = 0;
/// Times a still-pending **other** client's deadline was re-armed after some
/// client fired or cancelled - i.e. the deadlines the pre-N2h direct-`timer_arm`
/// pattern would have silently lost. The conflict-avoidance counter.
static mut PRESERVED: u64 = 0;

/// Clear every deadline and counter (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs; no deadline can be outstanding.
    unsafe {
        *addr_of_mut!(SLOTS) = [EMPTY; CLIENTS];
        *addr_of_mut!(ARMS) = 0;
        *addr_of_mut!(FIRINGS) = 0;
        *addr_of_mut!(PARKS) = 0;
        *addr_of_mut!(PRESERVED) = 0;
    }
    if arch::timer_irq_enabled() {
        arch::timer_disarm();
    }
}

/// Monotonic now, in nanoseconds, in the hardware timer's own domain (see the
/// module docs). Readable on every ISA whether or not the timer interrupt is
/// wired.
pub fn now_ns() -> u64 {
    arch::timer_now_ns()
}

/// Register `client`'s deadline `in_ns` nanoseconds from now, replacing any
/// deadline it already had. The hardware is (re-)armed for the nearest deadline
/// across **all** clients, so registering a far deadline never pushes out a
/// nearer one and vice versa.
pub fn register(client: TimerClient, in_ns: u64) {
    let now = now_ns();
    // SAFETY: single CPU; a plain table write.
    unsafe {
        let slots = &mut *addr_of_mut!(SLOTS);
        slots[client as usize] = Slot {
            deadline_ns: now.wrapping_add(in_ns.max(1)),
            armed: true,
            fired: false,
        };
    }
    rearm(now, false);
}

/// Drop `client`'s deadline (and its due flag). **Never** disarms another
/// client's pending deadline: the hardware is re-armed for the nearest remaining
/// one, which is the whole point of this module.
pub fn cancel(client: TimerClient) {
    // SAFETY: single CPU; a plain table write.
    let was_armed = unsafe {
        let slots = &mut *addr_of_mut!(SLOTS);
        let was = slots[client as usize].armed;
        slots[client as usize] = EMPTY;
        was
    };
    rearm(now_ns(), was_armed);
}

/// Whether `client`'s registered deadline has passed. Services the table first,
/// so a caller that only polls (never [`park`]s) still observes its deadline.
pub fn expired(client: TimerClient) -> bool {
    service();
    // SAFETY: single CPU; a plain table read.
    unsafe { (*addr_of!(SLOTS))[client as usize].fired }
}

/// Whether `client` has an outstanding (registered, not yet due) deadline.
pub fn pending(client: TimerClient) -> bool {
    // SAFETY: single CPU; a plain table read.
    unsafe { (*addr_of!(SLOTS))[client as usize].armed }
}

/// The nearest outstanding deadline, if any (absolute ns, timer domain).
pub fn nearest_ns() -> Option<u64> {
    // SAFETY: single CPU; a plain table read.
    let slots = unsafe { *addr_of!(SLOTS) };
    slots
        .iter()
        .filter(|s| s.armed)
        .map(|s| s.deadline_ns)
        .min()
}

/// Mark **every** client whose deadline has passed as due, then re-arm the
/// hardware for the nearest remaining deadline. Returns whether anything became
/// due.
///
/// Called after a halt (from [`park`]) and lazily from [`expired`]. Marking *all*
/// due clients - not just the one the hardware was armed for - is what makes two
/// deadlines that fall in the same instant both honoured.
pub fn service() -> bool {
    let now = now_ns();
    let mut due = false;
    // SAFETY: single CPU; a plain table update.
    unsafe {
        let slots = &mut *addr_of_mut!(SLOTS);
        for s in slots.iter_mut() {
            if s.armed && now.wrapping_sub(s.deadline_ns) < (1 << 63) {
                s.armed = false;
                s.fired = true;
                due = true;
                *addr_of_mut!(FIRINGS) = (*addr_of!(FIRINGS)).wrapping_add(1);
            }
        }
    }
    if due {
        rearm(now, true);
    }
    due
}

/// Halt the CPU once, until the nearest registered deadline (or any other enabled
/// interrupt) fires, then [`service`] the table. Returns whether it actually
/// halted.
///
/// `other_source` says the caller has **another wired interrupt** that can wake
/// the CPU (the NIC's RX line, in `net_rx`'s interrupt mode). It is the caller's
/// precondition: with no deadline armed and no other source there is nothing to
/// wake a halt, so this returns false without halting rather than wedging the
/// machine.
///
/// Never halts on a one-shot that cannot fire. The hardware timer and [`now_ns`]
/// are the same time *domain* but not the same *device* (x86-64 counts down the
/// LAPIC's own clock, calibrated against the TSC), so a one-shot can fire slightly
/// before the software deadline it was armed for. A halt on an already-fired
/// one-shot would never wake, so this **re-arms the remaining delta before every
/// halt** and refuses to halt while the hardware still reports its one-shot
/// elapsed - a domain mismatch then costs an extra wakeup, never a wedge.
pub fn park(other_source: bool) -> bool {
    if service() {
        return false; // a deadline came due: let the caller act on it
    }
    let armed = arch::timer_irq_enabled() && nearest_ns().is_some();
    if !armed && !other_source {
        return false;
    }
    if armed {
        rearm(now_ns(), false);
        if arch::timer_expired() {
            // The remaining delta is below the one-shot's resolution: nothing would
            // wake the halt. Report no halt so the caller re-checks in software.
            return false;
        }
    }
    // SAFETY: single CPU.
    unsafe {
        *addr_of_mut!(PARKS) = (*addr_of!(PARKS)).wrapping_add(1);
    }
    arch::timer_park();
    service();
    true
}

/// Arm the hardware for the nearest outstanding deadline, or disarm it when none
/// is left. `after_release` marks the calls that follow a client firing or
/// cancelling, so [`preserved`] can count the deadlines that survived a
/// completion - the ones the old direct-`timer_arm` pattern lost.
fn rearm(now: u64, after_release: bool) {
    let nearest = nearest_ns();
    if after_release && nearest.is_some() {
        // SAFETY: single CPU.
        unsafe {
            *addr_of_mut!(PRESERVED) = (*addr_of!(PRESERVED)).wrapping_add(1);
        }
    }
    if !arch::timer_irq_enabled() {
        // No timer interrupt on this ISA/kernel: the arbiter is pure software and
        // every deadline is honoured by comparison against `now_ns()`. Touching
        // the hardware here would be wrong (on RISC-V the `stimecmp` write only
        // works once Sstc has been brought up).
        return;
    }
    match nearest {
        Some(deadline) => {
            let delta = deadline.wrapping_sub(now);
            // A deadline already in the past still arms the shortest possible
            // one-shot, so a park cannot sleep through it.
            let delta = if delta >= (1 << 63) { 1 } else { delta.max(1) };
            arch::timer_arm(delta);
            // SAFETY: single CPU.
            unsafe {
                *addr_of_mut!(ARMS) = (*addr_of!(ARMS)).wrapping_add(1);
            }
        }
        None => arch::timer_disarm(),
    }
}

/// Hardware arm operations performed.
pub fn arms() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(ARMS) }
}

/// Client deadlines marked due.
pub fn firings() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(FIRINGS) }
}

/// Halts performed by [`park`].
pub fn parks() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(PARKS) }
}

/// Deadlines that survived another client's completion because the arbiter
/// re-armed the nearest **remaining** deadline instead of disarming. Non-zero is
/// direct evidence of the conflict the pre-N2h code had (docs/NETSTACK.md 16).
pub fn preserved() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(PRESERVED) }
}
