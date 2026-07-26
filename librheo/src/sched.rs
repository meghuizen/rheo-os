//! Reservations & the `lattice-rt` time-certain surface (docs/LIBRHEO.md Phase
//! C, docs/ARCHITECTURE.md 3 object 7, docs/SCHEDULING.md, docs/REALTIME.md).
//!
//! A [`Reservation`] asks the kernel for a CPU budget/period/deadline (plus an
//! advisory memory floor); the kernel runs the EDF schedulability test and
//! either admits it - returning a capability-backed handle - or refuses it with
//! a typed [`ReserveError`]. Admission is real and enforced at admit time; the
//! run-queue *enforcement* (a reserved cell actually getting its budget) lands
//! with SMP/preemption (task #27) and is documented as such - today the runtime
//! is single-CPU cooperative, so a reservation is an admitted guarantee, not yet
//! a scheduled one.
//!
//! On top of the reservation sit the small `lattice-rt`-shaped types
//! ([`Priority`], [`PeriodicTask`], [`TimingReport`]) a time-certain program
//! builds against.

use crate::sys;

/// Why a reservation was refused (mirrors the kernel's admit rejection codes).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ReserveError {
    /// Deadline longer than period, or budget larger than period.
    BadParams,
    /// Admitting would push total CPU utilization over 100% (EDF test failed).
    Overcommit,
    /// The requested memory floor exceeds what the frame pool can currently back.
    MemoryFloor,
    /// The kernel returned an unexpected code (should not happen).
    Unknown,
}

/// An admitted CPU/memory reservation (object 7). RAII: dropping it releases the
/// admitted utilization back to the cell's admission controller.
pub struct Reservation {
    handle: u32,
    committed_ppm: u64,
    budget: u64,
    period: u64,
    deadline: u64,
}

impl Reservation {
    /// Request a reservation: `budget` ticks of CPU every `period` ticks with a
    /// relative `deadline` (<= period), plus a memory floor of `mem_floor_pages`
    /// 4 KiB pages (advisory in QEMU). `Ok` if the EDF admission test passes.
    pub fn request(
        budget: u64,
        period: u64,
        deadline: u64,
        mem_floor_pages: u64,
    ) -> Result<Reservation, ReserveError> {
        let mut info = sys::ReserveInfo {
            handle: 0,
            committed_ppm: 0,
        };
        let out = &mut info as *mut sys::ReserveInfo as u64;
        match sys::reserve_admit(out, budget, period, deadline, mem_floor_pages) {
            0 => Ok(Reservation {
                handle: info.handle as u32,
                committed_ppm: info.committed_ppm,
                budget,
                period,
                deadline,
            }),
            1 => Err(ReserveError::BadParams),
            2 => Err(ReserveError::Overcommit),
            3 => Err(ReserveError::MemoryFloor),
            _ => Err(ReserveError::Unknown),
        }
    }

    /// The cell's total committed CPU utilization after this admission
    /// (parts-per-million), as reported by the kernel at admit time.
    pub fn committed_ppm(&self) -> u64 {
        self.committed_ppm
    }
    /// This reservation's utilization, `budget/period` in parts-per-million.
    pub fn utilization_ppm(&self) -> u64 {
        self.budget.saturating_mul(1_000_000) / self.period.max(1)
    }
    pub fn budget(&self) -> u64 {
        self.budget
    }
    pub fn period(&self) -> u64 {
        self.period
    }
    pub fn deadline(&self) -> u64 {
        self.deadline
    }
    /// The 32-bit capability id backing this reservation.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// The cell's committed CPU utilization right now (a live query, not the
    /// value cached at admit time).
    pub fn query_committed_ppm() -> u64 {
        sys::reserve_query()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // Return the admitted utilization to the cell's admission controller.
        sys::reserve_release(self.handle);
    }
}

/// A scheduling priority band (the `lattice-rt` surface, docs/REALTIME.md).
/// Advisory today (single-CPU cooperative); it rides with the reservation for
/// when preemptive scheduling lands (task #27).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Priority {
    Idle,
    Low,
    #[default]
    Normal,
    High,
    Realtime,
}

/// A periodic real-time task builder (docs/REALTIME.md). Set the timing
/// parameters, then [`build`](PeriodicTask::build) runs admission and returns
/// the admitted [`Reservation`] or a typed rejection.
pub struct PeriodicTask {
    period: u64,
    budget: u64,
    deadline: u64,
    mem_floor_pages: u64,
    priority: Priority,
}

impl PeriodicTask {
    /// Start a task with a `period` (ticks); budget defaults to the whole period
    /// and deadline to the period until narrowed.
    pub fn new(period: u64) -> PeriodicTask {
        PeriodicTask {
            period,
            budget: period,
            deadline: period,
            mem_floor_pages: 0,
            priority: Priority::Normal,
        }
    }
    /// The CPU budget consumed each period (ticks).
    pub fn budget(mut self, budget: u64) -> PeriodicTask {
        self.budget = budget;
        self
    }
    /// The relative deadline within the period (ticks, <= period).
    pub fn deadline(mut self, deadline: u64) -> PeriodicTask {
        self.deadline = deadline;
        self
    }
    /// A memory floor in 4 KiB pages (advisory).
    pub fn memory_floor_pages(mut self, pages: u64) -> PeriodicTask {
        self.mem_floor_pages = pages;
        self
    }
    /// The scheduling priority band.
    pub fn priority(mut self, priority: Priority) -> PeriodicTask {
        self.priority = priority;
        self
    }
    /// The task's priority band.
    pub fn get_priority(&self) -> Priority {
        self.priority
    }

    /// Run admission for this task, returning its reservation or a typed
    /// rejection. This is where the EDF schedulability math actually decides.
    pub fn build(self) -> Result<Reservation, ReserveError> {
        Reservation::request(
            self.budget,
            self.period,
            self.deadline,
            self.mem_floor_pages,
        )
    }
}

/// A snapshot of the cell's timing/QoS state (docs/REALTIME.md `TimingReport`):
/// the committed CPU utilization the admission controller is tracking.
#[derive(Copy, Clone, Debug)]
pub struct TimingReport {
    pub committed_ppm: u64,
}

impl TimingReport {
    /// Read the current committed utilization from the kernel.
    pub fn read() -> TimingReport {
        TimingReport {
            committed_ppm: sys::reserve_query(),
        }
    }
    /// Headroom left before the CPU is fully committed (parts-per-million).
    pub fn headroom_ppm(&self) -> u64 {
        1_000_000u64.saturating_sub(self.committed_ppm)
    }
}
