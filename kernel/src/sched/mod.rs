//! Reservations (docs/ARCHITECTURE.md 3 object 7, docs/SCHEDULING.md 4):
//! admission-checked CPU guarantees. A reservation is (budget, period,
//! deadline); the admission controller runs the EDF schedulability test
//! and refuses a set it cannot guarantee - the same math that refuses an
//! over-committed real-time system rather than accepting a lie.
//!
//! Single-host, single-core scope: EDF on one core is schedulable iff the
//! total utilization U = sum(budget_i / period_i) <= 1. Utilization is
//! tracked in fixed-point parts-per-million to stay integer-only (no FPU
//! in the kernel). Pools (latency/shared/system) are later work
//! (BUILD-ORDER.md step 9).
//!
//! The **ready order** the admitted reservations are dispatched in lives in
//! [`vcore`], which unifies them with best-effort work in one deadline-ordered
//! queue (EEVDF virtual deadlines beside these hard ones), weighted by the BORE
//! burst score in [`bore`] - docs/SCHEDULING.md 11.3-11.4, docs/SUBSTRATE.md
//! pillar 3. Admission (this module) decides *whether* a guarantee can be kept;
//! the run queue decides *who runs next*. Keeping the two apart is what lets the
//! admission math stay unchanged while the dispatch order is re-founded.

pub mod bore;
pub mod dispatch;
pub mod preempt;
pub mod vcore;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AdmitError {
    /// Admitting this reservation would push utilization over 100%.
    Overcommit,
    /// Deadline longer than period is not modelled here.
    BadParams,
}

/// One admission-checked CPU reservation.
#[derive(Copy, Clone, Debug)]
pub struct Reservation {
    /// Runtime budget per period (ticks).
    pub budget: u64,
    /// Period (ticks).
    pub period: u64,
    /// Relative deadline (ticks); must be <= period here.
    pub deadline: u64,
    /// Utilization contributed, in parts per million.
    util_ppm: u64,
}

impl Reservation {
    /// The all-zero reservation, for fixed-capacity reservation tables
    /// (docs/LIBRHEO.md Phase C).
    pub const ZERO: Reservation = Reservation {
        budget: 0,
        period: 0,
        deadline: 0,
        util_ppm: 0,
    };

    /// Utilization this reservation contributes, in parts per million.
    pub fn util_ppm(&self) -> u64 {
        self.util_ppm
    }
}

/// The admission controller: tracks committed utilization and gates new
/// reservations. One per core.
pub struct Admission {
    committed_ppm: u64,
}

const FULL_PPM: u64 = 1_000_000;

impl Admission {
    pub const fn new() -> Admission {
        Admission { committed_ppm: 0 }
    }

    /// Current committed utilization (parts per million).
    pub fn committed_ppm(&self) -> u64 {
        self.committed_ppm
    }

    /// Try to admit a reservation. On success the utilization is committed
    /// and a Reservation handle is returned; on failure nothing changes.
    pub fn admit(
        &mut self,
        budget: u64,
        period: u64,
        deadline: u64,
    ) -> Result<Reservation, AdmitError> {
        if period == 0 || deadline == 0 || deadline > period || budget > period {
            return Err(AdmitError::BadParams);
        }
        let util_ppm = budget.saturating_mul(FULL_PPM) / period;
        if self.committed_ppm + util_ppm > FULL_PPM {
            return Err(AdmitError::Overcommit);
        }
        self.committed_ppm += util_ppm;
        Ok(Reservation {
            budget,
            period,
            deadline,
            util_ppm,
        })
    }

    /// Release a previously admitted reservation, freeing its utilization.
    pub fn release(&mut self, r: &Reservation) {
        self.committed_ppm = self.committed_ppm.saturating_sub(r.util_ppm);
    }
}

impl Default for Admission {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------- the system-wide ledger

/// The **system-wide** admission ledger (docs/ARCHITECTURE-DEBT.md 2.5,
/// docs/SCHEDULING.md 4).
///
/// The defect this closes: admission was tested **only** against the *calling
/// cell's* controller, so sixteen cells each admitting 90% all succeeded - 1440% of
/// one CPU admitted, nothing refused. Doctrine 5 ("accepted by math or rejected
/// loudly") was scoped so that it could not reject the only over-commit that
/// matters. Aggravating it, a **second** global controller existed for the same
/// object, behind the legacy `SYS_RESERVE` verb.
///
/// So there is now exactly one ledger for the machine, and it is this. A
/// reservation must fit **both** its cell's controller and this one; the per-cell
/// controller stays because it is what makes a *cell's* own set schedulable
/// (docs/SCHEDULING.md 4) and what `SYS_RESERVE_QUERY` reports.
///
/// Single CPU today (task #27). Under SMP a reservation is admitted against a
/// *core*, so this becomes one ledger per core plus a placement decision; the shape
/// - admit against the resource, not against the requester - is what matters here.
static mut SYSTEM: Admission = Admission::new();

/// The machine-wide admission ledger. Every reservation is charged here as well as
/// to its own cell.
pub fn system() -> &'static mut Admission {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut *core::ptr::addr_of_mut!(SYSTEM) }
}

/// Clear the system-wide ledger (called from `user::reset`, between runs).
pub fn reset_system() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *core::ptr::addr_of_mut!(SYSTEM) = Admission::new();
    }
}

// ------------------------------------------------------- the per-CPU run queue

/// Per-CPU ready queues (docs/SUBSTRATE.md pillar 3).
///
/// One queue per core, never a global one: the multikernel model partitions cores
/// rather than balancing across shared scheduler state, and a cross-LLC balancer
/// is explicitly not taken (docs/SCHEDULING.md 1a, 11.5). Not `Copy` - a queue
/// owns funded storage - so this uses [`crate::smp::PerCpu::from_array`].
static QUEUES: crate::smp::PerCpu<vcore::RunQueue> =
    crate::smp::PerCpu::from_array([const { vcore::RunQueue::new() }; crate::smp::MAX_CPUS]);

/// This CPU's ready queue.
///
/// # Safety
/// The returned reference must not outlive the caller's critical section and no
/// second reference may be taken while it lives. A core touches only its own
/// queue, so there is no cross-core obligation.
#[inline]
#[allow(clippy::mut_from_ref)]
pub unsafe fn run_queue() -> &'static mut vcore::RunQueue {
    // SAFETY: this CPU's own slot; the intra-CPU obligation is the caller's.
    unsafe { QUEUES.this_mut() }
}

/// CPU `cpu`'s ready queue, for a cross-core placement decision or a test oracle.
///
/// # Safety
/// The caller must know CPU `cpu` is not concurrently mutating its queue (it is
/// parked, or not yet online).
#[inline]
#[allow(clippy::mut_from_ref)]
pub unsafe fn run_queue_of(cpu: usize) -> &'static mut vcore::RunQueue {
    // SAFETY: delegated to the caller per the contract above.
    unsafe { QUEUES.get_mut(cpu) }
}

/// Initialise this CPU's ready queue, charging its storage to the kernel.
pub fn init_run_queue() {
    // SAFETY: a short setup call on this CPU's own queue.
    unsafe {
        run_queue().init(crate::mm::kmeta::Owner::KERNEL);
    }
}

/// Release this CPU's ready queue (between runs).
pub fn reset_run_queue() {
    // SAFETY: a short teardown call on this CPU's own queue.
    unsafe {
        run_queue().release();
    }
}
