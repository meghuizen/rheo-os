//! **Structured tracing**: a stream of typed numeric events the kernel can be asked to
//! narrate, and the host can window and query (docs/LOGGING.md 5-6).
//!
//! # Why this exists, from evidence rather than from principle
//!
//! [`crate::telemetry`] already carries **text** lines - formatted, coalesced, per-CPU,
//! with loss recorded in place. That is the right shape for "what happened, in words".
//! It is the wrong shape for "what is this resource doing over time", and that second
//! question is the one this tree keeps failing to answer cheaply. Recorded instances,
//! all from one session's work on the fixed-table ceilings:
//!
//! - Frame costs were measured **six times** by hand, each time as a pool delta taken
//!   around an operation. A delta says a number changed; it does not say who caused it,
//!   so a first version of one such oracle reported "2431 frames" for a fork that had
//!   copied nothing (docs/ENGINEERING.md 11).
//! - Three separate leaks were found by an assertion noticing a nonzero total at the
//!   end, not by seeing the missing release. Each cost a full 10-minute matrix to
//!   localise.
//! - Three assertions were written at the **end** of a kernel where the harness resets
//!   at the *start* of a run, so they were vacuous - and that was invisible, because a
//!   final total cannot show that the thing it counts was destroyed before it looked.
//!
//! Every one of those is the same missing capability: **the lifecycle is not observable,
//! only its endpoints are.** A stream of `(who, what, when)` makes a leak the absence of
//! an event rather than an unexplained total, and makes a vacuous check visible as a
//! window with nothing in it.
//!
//! # Why numeric rather than text
//!
//! An event here is six integers, so emitting one is a bounds check and six stores - no
//! formatting, no allocation, nothing that changes the timing of what is being observed.
//! That matters because the paths worth tracing are the hot ones (frame allocation, a
//! table growing, a context switch), and a tracer that perturbs them measures itself.
//! Text is [`crate::telemetry`]'s job and stays there; the two are separate streams on
//! purpose, and a reader correlates them by `ts_ns`.
//!
//! # Windowing
//!
//! Every event carries a [`Subsys`] and an owner, and those two fields are the whole
//! point: they are what lets a reader ask for *one* subsystem's stream, or one cell's,
//! instead of a single interleaved scrollback in which the interesting thing is three
//! thousand lines from anything related to it. The shape is taken from cat9's treatment
//! of a command's output - a navigable buffer per source rather than one merged log -
//! and the host half is `cargo xtask trace`.
//!
//! # Cost when off
//!
//! One relaxed atomic load and a branch. Nothing is recorded until a boot calls
//! [`enable`], so every kernel that does not is byte-for-byte what it was.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Which subsystem produced an event - the **window key**.
///
/// Deliberately coarse: a window is only useful if a reader can name it without reading
/// the source first, and a taxonomy with fifty entries is a grep by another spelling.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Subsys {
    /// Kernel metadata frames (`mm::kmeta`) - the funded tables.
    Kmeta = 0,
    /// The physical frame allocator (`mm::frames`).
    Frames = 1,
    /// Execution entities: create, claim, park, exit (`sched::entity`).
    Entity = 2,
    /// Cell lifecycle: install, fork, free (`user`).
    Cell = 3,
    /// Scheduling decisions: dispatch, preempt, yield.
    Sched = 4,
    /// The Linux personality's synthesized state.
    Linux = 5,
}

impl Subsys {
    /// Every window, for a reader that wants to enumerate them.
    pub const ALL: [Subsys; 6] = [
        Subsys::Kmeta,
        Subsys::Frames,
        Subsys::Entity,
        Subsys::Cell,
        Subsys::Sched,
        Subsys::Linux,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Subsys::Kmeta => "kmeta",
            Subsys::Frames => "frames",
            Subsys::Entity => "entity",
            Subsys::Cell => "cell",
            Subsys::Sched => "sched",
            Subsys::Linux => "linux",
        }
    }
}

/// What happened. Subsystem-local, so `Kind::Take` under `Kmeta` and under `Frames` are
/// different events and read as such.
///
/// **Acquire/Release are a pair on purpose**: the host-side ledger balances them per
/// owner, and an unmatched acquire *is* the leak - visible as a line in the ledger rather
/// than inferred from a total that did not return to zero.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// A resource was taken. `a` = how many units, `b` = subsystem-specific detail.
    Acquire = 0,
    /// A resource was given back. Same fields.
    Release = 1,
    /// A charge moved between owners. `a` = units, `b` = the owner it came from.
    Transfer = 2,
    /// A request was refused. `a` = how many units were wanted.
    Refuse = 3,
    /// A state change with no resource attached. `a`/`b` are subsystem-specific.
    Note = 4,
}

/// The owner tag for the kernel itself, matching `mm::kmeta::Owner::KERNEL`.
pub const OWNER_KERNEL: u16 = u16::MAX;

/// One traced event: six integers, no formatting.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct Event {
    /// Monotonic nanoseconds, so this stream and the text one can be merged by a reader.
    pub ts_ns: u64,
    pub a: u64,
    pub b: u64,
    pub subsys: Subsys,
    pub kind: Kind,
    /// Cell index, or [`OWNER_KERNEL`].
    pub owner: u16,
    pub cpu: u16,
    /// Sequence number, so a reader can tell "nothing happened" from "the ring dropped
    /// it". A gap in the sequence is loss, located rather than counted.
    pub seq: u32,
}

impl Event {
    pub const EMPTY: Event = Event {
        ts_ns: 0,
        a: 0,
        b: 0,
        subsys: Subsys::Kmeta,
        kind: Kind::Note,
        owner: 0,
        cpu: 0,
        seq: 0,
    };
}

/// Events held before the oldest is overwritten.
///
/// A power of two so the index is a mask. 4096 events is 192 KiB, which is more than the
/// static tables this module exists to help remove - so it is **allocated only when a
/// boot enables tracing**, from the frame pool, and released on `reset`.
pub const CAPACITY: usize = 4096;

/// Ring storage, funded rather than static, for the reason above.
static mut RING: crate::mm::kmeta::Funded<Event> = crate::mm::kmeta::Funded::new();
static mut WRITTEN: u64 = 0;
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Events the ring could not hold. A reader sees loss as a sequence gap; this is the
/// total, for the summary line.
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn ring() -> &'static mut crate::mm::kmeta::Funded<Event> {
    // SAFETY: single writer per CPU is not yet enforced here - see the honesty note on
    // `enable`. Tracing is opt-in and used by single-CPU phases today.
    unsafe { &mut *core::ptr::addr_of_mut!(RING) }
}

/// Start recording, funding the ring from the kernel's own budget.
///
/// Returns false when the pool refuses it, which is a clean "tracing is off" rather than
/// a boot failure: an observability facility that can take a machine down is worse than
/// one that is absent.
///
/// **Honest scope**: the ring is one shared buffer with a plain counter, so it is
/// single-CPU today. Making it per-CPU is the same change [`crate::telemetry`] already
/// made (`MAX_RING_CPUS`), and is deliberately not copied here until a multi-core boot
/// wants to trace - a second unexercised mechanism is what this module exists to argue
/// against.
pub fn enable() -> bool {
    let r = ring();
    r.set_owner(crate::mm::kmeta::Owner::KERNEL);
    if !r.reserve(CAPACITY) {
        return false;
    }
    // SAFETY: single CPU, at enable time.
    unsafe { *core::ptr::addr_of_mut!(WRITTEN) = 0 };
    DROPPED.store(0, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
    true
}

/// Stop recording and give the ring's frames back.
pub fn reset() {
    ENABLED.store(false, Ordering::Release);
    ring().release();
    // SAFETY: single CPU, between runs.
    unsafe { *core::ptr::addr_of_mut!(WRITTEN) = 0 };
    DROPPED.store(0, Ordering::Relaxed);
}

/// Whether anything is being recorded.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Record one event. A no-op - one relaxed load and a branch - when tracing is off.
#[inline]
pub fn emit(subsys: Subsys, kind: Kind, owner: u16, a: u64, b: u64) {
    if !enabled() {
        return;
    }
    emit_slow(subsys, kind, owner, a, b);
}

#[inline(never)]
fn emit_slow(subsys: Subsys, kind: Kind, owner: u16, a: u64, b: u64) {
    let r = ring();
    if r.capacity() == 0 {
        return;
    }
    // SAFETY: single CPU while tracing (see `enable`).
    let seq = unsafe {
        let w = &mut *core::ptr::addr_of_mut!(WRITTEN);
        *w += 1;
        *w
    };
    if seq as usize > CAPACITY {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    let slot = ((seq - 1) as usize) & (CAPACITY - 1);
    r.set(
        slot,
        Event {
            ts_ns: crate::arch::timer_now_ns(),
            a,
            b,
            subsys,
            kind,
            owner,
            cpu: crate::smp::cpu_index() as u16,
            seq: seq as u32,
        },
    );
}

/// Events recorded, and events lost to the ring being full.
pub fn counters() -> (u64, u64) {
    // SAFETY: a read.
    let w = unsafe { *core::ptr::addr_of!(WRITTEN) };
    (w, DROPPED.load(Ordering::Acquire))
}

/// Print the recorded stream in the machine-readable form `cargo xtask trace` parses.
///
/// One line per event, prefixed `@E`, on the ordinary console - which is the channel that
/// already leaves QEMU, so this needs no new device and works identically on all three
/// ISAs. The fields are printed in a fixed order and nothing else is interleaved on those
/// lines, so the parser is a split rather than a grammar.
pub fn dump() {
    let (written, dropped) = counters();
    let held = (written as usize).min(CAPACITY);
    // The oldest surviving event: everything before it was overwritten.
    let first = written.saturating_sub(held as u64);
    crate::println!("@E# written={written} dropped={dropped} held={held} cap={CAPACITY}");
    for i in 0..held {
        let slot = ((first as usize) + i) & (CAPACITY - 1);
        let Some(e) = ring().get(slot) else { continue };
        if e.seq == 0 {
            continue;
        }
        crate::println!(
            "@E {} {} {} {} {} {} {} {}",
            e.seq,
            e.ts_ns,
            e.cpu,
            e.subsys.name(),
            e.kind as u8,
            e.owner,
            e.a,
            e.b
        );
    }
    crate::println!("@E. end");
}
