//! SMP: per-CPU state, a kernel spinlock, and secondary-core bring-up
//! (docs/SMP.md, task #27; docs/SUBSTRATE.md pillar 3).
//!
//! This module is **portable** - no `cfg(target_arch)` here. The per-ISA bits
//! (the secondary entry trampoline, the SBI/PSCI bring-up call, and the per-CPU
//! identity register) live in `kernel/src/arch/`. What is portable: the
//! `SpinLock<T>` mutual-exclusion primitive, the generic [`PerCpu<T>`] container,
//! a fixed per-CPU registry indexed by CPU index, and the driver that asks the
//! arch layer to start one secondary and waits (bounded) for it to run kernel
//! code.
//!
//! ## Why the primitives are compiled unconditionally
//!
//! The bring-up half of this module (everything that calls `arch::smp_*`) stays
//! behind the `smp` cargo feature, because those arch entry points do. The
//! **primitives** - [`SpinLock`], [`PerCpu`], [`cpu_index`] - are always
//! compiled, and that is a deliberate change of shape (docs/SUBSTRATE.md
//! pillar 3).
//!
//! The reason is that per-CPU-ness is a property of a *data structure*, not of a
//! build configuration. The subsystems being re-founded on top of this - the
//! timer arbiter (one hardware one-shot per core), the metrics histograms, the
//! vcore run queues - are correct only if each core owns its own instance. If
//! the container that expresses that were feature-gated, every one of them would
//! have to be written twice: once as a global `static mut` for the default build
//! and once as per-CPU state for the SMP build. Two implementations of the same
//! subsystem is precisely how the FP/SIMD `SYS_YIELD` defect happened (two
//! switch paths, one of them forgotten - docs/LIBRHEO.md), and SMP.md 10.2's
//! audit is a list of statics whose ownership discipline must be *stated in one
//! place*. So the discipline is written once, and the feature decides only
//! whether a second CPU exists to exercise it.
//!
//! **The single-CPU path is unchanged, by construction rather than by
//! configuration.** Without the `smp` feature [`cpu_index`] is a `const 0`, so
//! every `PerCpu<T>` resolves to slot 0 with no indexing at run time, and every
//! `SpinLock` is an uncontended flag (one atomic exchange with no possible
//! contender). The property that matters going forward is therefore stronger and
//! more useful than the byte-identity the pre-pillar-3 kernel maintained:
//! *enabling the feature must not change single-CPU behaviour*. The earlier
//! property - that adding the module leaves the non-SMP library byte-identical -
//! is superseded here and is recorded as such in docs/SMP.md rather than being
//! quietly dropped.
//!
//! Honest per-ISA status (docs/SMP.md): a real secondary core now runs kernel
//! code on all three ISAs (SBI HSM on RISC-V, INIT-SIPI-SIPI on x86-64, PSCI
//! `CPU_ON` over the probed conduit on ARM64). What is *not* yet done is
//! scheduling work onto it - that is SMP.md 10 (phase 2), whose first
//! deliverable is the safety audit, not a scheduler.

// `arch` is reached only from the feature-gated bring-up half and from
// `cpu_index`'s SMP arm, so the import itself is gated: without the feature this
// module names nothing per-ISA at all.
#[cfg(feature = "smp")]
use crate::arch;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Registry capacity: matches the machine inventory's CPU ceiling.
pub const MAX_CPUS: usize = crate::hw::MAX_CPUS;

/// Index of the CPU this call runs on. **Always a valid registry index.**
///
/// The **one** place the rest of the kernel asks "which core am I?", so that
/// per-CPU state has a single addressing rule. Without the `smp` feature there
/// is exactly one CPU and this is a compile-time `0`, which is what makes every
/// [`PerCpu<T>`] lookup free and every [`SpinLock`] uncontended on the default
/// build.
///
/// ## Totality is a requirement, not a convenience
///
/// The returned value indexes every per-CPU structure in the kernel, so an
/// out-of-range answer is not a wrong number - it is a wild memory access. This
/// function is therefore **total**: whatever the arch layer reports is bounded
/// here, and anything out of range resolves to CPU 0.
///
/// That guard is load-bearing rather than defensive padding, because the arch
/// implementations differ in how they answer *before* bring-up has run. x86-64
/// and ARM64 search a table of hardware ids and fall back to 0, so they are
/// already safe early. RISC-V reads `tp` directly, which holds whatever the
/// previous boot stage left there until `smp_set_this_cpu` runs - and the
/// subsystems re-founded on per-CPU state (the timer arbiter, the metrics
/// histograms, the DRBG roots, the run queue) all touch it during boot, long
/// before `smp::init`. That combination panicked the `smp` kernel with an index
/// of 2147790848. `tp` is now zeroed in the RISC-V boot stub so the value is
/// genuinely 0 rather than garbage that happens to be masked, and this bound
/// stays as the structural backstop: a future port that forgets to initialise
/// its identity register gets CPU 0, not corruption.
#[inline(always)]
pub fn cpu_index() -> usize {
    #[cfg(feature = "smp")]
    {
        let raw = arch::cpu_index();
        if raw < MAX_CPUS { raw } else { 0 }
    }
    #[cfg(not(feature = "smp"))]
    {
        0
    }
}

// ------------------------------------------------------------- per-CPU container

/// One instance of `T` per CPU, addressed by [`cpu_index`].
///
/// The container that lets a subsystem be per-core without being written twice.
/// A core touches only its own slot, so no lock is needed for the common case -
/// which is the entire point: partitioning replaces synchronisation
/// (docs/SCHEDULING.md 1a, the multikernel model). Cross-CPU reads exist for
/// *aggregation* (a test summing every core's counters, a metrics reader) and
/// are explicitly separate ([`PerCpu::iter`]), so a cross-core access is always
/// visible at the call site rather than implied.
///
/// The slot array is stored inside a single [`UnsafeCell`] because a core
/// mutates its own slot through a shared reference. The safety obligation is the
/// partitioning itself and is stated at each accessor.
pub struct PerCpu<T> {
    slots: UnsafeCell<[T; MAX_CPUS]>,
}

// SAFETY: each CPU accesses only its own slot (see the accessors), so the
// container is shareable across cores when `T` can move between them. This is a
// partitioning argument, not a locking one - `PerCpu` provides no mutual
// exclusion and does not pretend to.
unsafe impl<T: Send> Sync for PerCpu<T> {}
unsafe impl<T: Send> Send for PerCpu<T> {}

impl<T: Copy> PerCpu<T> {
    /// A container whose every slot starts as `value`. `const`, so per-CPU state
    /// can be a plain `static` with no initialisation phase to forget.
    pub const fn new(value: T) -> PerCpu<T> {
        PerCpu {
            slots: UnsafeCell::new([value; MAX_CPUS]),
        }
    }
}

impl<T> PerCpu<T> {
    /// A container from a caller-built array - the constructor for state that is
    /// **not** `Copy`.
    ///
    /// Per-CPU state that owns a resource (the timer wheel owns funded frames)
    /// must not be `Copy`: duplicating such a value would duplicate the claim on
    /// its frames and end in a double free. Those types offer a `const fn new()`
    /// instead, and the caller writes
    /// `PerCpu::from_array([const { Wheel::new() }; MAX_CPUS])` - the const block
    /// is monomorphic at the use site, which is what makes an array of a non-`Copy`
    /// type const-constructible at all.
    pub const fn from_array(slots: [T; MAX_CPUS]) -> PerCpu<T> {
        PerCpu {
            slots: UnsafeCell::new(slots),
        }
    }
}

impl<T> PerCpu<T> {
    /// This CPU's slot, immutably.
    ///
    /// Safe because of the partitioning: the slot is selected by [`cpu_index`],
    /// so no other core addresses it, and a core cannot be inside two kernel
    /// entries at once.
    #[inline(always)]
    pub fn this(&self) -> &T {
        // SAFETY: indexed by this CPU's own index, so no other core aliases it.
        unsafe { &(*self.slots.get())[cpu_index()] }
    }

    /// This CPU's slot, mutably.
    ///
    /// # Safety
    /// The caller must not hold another reference to this CPU's slot (including
    /// one from [`PerCpu::this`]) while the returned reference lives. Cross-CPU
    /// aliasing is impossible by the indexing; the obligation is the *intra*-CPU
    /// one, which is why this is `unsafe` and [`PerCpu::this`] is not.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn this_mut(&self) -> &mut T {
        // SAFETY: delegated to the caller per the contract above.
        unsafe { &mut (*self.slots.get())[cpu_index()] }
    }

    /// CPU `i`'s slot, for **aggregation across cores** (a metrics reader, a test
    /// oracle). Deliberately separate from [`PerCpu::this`] so every cross-core
    /// read is visible in the source.
    ///
    /// # Safety
    /// The caller must accept a torn read: CPU `i` may be mutating its slot
    /// concurrently. Aggregators here read counters, where a stale value is a
    /// stale count and not a memory-safety problem; anything needing a
    /// consistent snapshot must arrange it (a lock, or asking core `i`).
    #[inline]
    pub unsafe fn get(&self, i: usize) -> &T {
        // SAFETY: index clamped into the array; tearing is the caller's contract.
        unsafe { &(*self.slots.get())[i.min(MAX_CPUS - 1)] }
    }

    /// CPU `i`'s slot, mutably - for a core initialising or servicing another
    /// core's state during bring-up, before that core is running.
    ///
    /// # Safety
    /// The caller must know CPU `i` is not concurrently touching its slot (it is
    /// not yet online, or is parked). Nothing here checks that.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self, i: usize) -> &mut T {
        // SAFETY: delegated to the caller per the contract above.
        unsafe { &mut (*self.slots.get())[i.min(MAX_CPUS - 1)] }
    }

    /// Every CPU's slot, for aggregation.
    ///
    /// # Safety
    /// As [`PerCpu::get`].
    pub unsafe fn iter(&self) -> impl Iterator<Item = &T> {
        // SAFETY: delegated per `get`.
        (0..MAX_CPUS).map(move |i| unsafe { self.get(i) })
    }
}

// ------------------------------------------------------------------ spinlock

/// A test-and-set (TTAS) spinlock for the SMP kernel. Acquire loops on a plain
/// relaxed read (spinning on the cache line, not on the atomic RMW) and only
/// attempts the `compare_exchange` when the lock looks free; the `Acquire`
/// success / `Release` unlock pair gives the standard critical-section ordering.
/// The guard releases on drop. Intended for short kernel critical sections that
/// cross cores (a longer wait belongs on the runtime's async `Mutex`,
/// docs/CONCURRENCY.md 6).
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `data`, so it is safe to share
// across cores as long as `T` can move between cores.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> SpinLock<T> {
        SpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free. Returns a guard that
    /// releases the lock when dropped.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Spin on a relaxed read so contenders share the cache line
            // read-only until the holder releases (test-and-test-and-set).
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

/// RAII guard for a held [`SpinLock`]; releases on drop.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding the guard means we hold the lock, so we are the sole
        // accessor of `data`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive access is guaranteed while the guard is held.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

// ------------------------------------------------------------- per-CPU state

/// The CPU **registry** entry: identity and liveness for one core - the
/// hardware CPU id (hart id / MPIDR affinity / APIC id) and an online flag.
///
/// Deliberately only identity. The growable per-core state a real SMP kernel
/// needs (run queue, current cell, timer arbiter, metrics) does **not** accrete
/// here as more fields: each such subsystem owns its own [`PerCpu<T>`] beside
/// its own code, so ownership is stated where the data lives rather than in one
/// shared block every subsystem reaches into. This entry answers "does core `i`
/// exist and what is it", nothing more.
pub struct CpuState {
    hw_id: AtomicU32,
    online: AtomicBool,
}

impl CpuState {
    const fn new() -> CpuState {
        CpuState {
            hw_id: AtomicU32::new(0),
            online: AtomicBool::new(false),
        }
    }

    /// Whether this CPU has been marked online (has run kernel code).
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    /// The hardware CPU id recorded for this slot.
    pub fn hw_id(&self) -> u32 {
        self.hw_id.load(Ordering::Relaxed)
    }
}

static CPUS: [CpuState; MAX_CPUS] = [const { CpuState::new() }; MAX_CPUS];

/// The per-CPU block for CPU index `i`.
pub fn cpu(i: usize) -> &'static CpuState {
    &CPUS[i]
}

/// The per-CPU block for the CPU this call runs on. Defaults to CPU 0 on the
/// single-CPU path ([`cpu_index`] is a compile-time 0 without the `smp`
/// feature, and returns 0 under it until a secondary establishes its own
/// identity), so callers that never opt into SMP always see CPU 0.
pub fn this_cpu() -> &'static CpuState {
    &CPUS[cpu_index()]
}

/// Mark CPU index `i` online in the registry with its hardware id. Called once
/// per CPU, from the CPU itself, as it comes up.
pub fn set_online(i: usize, hw_id: u32) {
    CPUS[i].hw_id.store(hw_id, Ordering::Relaxed);
    CPUS[i].online.store(true, Ordering::Release);
}

/// Number of CPUs currently marked online.
pub fn online_count() -> usize {
    CPUS.iter().filter(|c| c.is_online()).count()
}

// -------------------------------------------------- secondary bring-up proof
//
// Everything below drives `arch::smp_*` (the secondary trampoline, the SBI/PSCI
// start call, the per-CPU identity register), which exists only under the `smp`
// feature - so each item here stays gated while the primitives above do not. See
// the module header for why that split is where it is.

#[cfg(feature = "smp")]
/// A shared counter guarded by the [`SpinLock`], written by a secondary core and
/// read back by the primary - the observable cross-core proof that the lock and
/// shared memory work between cores.
static SHARED: SpinLock<u64> = SpinLock::new(0);

#[cfg(feature = "smp")]
/// Bumped (release) by a secondary once it has done its work, so the primary can
/// wait on it without holding the lock (no hold-and-wait deadlock under QEMU's
/// round-robin TCG).
static SECONDARY_UP: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// The value a secondary adds to the shared counter, so the primary can assert
/// the exact result (a fixed magic, not 1, to catch a stuck/garbage write).
pub const SECONDARY_MARK: u64 = 0x5EC0;

#[cfg(feature = "smp")]
/// The next free registry index to hand a secondary. The boot CPU is index 0
/// ([`init`]); secondaries take 1, 2, ... as they come up. This keeps the
/// registry index independent of the hardware CPU id (the boot hart id may be
/// nonzero - QEMU's RISC-V boot hart is often not hart 0).
static NEXT_INDEX: AtomicUsize = AtomicUsize::new(1);

#[cfg(feature = "smp")]
/// Portable entry the arch secondary trampoline calls once it is running kernel
/// code on the shared address space. It claims a registry index, establishes its
/// per-CPU identity, records itself online, exercises the spinlock on shared
/// memory, then signals the primary. Runs on the **secondary** core.
pub fn secondary_run(hw_id: u32) {
    let idx = NEXT_INDEX.fetch_add(1, Ordering::AcqRel);
    // Establish this CPU's identity so this_cpu() resolves to its own block.
    arch::smp_set_this_cpu(idx);
    set_online(idx, hw_id);
    // Genuine cross-core critical section: take the shared lock and write.
    {
        let mut g = SHARED.lock();
        *g += SECONDARY_MARK;
    }
    // this_cpu() must resolve to *this* secondary's block now that its identity
    // is set - a check that per-CPU addressing works off the boot core.
    debug_assert!(this_cpu().is_online());
    SECONDARY_UP.fetch_add(1, Ordering::Release);
    // Then wait for real work and do it. Before this, a secondary proved it could
    // execute and parked - which demonstrates bring-up and nothing about whether the
    // two cores can compute at the same time (docs/SMP.md 10).
    secondary_work_loop();
}

#[cfg(feature = "smp")]
/// The secondary's work loop: meet the primary at the rendezvous, compute its share
/// of the published GEMM, signal done, park.
///
/// Bounded rather than infinite. A secondary that spun forever waiting for work would
/// be correct in a running system and wrong in a boot test, where the primary finishes
/// and exits QEMU - and worse, it would hold a core against the host for the rest of
/// the run, which under TCG slows the primary down. One job then park is what this
/// stage needs; a real dispatch loop belongs with per-CPU scheduling, not here.
fn secondary_work_loop() {
    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    loop {
        // Copied out under the lock; the compute below must not hold it, because the
        // primary is computing its own rows concurrently and would block on it.
        let job = *JOB.lock();
        if let Some(job) = job {
            if rendezvous(&RV_SECONDARY, &RV_PRIMARY) {
                // SAFETY: the primary's `run_gemm_with_secondary` contract - the
                // buffers are valid for the exchange, and each block this claims is
                // its alone.
                unsafe { drain_blocks(&job) };
            }
            JOBS_DONE.fetch_add(1, Ordering::Release);
            return;
        }
        if arch::timer_now_ns() >= deadline {
            return; // no work was published; park rather than spin the core forever
        }
        core::hint::spin_loop();
    }
}

#[cfg(feature = "smp")]
/// Read the shared counter (under the lock) - the primary's view of the
/// secondary's write.
pub fn shared_value() -> u64 {
    *SHARED.lock()
}

// ------------------------------------------------- parallel work on two cores
//
// Proof-of-life on a second core says the core executes; it says nothing about
// whether the two cores can do *useful work at the same time*. That needs three
// things this section provides: a way to hand a secondary a job, a way to prove the
// two ran **simultaneously** rather than one after the other, and a workload whose
// result is checkable against a single-core oracle.
//
// The workload is the tile framework's own `gemm_i8_i32` - integer, so the answer is
// bit-exact and the two cores' halves can be compared against a reference computed by
// one core, and shared verbatim with the librheo executor, the kernel engine, the
// benches and the host comparison (docs/TILES.md). Splitting a GEMM by output rows is
// how it is actually parallelised: each core writes a disjoint row range of C and
// reads all of A and B, so there is no write sharing at all and the only
// synchronisation needed is the barrier at the end.

#[cfg(feature = "smp")]
/// A row-range slice of an int8 GEMM, published by the primary for a secondary.
///
/// Raw addresses rather than slices because it crosses cores through a static and
/// must be `Copy` with no lifetime; the primary owns the buffers for the whole
/// exchange, which is the contract [`submit_gemm`] documents.
#[derive(Copy, Clone)]
pub struct GemmJob {
    /// Kernel VAs of A (m x k, i8), B (k x n, i8), C (m x n, i32).
    pub a: usize,
    pub b: usize,
    pub c: usize,
    /// Row strides in elements.
    pub as_: usize,
    pub bs: usize,
    pub cs: usize,
    /// Output rows this core owns: `[lo, hi)`.
    pub lo: usize,
    pub hi: usize,
    pub n: usize,
    pub k: usize,
}

#[cfg(feature = "smp")]
static JOB: SpinLock<Option<GemmJob>> = SpinLock::new(None);
#[cfg(feature = "smp")]
/// Bumped by a secondary when it has finished draining.
static JOBS_DONE: AtomicUsize = AtomicUsize::new(0);

// ------------------------------------------------------------- the work queue
//
// A published job is split into fixed row **blocks**, and both cores claim blocks
// from a shared cursor until it is exhausted. That is the difference between "the
// secondary was handed one thing to do" and "the secondary is a core that takes work":
// with a static half-and-half split the faster core finishes early and idles, and
// nothing about the division adapts to what the cores actually do. Claiming makes the
// split a *result* rather than an assumption - which is also what makes the per-core
// counts below evidence of load sharing rather than of arithmetic.
//
// The cursor is a single `fetch_add`, so claiming is wait-free and needs no lock: a
// core that loses a race simply gets the next block. Blocks are disjoint row ranges of
// C, so once claimed there is no sharing at all and the compute needs no
// synchronisation - the reason a GEMM parallelises by output rows in the first place.

#[cfg(feature = "smp")]
/// Rows per claimable block. Small enough that many blocks exist for two cores to
/// interleave over, large enough that the claim is negligible next to the work.
pub const GEMM_BLOCK_ROWS: usize = 4;

#[cfg(feature = "smp")]
/// Next unclaimed block index.
static NEXT_BLOCK: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Blocks completed by each core, indexed by CPU. The load-sharing witness: a run
/// where one entry is zero did not share anything.
static BLOCKS_DONE: PerCpu<AtomicUsize> =
    PerCpu::from_array([const { AtomicUsize::new(0) }; MAX_CPUS]);

#[cfg(feature = "smp")]
/// Claim and compute blocks until the queue is empty. Runs on **both** cores.
///
/// # Safety
/// As [`run_gemm_with_secondary`]: `job`'s buffers must be valid for the exchange.
unsafe fn drain_blocks(job: &GemmJob) {
    let total = job.hi.div_ceil(GEMM_BLOCK_ROWS);
    loop {
        let b = NEXT_BLOCK.fetch_add(1, Ordering::AcqRel);
        if b >= total {
            return;
        }
        let lo = b * GEMM_BLOCK_ROWS;
        let hi = (lo + GEMM_BLOCK_ROWS).min(job.hi);
        // SAFETY: the caller's contract, plus the claim above, which makes this row
        // range this core's alone.
        unsafe { gemm_rows(job, lo, hi) };
        BLOCKS_DONE.this().fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "smp")]
/// Blocks completed by CPU `cpu` in the last run.
pub fn blocks_done(cpu: usize) -> usize {
    // SAFETY: a cross-core read of a counter, which is what the aggregate is for.
    unsafe { BLOCKS_DONE.get(cpu) }.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// The two halves of a **rendezvous**: each core announces itself and then waits for
/// the other. Both can only pass if both are running at the same time.
static RV_PRIMARY: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static RV_SECONDARY: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Set when a rendezvous half gave up, so a failure is a reported observation rather
/// than a hang.
static RV_TIMEOUT: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// How long a rendezvous half waits before giving up, in timer-domain nanoseconds.
///
/// Generous (2 s) because under QEMU's TCG the two cores are time-sliced by the host
/// and a secondary can be descheduled for a long time. The bound exists so a
/// single-core machine - where the rendezvous genuinely cannot complete - reports that
/// instead of wedging the boot test into its 120 s timeout with no diagnostic.
const RV_TIMEOUT_NS: u64 = 2_000_000_000;

#[cfg(feature = "smp")]
/// Announce this side of the rendezvous and wait for the other. Returns false on
/// timeout.
///
/// **This is the parallelism proof.** Nothing about it depends on timing or on
/// counting: the primary cannot pass until the secondary has written its flag, and
/// the secondary cannot pass until the primary has written its own. Neither writes its
/// flag after passing. So both passing means both cores executed inside the same
/// interval - which one core cannot produce, with or without preemption, because
/// neither side yields and kernel-context preemption does not exist here.
fn rendezvous(mine: &AtomicUsize, theirs: &AtomicUsize) -> bool {
    mine.store(1, Ordering::Release);
    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while theirs.load(Ordering::Acquire) == 0 {
        if arch::timer_now_ns() >= deadline {
            RV_TIMEOUT.fetch_add(1, Ordering::Release);
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

#[cfg(feature = "smp")]
/// Publish `job` for a secondary and run the primary's own rows, then wait for the
/// secondary. Returns `(rendezvous_held, secondary_finished)`.
///
/// # Safety
/// `job`'s A/B/C addresses must be valid kernel VAs for the stated shapes, and must
/// stay valid until this returns. The primary's rows and the secondary's rows must be
/// **disjoint** in C - which is what makes this need no lock around the compute at
/// all, and is the caller's obligation because only the caller knows the split.
pub unsafe fn run_gemm_with_secondary(job: GemmJob, own_lo: usize, own_hi: usize) -> (bool, bool) {
    JOBS_DONE.store(0, Ordering::Release);
    NEXT_BLOCK.store(0, Ordering::Release);
    for cpu in 0..MAX_CPUS {
        // SAFETY: between runs; no core is draining.
        unsafe { BLOCKS_DONE.get(cpu) }.store(0, Ordering::Release);
    }
    RV_PRIMARY.store(0, Ordering::Release);
    RV_SECONDARY.store(0, Ordering::Release);
    RV_TIMEOUT.store(0, Ordering::Release);
    *JOB.lock() = Some(job);

    // Both cores meet here, so the compute below genuinely overlaps rather than the
    // secondary starting after the primary has finished.
    let met = rendezvous(&RV_PRIMARY, &RV_SECONDARY);

    // Both cores now drain the same queue. The primary takes no reserved share: if
    // the secondary is faster it does more, and if the secondary never arrives the
    // primary completes the whole job alone - which is what makes this degrade to
    // correct-but-serial rather than to a hang.
    // SAFETY: the caller's contract - valid buffers; the claim in `drain_blocks`
    // makes each block one core's alone.
    unsafe { drain_blocks(&job) };
    let _ = (own_lo, own_hi);

    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while JOBS_DONE.load(Ordering::Acquire) == 0 {
        if arch::timer_now_ns() >= deadline {
            return (met, false);
        }
        core::hint::spin_loop();
    }
    (met, true)
}

#[cfg(feature = "smp")]
/// Compute output rows `[lo, hi)` of the job's GEMM.
///
/// # Safety
/// As [`run_gemm_with_secondary`]: valid buffers, and `[lo, hi)` disjoint from any
/// range another core is computing.
unsafe fn gemm_rows(job: &GemmJob, lo: usize, hi: usize) {
    if hi <= lo {
        return;
    }
    // SAFETY: the caller's contract. The row offset is applied to A and C so this
    // core touches only its own rows of C; B is read-only and shared.
    unsafe {
        crate::engine::tile_kernels::gemm_i8_i32(
            (job.a as *const i8).add(lo * job.as_),
            job.as_,
            job.b as *const i8,
            job.bs,
            (job.c as *mut i32).add(lo * job.cs),
            job.cs,
            hi - lo,
            job.n,
            job.k,
        );
    }
}

#[cfg(feature = "smp")]
/// Whether either rendezvous half timed out (so "both cores ran at once" is
/// **not** claimed).
pub fn rendezvous_timed_out() -> bool {
    RV_TIMEOUT.load(Ordering::Acquire) != 0
}

#[cfg(feature = "smp")]
/// How many secondaries have signalled completion.
pub fn secondaries_up() -> usize {
    SECONDARY_UP.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// Why bringing up a secondary did not produce a running second core.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StartError {
    /// The inventory reports only one CPU - nothing to start.
    NoSecondary,
    /// The arch layer cannot start a secondary here; the string is the observed
    /// reason (e.g. the PSCI return, or the x86 trampoline gap).
    Blocked(&'static str),
    /// The start call was accepted but the secondary did not run kernel code
    /// within the bounded wait.
    Timeout,
}

#[cfg(feature = "smp")]
/// Bounded spin budget for the primary waiting on a secondary to come online.
/// Large enough that QEMU's round-robin TCG schedules the secondary; on timeout
/// the caller skips-with-reason rather than hanging.
const WAIT_BUDGET: u64 = 200_000_000;

#[cfg(feature = "smp")]
/// Initialise SMP on the **primary** CPU: establish CPU 0's identity and mark it
/// online. Idempotent-ish; call once before [`bring_up_one`].
pub fn init() {
    arch::smp_set_this_cpu(0);
    set_online(0, arch::boot_cpu_hw_id());
}

#[cfg(feature = "smp")]
/// Ask the arch layer to start **one** secondary core and wait (bounded) for it
/// to run kernel code (mark itself online + bump the shared counter). Returns
/// the CPU index that came online, or a [`StartError`] the caller can turn into
/// a skip-with-reason. Runs on the primary; never hangs (bounded wait).
pub fn bring_up_one() -> Result<usize, StartError> {
    let inv = crate::hw::inventory();
    let boot = arch::boot_cpu_hw_id();

    // Pick a target hardware id: a firmware-enumerated non-boot CPU if there is
    // one, else the next id after the boot CPU. The synthesized fallback lets an
    // ISA whose firmware cannot enumerate secondaries from the kernel's exception
    // level (ARM64: PSCI enumeration needs EL3, so only the boot CPU is in the
    // inventory) still make a *genuine* bring-up attempt and report the observed
    // blocker, rather than silently doing nothing.
    let mut target = None;
    for i in 0..inv.ncpus {
        if inv.cpus[i].hw_id != boot {
            target = Some(inv.cpus[i].hw_id);
            break;
        }
    }
    let target_hw = target.unwrap_or(boot + 1);

    let before = secondaries_up();
    match arch::smp_start_secondary(target_hw) {
        Ok(()) => {
            let mut budget = WAIT_BUDGET;
            while budget > 0 {
                if secondaries_up() > before {
                    // Return the registry index the secondary claimed (the first
                    // online slot above the boot CPU).
                    for slot in 1..MAX_CPUS {
                        if cpu(slot).is_online() {
                            return Ok(slot);
                        }
                    }
                    return Ok(0);
                }
                core::hint::spin_loop();
                budget -= 1;
            }
            Err(StartError::Timeout)
        }
        Err(reason) => Err(StartError::Blocked(reason)),
    }
}
