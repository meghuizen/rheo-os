//! **The per-CPU vcore run queue** (docs/SCHEDULING.md 11.3, docs/SUBSTRATE.md
//! pillar 3): one deadline-ordered ready structure holding reserved, fair and
//! residual work.
//!
//! ## The keystone decision: one order, not two schedulers
//!
//! This kernel already schedules *reserved* work by EDF - a reservation is
//! (budget, period, deadline), admitted by the math in [`super`] and ordered by
//! earliest deadline. The temptation, when best-effort work needs scheduling too,
//! is to bolt a fairness engine beside it and arbitrate between the two. That is
//! what Linux grew (a deadline class, a real-time class, and CFS), and it is what
//! makes "which class wins" a policy question with no good answer.
//!
//! EEVDF supplies the unification: give every best-effort vcore a **virtual
//! deadline** and the whole queue becomes one order. Reserved vcores carry *hard*
//! deadlines, fair vcores carry *virtual* ones, residual work carries a deadline
//! at infinity and runs only on slack. There is no priority axis, no class
//! arbitration, and nothing for a cell to declare and be believed about -
//! "importance is an admission-controlled contract, never a priority number"
//! (docs/SCHEDULING.md), now true of the ready queue itself.
//!
//! ## EEVDF in integers
//!
//! Two quantities per vcore:
//!
//! - **`vruntime`**: service received, measured in *virtual* time. Running for
//!   `d` nanoseconds advances it by `d * WEIGHT_BASE / weight`, so a heavy
//!   (interactive, low-burst) vcore's virtual clock runs slower and it is served
//!   more often.
//! - **`vdeadline`**: `vruntime + slice * WEIGHT_BASE / weight`. The load-bearing
//!   property is that **a vcore asking for a smaller slice gets an earlier
//!   deadline** - latency is bought by asking for less, not by ranking higher.
//!
//! And one per queue:
//!
//! - **`vtime`**: the queue's virtual clock, advanced by `d * WEIGHT_BASE /
//!   total_weight`. A vcore is **eligible** when `vruntime <= vtime`, i.e. when it
//!   has not yet consumed more than its fair share. Among eligible vcores, the
//!   earliest virtual deadline runs.
//!
//! The eligibility gate is what makes this EEVDF rather than plain
//! earliest-deadline-first over virtual deadlines: without it, a vcore that
//! repeatedly asks for tiny slices would monopolise the CPU by always having the
//! nearest deadline. With it, such a vcore runs first *and then becomes
//! ineligible* until the queue's clock catches up.
//!
//! All of it is `u64`/`i64` arithmetic. `WEIGHT_BASE` is the fixed-point scale;
//! there is no floating point, which is the same constraint that made BORE's
//! bit-length score usable (see [`super::bore`] and docs/SUBSTRATE.md pillar 4).
//!
//! ## What BORE contributes
//!
//! The `weight` term above **is** the BORE burst score's weight
//! ([`super::bore::Burst::weight`]). Nothing else connects them: a short-burst
//! vcore has a high weight, so its virtual clock advances slowly and its virtual
//! deadline is near, so it is served promptly. A compute-bound vcore's weight
//! falls, its deadline recedes, and throughput is preserved because it still runs
//! - just later. One measured signal, one order, no knobs.
//!
//! ## SMP
//!
//! One queue per CPU, in [`crate::smp::PerCpu`] state. No global run queue and no
//! cross-CPU balancer: the multikernel model partitions cores rather than
//! balancing across shared state (docs/SCHEDULING.md 1a and 11.5, which names
//! rejecting cross-LLC balancing explicitly). Placement - which core a vcore
//! belongs to - is a separate, slower decision made when a vcore is created or
//! when a NUMA/core-class policy migrates it, never per dispatch.
//!
//! ## Status
//!
//! **The ordering logic is complete and testable; preemptive dispatch is not
//! wired.** This module is a pure data structure plus its arithmetic: it decides
//! *who should run next* given a set of vcores and the time they have consumed,
//! and every one of those decisions is checkable against a hand-computed oracle
//! with no hardware. Actually taking the CPU away from a running vcore at a timer
//! deadline, and running two vcores on two cores, is the rest of SMP phase 2
//! (docs/SMP.md 10) - whose first deliverable is the safety audit, not this queue.
//! Keeping the two separable is deliberate: the algorithm can be proven before
//! anything preempts, which is the only order in which a scheduler change is
//! reviewable.

use super::bore::{self, Burst, WEIGHT_BASE};
use crate::mm::kmeta::{Funded, Owner};

/// Which order a vcore is scheduled in.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Class {
    /// An admitted reservation (object 7): a **hard** deadline, ordered by EDF.
    /// Always ahead of fair work, because the admission controller has already
    /// promised it and refused anything it could not keep.
    Reserved,
    /// Ordinary best-effort work: a **virtual** deadline, EEVDF-ordered, weighted
    /// by its BORE burst score.
    Fair,
    /// Work that runs only on slack - a deadline at infinity. Never starved to
    /// death (it runs whenever nothing else is eligible), never allowed to delay
    /// anything else.
    Residual,
}

/// A vcore: one kernel-scheduled execution context, the unit the kernel grants to
/// a cell (docs/CONCURRENCY.md - the cell's runtime schedules *strands* onto it).
#[derive(Copy, Clone, Debug)]
pub struct Vcore {
    /// Owning cell index.
    pub cell: u16,
    /// Which of the cell's contexts this is (a Linux thread, a native vcore).
    pub context: u16,
    /// Scheduling class.
    pub class: Class,
    /// Virtual time consumed.
    vruntime: u64,
    /// Virtual deadline: when this vcore should have been served by.
    vdeadline: u64,
    /// Requested slice in nanoseconds - how long it wants to run before being
    /// reconsidered. Smaller means an earlier deadline (see the module docs).
    slice_ns: u64,
    /// Hard deadline in absolute timer-domain nanoseconds. Meaningful only for
    /// [`Class::Reserved`].
    hard_deadline_ns: u64,
    /// Burst state, which supplies the weight.
    pub burst: Burst,
    /// Runnable (as opposed to blocked on a wake source).
    runnable: bool,
    /// In use.
    live: bool,
    /// Absolute time this vcore was last made runnable, so queue delay -
    /// runnable-to-running, the responsiveness number - can be measured rather
    /// than estimated.
    runnable_since_ns: u64,
    /// Cumulative CPU nanoseconds - what the cell is actually getting.
    pub service_ns: u64,
    /// Times dispatched.
    pub dispatches: u64,
}

impl Vcore {
    const EMPTY: Vcore = Vcore {
        cell: 0,
        context: 0,
        class: Class::Fair,
        vruntime: 0,
        vdeadline: 0,
        slice_ns: DEFAULT_SLICE_NS,
        hard_deadline_ns: u64::MAX,
        burst: Burst::new(),
        runnable: false,
        live: false,
        runnable_since_ns: 0,
        service_ns: 0,
        dispatches: 0,
    };

    /// This vcore's current scheduling weight (from its burst score).
    #[inline]
    pub fn weight(&self) -> u64 {
        match self.class {
            // A reservation's share is what admission granted it; its ordering is
            // by hard deadline, so the burst weight would double-count. Fixed at
            // the base weight so its virtual clock advances at real speed.
            Class::Reserved => WEIGHT_BASE,
            Class::Fair => self.burst.weight(),
            // Residual work gets the floor weight: it advances its virtual clock
            // fastest, so it yields to anything else that becomes eligible.
            Class::Residual => 1,
        }
    }

    /// Whether this vcore is live and runnable.
    pub fn ready(&self) -> bool {
        self.live && self.runnable
    }

    /// Virtual time consumed.
    pub fn vruntime(&self) -> u64 {
        self.vruntime
    }

    /// Current virtual deadline.
    pub fn vdeadline(&self) -> u64 {
        self.vdeadline
    }

    /// Requested slice.
    pub fn slice_ns(&self) -> u64 {
        self.slice_ns
    }

    /// Recompute the virtual deadline from the current vruntime, slice and weight.
    /// Called whenever any of the three changes.
    fn refresh_deadline(&mut self) {
        let w = self.weight().max(1);
        // slice scaled into virtual time: a lighter vcore's slice costs more
        // virtual time, pushing its deadline out.
        let scaled = self.slice_ns.saturating_mul(WEIGHT_BASE) / w;
        self.vdeadline = self.vruntime.saturating_add(scaled.max(1));
    }
}

/// Default slice a vcore asks for when it says nothing: 1 ms.
///
/// A middle value on purpose. Far smaller and a compute-bound vcore pays context
/// switches for no benefit; far larger and an interactive vcore waits behind it.
/// A vcore that cares states its own ([`RunQueue::set_slice`]) and, per EEVDF,
/// asking for less is exactly how it buys latency.
pub const DEFAULT_SLICE_NS: u64 = 1_000_000;

/// Why an operation on the queue failed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QueueError {
    /// The vcore table could not grow: the owner is out of budget.
    NoMetadata,
    /// No vcore with that handle.
    NoSuchVcore,
}

/// A handle to a vcore in a queue.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VcoreId(u32);

impl VcoreId {
    /// The raw index, for a diagnostic.
    pub fn index(self) -> u32 {
        self.0
    }
}

/// What one walk over the queue found: the best candidate per class, and
/// whether the eligibility gate is holding back the earliest fair deadline.
/// Produced by `RunQueue::scan`, consumed by `pick`, `eligibility_would_defer`
/// and - both answers from the same walk - `dispatch`.
struct Scan {
    reserved: Option<(u64, VcoreId)>,
    eligible: Option<(u64, VcoreId)>,
    ineligible: Option<(u64, VcoreId)>,
    residual: Option<(u64, VcoreId)>,
    earliest_fair_ineligible: bool,
}

impl Scan {
    /// The pick order [`RunQueue::pick`] documents: reserved, then eligible
    /// fair, then ineligible fair, then residual.
    fn pick_id(&self) -> Option<VcoreId> {
        self.reserved
            .or(self.eligible)
            .or(self.ineligible)
            .or(self.residual)
            .map(|(_, id)| id)
    }
}

/// One CPU's ready queue.
pub struct RunQueue {
    vcores: Funded<Vcore>,
    high_water: usize,
    /// The queue's virtual clock. Advanced by real service divided by total
    /// runnable weight, so it tracks "the virtual time a perfectly fair share
    /// would have reached".
    vtime: u64,
    /// Sum of the weights of runnable vcores - the denominator of the virtual
    /// clock. Cached because it is read on every dispatch and every service
    /// charge, and changes only when a vcore becomes runnable, blocks, or has its
    /// weight change.
    total_weight: u64,
    /// The currently running vcore, if any.
    current: Option<VcoreId>,
    /// Counters: the evidence for what the scheduler actually did.
    dispatches: u64,
    preemptions: u64,
    voluntary_yields: u64,
    /// Times the eligibility gate deferred a vcore that held the earliest
    /// deadline: direct evidence that EEVDF's distinguishing rule is doing
    /// something, which is otherwise invisible (the result looks like plain EDF).
    eligibility_defers: u64,
}

impl RunQueue {
    /// An empty queue holding no storage.
    pub const fn new() -> RunQueue {
        RunQueue {
            vcores: Funded::new(),
            high_water: 0,
            vtime: 0,
            total_weight: 0,
            current: None,
            dispatches: 0,
            preemptions: 0,
            voluntary_yields: 0,
            eligibility_defers: 0,
        }
    }

    /// Charge the vcore table to `owner`, and reset the queue.
    pub fn init(&mut self, owner: Owner) {
        self.release();
        self.vcores.set_owner(owner);
    }

    /// Release all storage and return to empty, counters included.
    ///
    /// The counters are cleared here rather than kept, because this is the
    /// between-runs teardown and a proof that asserts "N dispatches" must be
    /// asserting about *its* run. A queue that carried a previous boot's totals would
    /// make every such assertion depend on test ordering.
    pub fn release(&mut self) {
        self.vcores.release();
        self.high_water = 0;
        self.vtime = 0;
        self.total_weight = 0;
        self.current = None;
        self.dispatches = 0;
        self.preemptions = 0;
        self.voluntary_yields = 0;
        self.eligibility_defers = 0;
    }

    /// `(dispatches, preemptions, voluntary yields, eligibility defers)`.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.dispatches,
            self.preemptions,
            self.voluntary_yields,
            self.eligibility_defers,
        )
    }

    /// The queue's virtual clock.
    pub fn vtime(&self) -> u64 {
        self.vtime
    }

    /// Sum of runnable weights.
    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// The running vcore, if any.
    pub fn current(&self) -> Option<VcoreId> {
        self.current
    }

    /// Read a vcore.
    pub fn get(&self, id: VcoreId) -> Option<Vcore> {
        match self.vcores.get(id.0 as usize) {
            Some(v) if v.live => Some(v),
            _ => None,
        }
    }

    fn set(&mut self, id: VcoreId, v: Vcore) {
        self.vcores.set(id.0 as usize, v);
    }

    /// Live vcores.
    ///
    /// Bounded by `high_water`, not capacity, and reading each slot **by
    /// reference** before deciding to copy it: capacity is a whole page's worth
    /// of slots (85 for a 48-byte `Vcore`), so a queue holding a handful of
    /// vcores must not pay a full-page walk - the bound is what keeps the small
    /// queue cheap, and `get_ref` is what stops the 48-byte copy per dead slot.
    pub fn iter(&self) -> impl Iterator<Item = (VcoreId, Vcore)> + '_ {
        (0..self.high_water).filter_map(move |i| match self.vcores.get_ref(i) {
            Some(v) if v.live => Some((VcoreId(i as u32), *v)),
            _ => None,
        })
    }

    /// Live vcores.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Frames the vcore table holds - what the owner is charged for scheduler
    /// bookkeeping. The counterpart of [`crate::mm::vaspace::VaSpace::metadata_frames`].
    pub fn metadata_frames(&self) -> usize {
        self.vcores.frames_held()
    }

    /// Whether the queue holds no vcores.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Runnable vcores.
    pub fn runnable(&self) -> usize {
        self.iter().filter(|(_, v)| v.ready()).count()
    }

    /// Admit a vcore to this queue.
    ///
    /// It starts **at the queue's current virtual time**, not at zero. Starting a
    /// newcomer at zero would make it maximally eligible with the earliest
    /// possible deadline, so it would monopolise the CPU until its virtual clock
    /// caught up with everyone else's - the classic "new task starves the queue"
    /// bug. Joining at `vtime` gives it a fair share from now on and no claim on
    /// the past.
    pub fn admit(
        &mut self,
        cell: u16,
        context: u16,
        class: Class,
        burst: Burst,
        now_ns: u64,
    ) -> Result<VcoreId, QueueError> {
        let index = self.free_index().ok_or(QueueError::NoMetadata)?;
        let mut v = Vcore {
            cell,
            context,
            class,
            vruntime: self.vtime,
            vdeadline: 0,
            slice_ns: DEFAULT_SLICE_NS,
            hard_deadline_ns: u64::MAX,
            burst,
            runnable: true,
            live: true,
            runnable_since_ns: now_ns,
            service_ns: 0,
            dispatches: 0,
        };
        v.refresh_deadline();
        self.total_weight = self.total_weight.saturating_add(v.weight());
        let id = VcoreId(index as u32);
        if !self.vcores.set_growing(index, v) {
            self.total_weight = self.total_weight.saturating_sub(v.weight());
            return Err(QueueError::NoMetadata);
        }
        if index >= self.high_water {
            self.high_water = index + 1;
        }
        Ok(id)
    }

    /// The vcore representing cell `cell`'s context `context`, if this queue holds
    /// one. The reverse of [`Vcore::cell`]/[`Vcore::context`], so a caller that
    /// knows a cell can find its scheduling entity without keeping a second table
    /// (which would be one more thing to keep in step).
    pub fn find(&self, cell: u16, context: u16) -> Option<VcoreId> {
        self.iter()
            .find(|(_, v)| v.live && v.cell == cell && v.context == context)
            .map(|(id, _)| id)
    }

    /// Any live vcore belonging to cell `cell`, for a caller tearing the cell down.
    ///
    /// A `RunQueue` method rather than a `live` accessor on [`Vcore`], because
    /// liveness is the queue's own bookkeeping: a caller that could ask a `Vcore`
    /// whether it is live would be holding a copy of a slot that may already have
    /// been reused, and the answer would be about the copy.
    pub fn any_of_cell(&self, cell: u16) -> Option<VcoreId> {
        self.iter()
            .find(|(_, v)| v.live && v.cell == cell)
            .map(|(id, _)| id)
    }

    /// Bring every live vcore's runnable flag into agreement with `ready`, which
    /// answers "is this (cell, context) runnable?" from whatever holds the real
    /// authority.
    ///
    /// This exists because the queue is being adopted **beside** an existing
    /// scheduler rather than under it (docs/SUBSTRATE.md 15). The authority on
    /// runnability stays where it already is - the Linux personality's `PState` and
    /// the native process table's - and the queue supplies the *order*. Reconciling
    /// at the pick means the two can never disagree about who is eligible to run,
    /// which is the failure a second copy of the state would produce: a queue that
    /// picks a cell the personality considers blocked resumes a cell with an
    /// unsatisfied wait, and nothing would report it.
    ///
    /// A vcore the caller now considers blocked is recorded as having relinquished
    /// **voluntarily**, because in this kernel it did: every block reached here is a
    /// cell parking at a syscall boundary. An involuntary stop is recorded by the
    /// preemption path, which says so explicitly.
    pub fn sync_runnable<F: Fn(u16, u16) -> bool>(&mut self, now_ns: u64, ready: F) {
        for i in 0..self.high_water {
            // By reference, copying out only the four fields the decision needs -
            // this runs per dispatch, and the whole-struct copy per slot was the
            // dominant cost of the reconcile (docs/SUBSTRATE.md pillar 1). The
            // borrow must end before `wake`/`block` take `&mut self`.
            let (live, runnable, cell, context) = match self.vcores.get_ref(i) {
                Some(v) => (v.live, v.runnable, v.cell, v.context),
                None => continue,
            };
            if !live {
                continue;
            }
            let want = ready(cell, context);
            let id = VcoreId(i as u32);
            if want && !runnable {
                let _ = self.wake(id, now_ns);
            } else if !want && runnable {
                let _ = self.block(id, true);
            }
        }
    }

    fn free_index(&mut self) -> Option<usize> {
        for i in 0..self.high_water {
            if !self.vcores.get_ref(i).map(|v| v.live).unwrap_or(false) {
                return Some(i);
            }
        }
        Some(self.high_water)
    }

    /// Remove a vcore (its cell exited, or its context did).
    pub fn remove(&mut self, id: VcoreId) -> bool {
        let Some(v) = self.get(id) else {
            return false;
        };
        if v.ready() {
            self.total_weight = self.total_weight.saturating_sub(v.weight());
        }
        if self.current == Some(id) {
            self.current = None;
        }
        self.set(id, Vcore::EMPTY);
        true
    }

    /// Set the slice a vcore asks for. Per EEVDF this is how it buys latency:
    /// a smaller slice yields an earlier virtual deadline.
    pub fn set_slice(&mut self, id: VcoreId, slice_ns: u64) -> Result<(), QueueError> {
        let mut v = self.get(id).ok_or(QueueError::NoSuchVcore)?;
        v.slice_ns = slice_ns.max(1);
        v.refresh_deadline();
        self.set(id, v);
        Ok(())
    }

    /// Give a vcore a hard deadline (a [`Class::Reserved`] vcore's period edge).
    pub fn set_hard_deadline(&mut self, id: VcoreId, deadline_ns: u64) -> Result<(), QueueError> {
        let mut v = self.get(id).ok_or(QueueError::NoSuchVcore)?;
        v.hard_deadline_ns = deadline_ns;
        self.set(id, v);
        Ok(())
    }

    /// Mark a vcore runnable (it was woken).
    ///
    /// A waking vcore's vruntime is pulled forward to the queue's virtual time if
    /// it had fallen behind, so a vcore that slept for a long time cannot bank
    /// unbounded eligibility and then monopolise the CPU on waking. It keeps a
    /// small credit - one slice's worth - which is what gives a just-woken
    /// interactive vcore its prompt turn without letting a long sleep become a
    /// weapon.
    pub fn wake(&mut self, id: VcoreId, now_ns: u64) -> Result<(), QueueError> {
        let mut v = self.get(id).ok_or(QueueError::NoSuchVcore)?;
        if v.runnable {
            return Ok(());
        }
        let credit = v.slice_ns.saturating_mul(WEIGHT_BASE) / v.weight().max(1);
        let floor = self.vtime.saturating_sub(credit);
        if v.vruntime < floor {
            v.vruntime = floor;
        }
        v.runnable = true;
        v.runnable_since_ns = now_ns;
        v.refresh_deadline();
        self.total_weight = self.total_weight.saturating_add(v.weight());
        self.set(id, v);
        Ok(())
    }

    /// Mark a vcore blocked (it parked on a wake source). `voluntary` says whether
    /// it gave up the CPU itself, which is what ends a BORE burst.
    pub fn block(&mut self, id: VcoreId, voluntary: bool) -> Result<(), QueueError> {
        let mut v = self.get(id).ok_or(QueueError::NoSuchVcore)?;
        if v.runnable {
            self.total_weight = self.total_weight.saturating_sub(v.weight());
        }
        v.runnable = false;
        if voluntary {
            v.burst.relinquish();
            self.voluntary_yields = self.voluntary_yields.wrapping_add(1);
        } else {
            v.burst.preempted();
        }
        v.refresh_deadline();
        if self.current == Some(id) {
            self.current = None;
        }
        self.set(id, v);
        Ok(())
    }

    /// End a vcore's burst without blocking it: it gave up the CPU **voluntarily**
    /// but stays runnable (`SYS_YIELD`, `sched_yield`).
    ///
    /// Separate from [`RunQueue::block`] because the two are genuinely different
    /// transitions and collapsing them was tempting enough to be worth naming: a
    /// yielding vcore is still competing for the CPU, so marking it blocked would
    /// remove its weight from `total_weight` and make the queue's virtual clock
    /// advance too fast for everyone else - unfairness with no visible cause.
    pub fn relinquished(&mut self, id: VcoreId) {
        let Some(mut v) = self.get(id) else { return };
        let w = v.weight().max(1);
        v.burst.relinquish();
        let new_w = v.weight().max(1);
        if new_w != w && v.runnable {
            self.total_weight = self.total_weight.saturating_sub(w).saturating_add(new_w);
        }
        v.refresh_deadline();
        if self.current == Some(id) {
            self.current = None;
        }
        self.set(id, v);
        self.voluntary_yields = self.voluntary_yields.wrapping_add(1);
    }

    /// The vcore was taken off the CPU by preemption: still runnable, and its burst
    /// did **not** end voluntarily.
    ///
    /// The distinction is the whole point of the burst score. A compute-bound vcore
    /// that is preempted has not finished its burst, so recording the stop as a
    /// voluntary yield would hand it the interactive weight an event-driven vcore
    /// earns by actually waiting - which is the one thing BORE exists to tell apart.
    pub fn was_preempted(&mut self, id: VcoreId) {
        let Some(mut v) = self.get(id) else { return };
        let w = v.weight().max(1);
        v.burst.preempted();
        let new_w = v.weight().max(1);
        if new_w != w && v.runnable {
            self.total_weight = self.total_weight.saturating_sub(w).saturating_add(new_w);
        }
        v.refresh_deadline();
        if self.current == Some(id) {
            self.current = None;
        }
        self.set(id, v);
        self.preemptions = self.preemptions.wrapping_add(1);
    }

    /// Charge `delta_ns` of CPU time to a vcore, advancing both its virtual
    /// runtime and the queue's virtual clock.
    ///
    /// Called when a vcore stops running (at a syscall boundary today, at a
    /// preemption once that exists). The two advances are what keep eligibility
    /// meaningful: the vcore's own clock moves by its weighted share, the queue's
    /// by the fair share, and the difference is its lag.
    pub fn charge(&mut self, id: VcoreId, delta_ns: u64) -> Result<(), QueueError> {
        let mut v = self.get(id).ok_or(QueueError::NoSuchVcore)?;
        let w = v.weight().max(1);
        v.service_ns = v.service_ns.saturating_add(delta_ns);
        v.burst.charge(delta_ns);
        v.vruntime = v
            .vruntime
            .saturating_add(delta_ns.saturating_mul(WEIGHT_BASE) / w);
        // The weight may have just changed (a longer burst lowers it), so the
        // cached total and the deadline both need refreshing.
        let new_w = v.weight().max(1);
        if new_w != w && v.runnable {
            self.total_weight = self.total_weight.saturating_sub(w).saturating_add(new_w);
        }
        v.refresh_deadline();
        self.set(id, v);

        let denom = self.total_weight.max(1);
        self.vtime = self
            .vtime
            .saturating_add(delta_ns.saturating_mul(WEIGHT_BASE) / denom);
        Ok(())
    }

    /// Whether a vcore is **eligible**: it has not consumed more than its fair
    /// share of virtual time.
    fn eligible(&self, v: &Vcore) -> bool {
        v.vruntime <= self.vtime
    }

    /// Choose the vcore that should run next, without dispatching it.
    ///
    /// The order, in full:
    /// 1. **Reserved** vcores by earliest hard deadline. Admission has already
    ///    promised these and refused what it could not keep, so they precede
    ///    best-effort work unconditionally.
    /// 2. **Fair** vcores that are *eligible*, by earliest virtual deadline.
    /// 3. **Fair** vcores that are ineligible, by earliest virtual deadline - only
    ///    reached when nothing is eligible, so the CPU is never idled while
    ///    runnable work exists (an eligibility gate that could idle a CPU would be
    ///    a fairness rule bought with throughput).
    /// 4. **Residual** work, by virtual deadline.
    pub fn pick(&self) -> Option<VcoreId> {
        self.scan().pick_id()
    }

    /// One walk over the queue answering **both** of dispatch's questions - who
    /// runs next, and whether the eligibility gate changed that answer.
    ///
    /// They used to be two walks ([`Self::pick`] and
    /// [`Self::eligibility_would_defer`]) with identical filters, run
    /// back-to-back on every dispatch - two questions about the same elements
    /// are one walk, not two. The per-element work here is exactly the union of
    /// the two loops', including the strict-`<` first-seen tie rule both used,
    /// so the fused answers are bit-identical to the sequential ones.
    fn scan(&self) -> Scan {
        let mut s = Scan {
            reserved: None,
            eligible: None,
            ineligible: None,
            residual: None,
            earliest_fair_ineligible: false,
        };
        let mut earliest_fair: Option<u64> = None;
        for (id, v) in self.iter() {
            if !v.ready() {
                continue;
            }
            let key = |slot: &mut Option<(u64, VcoreId)>, k: u64| {
                if slot.map(|(bk, _)| k < bk).unwrap_or(true) {
                    *slot = Some((k, id));
                }
            };
            match v.class {
                Class::Reserved => key(&mut s.reserved, v.hard_deadline_ns),
                Class::Residual => key(&mut s.residual, v.vdeadline),
                Class::Fair => {
                    let e = self.eligible(&v);
                    if e {
                        key(&mut s.eligible, v.vdeadline)
                    } else {
                        key(&mut s.ineligible, v.vdeadline)
                    }
                    if earliest_fair.map(|k| v.vdeadline < k).unwrap_or(true) {
                        earliest_fair = Some(v.vdeadline);
                        s.earliest_fair_ineligible = !e;
                    }
                }
            }
        }
        s
    }

    /// Whether the eligibility gate changed the answer: the earliest-deadline
    /// fair vcore was passed over because it was ineligible.
    ///
    /// Exists so a test can assert EEVDF is doing something rather than
    /// degenerating to earliest-deadline-first: the two agree most of the time, so
    /// "it works" is not observable from the pick alone (docs/ENGINEERING.md 1).
    pub fn eligibility_would_defer(&self) -> bool {
        self.scan().earliest_fair_ineligible
    }

    /// Dispatch the picked vcore: mark it current, count it, and report how long
    /// it waited (runnable-to-running - the queue-delay number).
    ///
    /// Returns `(id, queue_delay_ns)`, or `None` when nothing is runnable.
    pub fn dispatch(&mut self, now_ns: u64) -> Option<(VcoreId, u64)> {
        let scan = self.scan();
        if scan.earliest_fair_ineligible {
            self.eligibility_defers = self.eligibility_defers.wrapping_add(1);
        }
        let id = scan.pick_id()?;
        let mut v = self.get(id)?;
        let delay = now_ns.saturating_sub(v.runnable_since_ns);
        v.dispatches = v.dispatches.saturating_add(1);
        self.set(id, v);
        if self.current.is_some() && self.current != Some(id) {
            self.preemptions = self.preemptions.wrapping_add(1);
        }
        self.current = Some(id);
        self.dispatches = self.dispatches.wrapping_add(1);
        crate::metrics::record(crate::metrics::Metric::RunDelayNs, delay);
        Some((id, delay))
    }

    /// Whether the running vcore should be preempted in favour of another.
    ///
    /// True when some other runnable vcore would be picked ahead of it - which,
    /// given the ordering above, is exactly "a reserved deadline arrived, or an
    /// eligible vcore has an earlier virtual deadline". This is the predicate a
    /// timer-driven preemption will consult; it is computable now, which is what
    /// lets the ordering be tested before preemption exists.
    pub fn should_preempt(&self) -> bool {
        let Some(cur) = self.current else {
            return self.pick().is_some();
        };
        match self.pick() {
            Some(next) => next != cur,
            None => false,
        }
    }

    /// How long the current vcore may run before it should be reconsidered - what
    /// a preemption timer would be armed for.
    ///
    /// The smaller of its own requested slice and the time until the next vcore's
    /// virtual deadline would overtake it, floored so a pathological set cannot
    /// produce a zero-length slice and livelock the dispatcher.
    pub fn current_slice_ns(&self) -> u64 {
        const FLOOR_NS: u64 = 50_000; // 50 us
        let Some(cur) = self.current.and_then(|id| self.get(id)) else {
            return DEFAULT_SLICE_NS;
        };
        let mut budget = cur.slice_ns;
        for (_, v) in self.iter() {
            if !v.ready() || v.vdeadline >= cur.vdeadline {
                continue;
            }
            // Convert the virtual-deadline gap back into real nanoseconds at this
            // vcore's weight.
            let gap = cur.vdeadline.saturating_sub(v.vdeadline);
            let real = gap.saturating_mul(cur.weight().max(1)) / WEIGHT_BASE;
            if real < budget {
                budget = real;
            }
        }
        budget.max(FLOOR_NS)
    }

    /// Whether the queue's invariants hold: the cached total weight equals the sum
    /// of runnable weights, and every live vcore's deadline is consistent with its
    /// vruntime, slice and weight.
    ///
    /// The cached `total_weight` is exactly the kind of denormalised state that
    /// drifts silently - a missed update makes the virtual clock advance at the
    /// wrong rate, which shows up as unfairness nobody can attribute - so it is
    /// checkable rather than trusted.
    pub fn invariant_holds(&self) -> bool {
        let mut sum = 0u64;
        for (_, v) in self.iter() {
            if v.ready() {
                sum = sum.saturating_add(v.weight());
            }
            let w = v.weight().max(1);
            let expect = v
                .vruntime
                .saturating_add((v.slice_ns.saturating_mul(WEIGHT_BASE) / w).max(1));
            if v.vdeadline != expect {
                return false;
            }
            if v.weight() != expected_weight(&v) {
                return false;
            }
        }
        sum == self.total_weight
    }
}

/// The weight a vcore should have, independent of the cached value - so the
/// invariant check does not merely compare a field to itself.
fn expected_weight(v: &Vcore) -> u64 {
    match v.class {
        Class::Reserved => WEIGHT_BASE,
        Class::Fair => bore::weight_of(v.burst.score()),
        Class::Residual => 1,
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> RunQueue {
        RunQueue::new()
    }

    /// A vcore asking for a smaller slice must get an earlier deadline - the
    /// property that makes latency purchasable without a priority.
    #[test]
    fn smaller_slice_earns_earlier_deadline() {
        let mut rq = q();
        let a = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        let b = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        rq.set_slice(a, 100_000).unwrap();
        rq.set_slice(b, 10_000_000).unwrap();
        assert!(rq.get(a).unwrap().vdeadline() < rq.get(b).unwrap().vdeadline());
        assert_eq!(rq.pick(), Some(a));
    }

    /// A reserved vcore precedes fair work regardless of virtual deadlines.
    #[test]
    fn reservations_precede_fair_work() {
        let mut rq = q();
        let fair = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        rq.set_slice(fair, 1).unwrap(); // as early a virtual deadline as possible
        let res = rq.admit(2, 0, Class::Reserved, Burst::new(), 0).unwrap();
        rq.set_hard_deadline(res, 5_000_000).unwrap();
        assert_eq!(rq.pick(), Some(res));
    }

    /// Residual work runs only when nothing else is runnable, and is never lost.
    #[test]
    fn residual_runs_on_slack_only() {
        let mut rq = q();
        let idle = rq.admit(1, 0, Class::Residual, Burst::new(), 0).unwrap();
        let fair = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        assert_eq!(rq.pick(), Some(fair));
        rq.block(fair, true).unwrap();
        assert_eq!(rq.pick(), Some(idle), "residual work must run on slack");
    }

    /// The eligibility gate must actually defer a vcore that has over-consumed,
    /// even when it holds the earliest deadline - otherwise this is EDF wearing
    /// EEVDF's name.
    #[test]
    fn eligibility_defers_an_over_consumer() {
        let mut rq = q();
        let hog = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        let other = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        rq.set_slice(hog, 1_000).unwrap(); // tiny slice = earliest deadline
        rq.set_slice(other, 1_000_000).unwrap();
        assert_eq!(rq.pick(), Some(hog));
        // Let the hog run a long time; its vruntime races ahead of vtime.
        rq.charge(hog, 50_000_000).unwrap();
        assert!(
            rq.eligibility_would_defer(),
            "an over-consuming vcore should become ineligible"
        );
        assert_eq!(rq.pick(), Some(other), "the fair share must move on");
    }

    /// A long sleep must not bank unbounded eligibility.
    #[test]
    fn waking_does_not_bank_unbounded_credit() {
        let mut rq = q();
        let sleeper = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        let busy = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        rq.block(sleeper, true).unwrap();
        for _ in 0..100 {
            rq.charge(busy, 1_000_000).unwrap();
        }
        rq.wake(sleeper, 0).unwrap();
        let v = rq.get(sleeper).unwrap();
        assert!(
            v.vruntime() + 1 >= rq.vtime().saturating_sub(v.slice_ns() * WEIGHT_BASE),
            "a sleeper must rejoin near the queue's virtual time"
        );
    }

    /// The cached total weight must track runnability exactly.
    #[test]
    fn total_weight_tracks_runnable_set() {
        let mut rq = q();
        assert!(rq.invariant_holds());
        let a = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        let b = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        assert!(rq.invariant_holds());
        rq.block(a, true).unwrap();
        assert!(rq.invariant_holds());
        rq.wake(a, 0).unwrap();
        assert!(rq.invariant_holds());
        rq.remove(b);
        assert!(rq.invariant_holds());
        rq.charge(a, 40_000_000).unwrap(); // changes the burst weight
        assert!(
            rq.invariant_holds(),
            "a weight change must update the total"
        );
    }

    /// A compute-bound vcore must lose weight relative to an interactive one, and
    /// so be served later - the BORE contribution, end to end.
    #[test]
    fn bursty_work_outranks_compute_bound_work() {
        let mut rq = q();
        let hog = rq.admit(1, 0, Class::Fair, Burst::new(), 0).unwrap();
        let ui = rq.admit(2, 0, Class::Fair, Burst::new(), 0).unwrap();
        // The hog runs a long burst without yielding; the interactive vcore runs
        // briefly and yields each time.
        rq.charge(hog, 500_000_000).unwrap();
        for _ in 0..5 {
            rq.charge(ui, 200_000).unwrap();
            rq.block(ui, true).unwrap();
            rq.wake(ui, 0).unwrap();
        }
        let hw = rq.get(hog).unwrap().weight();
        let uw = rq.get(ui).unwrap().weight();
        assert!(
            uw > hw,
            "interactive weight {uw} should exceed compute-bound weight {hw}"
        );
    }
}
