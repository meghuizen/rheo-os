//! **Lock contention accounting** for named [`crate::smp::SpinLock`]s
//! (docs/OBSERVABILITY.md 11, phase S5).
//!
//! # What is measured, and what deliberately is not
//!
//! An *uncontended* acquire is never measured: `SpinLock::lock`'s fast path is
//! one `compare_exchange` whatever this module does, and the only cost a named
//! lock adds there is one `id != 0` test plus one mask test. Everything below
//! runs on the `#[cold]` contended path - which has, by definition, already
//! spent longer spinning than any bookkeeping costs - and only when the
//! [`crate::obs::Window::Lock`] window is on.
//!
//! Hold time is behind its **own** modifier bit (`W_LOCK_HOLD`), separate from
//! the wait, because measuring hold time reads the clock inside the critical
//! section and lengthens the very region it measures. A `W_LOCK`-only run gives
//! contention with no perturbation of the held region, and the two runs can be
//! diffed - which is what makes the hold-time number trustworthy when it is on.
//!
//! # Why atomics here when the counter plane exists
//!
//! The S4 counter plane is per-CPU: one writer per slot. Contention is the one
//! quantity that is per-**lock**, not per-CPU - "which lock is hot" is the
//! question, and folding it into per-CPU slots would answer "which CPU waited"
//! instead. A relaxed `fetch_add` per contention is the right cost on a path
//! that just spun: the RMW is noise against the wait it records.
//!
//! # The recording rule this module must obey
//!
//! Everything recorded from lock paths goes through
//! [`crate::metrics::record_noalloc`], never the allocating `record`: the frames
//! pool lock is itself named, so a record that could allocate would re-enter the
//! pool lock under its own guard (a self-deadlock), and even after release would
//! re-enter `metrics` through the inner guard's drop while the outer `&mut`
//! histogram is live. Bucket storage comes from [`crate::metrics::prefund`], a
//! bring-up act.

use core::sync::atomic::{AtomicU64, Ordering};

/// Which named lock a measurement is about. A **closed, kernel-defined set**,
/// like [`crate::ktimer::TimerClient`]: a reader names these without reading
/// kernel source, and an unnamed lock (`SpinLock::new`) is id 0 and never
/// measured.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum LockId {
    /// Unnamed - the default; never measured.
    None = 0,
    /// `mm::frames`' pool lock - the hottest lock in the tree (every frame
    /// allocation and free).
    FramePool = 1,
    /// `mm::frames_pmem`'s pool lock.
    PmemPool = 2,
    /// The console output lock.
    Console = 3,
    /// `sched`'s machine-wide admission ledger.
    SchedSystem = 4,
    /// `rng::entropy`'s pool lock.
    EntropyPool = 5,
    /// The NVMe driver's queue lock (never contended by design - the queues are
    /// partitioned per core - so a nonzero count here is a finding).
    Nvme = 6,
    /// A proof kernel's probe lock: lets a test contend on a lock that is not
    /// load-bearing, so its numbers are attributable to the test alone.
    Probe = 7,
}

/// One entry per [`LockId`].
pub const LOCKS: usize = 8;

/// Contended acquisitions per lock (the fast-path CAS found the lock held).
static CONTENTIONS: [AtomicU64; LOCKS] = [const { AtomicU64::new(0) }; LOCKS];
/// Spin iterations per lock, summed over its contended acquisitions.
static SPINS: [AtomicU64; LOCKS] = [const { AtomicU64::new(0) }; LOCKS];

/// Record one contended acquisition: `spins` iterations, `wait_ns` from first
/// failed CAS to acquisition. Called by `SpinLock`'s `#[cold]` contended path,
/// gated there on the window, so this never runs while recording is off.
pub(crate) fn contended(id: u8, spins: u64, wait_ns: u64) {
    let i = (id as usize).min(LOCKS - 1);
    CONTENTIONS[i].fetch_add(1, Ordering::Relaxed);
    SPINS[i].fetch_add(spins, Ordering::Relaxed);
    crate::metrics::record_noalloc(crate::metrics::Metric::LockWaitNs, wait_ns);
    // The Lock window's event: `a` names the lock, `b` carries the wait, so the
    // event stream shows *which* lock was hot and when - the question a total
    // cannot answer.
    crate::obs_event!(
        crate::obs::Window::Lock,
        crate::obs::Kind::Note,
        crate::obs::OWNER_KERNEL,
        id as u64,
        wait_ns
    );
}

/// Record one hold interval. Called from the guard's drop **after** the release
/// store, so the measured region never includes the recording.
pub(crate) fn held(_id: u8, hold_ns: u64) {
    crate::metrics::record_noalloc(crate::metrics::Metric::LockHoldNs, hold_ns);
}

/// Contended acquisitions of `id`.
pub fn contentions(id: LockId) -> u64 {
    CONTENTIONS[id as usize].load(Ordering::Relaxed)
}

/// Spin iterations spent waiting for `id`, summed over its contentions.
pub fn lock_spins(id: LockId) -> u64 {
    SPINS[id as usize].load(Ordering::Relaxed)
}

/// Clear the per-lock counters (between runs / between proof rounds).
pub fn reset() {
    for i in 0..LOCKS {
        CONTENTIONS[i].store(0, Ordering::Relaxed);
        SPINS[i].store(0, Ordering::Relaxed);
    }
}
