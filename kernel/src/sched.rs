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
