//! **The scheduler seam**: where the kernel's cross-cell scheduler asks the
//! EEVDF+BORE run queue who runs next (docs/SUBSTRATE.md pillar 3, migration S3').
//!
//! ## What this is for
//!
//! [`super::vcore::RunQueue`] was built and proven as a data structure, and then
//! nothing dispatched through it - the honest disclosure in docs/SUBSTRATE.md was
//! that "no vcore is dispatched". Two schedulers picked the next cell instead, both
//! by the same rule: `(leaving + k) % MAX_CELLS`, first `Runnable` wins
//! (`linux::proc::reschedule` and `nproc::reschedule`). Round-robin has no
//! responsiveness model at all: a cell that woke on a keystroke waits behind every
//! compute-bound sibling that happens to sit at a lower index, and nothing about
//! the order can be tuned, measured, or reasoned about.
//!
//! This module is the seam that replaces that rule, and it is deliberately a *seam*
//! rather than a rewrite:
//!
//! - **The authority on runnability does not move.** The Linux personality's
//!   `PState` and the native process table's remain the only things that decide
//!   whether a cell can run; they know about pipes, futexes, deadlines and child
//!   reaping, and duplicating that into the queue would create two answers to one
//!   question. The queue is asked only for an **order** over the cells the caller
//!   already considers runnable ([`RunQueue::sync_runnable`]).
//! - **Off is byte-identical.** With [`enabled`] false, [`pick`] *is* the old
//!   round-robin expression, so every pre-existing proof keeps its exact previous
//!   behaviour and the migration can be turned on one boot at a time
//!   (docs/SUBSTRATE.md 15). That is the same trade docs/SMP.md records for the
//!   `smp` feature: enabling a mechanism must not change single-CPU behaviour, and
//!   the way to be sure is to keep the old path reachable and identical.
//!
//! ## What it charges
//!
//! A vcore's weight comes from its BORE burst score, and a burst score is only
//! meaningful if CPU time is actually charged and relinquishes are actually
//! observed. This OS is unusually well shaped for that (docs/SUBSTRATE.md pillar
//! 3): a relinquish is not inferred from a heuristic as it is in Linux, it is an
//! explicit, already-counted transition - a cell blocking at a syscall, a
//! `SYS_YIELD`, an exit. So:
//!
//! - [`running`] records who started running and when (in the timer's own ns
//!   domain, [`crate::arch::timer_now_ns`] - the one monotonic counter the whole
//!   kernel already agrees on, per docs/NETSTACK.md 16 Phase N2h).
//! - [`relinquish`] charges the elapsed time to that vcore before anything else
//!   runs, which is what makes the burst score observed rather than estimated.
//! - [`preempted`] does the same but records the stop as involuntary, which is the
//!   one distinction BORE actually needs: a task that is *taken off* the CPU has
//!   not finished its burst, and treating that as a voluntary yield would reward a
//!   compute-bound task with interactive weight.
//!
//! ## SMP
//!
//! Everything here is per-CPU by construction: the queue is
//! [`super::run_queue`] (this CPU's own), and the "who is running" record below is
//! a [`crate::smp::PerCpu`]. No cross-core state, no lock. Placement across cores
//! is a separate decision and is not made here.

use super::vcore::{Class, RunQueue, VcoreId};
use crate::smp::PerCpu;
use core::ptr::{addr_of, addr_of_mut};

/// Whether the run queue drives the cell pick.
///
/// A boot-time switch rather than a compile-time one, because the two orders must
/// be comparable **in the same binary** for a proof to show the ordering does
/// something: the `substrate` kernel enables it, runs a scenario, and asserts the
/// order differs from the round-robin the same scenario produces with it off. A
/// `cfg` would make that a two-binary comparison and therefore not a proof of
/// anything about either.
static mut ENABLED: bool = false;

/// The vcore this CPU last dispatched, and when (timer-domain ns).
///
/// `None` means nothing has been dispatched since the last reset - not "the CPU is
/// idle", which is a different statement the queue itself answers.
#[derive(Copy, Clone)]
struct Running {
    id: Option<VcoreId>,
    since_ns: u64,
}

impl Running {
    const NONE: Running = Running {
        id: None,
        since_ns: 0,
    };
}

static CURRENT: PerCpu<Running> = PerCpu::from_array([Running::NONE; crate::smp::MAX_CPUS]);

// Counters, so the seam's behaviour is observed rather than assumed
// (docs/ENGINEERING.md 1). They live in the observability plane's per-CPU blocks
// (`obs::cpu`, S4): every CPU dispatching a cell counts on its **own** line -
// cheaper than the relaxed `fetch_add`s these replaced (no locked RMW, no shared
// line bouncing between dispatching cores) and correct for the same reason, one
// writer per slot. The accessors below sum over CPUs, so every existing assertion
// reads the same totals.

/// Turn queue-driven dispatch on or off. Off is the pre-migration behaviour,
/// exactly.
pub fn enable(on: bool) {
    // SAFETY: single CPU at the boot/setup point this is called from.
    unsafe { *addr_of_mut!(ENABLED) = on };
}

/// Whether the run queue is driving the pick.
#[inline]
pub fn enabled() -> bool {
    // SAFETY: a plain bool read.
    unsafe { *addr_of!(ENABLED) }
}

/// (queue picks, round-robin picks, picks where the queue chose a different cell
/// than round-robin would have, ns charged).
///
/// The third number is the one that matters: it is the only direct evidence that
/// adopting the queue changed the order, and a proof that runs a mixed workload and
/// finds it zero has learned that the queue is decorative.
pub fn counters() -> (u64, u64, u64, u64) {
    (
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_PICKS),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_RR_PICKS),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_DIVERGED),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_CHARGED_NS),
    )
}

/// Clear the seam's per-CPU record and counters (between runs). Does **not** touch
/// the queue itself (`super::reset_run_queue` owns that) and does **not** clear
/// [`enable`].
///
/// The enable flag is deliberately left alone. It is a *policy* choice a boot makes
/// once, and `crate::user::reset` runs in the middle of setting up a run - on ARM64
/// after the cells' trap frames have been built, whose SPSR carries the IRQ mask
/// derived from this very flag. Clearing it here would silently produce frames built
/// for preemption running under a scheduler that had switched itself off, which is a
/// disagreement nothing would report.
pub fn reset() {
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs, nothing else is running on any CPU.
        unsafe { *CURRENT.get_mut(cpu) = Running::NONE };
    }
    for slot in [
        crate::obs::cpu::CTR_SCHED_PICKS,
        crate::obs::cpu::CTR_SCHED_RR_PICKS,
        crate::obs::cpu::CTR_SCHED_DIVERGED,
        crate::obs::cpu::CTR_SCHED_CHARGED_NS,
        crate::obs::cpu::CTR_SCHED_REARM_CALLS,
        crate::obs::cpu::CTR_SCHED_REARM_NO_RECORD,
    ] {
        crate::obs::cpu_counter_clear(slot);
    }
}

/// (return-to-user sites reached, of which found no running record). The pair
/// distinguishes stage E5 not being reached from E5 being reached and declining.
pub fn rearm_counters() -> (u64, u64) {
    (
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_REARM_CALLS),
        crate::obs::cpu_counter_sum(crate::obs::cpu::CTR_SCHED_REARM_NO_RECORD),
    )
}

/// Now, in the timer's own monotonic nanosecond domain.
#[inline]
fn now_ns() -> u64 {
    crate::arch::timer_now_ns()
}

/// This CPU's queue.
///
/// # Safety
/// The reference must not outlive the caller's critical section, and no second
/// reference may be alive. Every use below is a short, non-reentrant call.
#[inline]
#[allow(clippy::mut_from_ref)]
unsafe fn queue() -> &'static mut RunQueue {
    // SAFETY: delegated to the caller per the contract above.
    unsafe { super::run_queue() }
}

/// Make sure cell `cell`'s context `context` has a fair-class vcore on this CPU's
/// queue, returning its id. Idempotent.
///
/// A new vcore inherits its parent's burst state where there is one, which is what
/// keeps a forked child from arriving with a fresh interactive weight it did not
/// earn (`Burst::inherit`); `parent` is the *cell* whose context 0 to inherit from,
/// or `None` for a top-level cell.
pub fn track(cell: usize, context: usize, parent: Option<usize>) -> Option<VcoreId> {
    let (c, x) = (cell as u16, context as u16);
    // SAFETY: a short call on this CPU's own queue.
    let q = unsafe { queue() };
    if let Some(id) = q.find(c, x) {
        return Some(id);
    }
    let burst = parent
        .and_then(|p| q.find(p as u16, 0))
        .and_then(|pid| q.get(pid))
        .map(|pv| super::bore::Burst::inherit(&pv.burst))
        .unwrap_or_default();
    q.admit(c, x, Class::Fair, burst, now_ns()).ok()
}

/// Drop every vcore belonging to cell `cell` - its slot was handed back.
///
/// Called from the same places that free a cell slot, because a queue entry for a
/// cell that no longer exists is worse than a missing one: `sync_runnable` would
/// ask the personality about a dead cell, get "not runnable", and the entry would
/// sit blocked forever holding a `high_water` slot.
pub fn untrack(cell: usize) {
    let c = cell as u16;
    // SAFETY: a short call on this CPU's own queue.
    let q = unsafe { queue() };
    // Collected first: `remove` mutates, and the iterator borrows.
    while let Some(id) = q.any_of_cell(c) {
        // SAFETY: `CURRENT` names a vcore that is about to stop existing.
        unsafe {
            let cur = CURRENT.this_mut();
            if cur.id == Some(id) {
                *cur = Running::NONE;
            }
        }
        q.remove(id);
    }
}

/// Record that cell `cell`'s context `context` has started running, **arm its
/// preemption slice**, and report that slice.
///
/// Arming here rather than at each of the four switch sites is what makes "every
/// entry into a cell runs under a slice" structural rather than a convention four
/// call sites have to remember - the same reduction that made the FP/SIMD swap
/// correct once `switch_native_cell` became the only native cross-cell switch
/// (docs/LIBRHEO.md, the `SYS_YIELD` scar). [`super::preempt::arm`] is a no-op when
/// dispatch is disabled or the ISA has no wired timer interrupt, and says which
/// through its counters.
///
/// The slice is returned as well because a caller that wants to report or assert it
/// should not have to re-derive it.
pub fn running(cell: usize, context: usize) -> u64 {
    // SAFETY: a short call on this CPU's own queue.
    let q = unsafe { queue() };
    let now = now_ns();
    // **Admit it if the queue has not seen it.** "The vcore this CPU is running is in
    // the queue" is an invariant, not a hope, and it was neither before: the only thing
    // that admitted a vcore was `pick`'s `sync_runnable`, so a cell that never reached a
    // cell-level reschedule was never in the queue at all. `find` then returned `None`
    // here, the running record stayed empty, and everything downstream that needs it -
    // the CPU-time charge, the burst score, and stage E5's slice re-arm - silently did
    // nothing.
    //
    // It was measured rather than reasoned about, and only on one ISA: the same
    // multi-threaded binary produced 322 armed slices on x86-64 and **2** on ARM64,
    // because the first two happened to reschedule at cell level and the third did not.
    // The E5 return-to-user site was reached 472 times there and declined all 472,
    // which is what the two `rearm_counters` exist to distinguish (docs/SMP.md 10.2a).
    let id = match q.find(cell as u16, context as u16) {
        Some(id) => Some(id),
        // Fair class and a fresh burst: a vcore admitted here has been observed doing
        // nothing yet, and inventing a burst score for it would be the guess the BORE
        // design refuses. A queue with no metadata left is a `None` record, which is
        // exactly the pre-existing behaviour - degraded, not wrong.
        None => q
            .admit(
                cell as u16,
                context as u16,
                Class::Fair,
                super::bore::Burst::new(),
                now,
            )
            .ok(),
    };
    // SAFETY: this CPU's own slot.
    unsafe {
        *CURRENT.this_mut() = Running { id, since_ns: now };
    }
    let slice = q.current_slice_ns();
    super::preempt::arm(slice);
    slice
}

/// Re-arm the running vcore's slice for the time it has **left**, at the single
/// return-to-user site (docs/EXECUTION-MODEL.md 9, stage E5).
///
/// # The defect this fixes
///
/// A slice was armed at first entry, at a cell-level reschedule, and by the preemption
/// path itself - **not** on an ordinary syscall return. So a cell whose contexts are
/// scheduled *inside* the cell, which is Node's and Bun's shape, got one slice at first
/// entry, and if that slice did not happen to fire, nothing armed another. Measured on
/// the same binary: ARM64 armed **2** slices for two whole programs and took **0** timer
/// interrupts, where x86-64 armed 322 and riscv64 150 - the difference being futex timing
/// producing cell-level reschedules on two ISAs and not the third (docs/SMP.md 10.2a).
/// A preemption model that depends on how often a workload happens to reschedule is not
/// a preemption model.
///
/// # Why "remaining" and not a fresh slice
///
/// Arming a *full* slice here would be worse than the bug. A cell issuing a syscall
/// every 100 us would push its deadline out by a millisecond on every return and could
/// never be preempted at all - the starvation this is supposed to prevent, dressed as a
/// fix. So the deadline stays anchored to when the vcore **started running**: the slice
/// left is `slice_ns - (now - since_ns)`, and a vcore that has already spent its slice
/// gets the shortest deadline the arbiter will take, so it is preempted at the next
/// opportunity rather than never.
///
/// Nothing else about the running record is touched - in particular `since_ns` is **not**
/// reset, which is the whole point: resetting it would make the charge cover only the
/// time since the last syscall, so a syscall-heavy cell would be charged almost nothing
/// and its BORE burst score would be measured off a lie.
///
/// A no-op when this CPU has no running record (nothing dispatched here yet), and
/// [`super::preempt::arm`] is itself a no-op when dispatch is off or the ISA has no wired
/// timer - so a cooperative boot is byte-for-byte unchanged and says so through its
/// counters.
///
/// # Cost, measured
///
/// This is on the hottest path in the kernel, so its cost was measured rather than
/// assumed (riscv64 icount, `bench_core`'s `p2_user_syscall_floor`, which crosses the
/// real U-mode syscall boundary and runs on a boot with dispatch **off**):
///
/// | version | instructions per syscall |
/// |---|---|
/// | no re-arm at all | 178 |
/// | out-of-line, check inside | 197 (+19) |
/// | inlined check, cold body | **183 (+5)** |
///
/// +19 instructions to decide *not* to do anything is a call frame, not a decision. So
/// the check is `#[inline]` here and the work is [`rearm_remaining_slow`], which is
/// deliberately `#[inline(never)]`: a cooperative boot pays one load of a `bool` and a
/// branch, and a preemptive boot pays the call it actually needs. +5 is the floor without
/// making dispatch a `cfg`, and it is deliberately **not** a `cfg` - the two orders have
/// to be comparable in the same binary for any proof about the ordering to mean anything
/// (see [`ENABLED`]).
///
/// **What this does not measure, said plainly:** the cost on a boot where dispatch is
/// **on**, which is the one Node, Bun and Claude Code run under. There the body executes,
/// so each syscall return pays a timer-counter read and an arbiter registration that may
/// program the hardware one-shot. `bench_core` runs cooperatively and therefore cannot
/// report it, and enabling dispatch there would move every existing `p2_*`/`p5_*` number
/// and destroy their comparability. It is a named unmeasured cost, not a claimed-free one.
/// The trade it buys is not small: before this, a syscall-heavy cell was **never**
/// preempted at all on ARM64, so its siblings did not run.
///
/// The ordering inside matters for the same reason. `preempt::arm` already declines when
/// dispatch is off, but declining *after* the work is done is not declining - the first
/// version read the per-CPU record, took the queue and read the timer (a CSR on RISC-V,
/// `CNTVCT` on ARM64, APIC/TSC on x86-64) before reaching that check, so every
/// cooperative boot paid for a value it then threw away.
#[inline]
pub fn rearm_remaining() {
    if !enabled() {
        return;
    }
    rearm_remaining_slow();
}

/// The body of [`rearm_remaining`], out of line so the common "dispatch is off" case
/// costs a load and a branch at the call site rather than a call.
#[inline(never)]
fn rearm_remaining_slow() {
    crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_REARM_CALLS, 1);
    let rec = *CURRENT.this();
    if rec.id.is_none() {
        crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_REARM_NO_RECORD, 1);
        return;
    }
    // SAFETY: a short call on this CPU's own queue.
    let q = unsafe { queue() };
    let slice = q.current_slice_ns();
    let used = now_ns().saturating_sub(rec.since_ns);
    // `max(1)`: a deadline of 0 is "no deadline" to the arbiter, and this vcore is
    // exactly the one that must be preempted, so it gets the smallest real deadline
    // instead of none.
    super::preempt::arm(slice.saturating_sub(used).max(1));
}

/// Charge the running vcore for the CPU time it just used and mark it stopped.
///
/// `voluntary` distinguishes a cell that parked itself from one that was taken off
/// the CPU - the distinction BORE's score depends on. Returns the nanoseconds
/// charged, which is 0 when nothing was recorded as running (a first dispatch, or a
/// path that never called [`running`]).
fn stop(voluntary: bool, still_runnable: bool) -> u64 {
    let rec = *CURRENT.this();
    let Some(id) = rec.id else { return 0 };
    let now = now_ns();
    let delta = now.saturating_sub(rec.since_ns);
    // SAFETY: a short call on this CPU's own queue.
    let q = unsafe { queue() };
    if delta > 0 {
        let _ = q.charge(id, delta);
        crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_CHARGED_NS, delta);
        if voluntary {
            // `BurstNs` is defined as "how long a vcore ran before *voluntarily*
            // relinquishing" - the distribution the burst score claims to
            // summarise - so a preemption is deliberately not recorded into it. A
            // preempted run is not evidence about how long the vcore wanted to run.
            crate::metrics::record(crate::metrics::Metric::BurstNs, delta);
        }
    }
    // A vcore that stays runnable (a yield, a preemption) must NOT be marked
    // blocked - it is still competing. Only its burst state changes, and `block`
    // is the only thing that owns that transition, so the two cases are kept
    // apart here rather than by passing a flag into the queue.
    if !still_runnable {
        let _ = q.block(id, voluntary);
    } else if voluntary {
        q.relinquished(id);
    } else {
        q.was_preempted(id);
    }
    // SAFETY: this CPU's own slot.
    unsafe { *CURRENT.this_mut() = Running::NONE };
    delta
}

/// The running cell parked on a wake source: charge its time and mark it blocked.
///
/// The preemption slice is cancelled: the cell it was about is no longer running, and
/// leaving the deadline registered would keep the hardware one-shot armed for an
/// event nobody is waiting for - which on an ISA where the arbiter arms the *nearest*
/// deadline costs a spurious wake out of every idle. Whoever runs next gets a fresh
/// slice from [`running`].
pub fn relinquish() -> u64 {
    let ns = stop(true, false);
    super::preempt::disarm();
    ns
}

/// Cell `cell` gave up the CPU but stays runnable (`SYS_YIELD`, `sched_yield`).
pub fn yielded() -> u64 {
    stop(true, true)
}

/// The running cell was taken off the CPU by the preemption timer: it stays
/// runnable, and its burst is recorded as **not** having ended voluntarily.
pub fn preempted() -> u64 {
    stop(false, true)
}

/// Choose the next cell to run after `leaving`, over `cells` cell slots, given a
/// predicate that answers whether a cell slot is runnable **right now**.
///
/// With the queue disabled this is the pre-migration round-robin, expression for
/// expression. With it enabled the predicate still decides *who may* run and the
/// queue decides *who does*, and the two are reconciled first so they cannot
/// disagree.
///
/// The queue's answer is filtered through the predicate one last time before it is
/// returned. That is not belt-and-braces: `sync_runnable` reconciles by (cell,
/// context) and a cell with several contexts is runnable if *any* of them is, so a
/// queue entry can legitimately be ready while the caller's predicate - which is
/// per *cell* - answered differently. Preferring the predicate keeps the old
/// safety property exactly: a cell is only ever resumed when the personality says
/// it may be.
///
/// **The predicate is asked once per cell per decision**, memoized for the rest of
/// it. One pick used to evaluate it up to three times per cell - per queue entry
/// in `sync_runnable` (a multi-context cell has several), once in the final
/// filter, and up to once more in the divergence probe below - and the predicate
/// can itself walk the cell's whole context table (`linux::proc`'s `any_ready`),
/// so the repeats multiplied two table walks together. Nothing inside one pick
/// mutates personality state (`sync_runnable` writes only the queue), so the memo
/// is not a staleness trade: it makes the decision read **one** snapshot of
/// runnability instead of several, which is what reconcile-at-the-pick means.
pub fn pick<F: Fn(usize) -> bool>(leaving: usize, cells: usize, runnable: F) -> Option<usize> {
    // 0 = not asked, 1 = no, 2 = yes. `Cell` because the closure is shared with
    // `sync_runnable` as `Fn`; 16 bytes copied per query is nothing against the
    // context-table walk it replaces.
    let memo = core::cell::Cell::new([0u8; crate::user::MAX_CELLS]);
    let runnable = |i: usize| {
        let mut m = memo.get();
        match m.get(i).copied() {
            Some(1) => false,
            Some(2) => true,
            _ => {
                let r = runnable(i);
                if let Some(slot) = m.get_mut(i) {
                    *slot = 1 + r as u8;
                    memo.set(m);
                }
                r
            }
        }
    };
    let round_robin = || {
        (1..=cells)
            .map(|k| (leaving + k) % cells)
            .find(|&i| runnable(i))
    };
    if !enabled() {
        crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_RR_PICKS, 1);
        return round_robin();
    }
    let now = now_ns();
    // SAFETY: a short call on this CPU's own queue; no other reference is alive
    // (the closure below reads the caller's predicate, not the queue).
    let q = unsafe { queue() };
    q.sync_runnable(now, |cell, _context| runnable(cell as usize));
    let chosen = q.dispatch(now).and_then(|(id, _)| q.get(id)).and_then(|v| {
        let cell = v.cell as usize;
        runnable(cell).then_some(cell)
    });
    crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_PICKS, 1);
    if chosen.is_some() && chosen != round_robin() {
        crate::obs::cpu_bump(crate::obs::cpu::CTR_SCHED_DIVERGED, 1);
    }
    // Falling back to round-robin when the queue has no answer is deliberate and
    // is not a silent divergence: the queue only holds cells something called
    // `track` for, and a boot that enables dispatch without tracking every cell
    // would otherwise idle a CPU with runnable work on it. The counters above make
    // the two cases distinguishable after the fact.
    chosen.or_else(round_robin)
}

/// [`pick`], but never returning `leaving` itself - what `SYS_YIELD` needs.
///
/// A yield means "someone else, please": returning the caller would make the
/// syscall a no-op that still cost a switch. Kept as its own entry point rather
/// than a flag on [`pick`], because the two ranges (`1..cells` and `1..=cells`) are
/// the entire difference between "hand over" and "reconsider", and a boolean
/// argument at the call site would not say which is which.
pub fn pick_excluding_self<F: Fn(usize) -> bool>(
    leaving: usize,
    cells: usize,
    runnable: F,
) -> Option<usize> {
    pick(leaving, cells, |i| i != leaving && runnable(i))
}

/// Whether the running cell should be preempted in favour of another - the
/// predicate the preemption timer consults.
pub fn should_preempt() -> bool {
    if !enabled() {
        return false;
    }
    // SAFETY: a short call on this CPU's own queue.
    unsafe { queue() }.should_preempt()
}
