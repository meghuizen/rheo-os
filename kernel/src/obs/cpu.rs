//! The **snapshot plane**: one CPU's live state under a seqlock, and its monotone
//! counters beside it (docs/OBSERVABILITY.md 11, phase S3).
//!
//! This is the plane that answers "what is this CPU doing **now**" - the one
//! question the event ring cannot answer without replaying history and the text
//! ring cannot answer without a parse. The layout is [`crate::abi::obs::ObsCpu`]:
//! line 0 is a seqlock'd **coupled group** (state, current cell/entity/vcore, when
//! that began, the armed deadline, the receive tier), lines 1..7 are monotone
//! counters.
//!
//! # Why only the group is under the lock
//!
//! `(state, cur_cell, cur_entity, since_tick)` is one fact: a reader that catches
//! half of an update sees a cell that never ran an entity that never existed - not
//! a stale reading but a **false** one. The counters are outside because each is
//! independently meaningful - `irqs` from instant *t* beside `busy_ticks` from
//! *t+1* is two true facts - and bringing them in would put a fence on every bump.
//!
//! # The write protocol, and why these orderings
//!
//! The writer is always the owning CPU (partitioning, the [`crate::telemetry`]
//! argument), so the seqlock defends only the *reader*: another core, or a host
//! tool reading guest memory. Begin is a `fetch_add(1, Acquire)` - a store cannot
//! carry `Acquire`, and the RMW's acquire is what keeps the field writes from
//! being hoisted above the odd count. End is a `store(seq + 2, Release)`, which
//! keeps the field writes from sinking below the even count. The reader takes
//! `load(Acquire)`, reads the fields, then re-checks the count behind an
//! `Acquire` fence; odd or changed means retry. Field access is volatile on both
//! sides, because the race with a cross-core reader is the design, not an
//! accident to be narrowed away.
//!
//! On x86 (TSO) the two fences cost nothing and the protocol cannot be observed
//! failing; the orderings are for the weakly-ordered ISAs, and the host fuzzer
//! (`verify/obs`) drives writer against reader on real threads - which is the
//! only place in the tree the retry loop is reachable at all, QEMU TCG being far
//! too coarse to interleave a 6-store window.
//!
//! # Dependency-free
//!
//! Like [`super::ring`], this file names nothing but `crate::abi::obs`, so
//! `verify/obs/fuzz.rs` includes it verbatim and drives the shipped protocol on
//! the host. The per-CPU array, the enable gate and the call sites live in
//! [`super`] (`obs/mod.rs`).

use crate::abi::obs::ObsCpu;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

// ------------------------------------------------------------- counter slots
//
// Slot meanings are runtime data published through the name table
// (`OBS_SEC_NAMES`), not an ABI contract - a reader that meets an unnamed slot
// reports it unnamed rather than guessing. These constants are the kernel's own
// bookkeeping of which slot it writes.

/// Ticks this CPU spent executing anything - the complement of [`CTR_IDLE_TICKS`].
/// A spin that could not halt counts here, never as idle (docs/ENGINEERING.md 7).
pub const CTR_BUSY_TICKS: usize = 0;
/// Ticks this CPU spent genuinely halted in the scheduler idle state.
pub const CTR_IDLE_TICKS: usize = 1;
/// Entries into an execution entity (context switches into user work).
pub const CTR_DISPATCHES: usize = 2;
/// Parks that really halted the CPU. **Unconditional** (S4): bumped by the
/// scheduler idle state itself, mask or no mask - `idle::halts()` reads this
/// slot, and existing kernels assert it with recording off. What stays gated is
/// the *time attribution* ([`CTR_IDLE_TICKS`]), which needs the seqlock'd group.
pub const CTR_HALTS: usize = 3;
/// Idle iterations that could not halt and spun instead. Unconditional, as
/// [`CTR_HALTS`] - `idle::spins()` reads it.
pub const CTR_SPINS: usize = 4;
/// NIC receive interrupts taken (`net_rx::on_irq`). Non-zero is proof a real
/// device interrupt was delivered and serviced.
pub const CTR_NET_IRQS: usize = 5;
/// Receive-queue checks performed in the **hot** (bounded busy-poll) receive tier.
pub const CTR_NET_SPIN_POLLS: usize = 6;
/// Timer slices a receive wait halted for (warm + cold tiers).
pub const CTR_NET_SLICES: usize = 7;
/// Halts performed inside a receive wait (either idle mode).
pub const CTR_NET_HALTS: usize = 8;
/// Receive-tier escalations (hot -> warm, warm -> cold), summed over all waits.
pub const CTR_NET_ESCALATIONS: usize = 9;
/// Console bytes received (`input::rx_push`). The running count doubles as the
/// sequence number handed to the entropy pool - never the byte itself.
pub const CTR_CONSOLE_BYTES: usize = 10;
/// Injected console bytes recovered from the UART FIFO because the interrupt
/// did not deliver them. Zero on a healthy path.
pub const CTR_PUMP_FIFO_TAKES: usize = 11;
/// Injected console bytes pushed directly because neither the interrupt nor the
/// FIFO produced them. Zero on a healthy path.
pub const CTR_PUMP_DIRECT_PUSHES: usize = 12;

/// One coherent reading of the coupled group.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuSnap {
    pub state: u32,
    pub cell: u32,
    pub entity: u32,
    pub vcore: u32,
    pub since_tick: u64,
    pub timer_deadline_ns: u64,
    pub net_tier: u32,
}

/// Publish a new coupled group - the context-switch write.
///
/// `now` is the caller's `obs_tick()` reading; the elapsed interval since the
/// previous transition is charged to [`CTR_IDLE_TICKS`] when `charge_idle` (the
/// interval was a genuine halt) and to [`CTR_BUSY_TICKS`] otherwise, **before**
/// the group changes hands - so busy/idle accounting and the group's `since_tick`
/// are maintained by one writer at one place and cannot drift apart.
///
/// # Safety
/// `c` must be this CPU's own block (single writer by partitioning), valid for
/// the call's duration.
pub unsafe fn transition(
    c: *mut ObsCpu,
    new_state: u32,
    cell: u32,
    entity: u32,
    vcore: u32,
    now: u64,
    charge_idle: bool,
) {
    // SAFETY: owning CPU, per the contract; volatile because a cross-core reader
    // races these fields by design.
    unsafe {
        let since = read_volatile(&raw const (*c).since_tick);
        // First transition ever has since == 0; charging "everything since the
        // counter's origin" to busy would swamp the numbers, so the first interval
        // is dropped - the honest cost of not having a stamp before recording began.
        if since != 0 {
            let slot = if charge_idle {
                CTR_IDLE_TICKS
            } else {
                CTR_BUSY_TICKS
            };
            bump(c, slot, now.saturating_sub(since));
        }

        let seq = &(*c).seq;
        // Begin: odd. The RMW's Acquire keeps the field writes below it.
        let s = seq.fetch_add(1, Ordering::Acquire);
        write_volatile(&raw mut (*c).state, new_state);
        write_volatile(&raw mut (*c).cur_cell, cell);
        write_volatile(&raw mut (*c).cur_entity, entity);
        write_volatile(&raw mut (*c).cur_vcore, vcore);
        write_volatile(&raw mut (*c).since_tick, now);
        // End: even. The Release keeps the field writes above it.
        seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// Update one auxiliary field of the coupled group without changing the state.
///
/// Same bracket as [`transition`] - a single u64 store would not tear, but a
/// reader validating `seq` must never see a group that existed at no instant,
/// and "the deadline from *t+1* beside the cell from *t*" is exactly that.
///
/// # Safety
/// As [`transition`].
pub unsafe fn set_aux(c: *mut ObsCpu, timer_deadline_ns: Option<u64>, net_tier: Option<u32>) {
    // SAFETY: owning CPU, per the contract.
    unsafe {
        let seq = &(*c).seq;
        let s = seq.fetch_add(1, Ordering::Acquire);
        if let Some(d) = timer_deadline_ns {
            write_volatile(&raw mut (*c).timer_deadline_ns, d);
        }
        if let Some(t) = net_tier {
            write_volatile(&raw mut (*c).net_tier, t);
        }
        seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// Add `delta` to counter `slot`, returning the new value. Not an atomic RMW:
/// the owning CPU is the only writer, so a volatile read-add-write cannot lose
/// an update, and a cross-core reader of an aligned u64 sees either the old or
/// the new value. The return is free (the sum was just computed) and lets a
/// caller that needs the running count - an interrupt handler feeding a
/// sequence number to the entropy pool - avoid a second volatile read.
///
/// # Safety
/// `c` must be this CPU's own block.
pub unsafe fn bump(c: *mut ObsCpu, slot: usize, delta: u64) -> u64 {
    if slot >= crate::abi::obs::OBS_COUNTERS {
        return 0;
    }
    // SAFETY: owning CPU; in-bounds by the check above.
    unsafe {
        let p = (&raw mut (*c).counters[0]).add(slot);
        let v = read_volatile(p).wrapping_add(delta);
        write_volatile(p, v);
        v
    }
}

/// Overwrite counter `slot` with `v`. **Between-runs only**: the single-writer
/// rule means the owning CPU, and a reset path writing another CPU's slot is
/// sound only where every other reset in the tree is - no secondary executing.
///
/// # Safety
/// Either `c` is this CPU's own block, or no other CPU is executing.
pub unsafe fn set(c: *mut ObsCpu, slot: usize, v: u64) {
    if slot >= crate::abi::obs::OBS_COUNTERS {
        return;
    }
    // SAFETY: caller's contract; in-bounds by the check above.
    unsafe { write_volatile((&raw mut (*c).counters[0]).add(slot), v) }
}

/// Read counter `slot` - any core, torn-free because an aligned u64 load is
/// single-copy atomic on every ISA here.
///
/// # Safety
/// `c` must point at a live block. Racing the owning writer is the design.
pub unsafe fn counter(c: *const ObsCpu, slot: usize) -> u64 {
    if slot >= crate::abi::obs::OBS_COUNTERS {
        return 0;
    }
    // SAFETY: in-bounds; volatile read of a live block.
    unsafe { read_volatile((&raw const (*c).counters[0]).add(slot)) }
}

/// How many times [`read`] retries before reporting the group unreadable.
///
/// A reader must not loop forever: a writer that died inside its bracket (odd
/// `seq`, never closed) would hang every future reader, and a diagnostic plane
/// that can hang its reader is worse than none. 64 attempts is thousands of
/// cycles - far past any writer's 6-store window - so exhausting it means the
/// writer is wedged, which is itself the finding.
pub const READ_RETRIES: usize = 64;

/// One coherent reading of the coupled group, or `None` when a writer held the
/// bracket across [`READ_RETRIES`] attempts.
///
/// # Safety
/// `c` must point at a live block. Racing the owning writer is the design - the
/// seqlock is what turns that race into a retry instead of a torn group.
pub unsafe fn read(c: *const ObsCpu) -> Option<CpuSnap> {
    for _ in 0..READ_RETRIES {
        // SAFETY: volatile reads of a live block; racing the writer is the design.
        unsafe {
            let s1 = (*c).seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            let snap = CpuSnap {
                state: read_volatile(&raw const (*c).state),
                cell: read_volatile(&raw const (*c).cur_cell),
                entity: read_volatile(&raw const (*c).cur_entity),
                vcore: read_volatile(&raw const (*c).cur_vcore),
                since_tick: read_volatile(&raw const (*c).since_tick),
                timer_deadline_ns: read_volatile(&raw const (*c).timer_deadline_ns),
                net_tier: read_volatile(&raw const (*c).net_tier),
            };
            // The field reads must complete before the re-check; Acquire on the
            // second load alone would order later reads, not these.
            fence(Ordering::Acquire);
            if (*c).seq.load(Ordering::Relaxed) == s1 {
                return Some(snap);
            }
        }
    }
    None
}
