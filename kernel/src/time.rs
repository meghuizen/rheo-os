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
/// elapse from the call, then return (docs/LIBRHEO.md Phase F). Where a hardware
/// timer interrupt is wired ([`timer_interrupt_driven`]) the kernel registers the
/// deadline with the **timer arbiter** ([`crate::ktimer`]) and halts at WFI until
/// it fires - a genuine 0%-CPU park, the OS's second interrupt. Otherwise it falls
/// back to a **cooperative deadline check** against the monotonic cycle counter
/// (honest, not 0%-idle). Honors docs/POWER.md: the kernel waits here only when a
/// real deadline was requested (librheo arms a timer only for an actual
/// `sleep`/`timeout`).
///
/// The deadline goes through the arbiter rather than straight to
/// `arch::timer_arm`/`timer_wait` (rheo-net N2h, docs/NETSTACK.md 16): the
/// hardware has **one** one-shot, and arming it directly here used to cancel a
/// receive deadline or a poll slice another subsystem had armed - and be cancelled
/// by them. The arbiter arms only the nearest deadline and re-arms the nearest
/// remaining one whenever a client completes, so a cell's sleep and a network
/// deadline now coexist.
///
/// The busy-wait fallback is deterministic under QEMU `-icount`: each spin
/// advances the instruction count, which advances the cycle counter, so the
/// deadline is reached in a bounded number of iterations.
pub fn arm_timer(deadline_ns: u64) {
    if deadline_ns == 0 {
        return;
    }
    if arch::timer_irq_enabled() {
        // Genuine 0%-CPU park: register the deadline and halt at WFI until the
        // arbiter reports *this* client's deadline elapsed (another client's
        // nearer deadline may wake the CPU first; that is not our deadline).
        use crate::ktimer::{self, TimerClient};
        ktimer::register(TimerClient::CellSleep, deadline_ns);
        while !ktimer::expired(TimerClient::CellSleep) {
            if ktimer::park(false) {
                // The CPU really halted - only then is this a 0%-CPU park.
                TIMER_IDLED.store(1, Ordering::Relaxed);
            } else {
                // Either another client's deadline just came due (re-checked above)
                // or the remaining delta is below the one-shot's resolution: spin
                // out the last ticks rather than halting with no wake source.
                arch::spin_loop(1);
            }
        }
        ktimer::cancel(TimerClient::CellSleep);
        return;
    }
    let start = arch::cycles();
    loop {
        let elapsed = arch::cycles().wrapping_sub(start);
        if arch::ticks_to_ns(elapsed) >= deadline_ns {
            return;
        }
        arch::spin_loop(1);
    }
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
