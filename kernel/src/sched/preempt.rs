//! **Timer preemption**: taking the CPU away from a cell that will not give it up
//! (docs/SUBSTRATE.md pillar 3, migration S3'; task #27).
//!
//! ## What was wrong, measured
//!
//! Every scheduler in this tree was cooperative: a cell kept the CPU until it
//! parked at a syscall boundary, yielded, or exited. Two consequences were
//! disclosed rather than fixed, and both are recorded with evidence:
//!
//! - **A compute-bound cell starves every sibling.** There was no mechanism at all
//!   by which a cell that issues no syscall could be made to stop.
//! - **`linuxbun` is an accepted partial for exactly this reason.** All 205 of
//!   Bun's startup syscalls came from its main thread; the worker it spawned never
//!   got the CPU, because Bun's main thread requires the worker to make progress
//!   *concurrently* before it will proceed. The whole load path worked - dynamic
//!   linking, the 128 GiB Gigacage, `clone3` - and the program still aborted.
//!
//! ## The mechanism
//!
//! One flag, set by an interrupt, read on the way out of the trap:
//!
//! 1. When a cell is dispatched, the scheduler arms the timer arbiter's
//!    [`crate::ktimer::TimerClient::Preempt`] slot for the slice the ready queue
//!    says it may run ([`crate::sched::vcore::RunQueue::current_slice_ns`]).
//! 2. The timer fires. On every ISA the interrupt is taken **while the cell is in
//!    user mode** and lands in that ISA's user-trap entry, which already services
//!    device interrupts and resumes the interrupted cell (the path rheo-net N2d
//!    added for the NIC receive line). It calls [`arm`]'s counterpart [`note`],
//!    which sets the flag - and does nothing else, because an interrupt handler is
//!    the wrong place to run a scheduler.
//! 3. The user-trap entry then asks [`crate::user::on_user_interrupt`] for the frame
//!    to resume. That is where the switch happens, in ordinary kernel context with
//!    the full scheduler available.
//!
//! Splitting it into "note it" and "act on it" is not ceremony. The interrupt can
//! arrive in the middle of anything, including inside the kernel while it holds a
//! reference into a funded table; a scheduler invoked from there would be
//! reentrant. Deferring to the trap-exit point makes preemption land at exactly one
//! place per ISA, which is the same discipline the FP/SIMD switch had to be reduced
//! to before it was correct (docs/LIBRHEO.md, the `SYS_YIELD` scar).
//!
//! ## Why the arbiter and not the timer
//!
//! The hardware has one one-shot per CPU and [`crate::ktimer`] is its single owner -
//! an invariant a real defect was fixed to establish (docs/NETSTACK.md 16 Phase
//! N2h: two subsystems arming it directly meant the inner requester's completion
//! destroyed the outer's deadline *and* made `timer_expired` report it elapsed).
//! Preemption is one more client of that arbiter, in its own slot, so a preemption
//! deadline and a cell's `sleep` and a BBR pacing deadline coexist and the nearest
//! one is armed.
//!
//! ## Honest scope
//!
//! - Preemption is **enabled with queue-driven dispatch** ([`crate::sched::dispatch::enabled`])
//!   and off otherwise, so every pre-existing proof keeps its exact cooperative
//!   behaviour until a boot opts in.
//! - It requires the ISA's timer interrupt to be wired ([`crate::arch::timer_irq_enabled`]).
//!   Where it is not, [`arm`] does nothing and says so through [`counters`] rather
//!   than pretending a slice was enforced.
//! - Preemption switches **contexts within a cell first, then cells**. That is what
//!   Bun needs and it is the cheaper switch (one address space, one register file).

use crate::ktimer::{self, TimerClient};
use crate::smp::PerCpu;

/// Per-CPU "a preemption is due" flag, set by the timer interrupt and consumed at
/// trap exit.
///
/// A plain `bool` per CPU rather than an atomic: it is set by an interrupt handler
/// and read by the trap-exit path **on the same CPU**, which is a data race only if
/// the two can interleave on different cores - they cannot, because a CPU's
/// preemption timer interrupts that CPU.
static PENDING: PerCpu<bool> = PerCpu::from_array([false; crate::smp::MAX_CPUS]);

// Slices armed, preemptions actually taken, slices that could not be armed
// (no wired timer interrupt), the sibling-vs-cell split (different capabilities:
// the first is what an intra-process event loop with a worker needs - the
// `linuxbun` case - the second what a multi-tenant machine needs), and how many
// preemption-timer interrupts actually **arrived** - distinct from taken because
// "not delivered" and "delivered with nobody to switch to" are different faults
// with the same symptom (the first bring-up run showed `armed=1, taken=0`, which
// could have been either). All live in the observability plane's per-CPU blocks
// (`obs::cpu`, S4): each CPU counts its own arms/takes on its own line - cheaper
// than the relaxed `fetch_add`s these replaced and correct for the same reason,
// one writer per slot - and the accessors sum, so every existing assertion reads
// the same totals.

/// (armed, taken, unarmable, to-sibling, to-cell).
pub fn counters() -> (u64, u64, u64, u64, u64) {
    (
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_ARMED),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_TAKEN),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_UNARMABLE),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_TO_SIBLING),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_TO_CELL),
    )
}

/// Clear the flags and counters (between runs).
pub fn reset() {
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs.
        unsafe { *PENDING.get_mut(cpu) = false };
    }
    for slot in [
        crate::obs::cpu::CTR_PREEMPT_ARMED,
        crate::obs::cpu::CTR_PREEMPT_TAKEN,
        crate::obs::cpu::CTR_PREEMPT_UNARMABLE,
        crate::obs::cpu::CTR_PREEMPT_NOTES,
        crate::obs::cpu::CTR_PREEMPT_TO_SIBLING,
        crate::obs::cpu::CTR_PREEMPT_TO_CELL,
    ] {
        crate::obs::cpu_counter_clear(slot);
    }
}

/// Arm the preemption slice for the cell about to run.
///
/// `slice_ns` comes from the ready queue and is already floored, so it cannot be
/// zero. Called at trap exit, once per entry into a cell; re-arming replaces the
/// previous registration in the same arbiter slot, which is the arbiter's own
/// semantics and is why a cell that traps repeatedly does not accumulate deadlines.
pub fn arm(slice_ns: u64) {
    if !crate::sched::dispatch::enabled() {
        return;
    }
    if !crate::arch::timer_irq_enabled() {
        // Nothing here can enforce a slice, and saying so is the point: a slice
        // reported as armed but never delivered would make a cooperative run look
        // preemptive (docs/ENGINEERING.md 1).
        crate::obs::cpu_bump(crate::obs::cpu::CTR_PREEMPT_UNARMABLE, 1);
        return;
    }
    ktimer::register(TimerClient::Preempt, slice_ns);
    crate::obs::cpu_bump(crate::obs::cpu::CTR_PREEMPT_ARMED, 1);
}

/// Cancel any outstanding preemption slice - the cell stopped for its own reasons
/// (it blocked, yielded or exited), so the slice is no longer about anything.
///
/// Leaving it registered would be harmless to *correctness* (the flag is only acted
/// on when a cell is running) but not to the timer: the arbiter would keep the
/// hardware armed for a deadline nobody is waiting for, and on an ISA where the
/// nearest deadline determines the arming that costs a spurious wake per slice.
pub fn disarm() {
    ktimer::cancel(TimerClient::Preempt);
    // SAFETY: this CPU's own slot.
    unsafe { *PENDING.this_mut() = false };
}

/// Record that the preemption timer fired. Called from an interrupt handler, so it
/// does exactly one store and nothing else.
#[inline]
pub fn note() {
    // SAFETY: this CPU's own slot; the setter is this CPU's interrupt handler and
    // the reader is this CPU's trap-exit path, so the two cannot interleave across
    // cores.
    unsafe {
        *PENDING.this_mut() = true;
    }
    crate::obs::cpu_bump(crate::obs::cpu::CTR_PREEMPT_NOTES, 1);
}

/// How many preemption-timer interrupts arrived.
pub fn notes() -> u64 {
    crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_PREEMPT_NOTES)
}

/// Whether a preemption is due on this CPU.
#[inline]
pub fn due() -> bool {
    *PENDING.this()
}

/// Consume the pending flag, returning whether one was set.
#[inline]
pub fn take() -> bool {
    // SAFETY: this CPU's own slot.
    unsafe {
        let p = PENDING.this_mut();
        let was = *p;
        *p = false;
        was
    }
}

/// Count a preemption that actually moved the CPU, and to what.
pub(crate) fn took(to_sibling: bool) {
    crate::obs::cpu_bump(crate::obs::cpu::CTR_PREEMPT_TAKEN, 1);
    let slot = if to_sibling {
        crate::obs::cpu::CTR_PREEMPT_TO_SIBLING
    } else {
        crate::obs::cpu::CTR_PREEMPT_TO_CELL
    };
    crate::obs::cpu_bump(slot, 1);
}
