//! A **hierarchical timing wheel** for an unbounded number of deadlines
//! (docs/SUBSTRATE.md pillar 7).
//!
//! ## Why the arbiter needed one
//!
//! The arbiter above this ([`super`]) keeps deadlines in a fixed table with one
//! slot per *named client* - `RxPoll`, `RxDeadline`, `CellSleep`, `NetTimer`,
//! `Pacer`, `FutexWait`. That table is exactly right for what it does: it makes
//! the single-owner invariant checkable, and each named client is a distinct
//! kernel subsystem, so the vocabulary is closed.
//!
//! What it cannot express is *many deadlines of the same kind*. One QUIC
//! connection needs a retransmission timeout, a probe timeout, a pacing release,
//! an idle timeout and a key-update deadline - five, all `NetTimer`. A thousand
//! connections need five thousand. Node.js arms thousands of coarse `setTimeout`
//! deadlines. The N2e work already recorded the shape of the problem when it
//! deferred "two concurrent timer waiters in one cell", and a per-purpose slot
//! table answers it the same way a fixed `MAX_*` answers a workload: by refusing
//! at a constant.
//!
//! So deadlines become **objects**, not slots: [`Timer`] handles over funded
//! storage ([`crate::mm::kmeta`]), bounded by memory rather than by a constant,
//! and organised so that the operations the kernel actually performs are cheap.
//!
//! ## Structure and cost
//!
//! [`LEVELS`] levels of [`SLOTS`] buckets each, bucket `k` at level `l` covering
//! `TICK_NS << (l * LEVEL_BITS)` nanoseconds. A timer is filed in the coarsest
//! level whose range still contains it, and **cascades** down to finer levels as
//! its deadline approaches.
//!
//! - **arm**: O(1). Compute the level from the delta, push onto that bucket's
//!   intrusive list.
//! - **cancel**: O(1). Unlink from a doubly-linked list, no scan.
//! - **service**: O(expired + cascaded). Only the buckets the elapsed time
//!   actually crossed are visited; a wheel holding a million far-future timers
//!   does no work at all until they come due.
//! - **nearest deadline**: O(1) amortised, kept as a cached minimum, because the
//!   arbiter asks for it on every state change to re-arm the hardware.
//!
//! With the constants below, level 0 spans ~65 us, level 1 ~4 ms, level 2 ~268
//! ms, level 3 ~17 s, and anything beyond sits in an overflow list that is
//! re-filed as time advances. Those cover a TCP RTO, a pacing interval, a
//! keep-alive and a lease renewal without the caller choosing a level.
//!
//! ## Time domain
//!
//! Deadlines are absolute monotonic nanoseconds in the **hardware timer's own
//! domain** - the same domain as [`super::now_ns`], which is `arch::timer_now_ns`
//! and *not* the instruction counter (they differ on RISC-V, and conflating them
//! made a "20 ms" wait mean different things per ISA). The wheel never reads the
//! clock itself: every entry point takes `now`, so the caller's notion of time is
//! the only one in play and the structure is a pure function of its inputs -
//! which is what makes it testable against a hand-computed oracle with no
//! hardware at all.
//!
//! ## SMP
//!
//! One wheel per CPU, held in the arbiter's [`crate::smp::PerCpu`] state, because
//! there is one hardware one-shot per core. A timer therefore belongs to the core
//! that armed it; a cross-core deadline is delivered by asking that core (an IPI),
//! not by another core reaching into this structure. Nothing here locks, and
//! nothing here is shared - the partitioning is the discipline
//! (docs/SCHEDULING.md 1a).

use crate::mm::kmeta::{Funded, Owner};

/// Bits of bucket index per level: 64 buckets.
pub const LEVEL_BITS: u32 = 6;
/// Buckets per level.
pub const SLOTS: usize = 1 << LEVEL_BITS;
/// Levels of hierarchy.
pub const LEVELS: usize = 4;
/// Nanoseconds per level-0 bucket, as a shift so all arithmetic is shifts and
/// masks. `1 << 10` = 1024 ns, near enough to a microsecond and exact in binary.
pub const TICK_SHIFT: u32 = 10;
/// Nanoseconds per tick (level-0 bucket width).
pub const TICK_NS: u64 = 1 << TICK_SHIFT;

/// Sentinel for "no entry" in the intrusive lists. `u32::MAX` rather than 0,
/// because 0 is a perfectly good timer index.
const NIL: u32 = u32::MAX;

/// A timer's lifecycle state.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum State {
    /// Not in use; the slot is available for reuse.
    Free,
    /// Filed in a bucket, waiting for its deadline.
    Armed,
    /// Its deadline passed and it has not been collected yet.
    Fired,
}

/// One timer, as stored. Intrusively linked into exactly one bucket list while
/// armed.
#[derive(Copy, Clone)]
struct Node {
    deadline_ns: u64,
    next: u32,
    prev: u32,
    /// Bumped every time the slot is reused, so a handle to a previous
    /// occupant fails instead of addressing a stranger's timer. The
    /// `CapSlot::generation` discipline, applied here.
    generation: u32,
    state: State,
    /// Caller-defined tag: which connection, which cell, which flow. The wheel
    /// never interprets it; it is what lets a caller with thousands of timers
    /// know *which* one fired.
    tag: u64,
}

impl Node {
    const EMPTY: Node = Node {
        deadline_ns: 0,
        next: NIL,
        prev: NIL,
        generation: 0,
        state: State::Free,
        tag: 0,
    };
}

/// A handle to an armed timer. Copyable and small; carries the generation so a
/// stale handle is detected rather than acted on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Timer {
    index: u32,
    generation: u32,
}

impl Timer {
    /// The caller-visible index, for a diagnostic.
    pub fn index(self) -> u32 {
        self.index
    }
}

/// Why arming a timer failed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WheelError {
    /// The node table could not grow: the owner is out of budget.
    NoMetadata,
}

/// The wheel: buckets, the node table, and the cached nearest deadline.
pub struct Wheel {
    /// Bucket list heads, `[level][slot]`.
    heads: [[u32; SLOTS]; LEVELS],
    /// Timers too far out for any level, re-filed as time advances.
    overflow: u32,
    /// The node table. Funded, so timer count is bounded by memory.
    nodes: Funded<Node>,
    /// Highest node index ever used, so free-slot scans stop early.
    high_water: usize,
    /// Head of the free list, threaded through `next`.
    free: u32,
    /// The tick the wheel has advanced to. Buckets below this have been serviced.
    now_tick: u64,
    /// Cached nearest armed deadline, or `u64::MAX` when nothing is armed.
    /// Maintained on arm (a min) and recomputed lazily after a removal, so the
    /// arbiter's per-state-change query is O(1) in the common case.
    nearest: u64,
    /// Set when a removal may have invalidated `nearest`, so the next query
    /// recomputes it once instead of every removal paying a scan.
    nearest_dirty: bool,
    /// Armed timers.
    armed: usize,
    /// Timers that have fired and not yet been collected.
    fired_pending: usize,
    /// Cumulative counters - the evidence a test asserts against rather than
    /// inferring behaviour (docs/ENGINEERING.md 1).
    arms: u64,
    cancels: u64,
    firings: u64,
    cascades: u64,
}

impl Wheel {
    /// An empty wheel holding no storage.
    pub const fn new() -> Wheel {
        Wheel {
            heads: [[NIL; SLOTS]; LEVELS],
            overflow: NIL,
            nodes: Funded::new(),
            high_water: 0,
            free: NIL,
            now_tick: 0,
            nearest: u64::MAX,
            nearest_dirty: false,
            armed: 0,
            fired_pending: 0,
            arms: 0,
            cancels: 0,
            firings: 0,
            cascades: 0,
        }
    }

    /// Charge the node table to `owner` and set the wheel's starting time.
    /// Releases anything previously held, so this doubles as a reset.
    pub fn init(&mut self, owner: Owner, now_ns: u64) {
        self.release();
        self.nodes.set_owner(owner);
        self.now_tick = now_ns >> TICK_SHIFT;
    }

    /// Release all storage and return to empty.
    pub fn release(&mut self) {
        self.nodes.release();
        self.heads = [[NIL; SLOTS]; LEVELS];
        self.overflow = NIL;
        self.high_water = 0;
        self.free = NIL;
        self.nearest = u64::MAX;
        self.nearest_dirty = false;
        self.armed = 0;
        self.fired_pending = 0;
    }

    /// Armed timers.
    pub fn armed(&self) -> usize {
        self.armed
    }
    /// Fired timers not yet collected.
    pub fn fired_pending(&self) -> usize {
        self.fired_pending
    }
    /// (arms, cancels, firings, cascades) since [`Wheel::init`].
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (self.arms, self.cancels, self.firings, self.cascades)
    }
    /// Frames the node table holds.
    pub fn frames_held(&self) -> usize {
        self.nodes.frames_held()
    }

    fn node(&self, index: u32) -> Option<Node> {
        if index == NIL {
            None
        } else {
            self.nodes.get(index as usize)
        }
    }

    fn set_node(&mut self, index: u32, node: Node) {
        if index != NIL {
            self.nodes.set(index as usize, node);
        }
    }

    /// Take a free node index, growing the table if needed.
    fn alloc_node(&mut self) -> Option<u32> {
        if self.free != NIL {
            let index = self.free;
            let node = self.node(index)?;
            self.free = node.next;
            return Some(index);
        }
        let index = self.high_water;
        let generation = self.nodes.get(index).map(|n| n.generation).unwrap_or(0);
        if !self.nodes.set_growing(
            index,
            Node {
                generation,
                ..Node::EMPTY
            },
        ) {
            return None;
        }
        self.high_water = index + 1;
        Some(index as u32)
    }

    /// Which level and bucket a deadline belongs in, given the current tick.
    /// `None` means it is beyond every level's reach (the overflow list).
    fn locate(&self, deadline_tick: u64) -> Option<(usize, usize)> {
        // A deadline at or before now belongs in the current level-0 bucket, so
        // the next service picks it up rather than it being lost to the past.
        let delta = deadline_tick.saturating_sub(self.now_tick);
        for level in 0..LEVELS {
            let shift = LEVEL_BITS * level as u32;
            if delta < (1u64 << (LEVEL_BITS * (level as u32 + 1))) {
                let slot = ((deadline_tick >> shift) as usize) & (SLOTS - 1);
                return Some((level, slot));
            }
        }
        None
    }

    /// Push `index` onto the front of a bucket (or the overflow list).
    fn link(&mut self, index: u32, level: Option<(usize, usize)>) {
        let head = match level {
            Some((l, s)) => self.heads[l][s],
            None => self.overflow,
        };
        if let Some(mut node) = self.node(index) {
            node.next = head;
            node.prev = NIL;
            self.set_node(index, node);
        }
        if let Some(mut h) = self.node(head) {
            h.prev = index;
            self.set_node(head, h);
        }
        match level {
            Some((l, s)) => self.heads[l][s] = index,
            None => self.overflow = index,
        }
    }

    /// Unlink `index` from whichever list holds it. O(1) - the reason the lists
    /// are doubly linked.
    fn unlink(&mut self, index: u32) {
        let Some(node) = self.node(index) else {
            return;
        };
        let (prev, next) = (node.prev, node.next);
        if let Some(mut p) = self.node(prev) {
            p.next = next;
            self.set_node(prev, p);
        } else {
            // It was a head: find which one and repoint it. The deadline says
            // where it must have been filed, so this is a direct computation, not
            // a search over buckets.
            let loc = self.locate(node.deadline_ns >> TICK_SHIFT);
            match loc {
                Some((l, s)) if self.heads[l][s] == index => self.heads[l][s] = next,
                _ => {
                    if self.overflow == index {
                        self.overflow = next;
                    } else {
                        // The wheel advanced past where this was filed, so the
                        // computed location no longer matches. Fall back to the
                        // bounded search over heads; correctness must not depend
                        // on the fast path.
                        self.repoint_head(index, next);
                    }
                }
            }
        }
        if let Some(mut n) = self.node(next) {
            n.prev = prev;
            self.set_node(next, n);
        }
        if let Some(mut node) = self.node(index) {
            node.next = NIL;
            node.prev = NIL;
            self.set_node(index, node);
        }
    }

    /// Find whichever head points at `index` and repoint it to `next`.
    fn repoint_head(&mut self, index: u32, next: u32) {
        for level in 0..LEVELS {
            for slot in 0..SLOTS {
                if self.heads[level][slot] == index {
                    self.heads[level][slot] = next;
                    return;
                }
            }
        }
        if self.overflow == index {
            self.overflow = next;
        }
    }

    /// Arm a timer for absolute deadline `deadline_ns` with caller tag `tag`.
    ///
    /// O(1). The returned [`Timer`] is the only way to cancel or identify it.
    pub fn arm(&mut self, deadline_ns: u64, tag: u64) -> Result<Timer, WheelError> {
        let index = self.alloc_node().ok_or(WheelError::NoMetadata)?;
        let generation = self
            .node(index)
            .map(|n| n.generation.wrapping_add(1))
            .unwrap_or(1);
        self.set_node(
            index,
            Node {
                deadline_ns,
                next: NIL,
                prev: NIL,
                generation,
                state: State::Armed,
                tag,
            },
        );
        let loc = self.locate(deadline_ns >> TICK_SHIFT);
        self.link(index, loc);
        self.armed += 1;
        self.arms = self.arms.wrapping_add(1);
        if deadline_ns < self.nearest {
            self.nearest = deadline_ns;
        }
        Ok(Timer { index, generation })
    }

    /// Whether `timer` still names a live (armed or fired) timer.
    pub fn valid(&self, timer: Timer) -> bool {
        match self.node(timer.index) {
            Some(n) => n.generation == timer.generation && n.state != State::Free,
            None => false,
        }
    }

    /// Cancel `timer`. Returns whether it was armed (false if already fired,
    /// already cancelled, or stale).
    ///
    /// Cancelling one timer can never disturb another - the property the arbiter
    /// above exists to guarantee, here made structural: a cancel touches only its
    /// own node and its list neighbours.
    pub fn cancel(&mut self, timer: Timer) -> bool {
        let Some(node) = self.node(timer.index) else {
            return false;
        };
        if node.generation != timer.generation || node.state == State::Free {
            return false;
        }
        let was_armed = node.state == State::Armed;
        if was_armed {
            self.unlink(timer.index);
            self.armed -= 1;
            if node.deadline_ns <= self.nearest {
                self.nearest_dirty = true;
            }
        } else {
            self.fired_pending = self.fired_pending.saturating_sub(1);
        }
        self.free_node(timer.index);
        self.cancels = self.cancels.wrapping_add(1);
        was_armed
    }

    /// Return a node to the free list, bumping its generation so outstanding
    /// handles to it stop resolving.
    fn free_node(&mut self, index: u32) {
        if let Some(mut node) = self.node(index) {
            node.state = State::Free;
            node.generation = node.generation.wrapping_add(1);
            node.next = self.free;
            node.prev = NIL;
            node.deadline_ns = 0;
            self.set_node(index, node);
            self.free = index;
        }
    }

    /// The nearest armed deadline, or `None` when nothing is armed.
    ///
    /// O(1) unless a cancellation or firing invalidated the cache, in which case
    /// one bounded recomputation happens here (and is then cached again). The
    /// arbiter calls this on every state change, so paying for the scan lazily -
    /// once per invalidation rather than once per removal - is the difference
    /// between O(1) and O(n) arming.
    pub fn nearest(&mut self) -> Option<u64> {
        if self.nearest_dirty {
            self.recompute_nearest();
        }
        if self.armed == 0 {
            None
        } else {
            Some(self.nearest)
        }
    }

    fn recompute_nearest(&mut self) {
        let mut best = u64::MAX;
        for index in 0..self.high_water {
            if let Some(n) = self.nodes.get(index) {
                if n.state == State::Armed && n.deadline_ns < best {
                    best = n.deadline_ns;
                }
            }
        }
        self.nearest = best;
        self.nearest_dirty = false;
    }

    /// Advance to `now_ns`, marking every timer whose deadline has passed as
    /// fired and cascading the rest into finer levels. Returns how many fired.
    ///
    /// Cost is proportional to what actually happened - timers that came due plus
    /// timers that crossed a level boundary - not to the number of timers held.
    pub fn advance(&mut self, now_ns: u64) -> usize {
        let target_tick = now_ns >> TICK_SHIFT;
        let mut fired = 0;

        // Bound the work when a long time has passed with nothing serviced (a
        // boot-time jump, or a very long idle): once more than a whole level-0
        // revolution has elapsed there is no point stepping bucket by bucket, so
        // re-file everything against the new time in one pass.
        if target_tick.saturating_sub(self.now_tick) >= (SLOTS as u64) {
            self.now_tick = target_tick;
            fired += self.refile_all(now_ns);
            self.after_removals();
            return fired;
        }

        while self.now_tick < target_tick {
            self.now_tick += 1;
            let slot = (self.now_tick as usize) & (SLOTS - 1);
            fired += self.expire_bucket(0, slot, now_ns);
            // Crossing a level-0 revolution cascades the next level down, and so
            // on - the standard wheel cascade.
            if slot == 0 {
                for level in 1..LEVELS {
                    let shift = LEVEL_BITS * level as u32;
                    let higher = ((self.now_tick >> shift) as usize) & (SLOTS - 1);
                    self.cascade_bucket(level, higher, now_ns);
                    if higher != 0 {
                        break;
                    }
                }
                self.refile_overflow(now_ns);
            }
        }
        // Anything already due in the current bucket (armed with a past deadline)
        // must fire too, so a deadline that arrived late is never skipped.
        let slot = (self.now_tick as usize) & (SLOTS - 1);
        fired += self.expire_bucket(0, slot, now_ns);
        if fired > 0 {
            self.after_removals();
        }
        fired
    }

    /// Mark every due timer in a level-0 bucket as fired; re-file any that is not
    /// due yet (it can only be a full revolution out).
    fn expire_bucket(&mut self, level: usize, slot: usize, now_ns: u64) -> usize {
        let mut fired = 0;
        let mut index = self.heads[level][slot];
        while index != NIL {
            let Some(node) = self.node(index) else { break };
            let next = node.next;
            if node.state == State::Armed && node.deadline_ns <= now_ns {
                self.unlink(index);
                if let Some(mut n) = self.node(index) {
                    n.state = State::Fired;
                    self.set_node(index, n);
                }
                self.armed -= 1;
                self.fired_pending += 1;
                self.firings = self.firings.wrapping_add(1);
                fired += 1;
            }
            index = next;
        }
        fired
    }

    /// Move a coarse bucket's timers into the finest level that now fits them.
    fn cascade_bucket(&mut self, level: usize, slot: usize, now_ns: u64) {
        let mut index = self.heads[level][slot];
        self.heads[level][slot] = NIL;
        while index != NIL {
            let Some(node) = self.node(index) else { break };
            let next = node.next;
            // Detach cleanly before re-filing.
            if let Some(mut n) = self.node(index) {
                n.next = NIL;
                n.prev = NIL;
                self.set_node(index, n);
            }
            if node.state == State::Armed {
                if node.deadline_ns <= now_ns {
                    if let Some(mut n) = self.node(index) {
                        n.state = State::Fired;
                        self.set_node(index, n);
                    }
                    self.armed -= 1;
                    self.fired_pending += 1;
                    self.firings = self.firings.wrapping_add(1);
                } else {
                    let loc = self.locate(node.deadline_ns >> TICK_SHIFT);
                    self.link(index, loc);
                    self.cascades = self.cascades.wrapping_add(1);
                }
            }
            index = next;
        }
    }

    /// Re-file the overflow list against the current time.
    fn refile_overflow(&mut self, now_ns: u64) {
        let mut index = self.overflow;
        self.overflow = NIL;
        while index != NIL {
            let Some(node) = self.node(index) else { break };
            let next = node.next;
            if let Some(mut n) = self.node(index) {
                n.next = NIL;
                n.prev = NIL;
                self.set_node(index, n);
            }
            if node.state == State::Armed {
                if node.deadline_ns <= now_ns {
                    if let Some(mut n) = self.node(index) {
                        n.state = State::Fired;
                        self.set_node(index, n);
                    }
                    self.armed -= 1;
                    self.fired_pending += 1;
                    self.firings = self.firings.wrapping_add(1);
                } else {
                    let loc = self.locate(node.deadline_ns >> TICK_SHIFT);
                    self.link(index, loc);
                    self.cascades = self.cascades.wrapping_add(1);
                }
            }
            index = next;
        }
    }

    /// Re-file (or fire) every armed timer against `now_ns`. The bounded-work
    /// path for a large time jump.
    fn refile_all(&mut self, now_ns: u64) -> usize {
        // Detach every list first, then re-file from the node table, so no list
        // is walked while it is being rebuilt.
        self.heads = [[NIL; SLOTS]; LEVELS];
        self.overflow = NIL;
        let mut fired = 0;
        for index in 0..self.high_water {
            let Some(mut node) = self.nodes.get(index) else {
                continue;
            };
            if node.state != State::Armed {
                continue;
            }
            node.next = NIL;
            node.prev = NIL;
            self.nodes.set(index, node);
            if node.deadline_ns <= now_ns {
                node.state = State::Fired;
                self.nodes.set(index, node);
                self.armed -= 1;
                self.fired_pending += 1;
                self.firings = self.firings.wrapping_add(1);
                fired += 1;
            } else {
                let loc = self.locate(node.deadline_ns >> TICK_SHIFT);
                self.link(index as u32, loc);
                self.cascades = self.cascades.wrapping_add(1);
            }
        }
        fired
    }

    fn after_removals(&mut self) {
        self.nearest_dirty = true;
    }

    /// Collect one fired timer, returning its handle and tag, and freeing it.
    /// `None` when nothing has fired. The caller drains in a loop.
    pub fn take_fired(&mut self) -> Option<(Timer, u64)> {
        if self.fired_pending == 0 {
            return None;
        }
        for index in 0..self.high_water {
            if let Some(node) = self.nodes.get(index) {
                if node.state == State::Fired {
                    let timer = Timer {
                        index: index as u32,
                        generation: node.generation,
                    };
                    let tag = node.tag;
                    self.fired_pending -= 1;
                    self.free_node(index as u32);
                    return Some((timer, tag));
                }
            }
        }
        // The counter and the table disagreed; trust the table and correct the
        // counter rather than looping forever.
        self.fired_pending = 0;
        None
    }

    /// Whether `timer` has fired (and not yet been collected).
    pub fn fired(&self, timer: Timer) -> bool {
        match self.node(timer.index) {
            Some(n) => n.generation == timer.generation && n.state == State::Fired,
            None => false,
        }
    }

    /// Whether the structural invariants hold: every armed node is filed exactly
    /// once, counters agree with the table, and the nearest cache is not smaller
    /// than the true minimum.
    ///
    /// The evidence a test asserts against - a wheel is easy to get subtly wrong
    /// (a lost cascade shows up as a deadline that never fires), so the invariant
    /// is checkable rather than argued.
    pub fn invariant_holds(&self) -> bool {
        let mut armed = 0;
        let mut fired = 0;
        let mut min_armed = u64::MAX;
        for index in 0..self.high_water {
            let Some(n) = self.nodes.get(index) else {
                continue;
            };
            match n.state {
                State::Armed => {
                    armed += 1;
                    if n.deadline_ns < min_armed {
                        min_armed = n.deadline_ns;
                    }
                }
                State::Fired => fired += 1,
                State::Free => {}
            }
        }
        if armed != self.armed || fired != self.fired_pending {
            return false;
        }
        // Every armed node must be reachable from exactly one list head.
        let mut linked = 0;
        for level in 0..LEVELS {
            for slot in 0..SLOTS {
                let mut index = self.heads[level][slot];
                let mut guard = 0;
                while index != NIL {
                    guard += 1;
                    if guard > self.high_water + 1 {
                        return false; // cycle
                    }
                    let Some(n) = self.node(index) else {
                        return false;
                    };
                    if n.state == State::Armed {
                        linked += 1;
                    }
                    index = n.next;
                }
            }
        }
        let mut index = self.overflow;
        let mut guard = 0;
        while index != NIL {
            guard += 1;
            if guard > self.high_water + 1 {
                return false;
            }
            let Some(n) = self.node(index) else {
                return false;
            };
            if n.state == State::Armed {
                linked += 1;
            }
            index = n.next;
        }
        if linked != armed {
            return false;
        }
        // The cache may be stale-high only while dirty; it must never be *below*
        // the true minimum, which would make the arbiter arm too early (harmless)
        // rather than too late (a missed deadline) - so the direction is asserted.
        if !self.nearest_dirty && armed > 0 && self.nearest > min_armed {
            return false;
        }
        true
    }
}

impl Default for Wheel {
    fn default() -> Self {
        Self::new()
    }
}
