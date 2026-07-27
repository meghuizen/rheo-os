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
use core::ptr::{addr_of, addr_of_mut};

/// Per-CPU "a preemption is due" flag, set by the timer interrupt and consumed at
/// trap exit.
///
/// A plain `bool` per CPU rather than an atomic: it is set by an interrupt handler
/// and read by the trap-exit path **on the same CPU**, which is a data race only if
/// the two can interleave on different cores - they cannot, because a CPU's
/// preemption timer interrupts that CPU.
static PENDING: PerCpu<bool> = PerCpu::from_array([false; crate::smp::MAX_CPUS]);

/// Slices armed, preemptions actually taken, and slices that could not be armed
/// because the ISA has no wired timer interrupt.
static mut ARMED: u64 = 0;
static mut TAKEN: u64 = 0;
static mut UNARMABLE: u64 = 0;
/// Preemptions that moved to a **sibling context** of the same cell, versus to
/// another cell. Kept apart because they are different capabilities: the first is
/// what an intra-process event loop with a worker needs (the `linuxbun` case), the
/// second is what a multi-tenant machine needs.
static mut TO_SIBLING: u64 = 0;
static mut TO_CELL: u64 = 0;
/// Times the preemption timer interrupt actually **arrived**.
///
/// Distinct from `TAKEN` on purpose, and the distinction earns its keep: "the
/// interrupt is not being delivered" and "the interrupt arrives but there is nobody
/// to switch to" are different faults with the same symptom, and a single counter
/// cannot tell them apart. (It earned it immediately - the first bring-up run showed
/// `armed=1, taken=0`, which could have been either.)
static mut NOTES: u64 = 0;

/// (armed, taken, unarmable, to-sibling, to-cell).
pub fn counters() -> (u64, u64, u64, u64, u64) {
    // SAFETY: single CPU; plain counter reads.
    unsafe {
        (
            *addr_of!(ARMED),
            *addr_of!(TAKEN),
            *addr_of!(UNARMABLE),
            *addr_of!(TO_SIBLING),
            *addr_of!(TO_CELL),
        )
    }
}

/// Clear the flags and counters (between runs).
pub fn reset() {
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs.
        unsafe { *PENDING.get_mut(cpu) = false };
    }
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(ARMED) = 0;
        *addr_of_mut!(TAKEN) = 0;
        *addr_of_mut!(UNARMABLE) = 0;
        *addr_of_mut!(NOTES) = 0;
        *addr_of_mut!(TO_SIBLING) = 0;
        *addr_of_mut!(TO_CELL) = 0;
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
        // SAFETY: single CPU; a counter.
        unsafe { *addr_of_mut!(UNARMABLE) = (*addr_of!(UNARMABLE)).wrapping_add(1) };
        return;
    }
    ktimer::register(TimerClient::Preempt, slice_ns);
    // SAFETY: single CPU; a counter.
    unsafe { *addr_of_mut!(ARMED) = (*addr_of!(ARMED)).wrapping_add(1) };
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
        *addr_of_mut!(NOTES) = (*addr_of!(NOTES)).wrapping_add(1);
    }
}

/// How many preemption-timer interrupts arrived.
pub fn notes() -> u64 {
    // SAFETY: single CPU; a plain counter read.
    unsafe { *addr_of!(NOTES) }
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
    // SAFETY: single CPU; counters.
    unsafe {
        *addr_of_mut!(TAKEN) = (*addr_of!(TAKEN)).wrapping_add(1);
        if to_sibling {
            *addr_of_mut!(TO_SIBLING) = (*addr_of!(TO_SIBLING)).wrapping_add(1);
        } else {
            *addr_of_mut!(TO_CELL) = (*addr_of!(TO_CELL)).wrapping_add(1);
        }
    }
}
