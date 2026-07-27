//! Reservations (docs/ARCHITECTURE.md 3 object 7, docs/SCHEDULING.md 4):
//! admission-checked CPU guarantees. A reservation is (budget, period,
//! deadline); the admission controller runs the EDF schedulability test
//! and refuses a set it cannot guarantee - the same math that refuses an
//! over-committed real-time system rather than accepting a lie.
//!
//! Single-host, single-core scope: EDF on one core is schedulable iff the
//! total utilization U = sum(budget_i / period_i) <= 1. Utilization is
//! tracked in fixed-point parts-per-million to stay integer-only (no FPU
//! in the kernel). Pools (latency/shared/system) and the run queue itself
//! are later work (BUILD-ORDER.md step 9).

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
