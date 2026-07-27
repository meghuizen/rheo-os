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
//! ## SMP - the arbiter is per-CPU
//!
//! Every CPU has its **own** one-shot timer, so there is one arbiter per CPU
//! (docs/SMP.md 10.2 names this: "`ktimer` ... becomes per-CPU: every core has
//! its own timer arbiter over its own local timer"). The deadline table is
//! therefore [`crate::smp::PerCpu`] state rather than a global `static mut`, and
//! the single-owner invariant is stated per core: *this module is the only caller
//! of `arch::timer_*`, and a given core's deadlines are owned by that core.*
//!
//! No locking is needed or used: a core registers, cancels and services only its
//! own deadlines, which is the multikernel discipline (docs/SCHEDULING.md 1a) -
//! partitioning instead of synchronisation. A deadline that must wake a
//! *different* core is delivered by asking that core (an IPI), never by reaching
//! into its table; that path is part of SMP phase 2 and is deliberately absent
//! here rather than approximated.
//!
//! On the single-CPU build `crate::smp::cpu_index()` is a compile-time 0, so
//! every access resolves to slot 0 with no indexing - the behaviour is
//! identical to the fixed global table this replaced.
//!
//! ## Two deadline shapes, one owner
//!
//! - **Named clients** ([`TimerClient`]): one slot per kernel subsystem, a closed
//!   vocabulary. This is what makes the single-owner property checkable and is
//!   unchanged.
//! - **Dynamic timers** ([`wheel`]): an unbounded number of deadlines of the same
//!   kind, for the callers a fixed slot cannot serve (thousands of QUIC
//!   connection timeouts, a JavaScript runtime's `setTimeout` set). Funded
//!   storage, O(1) arm and cancel.
//!
//! Both feed the same "nearest deadline" computation, so the hardware is armed
//! once for whichever of the two is sooner and neither can lose the other's
//! deadline.

pub mod wheel;

use crate::arch;
use crate::mm::kmeta::Owner;
use crate::smp::PerCpu;
use wheel::Wheel;

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
    /// The **BBR pacer**'s send-pacing deadline (docs/NETSTACK.md 21, rheo-net
    /// N2e): "release the next segment at `bytes/rate` from the last one". This is
    /// the arbiter's first **continuously re-armed** client - a paced flow
    /// registers a fresh deadline after every send, for the life of the flow, so
    /// it is the requester that makes the pre-N2h conflict fatal rather than
    /// latent. A cell selects it on `SYS_ARM_TIMER` with
    /// [`crate::abi::TIMER_CLIENT_PACER`].
    Pacer = 4,
    /// A Linux-personality **futex wait's timeout** (docs/LINUX-COMPAT.md L4, the
    /// `futex` row): `pthread_cond_timedwait`'s deadline, honoured when no other
    /// context of the cell is runnable. It needs its own slot for the same reason
    /// the pacer does - a cell can have a futex deadline outstanding while its
    /// sleep, a receive deadline and the pacer are all also outstanding, and no
    /// client may cancel another's.
    FutexWait = 5,
}

/// Number of deadline slots (one per [`TimerClient`]).
pub const CLIENTS: usize = 6;

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

/// One CPU's arbiter state: the named-client deadline table, this core's dynamic
/// timer wheel, and the counters that make its behaviour observable.
///
/// Grouped into one struct rather than six parallel `PerCpu` statics so that
/// "this core's arbiter" is a single thing with a single owner, which is what the
/// invariant in the module header is about.
struct Arbiter {
    slots: [Slot; CLIENTS],
    /// Hardware arm operations performed (an arm of the nearest deadline).
    arms: u64,
    /// Client deadlines marked due.
    firings: u64,
    /// Halts performed by [`park`].
    parks: u64,
    /// Times a still-pending **other** client's deadline was re-armed after some
    /// client fired or cancelled - i.e. the deadlines the pre-N2h
    /// direct-`timer_arm` pattern would have silently lost. The
    /// conflict-avoidance counter.
    preserved: u64,
    /// Deadlines registered per client. Instrumentation only: it is what lets a
    /// test assert that a *cell's* pacer went through the [`TimerClient::Pacer`]
    /// slot (and how many times it re-armed) rather than some other client's.
    regs: [u64; CLIENTS],
}

impl Arbiter {
    const fn new() -> Arbiter {
        Arbiter {
            slots: [EMPTY; CLIENTS],
            arms: 0,
            firings: 0,
            parks: 0,
            preserved: 0,
            regs: [0; CLIENTS],
        }
    }
}

/// Per-CPU arbiter state. `Copy`, so the plain [`PerCpu::new`] constructor works.
static ARB: PerCpu<Arbiter> = PerCpu::new(Arbiter::new());

// `Arbiter` holds only integers and bools, so it is `Copy` - but deriving `Copy`
// on it would also make it silently duplicable, and a duplicated arbiter would
// mean two views of one core's deadlines. It is constructed once per core and
// only ever mutated in place, so the `Copy` bound `PerCpu::new` needs is provided
// explicitly here rather than by a derive that invites copying.
impl Clone for Arbiter {
    fn clone(&self) -> Arbiter {
        *self
    }
}
impl Copy for Arbiter {}

/// Per-CPU dynamic timer wheels. Not `Copy` (a wheel owns funded frames), so this
/// uses [`PerCpu::from_array`] with a const block.
static WHEELS: PerCpu<Wheel> = PerCpu::from_array([const { Wheel::new() }; crate::smp::MAX_CPUS]);

/// This CPU's arbiter state.
///
/// # Safety
/// The returned reference must not outlive the caller's critical section, and no
/// second reference may be taken while it lives. Every use below is a short,
/// straight-line update inside one function, and nothing here calls back into
/// this module.
#[inline(always)]
#[allow(clippy::mut_from_ref)]
unsafe fn arb() -> &'static mut Arbiter {
    // SAFETY: this CPU's own slot (see the module's SMP section); the obligation
    // above is discharged at each call site.
    unsafe { ARB.this_mut() }
}

/// This CPU's dynamic timer wheel.
///
/// # Safety
/// As [`arb`].
#[inline(always)]
#[allow(clippy::mut_from_ref)]
unsafe fn wheel_mut() -> &'static mut Wheel {
    // SAFETY: as `arb`.
    unsafe { WHEELS.this_mut() }
}

/// Clear every deadline and counter on **this CPU** (call before installing a
/// fresh set of cells).
pub fn reset() {
    // SAFETY: single short update of this CPU's own state.
    unsafe {
        *arb() = Arbiter::new();
        let now = arch::timer_now_ns();
        wheel_mut().init(Owner::KERNEL, now);
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
    // SAFETY: a plain update of this CPU's own table.
    unsafe {
        let a = arb();
        a.slots[client as usize] = Slot {
            deadline_ns: now.wrapping_add(in_ns.max(1)),
            armed: true,
            fired: false,
        };
        a.regs[client as usize] = a.regs[client as usize].wrapping_add(1);
    }
    rearm(now, false);
}

/// Drop `client`'s deadline (and its due flag). **Never** disarms another
/// client's pending deadline: the hardware is re-armed for the nearest remaining
/// one, which is the whole point of this module.
pub fn cancel(client: TimerClient) {
    // SAFETY: a plain update of this CPU's own table.
    let was_armed = unsafe {
        let a = arb();
        let was = a.slots[client as usize].armed;
        a.slots[client as usize] = EMPTY;
        was
    };
    rearm(now_ns(), was_armed);
}

/// Whether `client`'s registered deadline has passed. Services the table first,
/// so a caller that only polls (never [`park`]s) still observes its deadline.
pub fn expired(client: TimerClient) -> bool {
    service();
    // SAFETY: a plain read of this CPU's own table.
    unsafe { arb().slots[client as usize].fired }
}

/// Whether `client` has an outstanding (registered, not yet due) deadline.
pub fn pending(client: TimerClient) -> bool {
    // SAFETY: a plain read of this CPU's own table.
    unsafe { arb().slots[client as usize].armed }
}

/// The nearest outstanding deadline across **both** the named-client table and
/// the dynamic wheel, if any (absolute ns, timer domain).
pub fn nearest_ns() -> Option<u64> {
    // SAFETY: plain reads/updates of this CPU's own state.
    unsafe {
        let named = arb()
            .slots
            .iter()
            .filter(|s| s.armed)
            .map(|s| s.deadline_ns)
            .min();
        let dynamic = wheel_mut().nearest();
        match (named, dynamic) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        }
    }
}

/// Mark **every** client whose deadline has passed as due (and advance the
/// dynamic wheel), then re-arm the hardware for the nearest remaining deadline.
/// Returns whether anything became due.
///
/// Called after a halt (from [`park`]) and lazily from [`expired`]. Marking *all*
/// due clients - not just the one the hardware was armed for - is what makes two
/// deadlines that fall in the same instant both honoured.
pub fn service() -> bool {
    let now = now_ns();
    let mut due = false;
    // SAFETY: plain updates of this CPU's own state.
    unsafe {
        let a = arb();
        for s in a.slots.iter_mut() {
            if s.armed && now.wrapping_sub(s.deadline_ns) < (1 << 63) {
                s.armed = false;
                s.fired = true;
                due = true;
                a.firings = a.firings.wrapping_add(1);
            }
        }
        if wheel_mut().advance(now) > 0 {
            due = true;
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
    // SAFETY: a plain counter update of this CPU's own state.
    unsafe {
        let a = arb();
        a.parks = a.parks.wrapping_add(1);
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
        // SAFETY: a plain counter update of this CPU's own state.
        unsafe {
            let a = arb();
            a.preserved = a.preserved.wrapping_add(1);
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
            // SAFETY: a plain counter update of this CPU's own state.
            unsafe {
                let a = arb();
                a.arms = a.arms.wrapping_add(1);
            }
        }
        None => arch::timer_disarm(),
    }
}

/// Hardware arm operations performed on this CPU.
pub fn arms() -> u64 {
    // SAFETY: a plain read of this CPU's own state.
    unsafe { arb().arms }
}

/// Client deadlines marked due on this CPU.
pub fn firings() -> u64 {
    // SAFETY: a plain read.
    unsafe { arb().firings }
}

/// Halts performed by [`park`] on this CPU.
pub fn parks() -> u64 {
    // SAFETY: a plain read.
    unsafe { arb().parks }
}

/// Deadlines `client` has registered since the last [`reset`]. A continuously
/// re-armed client (the [`TimerClient::Pacer`]) counts one per re-arm, which is how
/// a test sees that a cell's pacing deadlines really landed in the pacer's own slot
/// (docs/NETSTACK.md 21).
pub fn registrations(client: TimerClient) -> u64 {
    // SAFETY: a plain read.
    unsafe { arb().regs[client as usize] }
}

/// Deadlines that survived another client's completion because the arbiter
/// re-armed the nearest **remaining** deadline instead of disarming. Non-zero is
/// direct evidence of the conflict the pre-N2h code had (docs/NETSTACK.md 16).
pub fn preserved() -> u64 {
    // SAFETY: a plain read.
    unsafe { arb().preserved }
}

// ------------------------------------------------------------- dynamic timers

/// Arm a **dynamic** deadline `in_ns` from now, tagged with `tag`, on this CPU's
/// wheel. Returns a handle, or `None` when the wheel's funded storage could not
/// grow.
///
/// Unlike [`register`], any number of these may be outstanding at once - that is
/// the whole point (see "Two deadline shapes" in the module header). The hardware
/// is re-armed if this deadline is nearer than everything else outstanding, so a
/// dynamic timer and a named client cannot lose each other's deadline any more
/// than two named clients can.
pub fn arm_dynamic(in_ns: u64, tag: u64) -> Option<wheel::Timer> {
    let now = now_ns();
    let deadline = now.wrapping_add(in_ns.max(1));
    // SAFETY: a short update of this CPU's own wheel.
    let timer = unsafe { wheel_mut().arm(deadline, tag).ok() }?;
    rearm(now, false);
    Some(timer)
}

/// Cancel a dynamic timer. Returns whether it was still armed. Cannot disturb any
/// other deadline, named or dynamic.
pub fn cancel_dynamic(timer: wheel::Timer) -> bool {
    // SAFETY: a short update of this CPU's own wheel.
    let was_armed = unsafe { wheel_mut().cancel(timer) };
    rearm(now_ns(), was_armed);
    was_armed
}

/// Collect one fired dynamic timer as `(handle, tag)`, freeing it. `None` when
/// none has fired. Drain in a loop; services the wheel first so a caller that
/// only polls still observes its deadlines.
pub fn take_fired_dynamic() -> Option<(wheel::Timer, u64)> {
    service();
    // SAFETY: a short update of this CPU's own wheel.
    unsafe { wheel_mut().take_fired() }
}

/// Whether a dynamic timer has fired and not yet been collected.
pub fn fired_dynamic(timer: wheel::Timer) -> bool {
    // SAFETY: a plain read of this CPU's own wheel.
    unsafe { wheel_mut().fired(timer) }
}

/// Dynamic timers currently armed on this CPU.
pub fn dynamic_armed() -> usize {
    // SAFETY: a plain read.
    unsafe { wheel_mut().armed() }
}

/// `(arms, cancels, firings, cascades)` for this CPU's wheel - the evidence a
/// test asserts against, including that a cascade actually happened (a lost
/// cascade shows up as a deadline that never fires).
pub fn dynamic_counters() -> (u64, u64, u64, u64) {
    // SAFETY: a plain read.
    unsafe { wheel_mut().counters() }
}

/// Whether this CPU's wheel's structural invariants hold. Asserted by the
/// `substrate` test kernel around the deadlines it drives.
pub fn dynamic_invariant_holds() -> bool {
    // SAFETY: a plain read.
    unsafe { wheel_mut().invariant_holds() }
}
