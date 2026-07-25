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

/// Record the boot instant. Called once during kernel init.
pub fn init() {
    BOOT_TICKS.store(arch::cycles(), Ordering::Relaxed);
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
/// elapse from the call, then return (docs/LIBRHEO.md Phase F). A **cooperative
/// deadline check** against the monotonic cycle counter - honest, not a 0%-CPU
/// idle: a true per-ISA timer interrupt (x86 TSC-deadline / LAPIC, aarch64
/// CNTV_*, riscv sstc) is documented future work, the OS's second interrupt.
/// Honors docs/POWER.md: the kernel only waits here when a real deadline was
/// requested (librheo arms a timer only for an actual `sleep`/`timeout`).
///
/// The busy-wait is deterministic under QEMU `-icount`: each spin advances the
/// instruction count, which advances the cycle counter, so the deadline is
/// reached in a bounded number of iterations.
pub fn arm_timer(deadline_ns: u64) {
    if deadline_ns == 0 {
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
