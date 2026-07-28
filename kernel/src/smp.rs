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
    if i != 0 {
        MULTICORE.store(true, Ordering::Release);
    }
}

/// Whether more than the boot CPU is online.
///
/// A cached flag rather than [`online_count`], which scans the whole registry: this is
/// read on **every Linux syscall** (the personality lock consults it to decide whether
/// to lock at all), and 64 atomic loads in front of every trap is not a thing to put
/// on that path to answer a yes/no question.
#[inline]
pub fn multicore() -> bool {
    MULTICORE.load(Ordering::Acquire)
}

static MULTICORE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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
    // The per-core register bits U-mode relies on. No bring-up path sets them, and on
    // RISC-V one of them is `sstatus.SUM`: without it this core runs cells fine until
    // the kernel first touches one's memory, and then takes a store page fault at a
    // kernel PC on a perfectly-mapped user page (docs/SMP.md 10.0).
    arch::user_mode_init_this_cpu();
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
/// The secondary's work loop: meet the primary at the rendezvous, do the published
/// work (a GEMM share, or a cell to run in user mode), signal done, look for more.
///
/// **Bounded idling, not bounded work.** The loop serves any number of jobs, but it
/// gives up after `RV_TIMEOUT_NS` of finding *none*: a secondary that spun forever
/// would be correct in a running system and wrong in a boot test, where the primary
/// finishes and exits QEMU - and worse, it would hold a core against the host for the
/// rest of the run, which under TCG slows the primary down. A real dispatch loop
/// belongs with per-CPU scheduling, not here.
fn secondary_work_loop() {
    let mut deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    loop {
        // A **queue of runnable cells** takes priority over everything: this is the
        // placement path, where no core is told which cell to run - it claims one
        // (docs/SMP.md 10.0).
        if PLACE_COUNT.load(Ordering::Acquire) > 0 {
            // SAFETY: `place_cells`' contract - present, native, listed once.
            unsafe { drain_cells() };
            deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            continue;
        }
        // A single named cell to run in **user mode**: the hand-placed path that
        // came first, kept because it is what pairs two cells at one instant.
        // **`swap`, not load-then-store.** With one secondary the two were equivalent;
        // with `start_all` bringing up three, two cores could both read the published
        // cell before either cleared it and both would run it - one cell, two cores,
        // one trap frame. It presented as two cores faulting at PC 0 at the same
        // instant, intermittently (docs/SMP.md 10.0). One atomic exchange makes the
        // claim exclusive, exactly as the `fetch_add` does for the placement queue.
        let cell = USER_CELL.swap(usize::MAX, Ordering::AcqRel);
        if cell != usize::MAX {
            if rendezvous(&RV_SECONDARY, &RV_PRIMARY) {
                let code = code_of(crate::user::run(cell).1);
                USER_CELL_CODE.store(code as usize, Ordering::Release);
            }
            USER_CELL_DONE.fetch_add(1, Ordering::Release);
            deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            continue;
        }
        // **Taken** out under the lock, not copied: the loop serves more than one job
        // now, so a job left in place would be drained again the moment this core came
        // back round. The compute below must not hold the lock, because the primary is
        // computing its own rows concurrently and would block on it.
        let job = JOB.lock().take();
        if let Some(job) = job {
            if rendezvous(&RV_SECONDARY, &RV_PRIMARY) {
                // SAFETY: the primary's `run_gemm_with_secondary` contract - the
                // buffers are valid for the exchange, and each block this claims is
                // its alone.
                unsafe { drain_blocks(&job) };
            }
            JOBS_DONE.fetch_add(1, Ordering::Release);
            deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            continue;
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
/// A cell index published for the secondary to **run in user mode**, or `usize::MAX`
/// for none (docs/SMP.md 10.0).
static USER_CELL: AtomicUsize = AtomicUsize::new(usize::MAX);
#[cfg(feature = "smp")]
/// The outcome code the secondary's cell exited with, and a done flag.
static USER_CELL_CODE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static USER_CELL_DONE: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// Publish cell `idx` for a secondary to run **in user mode**, meet it at the
/// rendezvous, and run `own` on this core at the same time.
///
/// This is the first time a cell executes on a core other than the boot CPU
/// (docs/SMP.md 10.0). The two cells are **partitioned**: each core owns a distinct
/// cell slot and a distinct address space, so the only state they share is the cell
/// table itself - which `run` reads and `finish` writes at *disjoint indices*. That
/// partitioning, not a lock, is what makes it safe, and it is the multikernel answer
/// this design commits to rather than a shortcut (docs/SCHEDULING.md 1a).
///
/// Returns `(rendezvous_held, secondary_finished, secondary_exit_code, own_outcome)`.
///
/// # Safety
/// Both cells must be installed, present, and **native**; neither may be the other.
pub unsafe fn run_cells_on_both(own: usize, other: usize) -> (bool, bool, usize, u64) {
    USER_CELL_DONE.store(0, Ordering::Release);
    USER_CELL_CODE.store(0, Ordering::Release);
    RV_PRIMARY.store(0, Ordering::Release);
    RV_SECONDARY.store(0, Ordering::Release);
    RV_TIMEOUT.store(0, Ordering::Release);
    USER_CELL.store(other, Ordering::Release);

    let met = rendezvous(&RV_PRIMARY, &RV_SECONDARY);
    // SAFETY: the caller's contract; this core touches only its own cell slot.
    let outcome = crate::user::run(own).1;

    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while USER_CELL_DONE.load(Ordering::Acquire) == 0 {
        if arch::timer_now_ns() >= deadline {
            return (met, false, 0, code_of(outcome));
        }
        core::hint::spin_loop();
    }
    (
        met,
        true,
        USER_CELL_CODE.load(Ordering::Acquire),
        code_of(outcome),
    )
}

// ------------------------------------------ placing runnable cells on free cores
//
// `run_cells_on_both` above hands *one named cell* to *the* secondary: the primary
// decides who runs where. That is a placement decision made by hand, and it is the
// thing a scheduler is supposed to make for you.
//
// This is the smallest honest step past it: **one shared queue of runnable cells,
// and every core claims from it whenever it is free**. A core that finishes a short
// cell comes back and takes the next one; a core stuck on a long cell takes no more.
// Nobody is assigned anything in advance, so the per-core counts are a *result* -
// exactly the reasoning the GEMM block queue already rests on, applied to cells
// instead of to rows. It is work-conserving (no core idles while the queue is
// non-empty) and self-balancing (by claim rate, not by prediction).
//
// It is **not** the full scheduler: there is no preemption across cores, no
// migration of a cell already running, no priority - a claim runs to completion.
// Those need the 10.2 audit; this needs only what 10.0 already established, because
// a claimed cell is still a *partitioned* cell (one core, one slot, one address
// space) - the claim is simply made at run time instead of at compile time.

#[cfg(feature = "smp")]
/// The most cells one placement round can hold. A fixed array; the kernel is
/// allocation-free.
pub const MAX_PLACED_CELLS: usize = 16;

#[cfg(feature = "smp")]
/// The runnable set for the current round, the claim cursor over it, and how many
/// cells have finished.
static PLACE_CELLS: SpinLock<[usize; MAX_PLACED_CELLS]> = SpinLock::new([0; MAX_PLACED_CELLS]);
#[cfg(feature = "smp")]
static PLACE_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static PLACE_NEXT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static PLACE_DONE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Whether the current round runs its cells **preemptively**.
static PLACE_PREEMPT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Which cores have already brought up their own preemption timer this boot.
static PREEMPT_READY: PerCpu<AtomicUsize> =
    PerCpu::from_array([const { AtomicUsize::new(0) }; MAX_CPUS]);

#[cfg(feature = "smp")]
/// Bring up **this** core's preemption timer, once.
///
/// Everything it touches is per-core hardware - the RISC-V `stimecmp`/`sie` CSRs,
/// this core's GICv3 redistributor and CPU interface, this core's LAPIC - and none
/// of it is set by any bring-up trampoline, so a secondary that skipped this would
/// run its cells to completion with no slice and the preemption would silently not
/// happen (docs/SMP.md 10.0). The per-core "already done" flag makes it idempotent
/// without a lock: only this core reads or writes its own slot.
fn enable_preemption_here() {
    // SAFETY: this core's own slot.
    let flag = PREEMPT_READY.this();
    if flag.swap(1, Ordering::AcqRel) == 1 {
        return;
    }
    crate::sched::init_run_queue();
    arch::enable_timer_irq_this_cpu();
}
#[cfg(feature = "smp")]
/// Per-cell exit code, and which CPU ran it. Written by the core that ran the cell,
/// at its own index - disjoint, so no lock.
static PLACE_CODE: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_PLACED_CELLS];
#[cfg(feature = "smp")]
static PLACE_CPU: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_PLACED_CELLS];
#[cfg(feature = "smp")]
/// How many cells each CPU claimed - the load-sharing evidence.
static PLACE_TAKEN: PerCpu<AtomicUsize> =
    PerCpu::from_array([const { AtomicUsize::new(0) }; MAX_CPUS]);

#[cfg(feature = "smp")]
/// How many cells a core claims at once.
///
/// **More than one, on purpose.** A core holding a single cell has nothing to
/// preempt *to*: the slice fires, the scheduler looks for another runnable cell this
/// core owns, finds none, and the cell runs on. Claiming a pair is the smallest set
/// that makes cross-core preemption a real thing to observe rather than a mechanism
/// with no witness.
pub const CLAIM_BATCH: usize = 2;

#[cfg(feature = "smp")]
/// Claim and run cells until the queue is empty. Runs on **any** core.
///
/// Claims come in batches of [`CLAIM_BATCH`] and the whole batch is stamped as this
/// core's ([`crate::user::claim_cell`]) before any of it runs, so no other core's
/// scheduler will ever see one of them. Within the batch the core runs them
/// **preemptively** where the boot enabled dispatch and a timer interrupt: `run`
/// unwinds when *some* cell of the batch exits, which need not be the one it was
/// entered with, so the loop simply re-enters on whatever is left.
///
/// # Safety
/// Every queued cell must be installed, present and **native**, and no cell may
/// appear twice (the claim gives one core exclusive ownership of a slot only if the
/// slot appears once).
unsafe fn drain_cells() {
    let n = PLACE_COUNT.load(Ordering::Acquire);
    let cpu = arch::cpu_index();
    if PLACE_PREEMPT.load(Ordering::Acquire) == 1 {
        enable_preemption_here();
    }
    loop {
        // Take a batch off the queue.
        let mut slot = [usize::MAX; CLAIM_BATCH];
        let mut cell = [usize::MAX; CLAIM_BATCH];
        let mut got = 0;
        while got < CLAIM_BATCH {
            let k = PLACE_NEXT.fetch_add(1, Ordering::AcqRel);
            if k >= n {
                break;
            }
            slot[got] = k;
            cell[got] = PLACE_CELLS.lock()[k];
            got += 1;
        }
        if got == 0 {
            return;
        }
        // Stamp ownership before anything runs: from here no other core's pick can
        // see these cells, which is what makes running them need no lock.
        for c in cell.iter().take(got) {
            crate::user::claim_cell(*c, cpu);
        }

        // Run until every cell of the batch has exited. `run` returns the cell that
        // actually ended the run - under preemption that is whichever of the batch
        // finished first, not necessarily the one entered.
        let mut left = got;
        while left > 0 {
            let Some(pos) = (0..got).find(|&i| slot[i] != usize::MAX) else {
                break;
            };
            // The caller's contract holds here: a present native cell this core owns
            // exclusively, because the `fetch_add` handed its index to nobody else.
            let (exited, outcome) = crate::user::run(cell[pos]);
            let code = code_of(outcome);
            // Attribute the outcome to whichever cell of the batch ended the run.
            let Some(done) = (0..got).find(|&i| cell[i] == exited && slot[i] != usize::MAX) else {
                // Not one of ours - impossible under the claim, but bailing out beats
                // spinning if the invariant is ever broken.
                break;
            };
            PLACE_CODE[slot[done]].store(code as usize, Ordering::Release);
            PLACE_CPU[slot[done]].store(cpu, Ordering::Release);
            slot[done] = usize::MAX;
            left -= 1;
            PLACE_TAKEN.this().fetch_add(1, Ordering::AcqRel);
            PLACE_DONE.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(feature = "smp")]
/// Publish `cells` as the runnable set and let **every** core - this one and every
/// online secondary - claim from it until it is empty.
///
/// Returns `(all_finished, per-cell (exit code, cpu that ran it))` truncated to
/// `cells.len()`. `all_finished` is false if the bound elapsed with work
/// outstanding, in which case the caller must claim nothing.
///
/// # Safety
/// As [`drain_cells`]: each entry installed, present, native, and listed once.
pub unsafe fn place_cells(cells: &[usize], out: &mut [(u64, usize)]) -> bool {
    // SAFETY: the caller's contract.
    unsafe { place_cells_inner(cells, out, false) }
}

#[cfg(feature = "smp")]
/// [`place_cells`], but every core runs its claimed batch **preemptively**: it brings
/// up its own preemption timer and the slice moves the CPU between the cells that
/// core owns. Requires the boot to have enabled queue-driven dispatch.
///
/// # Safety
/// As [`place_cells`].
pub unsafe fn place_cells_preemptive(cells: &[usize], out: &mut [(u64, usize)]) -> bool {
    // SAFETY: the caller's contract.
    unsafe { place_cells_inner(cells, out, true) }
}

#[cfg(feature = "smp")]
/// # Safety
/// As [`place_cells`].
unsafe fn place_cells_inner(cells: &[usize], out: &mut [(u64, usize)], preempt: bool) -> bool {
    let n = cells.len().min(MAX_PLACED_CELLS).min(out.len());
    {
        let mut q = PLACE_CELLS.lock();
        q[..n].copy_from_slice(&cells[..n]);
    }
    for k in 0..MAX_PLACED_CELLS {
        PLACE_CODE[k].store(usize::MAX, Ordering::Release);
        PLACE_CPU[k].store(usize::MAX, Ordering::Release);
    }
    for c in 0..MAX_CPUS {
        // SAFETY: between rounds; no core is draining.
        unsafe { PLACE_TAKEN.get(c) }.store(0, Ordering::Release);
    }
    PLACE_DONE.store(0, Ordering::Release);
    PLACE_NEXT.store(0, Ordering::Release);
    if preempt {
        // The **global** half of timer bring-up - the APIC mode probe, the IDT gate,
        // the one-shot self-test - happens here, on the primary, before any secondary
        // is told the round is preemptive. Four cores racing through it wrote one
        // shared IDT concurrently and printed four interleaved copies of the probe's
        // own line; each core still programs its own timer registers in
        // `enable_preemption_here`, which is the part that genuinely is per core.
        arch::enable_timer_irq();
        enable_preemption_here();
    }
    PLACE_PREEMPT.store(usize::from(preempt), Ordering::Release);
    // Last, and with release ordering: a secondary polls this, so publishing the
    // count is what opens the queue. Setting it before the cursor was reset would
    // let a secondary claim against the previous round's cursor.
    PLACE_COUNT.store(n, Ordering::Release);

    // This core is a worker too - it does not sit and wait while others work.
    // SAFETY: the caller's contract.
    unsafe { drain_cells() };

    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while PLACE_DONE.load(Ordering::Acquire) < n {
        if arch::timer_now_ns() >= deadline {
            PLACE_COUNT.store(0, Ordering::Release);
            return false;
        }
        core::hint::spin_loop();
    }
    PLACE_COUNT.store(0, Ordering::Release);
    for (k, slot) in out.iter_mut().enumerate().take(n) {
        *slot = (
            PLACE_CODE[k].load(Ordering::Acquire) as u64,
            PLACE_CPU[k].load(Ordering::Acquire),
        );
    }
    true
}

#[cfg(feature = "smp")]
/// How many cells CPU `cpu` claimed in the last [`place_cells`] round.
pub fn cells_taken(cpu: usize) -> usize {
    // SAFETY: a plain atomic read of another CPU's counter, after the round ended.
    unsafe { PLACE_TAKEN.get(cpu) }.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// Temporary diagnostic accessor.
pub fn dbg_code(o: crate::user::Outcome) -> u64 {
    code_of(o)
}

#[cfg(feature = "smp")]
fn code_of(o: crate::user::Outcome) -> u64 {
    match o {
        crate::user::Outcome::Exited(c) => c,
        _ => u64::MAX,
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

/// How many CPUs may be online at once.
///
/// Bounded by the smallest per-CPU array the ring-3 path needs: the per-core
/// KERNEL_CTX / GDT / TSS / stack slots each ISA sizes at 8. Raising it means
/// raising those together, which is why the number lives here rather than being
/// implied in three assembly files.
pub const MAX_SMP_CPUS: usize = 8;

#[cfg(feature = "smp")]
/// Start **every** secondary the firmware enumerates, each on its own stack, and
/// return how many came online (bounded wait per core; a core that does not answer
/// is skipped, not waited on forever).
///
/// This is what makes placement mean something: with one secondary, "the queue is
/// drained by whichever core is free" has two participants and looks a lot like a
/// split in half. Runs on the primary.
pub fn start_all() -> usize {
    let inv = crate::hw::inventory();
    let boot = arch::boot_cpu_hw_id();
    let mut started = 0;
    let try_start = |hw: u32| -> bool {
        if hw == boot || hw_online(hw) || online_count() >= MAX_SMP_CPUS {
            return false;
        }
        let before = secondaries_up();
        if arch::smp_start_secondary(hw).is_err() {
            return false;
        }
        let mut budget = WAIT_BUDGET;
        while budget > 0 && secondaries_up() == before {
            core::hint::spin_loop();
            budget -= 1;
        }
        secondaries_up() > before
    };

    for i in 0..inv.ncpus {
        if try_start(inv.cpus[i].hw_id) {
            started += 1;
        }
    }
    // Where the firmware cannot enumerate secondaries from the kernel's exception
    // level, the inventory holds only the boot CPU and the loop above starts nothing
    // (ARM64: PSCI enumeration needs EL3 - docs/SMP.md 7). Probe the next ids instead,
    // exactly as `bring_up_one` already synthesizes `boot + 1`: a *genuine* attempt
    // whose answer is observed, stopping at the first id that does not answer so a
    // machine with no more cores costs one refused call rather than a scan.
    if inv.ncpus <= 1 {
        for k in 1..MAX_SMP_CPUS as u32 {
            let hw = boot.wrapping_add(k);
            if hw_online(hw) {
                continue; // already started (e.g. by `bring_up_one`) - not a refusal
            }
            if !try_start(hw) {
                break;
            }
            started += 1;
        }
    }
    started
}

#[cfg(feature = "smp")]
/// Whether a CPU with this hardware id is already registered online.
fn hw_online(hw: u32) -> bool {
    CPUS.iter().any(|c| c.is_online() && c.hw_id() == hw)
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
