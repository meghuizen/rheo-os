//! `net::timer` - a **timer wheel** that multiplexes many logical timers onto the
//! reactor's **single** one-shot deadline (docs/NETSTACK.md §11, docs/LIBRHEO.md
//! Phase F). TCP needs several concurrent timers (per-connection RTO, delayed-ACK,
//! TIME-WAIT, keepalive), but `librheo::rt` exposes exactly one `timer_req` slot
//! (`rt::sleep_ns` over `SYS_ARM_TIMER`). This wheel is the userspace layer that
//! bridges the two: it holds an ordered set of `(deadline_ns, id)` entries, and a
//! driver arms the reactor for the **nearest** deadline only; when that fires,
//! [`expire`](TimerWheel::expire) pops every entry now due and re-arms for the new
//! nearest. No kernel or reactor ABI change - it is pure bookkeeping over the one
//! slot.
//!
//! ## Design (documented per the plan)
//! This is the **simple sorted-set** variant, not a hashed/hierarchical wheel: a
//! `BTreeSet<(deadline, id)>` for the ordered nearest-deadline lookup plus a
//! `BTreeMap<id, deadline>` for cancel/re-arm by id. Every operation is
//! `O(log n)`; for the connection counts N2a targets that is ample, and it is
//! obviously correct (the property that matters for the RTO math). A hashed
//! timing wheel (`O(1)` amortized) is the N2b optimization once the sharded
//! transport drives thousands of connections - documented, not built here.
//!
//! ## Two clocks, honestly
//! The wheel is driven by an explicit `now_ns`. In production the driver reads it
//! from `librheo::time::now()` and sleeps to the nearest deadline via
//! [`arm_nearest`](TimerWheel::arm_nearest) (`rt::sleep_ns`). In the deterministic
//! proof the test **advances `now` by hand** to fire the RTO without waiting on
//! wall-clock time - the same "deterministic core, thin live driver" split the
//! traceroute/DNS proofs use.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// A handle to a timer registered in a [`TimerWheel`]. Stable across re-arms;
/// pass it to [`cancel`](TimerWheel::cancel) / [`rearm`](TimerWheel::rearm).
pub type TimerId = u64;

/// A set of pending timers multiplexed onto one reactor deadline. See the module
/// docs for the design.
///
/// ## The logical clock (and why the wheel owns it)
/// The reactor's one-shot (`SYS_ARM_TIMER` behind `rt::sleep_ns`) is **relative** -
/// it fires after a *duration*, and the cell has no userspace ticks->ns reading to
/// turn an absolute deadline back into a duration. So the wheel keeps its own
/// monotonic `now_ns`: deadlines are stored absolute (in this clock's frame), and
/// [`run_once`](Self::run_once) sleeps the *delta* `(nearest - now)` then advances
/// `now` to it. The deterministic proof drives `now` by hand ([`set_now`]) instead
/// of sleeping, so RTO firing needs no wall-clock wait.
#[derive(Default)]
pub struct TimerWheel {
    /// Ordered by `(deadline_ns, id)` so the first element is always the nearest.
    by_deadline: BTreeSet<(u64, TimerId)>,
    /// `id -> deadline_ns`, so an entry can be cancelled/re-armed by id.
    by_id: BTreeMap<TimerId, u64>,
    /// Monotonic id source.
    next_id: TimerId,
    /// The wheel's logical monotonic clock (nanoseconds).
    now_ns: u64,
}

impl TimerWheel {
    /// An empty wheel with its logical clock at 0.
    pub fn new() -> TimerWheel {
        TimerWheel {
            by_deadline: BTreeSet::new(),
            by_id: BTreeMap::new(),
            next_id: 1,
            now_ns: 0,
        }
    }

    /// The wheel's current logical time (nanoseconds).
    pub fn now(&self) -> u64 {
        self.now_ns
    }

    /// Set the logical clock (the deterministic driver advances it by hand to fire
    /// a timer without a wall-clock wait). Never moves it backwards.
    pub fn set_now(&mut self, now_ns: u64) {
        self.now_ns = self.now_ns.max(now_ns);
    }

    /// Register a timer to fire at `deadline_ns`, returning its [`TimerId`].
    pub fn insert(&mut self, deadline_ns: u64) -> TimerId {
        let id = self.next_id;
        self.next_id += 1;
        self.by_deadline.insert((deadline_ns, id));
        self.by_id.insert(id, deadline_ns);
        id
    }

    /// Move an existing timer to a new `deadline_ns` (e.g. restarting an RTO). If
    /// `id` is unknown this is a no-op returning `false`.
    pub fn rearm(&mut self, id: TimerId, deadline_ns: u64) -> bool {
        let Some(old) = self.by_id.get(&id).copied() else {
            return false;
        };
        self.by_deadline.remove(&(old, id));
        self.by_deadline.insert((deadline_ns, id));
        self.by_id.insert(id, deadline_ns);
        true
    }

    /// Cancel a timer. Unknown ids are ignored.
    pub fn cancel(&mut self, id: TimerId) {
        if let Some(old) = self.by_id.remove(&id) {
            self.by_deadline.remove(&(old, id));
        }
    }

    /// Drop every pending timer (keeps the id counter monotonic).
    pub fn clear(&mut self) {
        self.by_deadline.clear();
        self.by_id.clear();
    }

    /// The nearest (earliest) pending deadline, or `None` if the wheel is empty.
    /// The one deadline a driver arms on the reactor.
    pub fn nearest(&self) -> Option<u64> {
        self.by_deadline.iter().next().map(|&(d, _)| d)
    }

    /// True if no timers are pending.
    pub fn is_empty(&self) -> bool {
        self.by_deadline.is_empty()
    }

    /// The number of pending timers.
    pub fn len(&self) -> usize {
        self.by_deadline.len()
    }

    /// Pop and return every timer whose deadline is `<= now_ns`, **in deadline
    /// order**, appending their ids to `fired`. After this the wheel holds only
    /// future timers, so [`nearest`](Self::nearest) gives the next deadline to arm.
    pub fn expire(&mut self, now_ns: u64, fired: &mut Vec<TimerId>) {
        while let Some(&(deadline, id)) = self.by_deadline.iter().next() {
            if deadline > now_ns {
                break;
            }
            self.by_deadline.remove(&(deadline, id));
            self.by_id.remove(&id);
            fired.push(id);
        }
    }

    /// The production driver step: park on the reactor's single one-shot for the
    /// **delta** to the nearest deadline (`rt::sleep_ns`), advance the logical
    /// clock to it, and return the ids that fired - **in deadline order**. Returns
    /// an empty `Vec` (without sleeping) if the wheel is empty. This is where many
    /// logical timers multiplex onto the one reactor slot: only the nearest is ever
    /// armed, and firing it re-arms for the new nearest on the next call.
    pub async fn run_once(&mut self) -> Vec<TimerId> {
        let Some(deadline) = self.nearest() else {
            return Vec::new();
        };
        if deadline > self.now_ns {
            librheo::rt::sleep_ns(deadline - self.now_ns).await;
        }
        self.now_ns = self.now_ns.max(deadline);
        let mut fired = Vec::new();
        self.expire(self.now_ns, &mut fired);
        fired
    }
}
