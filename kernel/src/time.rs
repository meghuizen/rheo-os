//! Clock object (docs/ARCHITECTURE.md 3 object 9, docs/TIME-IDENTITY.md).
//! Single-host scope: a monotonic clock read from the per-ISA cycle counter
//! and a wall clock expressed as a bounded interval (no PTP/NTS sync yet, so
//! the error bound is deliberately large and honest).
//!
//! Entropy - the other half of object 9 - lives in `crate::rng` (a ChaCha20
//! DRBG seeded from the hardware RNG). What is real in the clock here:
//! monotonic ordering and the interval-clock *shape* (every wall read is
//! [t-e, t+e], never a bare instant). Deferred: hardware time sync.

use crate::arch;
use core::sync::atomic::{AtomicU64, Ordering};

static BOOT_TICKS: AtomicU64 = AtomicU64::new(0);

/// Whether the last `arm_timer` genuinely idled at WFI/HLT/wfi (waiting on the
/// hardware timer interrupt) rather than busy-spinning. Meaningful only when
/// [`timer_interrupt_driven`]; the busy-wait build leaves it false (honest).
/// Mirrors `input::did_idle` for the timer, for the Phase F idle-park assertion.
static TIMER_IDLED: AtomicU64 = AtomicU64::new(0);

/// Record the boot instant. Called once during kernel init.
pub fn init() {
    BOOT_TICKS.store(arch::cycles(), Ordering::Relaxed);
    TIMER_IDLED.store(0, Ordering::Relaxed);
}

/// Whether this ISA delivers `SYS_ARM_TIMER` by a hardware timer interrupt (a
/// genuine 0%-CPU park) rather than a busy-wait. docs/LIBRHEO.md Phase F names
/// which ISA is which; the busy-wait build reports false everywhere (honest).
pub fn timer_interrupt_driven() -> bool {
    arch::timer_irq_enabled()
}

/// Whether the kernel actually idled at WFI during the last interrupt-driven
/// `arm_timer` (the Phase F idle-park assertion). False in the busy-wait build.
///
/// Recorded when a park **genuinely halted the CPU** (rheo-net N2h): it used to be
/// set on intent, just before the wait, which reported an idle on a machine whose
/// one-shot could not fire at all.
pub fn timer_did_idle() -> bool {
    TIMER_IDLED.load(Ordering::Relaxed) != 0
}

/// Record that the kernel genuinely halted on a timer deadline. Called by the
/// **scheduler idle state** ([`crate::idle`]): since the
/// docs/ARCHITECTURE-DEBT.md 2.4 slice, a cell's `sleep` registers its deadline and
/// returns to the scheduler, so the park that used to happen inside
/// `SYS_ARM_TIMER` now happens in the run loop when no sibling is runnable. It is
/// the same halt on the same one-shot; recording it here keeps
/// [`timer_did_idle`] meaning "a `sleep` really idled the CPU" and keeps it set
/// only from **inside** a park that stopped (docs/ENGINEERING.md 1).
pub fn mark_timer_idle() {
    TIMER_IDLED.store(1, Ordering::Relaxed);
}

/// Monotonic counter reading (raw ticks; per-ISA meaning, see
/// arch::cycles). Never goes backwards on a single core.
pub fn monotonic() -> u64 {
    arch::cycles()
}

/// Ticks elapsed since boot.
pub fn uptime_ticks() -> u64 {
    arch::cycles().wrapping_sub(BOOT_TICKS.load(Ordering::Relaxed))
}

/// `SYS_ARM_TIMER`: block until `deadline_ns` nanoseconds of monotonic time
/// elapse from the call, then return (docs/LIBRHEO.md Phase F). Honors
/// docs/POWER.md: the kernel waits here only when a real deadline was requested
/// (librheo arms a timer only for an actual `sleep`/`timeout`/pacing release).
///
/// The deadline always goes through the **timer arbiter** ([`crate::ktimer`]),
/// never straight to `arch::timer_arm` (rheo-net N2h, docs/NETSTACK.md 16): the
/// hardware has **one** one-shot, and arming it directly here used to cancel a
/// receive deadline or a poll slice another subsystem had armed - and be cancelled
/// by them. The arbiter arms only the nearest deadline and re-arms the nearest
/// remaining one whenever a client completes, so a cell's sleep, a pacing release
/// and a network deadline all coexist.
///
/// **One path, two honest outcomes.** Where a hardware timer interrupt is wired
/// ([`timer_interrupt_driven`]) the arbiter's park halts the CPU until the
/// interrupt fires - a genuine 0%-CPU idle. Where it is not (no ISA in this tree as
/// of docs/SMP.md phase 1 - x86-64's LAPIC one-shot is now reached over the xAPIC
/// MMIO page, so all three have a verified hardware timer), the arbiter honours the same
/// deadline **in software** by comparison against its monotonic clock and this
/// spins instead of halting: an honest deadline wait, not an idle, and
/// [`timer_did_idle`] stays false to say so. Before rheo-net N2e that fallback was
/// a separate loop that bypassed the arbiter entirely - so a deadline on that ISA
/// was invisible to every other client, which is the same class of defect N2h
/// removed. The spin is deterministic under QEMU `-icount`: each iteration
/// advances the instruction count, which advances the clock.
pub fn arm_timer(deadline_ns: u64) {
    arm_timer_as(deadline_ns, crate::ktimer::TimerClient::CellSleep);
}

/// [`arm_timer`] under a caller-chosen **arbiter client slot** (docs/NETSTACK.md 21,
/// rheo-net N2e). Identical in every respect except which
/// [`crate::ktimer::TimerClient`] slot holds the deadline.
///
/// The reason a cell gets to choose: a **paced** transport re-arms a deadline after
/// every segment it releases, for the life of the flow, while the same cell may also
/// have an ordinary `sleep` outstanding. Two deadlines from one cell need two slots,
/// and the pacer's is [`crate::ktimer::TimerClient::Pacer`] - which was reserved for
/// exactly this in N2h. `SYS_ARM_TIMER`'s second argument selects it
/// ([`crate::abi::TIMER_CLIENT_PACER`]); argument 0 (the only shape that existed
/// before) is `CellSleep`, so every pre-N2e caller is unchanged.
pub fn arm_timer_as(deadline_ns: u64, client: crate::ktimer::TimerClient) {
    if deadline_ns == 0 {
        return;
    }
    use crate::ktimer;
    ktimer::register(client, deadline_ns);
    while !ktimer::expired(client) {
        if ktimer::park(false) {
            // The CPU really halted - only then is this a 0%-CPU park.
            TIMER_IDLED.store(1, Ordering::Relaxed);
        } else {
            // No wake source (no timer interrupt on this ISA), another client's
            // nearer deadline just came due (re-checked above), or the remaining
            // delta is below the one-shot's resolution: spin out the last ticks
            // rather than halting with nothing to wake us.
            arch::spin_loop(1);
        }
    }
    ktimer::cancel(client);
}

/// A wall-clock reading as a bounded interval [center-e, center+e]
/// (docs/ARCHITECTURE.md 4.5). Without a synced time source the center is
/// "ticks since boot" and the error bound is the whole interval - the API
/// forces callers to see uncertainty rather than trust a fake instant.
#[derive(Copy, Clone, Debug)]
pub struct Interval {
    pub center: u64,
    pub error: u64,
}

pub fn wall() -> Interval {
    let t = uptime_ticks();
    Interval {
        center: t,
        // Unsynced: the true error is unbounded; report the reading itself
        // as the bound so no caller mistakes this for a precise clock.
        error: t,
    }
}
