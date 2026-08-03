//! **Structured tracing**, now a compatibility shim over [`crate::obs`]
//! (docs/OBSERVABILITY.md 11, docs/LOGGING.md 5-6).
//!
//! # What moved, and what did not
//!
//! This module's design was right and its storage was wrong. It said so itself: the
//! ring was "one shared buffer with a plain counter, so it is single-CPU today", and
//! deferred the fix until a multi-core boot wanted to trace. That fix is
//! [`crate::obs::ring`] - one ring per CPU, one sequence counter per CPU, safe by
//! partitioning rather than by hoping - and this file is the seam that keeps the
//! callers and the on-wire format from moving with it.
//!
//! Everything a caller names still resolves and still means the same thing:
//! [`Subsys`] and [`Kind`] keep their discriminants, [`emit`] keeps its argument
//! order, and the `@E` line format is unchanged, because `cargo xtask trace` parses
//! it and `tests/src/smp.rs` asserts on it. Renaming a module is not a proof, so
//! nothing was renamed; what changed is where the events go.
//!
//! # Why the original module's reasoning is worth keeping
//!
//! It was written from evidence rather than principle, and the evidence has not
//! expired. Frame costs were measured **six times** by hand as pool deltas, and a
//! delta says a number changed without saying who caused it - one such oracle
//! reported "2431 frames" for a fork that had copied nothing. Three leaks were found
//! by an assertion noticing a nonzero total at the end rather than by seeing the
//! missing release. Three assertions were written at the *end* of a kernel whose
//! harness resets at the *start* of a run, so they were vacuous, and that was
//! invisible because a final total cannot show that the thing it counts was
//! destroyed before it looked.
//!
//! All of those are one missing capability: **the lifecycle is not observable, only
//! its endpoints are.** A stream of `(who, what, when)` makes a leak the absence of
//! an event rather than an unexplained total, and a vacuous check a window with
//! nothing in it.

pub use crate::abi::obs::OWNER_KERNEL;
use crate::obs;

/// Which subsystem produced an event - the **window key**.
///
/// The six original windows. [`crate::obs::Window`] is the full set; this enum
/// remains because callers and the `@E` format name these six, and their
/// discriminants are part of that format.
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
        self.window().name()
    }

    /// The corresponding full-set window.
    fn window(self) -> obs::Window {
        match self {
            Subsys::Kmeta => obs::Window::Kmeta,
            Subsys::Frames => obs::Window::Frames,
            Subsys::Entity => obs::Window::Entity,
            Subsys::Cell => obs::Window::Cell,
            Subsys::Sched => obs::Window::Sched,
            Subsys::Linux => obs::Window::Linux,
        }
    }
}

/// What happened. Subsystem-local, so `Kind::Acquire` under `Kmeta` and under
/// `Frames` are different events and read as such.
///
/// **Acquire/Release are a pair on purpose**: the host-side ledger balances them per
/// owner, and an unmatched acquire *is* the leak - visible as a line in the ledger
/// rather than inferred from a total that did not return to zero.
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

impl Kind {
    fn kind(self) -> obs::Kind {
        match self {
            Kind::Acquire => obs::Kind::Acquire,
            Kind::Release => obs::Kind::Release,
            Kind::Transfer => obs::Kind::Transfer,
            Kind::Refuse => obs::Kind::Refuse,
            Kind::Note => obs::Kind::Note,
        }
    }
}

/// Events held before the oldest is overwritten - now **per CPU**.
pub const CAPACITY: usize = obs::ring::RING_EVENTS;

/// Start recording, funding this CPU's ring from the kernel's own budget.
///
/// Returns false when the pool refuses it, which is a clean "tracing is off" rather
/// than a boot failure: an observability facility that can take a machine down is
/// worse than one that is absent.
pub fn enable() -> bool {
    obs::enable()
}

/// Stop recording and give the rings' frames back.
pub fn reset() {
    obs::reset()
}

/// Whether anything is being recorded.
#[inline]
pub fn enabled() -> bool {
    obs::enabled()
}

/// Record one event. A no-op - one relaxed load and a branch - when tracing is off.
#[inline]
pub fn emit(subsys: Subsys, kind: Kind, owner: u16, a: u64, b: u64) {
    obs::emit(subsys.window(), kind.kind(), owner, a, b);
}

/// `(events recorded, events lost to a ring being full)`.
///
/// The second number is **derived** now rather than counted: a ring holds
/// [`CAPACITY`] events, so anything written beyond that has been overwritten, and no
/// increment on the emit path is needed to know it. Same meaning as before - "a total
/// computed from the dump below would be incomplete" - reached without an atomic
/// read-modify-write per event.
pub fn counters() -> (u64, u64) {
    let (written, _unfunded) = obs::counters();
    (written, obs::overwritten())
}

/// Print the recorded stream in the machine-readable form `cargo xtask trace` parses.
pub fn dump() {
    obs::dump()
}
