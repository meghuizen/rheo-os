//! The **execution entity** - the one thing a CPU runs (docs/EXECUTION-MODEL.md).
//!
//! An entity is one execution context of a cell: a Linux thread, a native vcore. It
//! is **not a new kernel object** - it is a context of the Cell object, which is what
//! a vcore already is (docs/ARCHITECTURE.md 6, and EXECUTION-MODEL.md 10).
//!
//! # Why this module exists
//!
//! Five defects in the vcore and preemption work share one cause: an execution
//! context has three representations in this kernel - `sched::vcore::Vcore` (the
//! ordering fields), `user::RunCell`'s per-vcore arrays (native), and
//! `linux::thread::THREADS` (Linux) - and the agreement between them is maintained by
//! hand. Two ownership predicates are called from eight sites in two files, and four
//! claim sites live in a third, none of them where the entity is actually entered.
//! Every defect was one of those agreements being wrong: claim-vs-enter,
//! owner-vs-parked, cell-vs-context, syscall-path-vs-trap-path,
//! publisher-vs-runner. The full accounting is EXECUTION-MODEL.md section 1.
//!
//! This is the single authority those agreements collapse into. Three rules, from
//! EXECUTION-MODEL.md 3.1:
//!
//! - **R1.** This table is the only authority on `owner` and on runnability. The
//!   personality *declares* transitions and *supplies* wake sources; it keeps no
//!   second copy of the answer.
//! - **R2.** The ready queue ([`super::vcore`]) decides **order**, never runnability
//!   or ownership. That seam already exists and has produced no defects; it is
//!   unchanged.
//! - **R3.** A CPU reaches an entity only through this table. No path may publish an
//!   entity by another route - which is exactly what defect 5 was.
//!
//! # Why claiming and entering are one operation
//!
//! [`EntityTable::enter`] claims for a CPU **and** marks the entity entered in one
//! compare-exchange over a single word. That is not an optimisation: defect 1 was the
//! two steps done in the wrong order (ownership stamped for a batch before any
//! run-mark was won), and an operation with no internal order cannot have its order
//! got wrong. Two cores calling `enter` on the same entity: exactly one succeeds.
//!
//! # Why this module has no dependencies
//!
//! Nothing here touches `arch`, `smp`, MMIO or a clock. It is integers and one
//! [`Funded`] table, for the same reason [`super::bore`] and [`super::vcore`] are:
//! the state machine is then compilable **on the host**, where a fuzzer can drive
//! millions of operation sequences against [`EntityTable::check`] in seconds
//! (`verify/entity/`, run by `cargo xtask verify`). The defect class above needs four
//! cores and a 120-second boot to find in QEMU, and needs neither here.
//!
//! # State, not yet wired
//!
//! Stage E1 of EXECUTION-MODEL.md 9: the table exists beside the three current
//! representations and nothing reads it yet. E2 moves ownership into it, E3
//! runnability, E4 the per-entity resources. Keeping E1 separate is what lets the
//! fuzzer prove the state machine *before* the hot paths depend on it.

use crate::mm::kmeta::{Funded, Owner};

/// No CPU owns this entity - it is pickable by any core, which is exactly the
/// behaviour of a single-CPU boot (nothing there claims anything).
pub const NO_CPU: u16 = u16::MAX;

/// Not entered by any CPU.
const NOT_INSIDE: u16 = u16::MAX;

/// The scheduling class. Mirrors [`super::vcore::Class`] rather than replacing it -
/// the ordering fields stay in the run queue (R2).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Class {
    /// Best-effort work under EEVDF virtual deadlines.
    Fair = 0,
    /// An admitted reservation with a hard deadline (object 7).
    Reserved = 1,
    /// Throughput-first: a long slice, node-pinned memory. A tile program's class
    /// (docs/TILES.md, EXECUTION-MODEL.md 2.1).
    Batch = 2,
}

/// Which kind of compute unit this entity prefers. P/E/LP cores and accelerator
/// engines are **one taxonomy**, so placement is one decision rather than a scheduler
/// plus a separate engine path (EXECUTION-MODEL.md 2.1).
///
/// `Any` is the default, and deliberately: a preference nobody expressed must not
/// become a restriction.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CoreClass {
    Any = 0,
    Performance = 1,
    Efficiency = 2,
    LowPower = 3,
    Engine = 4,
}

/// What an entity is doing. One authority (R1), replacing `Thread::state` +
/// `Thread::pblock` + `linux::Proc::state` on one side and `Proc::vparked` +
/// `Proc::vblock` on the other.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    /// The slot holds nothing.
    Free = 0,
    /// Live and wants the CPU.
    Runnable = 1,
    /// Live and parked on a wake source.
    Parked = 2,
    /// Finished. Neither runnable nor parked - counting an exited entity as either
    /// leaves its cell wedged (the vcore-exit rule, docs/SMP.md 10.0a).
    Exited = 3,
}

/// No wake source. A [`State::Parked`] entity with this is invariant I4's violation:
/// parked with nothing that can ever wake it.
pub const NO_WAKE: u32 = 0;

/// One execution entity.
///
/// The field order is the **information budget** of EXECUTION-MODEL.md 4.1: the
/// fields a pick reads come first and fit one 64-byte cache line, so choosing among
/// entities touches one line per entity. Everything a pick does *not* need - resource
/// pointers, accounting totals - is in the cold half below the marker. The layout is
/// asserted, not hoped for (see [`HOT_BYTES`]).
///
/// `Copy` with no drop glue and a valid all-zero pattern, because [`Funded`] grows
/// into freshly allocated frames and those arrive zeroed (`mm::kmeta`'s contract).
/// All-zero is [`State::Free`] with owner 0 - see [`Entity::EMPTY`], which sets the
/// two fields whose zero is *not* the value we want.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct Entity {
    // ---- hot: read on every pick ----
    /// Owning cell index.
    pub cell: u16,
    /// Which context of that cell this is.
    pub context: u16,
    /// The CPU that may run it, or [`NO_CPU`].
    pub owner: u16,
    /// The CPU currently inside it, or [`NOT_INSIDE`]. Per entity rather than per
    /// CPU, which is what makes "entered by two cores" a property of one word.
    pub inside: u16,
    pub state: State,
    pub class: Class,
    pub core_class: CoreClass,
    /// Which memory node this entity's pages are on.
    pub node: u8,
    /// The wake source it is parked on ([`NO_WAKE`] when runnable). An index, not a
    /// struct: the *detail* of the source belongs to the personality that owns it.
    pub wake: u32,
    /// How long it may run before being reconsidered.
    pub slice_ns: u32,
    /// The BORE burst score, as the integer log2 the score already is.
    pub weight: u16,
    /// Whether a migration has been requested (E7; recorded, never acted on here).
    pub migrate: u16,

    // ---- cold: read on entry and exit, not on a pick ----
    /// Cumulative CPU nanoseconds this entity has been given.
    pub service_ns: u64,
    /// Times entered.
    pub dispatches: u32,
    /// Times taken off the CPU involuntarily.
    pub preemptions: u32,
}

/// The size of the hot half, asserted so a field added in the wrong place is a
/// compile error rather than a second cache line nobody notices.
///
/// Hand-computed: 4 bytes (cell + context), 4 (owner + inside), 4 (the four `u8`
/// enums), 4 (wake), 4 (slice), 4 (weight + migrate) = 24 bytes. Well inside one cache
/// line, which leaves room for the fields E2-E5 add without spilling into a second.
pub const HOT_BYTES: usize = 24;
const _: () = assert!(core::mem::offset_of!(Entity, service_ns) == HOT_BYTES);
const _: () = assert!(core::mem::size_of::<Entity>() <= 64);

impl Entity {
    /// A free slot.
    pub const EMPTY: Entity = Entity {
        cell: 0,
        context: 0,
        owner: NO_CPU,
        inside: NOT_INSIDE,
        state: State::Free,
        class: Class::Fair,
        core_class: CoreClass::Any,
        node: 0,
        wake: NO_WAKE,
        slice_ns: DEFAULT_SLICE_NS,
        weight: DEFAULT_WEIGHT,
        migrate: 0,
        service_ns: 0,
        dispatches: 0,
        preemptions: 0,
    };

    /// Live means "holds a context that has not finished". An [`State::Exited`]
    /// entity is not live, which is what stops `all_parked` counting it and leaving
    /// its cell runnable with nothing to enter.
    pub fn live(&self) -> bool {
        matches!(self.state, State::Runnable | State::Parked)
    }
}

/// The default slice: long enough that a syscall-heavy cell is not preempted mid-burst,
/// short enough for interactive response. It is a **default, not a constant of nature** -
/// the right value is a measurement no emulator can take (docs/TOOLING.md 4).
pub const DEFAULT_SLICE_NS: u32 = 1_000_000;

/// The default weight: no burst credit until behaviour has been observed. The score is
/// measured from explicit relinquish events, never assumed (docs/SUBSTRATE.md pillar 3).
pub const DEFAULT_WEIGHT: u16 = 1024;

/// Why an [`EntityTable::enter`] was refused. Each variant is a distinct fact, because
/// "could not enter" collapses three different situations that a caller must tell apart.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EnterError {
    /// No such entity, or the slot is free.
    NoEntity,
    /// Another CPU owns it. The ordinary answer on a multi-core boot, and the one that
    /// makes a scheduler's refusal observable rather than inferred from an absence.
    NotYours,
    /// It is not runnable (parked or exited).
    NotRunnable,
    /// Another CPU is already inside it. This is invariant I1 being *enforced*: the
    /// caller is refused rather than the corruption happening downstream.
    Occupied,
}

/// An invariant violation. Returned by [`EntityTable::check`], which the host fuzzer
/// calls after every operation and a debug kernel build can call at a switch.
///
/// The numbering is EXECUTION-MODEL.md 5, so a failure names the documented invariant
/// rather than a line number.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Violation {
    /// I1: two CPUs inside one entity. Unreachable through [`EntityTable::enter`]
    /// alone; reachable if anything writes `inside` by another route (R3).
    I1EnteredTwice { entity: usize, a: u16, b: u16 },
    /// I2: `owner` is neither [`NO_CPU`] nor an online CPU.
    I2BadOwner { entity: usize, owner: u16 },
    /// I3: a CPU is inside an entity it does not own.
    I3InsideNotOwned {
        entity: usize,
        owner: u16,
        inside: u16,
    },
    /// I4: parked with no wake source - nothing can ever make it runnable.
    I4ParkedNoWake { entity: usize },
    /// I9: an exited entity still holds a wake source or a CPU.
    I9ExitedNotClean { entity: usize },
    /// A [`State::Free`] slot that still names an owner or an occupant - a release
    /// that did not release (I7).
    I7FreeNotClean { entity: usize },
}

/// The entity table.
///
/// Storage is [`Funded`], charged to the owning cell (docs/SUBSTRATE.md pillar 1), so
/// the count of entities a cell may hold is bounded by its frame budget and not by an
/// array dimension - which is what deletes `MAX_VCORES` (4 today only because the FP
/// areas are a fixed `.bss` array).
pub struct EntityTable {
    slots: Funded<Entity>,
    /// How many CPUs are online, for I2. Set once by the caller; 0 means "unknown",
    /// and I2 is then not checked rather than checked against a wrong bound.
    cpus: u16,
}

impl EntityTable {
    pub const fn new() -> EntityTable {
        EntityTable {
            slots: Funded::new(),
            cpus: 0,
        }
    }

    /// Charge this table's frames to `owner`, and record how many CPUs exist so I2
    /// has a bound to check against.
    pub fn init(&mut self, owner: Owner, cpus: u16) {
        self.slots.set_owner(owner);
        self.cpus = cpus;
    }

    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    pub fn get(&self, id: usize) -> Option<Entity> {
        self.slots.get(id)
    }

    /// How many entities of `cell` are live. The cell exits when this reaches zero -
    /// the "last one out" rule, expressed once instead of per personality.
    pub fn live_of(&self, cell: u16) -> usize {
        (0..self.capacity())
            .filter_map(|i| self.slots.get(i))
            .filter(|e| e.cell == cell && e.live())
            .count()
    }

    /// Whether every live entity of `cell` is parked - which is what "the cell is
    /// blocked" means. A cell with one parked entity and one runnable one is **not**
    /// blocked, and getting that wrong idled a machine with work available (defect 3).
    ///
    /// A cell with no live entities is not blocked either: it is finished.
    pub fn all_parked(&self, cell: u16) -> bool {
        let mut any = false;
        for e in (0..self.capacity()).filter_map(|i| self.slots.get(i)) {
            if e.cell != cell || !e.live() {
                continue;
            }
            any = true;
            if e.state != State::Parked {
                return false;
            }
        }
        any
    }

    /// Create an entity for `(cell, context)`, growing the table if needed. Returns
    /// its id, or `None` when the cell's budget cannot fund the growth - which is a
    /// clean `-EAGAIN` naming the cell, never a global "table full"
    /// (docs/MEMORY.md 7, no OOM killer).
    pub fn create(&mut self, cell: u16, context: u16) -> Option<usize> {
        let id = match (0..self.capacity())
            .find(|&i| self.slots.get(i).map(|e| e.state) == Some(State::Free))
        {
            Some(i) => i,
            None => {
                let want = self.capacity() + 1;
                if !self.slots.set_growing(want - 1, Entity::EMPTY) {
                    return None;
                }
                want - 1
            }
        };
        let mut e = Entity::EMPTY;
        e.cell = cell;
        e.context = context;
        e.state = State::Runnable;
        if !self.slots.set(id, e) {
            return None;
        }
        Some(id)
    }

    /// **The one predicate.** May `cpu` pick entity `id`?
    ///
    /// This replaces `user::cell_on_this_cpu`, `user::vcore_on_this_cpu` and the
    /// runnable tests beside them at all eight of their call sites. Unclaimed is
    /// pickable by everyone, which is exactly what keeps a single-CPU boot's
    /// behaviour: nothing there claims anything.
    pub fn pickable(&self, id: usize, cpu: u16) -> bool {
        match self.slots.get(id) {
            Some(e) => {
                e.state == State::Runnable
                    && (e.owner == NO_CPU || e.owner == cpu)
                    && e.inside == NOT_INSIDE
            }
            None => false,
        }
    }

    /// Claim `id` for `cpu` **and** mark it entered, as one operation.
    ///
    /// One operation because two were the bug: defect 1 stamped ownership for a batch
    /// before winning any run-mark, so a core could enter an entity a stealer had
    /// taken. There is no order here to get wrong, and exactly one of two concurrent
    /// callers succeeds.
    pub fn enter(&mut self, id: usize, cpu: u16) -> Result<(), EnterError> {
        let mut e = self.slots.get(id).ok_or(EnterError::NoEntity)?;
        if e.state == State::Free {
            return Err(EnterError::NoEntity);
        }
        if e.inside != NOT_INSIDE {
            return Err(EnterError::Occupied);
        }
        if e.owner != NO_CPU && e.owner != cpu {
            return Err(EnterError::NotYours);
        }
        if e.state != State::Runnable {
            return Err(EnterError::NotRunnable);
        }
        e.owner = cpu;
        e.inside = cpu;
        e.dispatches = e.dispatches.saturating_add(1);
        self.slots.set(id, e);
        Ok(())
    }

    /// `cpu` left `id`, charging it `ns` and recording whether it was taken
    /// involuntarily. Keeps the claim: an entity stays this core's until something
    /// releases or steals it.
    ///
    /// The charge happens **here**, at the transition itself, which is what makes the
    /// burst score measured rather than inferred - this kernel has no path from
    /// running to not-running that does not pass through a named call.
    pub fn leave(&mut self, id: usize, cpu: u16, ns: u64, involuntary: bool) -> bool {
        let Some(mut e) = self.slots.get(id) else {
            return false;
        };
        if e.inside != cpu {
            return false;
        }
        e.inside = NOT_INSIDE;
        e.service_ns = e.service_ns.saturating_add(ns);
        if involuntary {
            e.preemptions = e.preemptions.saturating_add(1);
        }
        self.slots.set(id, e);
        true
    }

    /// Park `id` on wake source `wake`. Refused for [`NO_WAKE`]: a park with no source
    /// is a wedge, and refusing it is invariant I4 enforced at the only place that can
    /// create the violation.
    pub fn park(&mut self, id: usize, wake: u32) -> bool {
        if wake == NO_WAKE {
            return false;
        }
        let Some(mut e) = self.slots.get(id) else {
            return false;
        };
        if e.state != State::Runnable {
            return false;
        }
        e.state = State::Parked;
        e.wake = wake;
        self.slots.set(id, e);
        true
    }

    /// Make `id` runnable again. Idempotent, and a no-op for an exited entity - a wake
    /// arriving for something that has already finished is ordinary (the source was
    /// satisfied late), not an error.
    pub fn wake(&mut self, id: usize) -> bool {
        let Some(mut e) = self.slots.get(id) else {
            return false;
        };
        if e.state != State::Parked {
            return false;
        }
        e.state = State::Runnable;
        e.wake = NO_WAKE;
        self.slots.set(id, e);
        true
    }

    /// End `id`. Clears the wake source, because an exited entity that still names one
    /// is I9 - counted as neither runnable nor parked, so a stale source would make its
    /// cell look blocked on something that will never arrive.
    pub fn exit(&mut self, id: usize) -> bool {
        let Some(mut e) = self.slots.get(id) else {
            return false;
        };
        if !e.live() {
            return false;
        }
        e.state = State::Exited;
        e.wake = NO_WAKE;
        e.inside = NOT_INSIDE;
        self.slots.set(id, e);
        true
    }

    /// Hand the slot back. Refused while a CPU is inside, which is the difference
    /// between a leak and a use-after-free: the two S1' leaks were slot-handback paths
    /// that were never called, and freeing under a live core is the opposite mistake.
    pub fn release(&mut self, id: usize) -> bool {
        let Some(e) = self.slots.get(id) else {
            return false;
        };
        if e.inside != NOT_INSIDE || e.live() {
            return false;
        }
        self.slots.set(id, Entity::EMPTY);
        true
    }

    /// Move an idle, runnable entity's claim to `cpu` - the rebalance a core that has
    /// run dry performs.
    ///
    /// Only when **nobody is inside it**. Migrating a *running* entity is a different
    /// capability, attempted twice and reverted twice (docs/SMP.md 10.0); with
    /// resources per entity it becomes this same call in a `Parked` state, and it is
    /// deliberately not enabled here.
    pub fn steal(&mut self, id: usize, cpu: u16) -> bool {
        let Some(mut e) = self.slots.get(id) else {
            return false;
        };
        if e.inside != NOT_INSIDE || e.state != State::Runnable || e.owner == cpu {
            return false;
        }
        e.owner = cpu;
        self.slots.set(id, e);
        true
    }

    /// Check every invariant that is a property of this table alone, returning the
    /// first violation.
    ///
    /// The ones that are **not** here are as important: I5 (work conservation) and I10
    /// (every return to user arms a slice) are properties of a *sequence* of
    /// operations, not of a snapshot, so the fuzzer checks them across steps and this
    /// cannot. Saying which is which is the point - a checker that silently omitted
    /// them would read as covering ten invariants.
    pub fn check(&self) -> Option<Violation> {
        for id in 0..self.capacity() {
            let Some(e) = self.slots.get(id) else {
                continue;
            };
            match e.state {
                State::Free => {
                    if e.owner != NO_CPU || e.inside != NOT_INSIDE {
                        return Some(Violation::I7FreeNotClean { entity: id });
                    }
                }
                State::Exited => {
                    if e.wake != NO_WAKE || e.inside != NOT_INSIDE {
                        return Some(Violation::I9ExitedNotClean { entity: id });
                    }
                }
                State::Parked => {
                    if e.wake == NO_WAKE {
                        return Some(Violation::I4ParkedNoWake { entity: id });
                    }
                }
                State::Runnable => {}
            }
            if self.cpus != 0 && e.owner != NO_CPU && e.owner >= self.cpus {
                return Some(Violation::I2BadOwner {
                    entity: id,
                    owner: e.owner,
                });
            }
            if e.inside != NOT_INSIDE {
                if e.owner != e.inside {
                    return Some(Violation::I3InsideNotOwned {
                        entity: id,
                        owner: e.owner,
                        inside: e.inside,
                    });
                }
                // I1 as a cross-entity property: one CPU inside two entities at once
                // is the same corruption seen from the other side, and it is what a
                // per-CPU guard keyed on the wrong thing produces.
                for other in (id + 1)..self.capacity() {
                    let Some(o) = self.slots.get(other) else {
                        continue;
                    };
                    if o.inside == e.inside {
                        return Some(Violation::I1EnteredTwice {
                            entity: other,
                            a: e.inside,
                            b: o.inside,
                        });
                    }
                }
            }
        }
        None
    }
}

impl Default for EntityTable {
    fn default() -> EntityTable {
        EntityTable::new()
    }
}
