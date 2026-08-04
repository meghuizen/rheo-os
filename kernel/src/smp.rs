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
/// `repr(transparent)` because the observability root publishes the addresses of
/// several `PerCpu` statics for a reader outside the guest to stride
/// (docs/OBSERVABILITY.md), and that reader needs the container's layout to be the
/// array's - guaranteed, rather than true today because there is one field.
#[repr(transparent)]
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
    /// [`crate::obs::lock::LockId`] as a byte; 0 = unnamed, never measured. The
    /// byte packs into `locked`'s padding, so naming costs no size.
    id: u8,
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
            id: 0,
            data: UnsafeCell::new(value),
        }
    }

    /// A lock that identifies itself to the contention instrumentation
    /// (docs/OBSERVABILITY.md 11, S5). Same lock, same fast path - the only
    /// costs a name adds with recording off are one `id` test on the contended
    /// path and one compare in the guard's drop.
    pub const fn named(value: T, id: crate::obs::lock::LockId) -> SpinLock<T> {
        SpinLock {
            locked: AtomicBool::new(false),
            id: id as u8,
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free. Returns a guard that
    /// releases the lock when dropped.
    ///
    /// The fast path is one **strong** `compare_exchange` - strong deliberately,
    /// because a weak CAS may fail spuriously on LL/SC ISAs and every failure
    /// here enters the contended path, whose count must mean "the lock was
    /// genuinely held" for the uncontended-equals-zero oracle to hold.
    #[inline]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return SpinGuard {
                lock: self,
                hold_start_ns: self.hold_stamp(),
            };
        }
        self.lock_contended()
    }

    /// Acquire the lock **or give up immediately**, never spinning.
    ///
    /// For a caller whose answer to "someone else is inside" is to do something
    /// else rather than to wait - `mm::frames`' flat combining publishes its
    /// request to a per-CPU slot instead (docs/SMP.md 10.0g), which is only cheaper
    /// than waiting if discovering the contention is cheap.
    ///
    /// Exactly [`SpinLock::lock`]'s fast path with the `#[cold]` fallback replaced
    /// by `None`, so a successful `try_lock` costs precisely what a successful
    /// `lock` costs: one strong `compare_exchange`. A failure is deliberately **not**
    /// counted as contention - the contention counters mean "an acquire had to wait",
    /// and a caller that never waits did not.
    #[inline]
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Some(SpinGuard {
                lock: self,
                hold_start_ns: self.hold_stamp(),
            });
        }
        None
    }

    /// The contended acquire: spin until free, and - for a named lock with the
    /// Lock window on - count the contention, the spin iterations and the wait.
    /// `#[cold]` so the fast path stays a test and a return; the clock is read
    /// only when observing, so an unobserved contended acquire costs exactly
    /// what it used to.
    #[cold]
    #[inline(never)]
    fn lock_contended(&self) -> SpinGuard<'_, T> {
        let observe = self.id != 0 && crate::obs::on(crate::obs::Window::Lock);
        let t0 = if observe {
            crate::arch::timer_now_ns()
        } else {
            0
        };
        let mut spins: u64 = 0;
        loop {
            // Spin on a relaxed read so contenders share the cache line
            // read-only until the holder releases (test-and-test-and-set).
            while self.locked.load(Ordering::Relaxed) {
                spins = spins.wrapping_add(1);
                core::hint::spin_loop();
            }
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        if observe {
            let wait = crate::arch::timer_now_ns().saturating_sub(t0);
            crate::obs::lock::contended(self.id, spins, wait);
        }
        SpinGuard {
            lock: self,
            hold_start_ns: self.hold_stamp(),
        }
    }

    /// When the hold began - stamped only for a named lock with the hold
    /// modifier on, because reading the clock here lengthens the critical
    /// section it measures (the reason `W_LOCK_HOLD` is its own bit).
    #[inline]
    fn hold_stamp(&self) -> u64 {
        if self.id != 0 && crate::obs::windows() & (1u32 << crate::abi::obs::W_LOCK_HOLD) != 0 {
            crate::arch::timer_now_ns()
        } else {
            0
        }
    }
}

/// RAII guard for a held [`SpinLock`]; releases on drop.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// When the hold began (`timer_now_ns` domain), or 0 = not measuring. The 8
    /// bytes and the one compare in drop are the whole cost of hold measurement
    /// existing (docs/OBSERVABILITY.md 11, S5).
    hold_start_ns: u64,
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
        // Record the hold AFTER the release store, so the measured region never
        // includes the recording - and so the recording (which may touch the
        // metrics of the very lock being dropped, e.g. the frames pool lock)
        // runs with the lock already free.
        if self.hold_start_ns != 0 {
            crate::obs::lock::held(
                self.lock.id,
                crate::arch::timer_now_ns().saturating_sub(self.hold_start_ns),
            );
        }
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
    // This core's own class and model. Only the calling core can read them - x86-64's CPUID
    // leaf 0x1A answers about whoever executed it - so a secondary classifies itself rather
    // than the boot CPU classifying it (docs/RESOURCE-GRAPH.md 2.4b).
    crate::hw::classify_this_cpu(idx);
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
                // **This core's own preemption timer**, if the publisher asked for one.
                // Every register involved is per-core hardware that no trampoline sets, so
                // a secondary that skipped this runs the cell cooperatively however the
                // boot is configured - which is what a Bun on a secondary did on the first
                // run, reporting 0 of 24 slices taken while the primary's identical boot
                // was preemptive. The *global* half (APIC-mode probe, IDT gate, one-shot
                // self-test) is the primary's and has already happened.
                if USER_CELL_PREEMPT.load(Ordering::Acquire) == 1 {
                    enable_preemption_here();
                }
                // **Claim it for this core before entering it.** An unclaimed cell is
                // visible to every core's scheduler, which is right on a single-CPU
                // boot and wrong here: with preemption on, the *peer*'s slice fires,
                // its `preempt_cell` scan sees this cell as runnable and switches into
                // it - one cell, two cores, one trap frame. Observed as an instruction
                // fetch at 0 on both cores with two multi-threaded cells
                // (docs/SMP.md 10.2a). Which core takes the published cell is not known
                // to the publisher, so the claim has to be made here, by the core that
                // wins it.
                crate::user::claim_cell(cell, cpu_index());
                let code = code_of(crate::user::run(cell).1);
                USER_CELL_CODE.store(code as usize, Ordering::Release);
            }
            USER_CELL_DONE.fetch_add(1, Ordering::Release);
            deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            continue;
        }
        // A bare function to run - the driver-path job (`run_fn_with_secondary`).
        // Claimed with a `swap` for the same reason `USER_CELL` is: two secondaries
        // must not both take it.
        let f = FN_JOB.swap(0, Ordering::AcqRel);
        if f != 0 {
            if rendezvous(&RV_SECONDARY, &RV_PRIMARY) {
                // SAFETY: `run_fn_with_secondary` stored a real `fn()` here, and
                // stores nothing else; 0 is the empty value and was excluded above.
                let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(f) };
                f();
            }
            FN_DONE.fetch_add(1, Ordering::Release);
            deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            continue;
        }
        // **Copied, not taken**, and gated on the round's generation: every online core
        // joins the same queue, instead of the first secondary removing the job and the
        // rest idling beside undrained blocks. The lock is released before the compute,
        // because peers are draining the same queue concurrently.
        let round = JOB_GEN.load(Ordering::Acquire);
        if round != 0 && JOB_SEEN.this().load(Ordering::Acquire) != round {
            JOB_SEEN.this().store(round, Ordering::Release);
            let job = *JOB.lock();
            if let Some(job) = job {
                // Every expected core meets here, so the drain below genuinely overlaps
                // across all of them rather than only across two.
                if gemm_barrier() {
                    // SAFETY: the primary's `run_gemm` contract - the buffers are valid
                    // for the exchange, and each block this claims is its alone.
                    unsafe { drain_blocks(&job) };
                }
                JOBS_DONE.fetch_add(1, Ordering::Release);
            }
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
/// Which round of GEMM work is published. Bumped by the primary; each core drains a
/// given generation once.
///
/// **Generation, not `take()`.** The job used to be removed from its slot by the first
/// secondary to see it, which made the phase inherently two-core: primary plus whoever
/// grabbed it. Every other core sat in its idle loop while a queue of blocks went
/// undrained, which is the opposite of what a work queue is for.
static JOB_GEN: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// The generation each core last drained, so a core neither misses a round nor drains
/// one twice.
static JOB_SEEN: PerCpu<AtomicUsize> =
    PerCpu::from_array([const { AtomicUsize::new(0) }; MAX_CPUS]);
#[cfg(feature = "smp")]
/// An **N-way barrier** for the GEMM round: arrivals, and how many are expected.
///
/// The two-way `rendezvous` proves two cores overlapped and cannot prove more, because
/// each half waits for exactly one peer. Here every participant waits for *all* of
/// them, sized from [`online_count`] - which the primary knows - so passing it means
/// every online core was inside the same interval. A core that never arrives lets the
/// others through on the deadline, and the round then honestly reports fewer
/// participants rather than hanging.
static GEMM_ARRIVE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static GEMM_EXPECT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Whether every expected core reached the GEMM barrier.
static GEMM_ALL_MET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "smp")]
/// Wait until every expected core has reached this point, or the deadline passes.
///
/// Returns whether the full set met. Bounded, like every other wait in this file: a
/// boot test must not be able to hang on a core that did not come up.
fn gemm_barrier() -> bool {
    let want = GEMM_EXPECT.load(Ordering::Acquire);
    GEMM_ARRIVE.fetch_add(1, Ordering::AcqRel);
    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while GEMM_ARRIVE.load(Ordering::Acquire) < want {
        if arch::timer_now_ns() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

#[cfg(feature = "smp")]
/// Whether every online core met at the last GEMM round's barrier.
pub fn gemm_all_met() -> bool {
    GEMM_ALL_MET.load(Ordering::Acquire)
}
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
/// Generous because under QEMU's TCG the two cores are time-sliced by the host and a
/// secondary can be descheduled for a long time. The bound exists so a single-core
/// machine - where the rendezvous genuinely cannot complete - reports that instead of
/// wedging the boot test into its 120 s timeout with no diagnostic.
///
/// **10 s, raised from 2 s against a measurement.** With the host deliberately
/// oversubscribed 3:1 (12 spinners against 4 cores), 2 s was not enough: the `smp`
/// kernel failed with "cores did not meet" about a kernel that was working, since
/// whether a vCPU thread gets scheduled inside a fixed window is a property of the
/// host and not of this code. CI runners are shared machines, so that is a real
/// condition rather than a contrived one. 10 s keeps the diagnostic purpose intact -
/// it is still an eighth of the boot budget, so a genuinely single-core machine
/// reports the timeout long before the harness gives up.
const RV_TIMEOUT_NS: u64 = 10_000_000_000;

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

// ------------------------------------------------- a plain job: run this function
//
// The GEMM job above carries matrix shapes because that is what it is for. Some
// work has nothing to describe but itself - a device driver's per-core submission
// path, for instance, where the whole question is *which core issued it* and the
// arguments are already reachable through a static. For those, the job is a bare
// function pointer.
//
// Kept separate from `GemmJob` rather than generalising it: a `GemmJob` with an
// optional function pointer would make both callers read the other's fields.

#[cfg(feature = "smp")]
/// The function a secondary should run, as a raw address (0 = none). A pointer
/// rather than a closure because it crosses cores through a static and must be
/// `Copy` with no captured environment - a captured reference would be a borrow
/// this side cannot prove outlives the other core.
static FN_JOB: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Bumped by a secondary when it has finished its function job.
static FN_DONE: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// Run `theirs` on a secondary and `mine` on this core **at the same time**,
/// meeting at a rendezvous first so the overlap is real rather than sequential.
/// Returns `(rendezvous_held, secondary_finished)`.
///
/// Both functions take no arguments and return nothing: anything they need is a
/// static, which is the only thing two cores can share without a lifetime one of
/// them cannot check.
pub fn run_fn_with_secondary(theirs: fn(), mine: fn()) -> (bool, bool) {
    FN_DONE.store(0, Ordering::Release);
    RV_PRIMARY.store(0, Ordering::Release);
    RV_SECONDARY.store(0, Ordering::Release);
    RV_TIMEOUT.store(0, Ordering::Release);
    FN_JOB.store(theirs as usize, Ordering::Release);

    let met = rendezvous(&RV_PRIMARY, &RV_SECONDARY);
    mine();

    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while FN_DONE.load(Ordering::Acquire) == 0 {
        if arch::timer_now_ns() >= deadline {
            return (met, false);
        }
        core::hint::spin_loop();
    }
    (met, true)
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
    RV_TIMEOUT.store(0, Ordering::Release);
    // **Every online core is a participant**, and the barrier is sized from the count
    // the primary already has. Publish the job, then bump the generation - in that
    // order, so a core that sees the new generation always finds the job behind it.
    GEMM_ARRIVE.store(0, Ordering::Release);
    GEMM_EXPECT.store(online_count(), Ordering::Release);
    GEMM_ALL_MET.store(false, Ordering::Release);
    *JOB.lock() = Some(job);
    JOB_GEN.fetch_add(1, Ordering::AcqRel);

    // Every expected core meets here, so the drain below genuinely overlaps across all
    // of them rather than only across two.
    let met = gemm_barrier();
    GEMM_ALL_MET.store(met, Ordering::Release);

    // Every core now drains the same queue. No core has a reserved share: a faster one
    // does more, and if no secondary arrives the primary completes the whole job alone -
    // which is what makes this degrade to correct-but-serial rather than to a hang.
    // SAFETY: the caller's contract - valid buffers; the claim in `drain_blocks`
    // makes each block one core's alone.
    unsafe { drain_blocks(&job) };
    let _ = (own_lo, own_hi);

    // Wait for **the queue**, not for one secondary's completion flag: with N
    // participants the primary cannot know how many will signal, but it knows exactly
    // how many blocks there are, and every block is accounted to the core that did it.
    let total = job.hi.div_ceil(GEMM_BLOCK_ROWS);
    let deadline = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    loop {
        let done: usize = (0..MAX_CPUS).map(blocks_done).sum();
        if done >= total {
            break;
        }
        if arch::timer_now_ns() >= deadline {
            *JOB.lock() = None;
            return (met, false);
        }
        core::hint::spin_loop();
    }
    // Retire the round so a core coming back round finds nothing to drain.
    *JOB.lock() = None;
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
/// Whether the secondary should arm its own preemption timer before entering the published
/// cell. 1 = yes. Per-core hardware, so the secondary has to do it itself.
static USER_CELL_PREEMPT: AtomicUsize = AtomicUsize::new(0);
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
/// With `preempt`, **both** cores run their cell under their own preemption timer, so
/// the two cells are preempted while they overlap. Every register involved is per-core
/// hardware, so each core arms its own; the global half of timer bring-up (APIC-mode
/// probe, IDT gate) is done here on the primary first, exactly as
/// `place_vcores_inner` does it. The caller must have enabled dispatch, or a slice has
/// nothing to hand the CPU to.
///
/// Returns `(rendezvous_held, secondary_finished, secondary_exit_code, own_outcome)`.
///
/// # Safety
/// Both cells must be installed, present, and distinct. Both may be Linux: their
/// per-cell rows are disjoint and the personality's global tables are serialised by
/// `crate::linux::plock`, which under `preempt` covers the trap-context entry points
/// too (docs/SMP.md 10.2a).
pub unsafe fn run_cells_on_both(
    own: usize,
    other: usize,
    preempt: bool,
) -> (bool, bool, usize, u64) {
    USER_CELL_PREEMPT.store(usize::from(preempt), Ordering::Release);
    if preempt {
        arch::enable_timer_irq();
        enable_preemption_here();
    }
    USER_CELL_DONE.store(0, Ordering::Release);
    USER_CELL_CODE.store(0, Ordering::Release);
    RV_PRIMARY.store(0, Ordering::Release);
    RV_SECONDARY.store(0, Ordering::Release);
    RV_TIMEOUT.store(0, Ordering::Release);
    USER_CELL.store(other, Ordering::Release);

    let met = rendezvous(&RV_PRIMARY, &RV_SECONDARY);
    // This core's own cell, claimed for the same reason the secondary claims its one
    // just above: an unclaimed cell is fair game for the peer's scheduler.
    crate::user::claim_cell(own, cpu_index());
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

#[cfg(feature = "smp")]
/// Run **one named cell on a secondary** while this core only waits. Returns
/// `(rendezvous_held_and_finished, exit_code)`.
///
/// [`run_cells_on_both`] needs a cell for the primary too, which is right when the point is
/// overlap. When the point is only "this cell ran off the boot CPU" - a production runtime
/// whose whole load path is being asked about, say - inventing a second cell adds a variable
/// to the experiment. This publishes the cell, meets the secondary so its start is known,
/// and waits.
///
/// # Safety
/// `cell` must be installed, present, and touched by nobody else for the duration.
pub unsafe fn run_cell_on_secondary(cell: usize, preempt: bool) -> (bool, usize) {
    USER_CELL_PREEMPT.store(usize::from(preempt), Ordering::Release);
    USER_CELL_DONE.store(0, Ordering::Release);
    USER_CELL_CODE.store(0, Ordering::Release);
    RV_PRIMARY.store(0, Ordering::Release);
    RV_SECONDARY.store(0, Ordering::Release);
    RV_TIMEOUT.store(0, Ordering::Release);
    USER_CELL.store(cell, Ordering::Release);

    let met = rendezvous(&RV_PRIMARY, &RV_SECONDARY);
    // **A different bound from the rendezvous's**, and not by taste. `RV_TIMEOUT_NS` (2 s)
    // answers "did a secondary arrive", which is a handshake. This waits for a whole
    // *program* to run, and a production runtime streaming ~100 MB off ext4 under TCG takes
    // tens of seconds - the first version reused the 2 s bound and reported "no secondary
    // came up" for a Bun that had already brought JSC up and taken its JIT grant. Sized to
    // sit under the harness's own 120 s boot timeout so a genuine hang still reports here,
    // with a reason, rather than as a bare timeout.
    let deadline = arch::timer_now_ns().wrapping_add(CELL_RUN_TIMEOUT_NS);
    while USER_CELL_DONE.load(Ordering::Acquire) == 0 {
        if arch::timer_now_ns() >= deadline {
            return (false, 0);
        }
        core::hint::spin_loop();
    }
    (met, USER_CELL_CODE.load(Ordering::Acquire))
}

#[cfg(feature = "smp")]
/// How long to wait for a **cell** handed to a secondary to finish, as distinct from how
/// long to wait for the secondary itself to show up. 100 s, under the boot test's 120 s.
const CELL_RUN_TIMEOUT_NS: u64 = 100_000_000_000;

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
static PLACE_DONE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Which CPU claimed each slot (`usize::MAX` = nobody yet) and whether its cell has
/// **started**. Together they are the steal protocol: a slot with an owner and a zero
/// run-mark is work that has been divided but not begun, and is therefore rebalanceable.
static PLACE_OWNER: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(usize::MAX) }; MAX_PLACED_CELLS];
#[cfg(feature = "smp")]
static PLACE_RUN: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(0) }; MAX_PLACED_CELLS];

#[cfg(feature = "smp")]
/// What kind of work each published slot holds, as [`crate::sched::hetero::ThreadClass`]'s
/// discriminant (docs/SCHEDULING.md 12). 0 = `Unknown`, which is what every existing caller
/// publishes and what makes the tier preference below inert for them.
static PLACE_CLASS: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(0) }; MAX_PLACED_CELLS];
#[cfg(feature = "smp")]
/// Cells taken out of a peer's claim by an idle core.
static STEALS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// **Per-node claim cursors** - the CPU half of "vcores follow memory"
/// (docs/SUBSTRATE.md pillar 6).
///
/// A core takes work from its *own* node's cursor first, so a cell runs on a core that
/// shares a memory controller with the cell's pages. That is the point of having placed
/// the pages at all: on real hardware a remote access costs roughly double, and the
/// placement is wasted if the CPU is on the other side of the interconnect.
///
/// It is the **same protocol replicated**, not a new one. Each cursor is a single
/// `fetch_add`, so exactly one core can obtain each index - which is the property the
/// one shared cursor had and the reason it was chosen over a scan-and-claim
/// (docs/SMP.md 10.0: two cores both entering one cell is the failure this design
/// exists to make impossible). Per-node cursors preserve it exactly; a scan would not.
///
/// [`PLACE_GROUP_END`] bounds each node's group in the published (node-sorted) queue.
/// With fewer than two nodes everything lands in one group and this degenerates to the
/// single cursor, byte-for-byte the pre-NUMA behaviour.
static PLACE_NEXT_NODE: [AtomicUsize; crate::hw::MAX_NUMA_NODES] =
    [const { AtomicUsize::new(0) }; crate::hw::MAX_NUMA_NODES];
#[cfg(feature = "smp")]
/// Exclusive end of each node's group in the published queue (`start` is the previous
/// node's end, or 0).
static PLACE_GROUP_END: [AtomicUsize; crate::hw::MAX_NUMA_NODES] =
    [const { AtomicUsize::new(0) }; crate::hw::MAX_NUMA_NODES];
#[cfg(feature = "smp")]
/// Where each published slot came from in the caller's list, so results can be
/// reported in the caller's order after the queue was sorted by node.
static PLACE_ORIGIN: [AtomicUsize; MAX_PLACED_CELLS] =
    [const { AtomicUsize::new(0) }; MAX_PLACED_CELLS];
#[cfg(feature = "smp")]
/// Claims served by the claiming core's own node, and claims that had to cross.
///
/// Counted, not assumed: a core that runs dry locally **must** take remote work -
/// leaving a core idle beside runnable cells is worse than a remote access - so the
/// question is never "did it stay local" but "how often could it not", and that is
/// only answerable if the crossing is recorded (docs/ENGINEERING.md 1).
static CLAIMS_LOCAL: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static CLAIMS_REMOTE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Crossings that **did not have to happen**: a core took work from another node's
/// group while its *own* group still had an unclaimed slot.
///
/// This is the invariant that separates "the preference is applied" from "the
/// distribution happened to look local". A local/remote ratio cannot: with cells
/// round-robin across two nodes and cores split evenly, *random* claiming already lands
/// ~half the cells locally, and a threshold above that is a guess that turns into
/// flakiness. This is exact and zero-tolerance - by construction a core reaches another
/// group only after its own returned nothing, so a nonzero value here means the
/// preference was not applied at all. Measured: 0 with the preference, positive without
/// (docs/SUBSTRATE.md pillar 6).
///
/// A group only ever shrinks, so "my own group was exhausted" cannot become false
/// later; there is no race to lose here.
static CLAIMS_AVOIDABLE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
/// Whether the claim path prefers a core's own NUMA node. On by default; switchable so
/// a proof can run the *same* placement round both ways in one binary and compare,
/// which is the `preempt` kernel's technique for the same problem - a distribution can
/// only be shown to be a consequence of the preference by measuring it without.
static NODE_AFFINITY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

#[cfg(feature = "smp")]
/// Turn the own-node claim preference on or off (docs/SUBSTRATE.md pillar 6). For
/// proofs; a system boot leaves it on.
pub fn set_node_affinity(on: bool) {
    NODE_AFFINITY.store(on, Ordering::Release);
}

#[cfg(feature = "smp")]
/// (claims served by the claiming core's own NUMA node, claims that crossed nodes).
pub fn node_claims() -> (usize, usize) {
    (
        CLAIMS_LOCAL.load(Ordering::Acquire),
        CLAIMS_REMOTE.load(Ordering::Acquire),
    )
}

#[cfg(feature = "smp")]
/// Crossings that did not have to happen. See [`CLAIMS_AVOIDABLE`]; must be zero.
pub fn avoidable_crossings() -> usize {
    CLAIMS_AVOIDABLE.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// The NUMA node this core sits on, or `None` where the machine reports one node (or
/// does not report this core) - in which case there is no preference to express and
/// the claim path takes the single group.
fn this_node() -> Option<u8> {
    if crate::mm::frames::nodes_known() < 2 {
        return None;
    }
    // The registry already holds each core's hardware id - every core stored its own
    // in `set_online`, from a register it read itself. No new arch accessor needed,
    // and no core is ever *told* which node it is on.
    crate::hw::inventory().cpu_node(this_cpu().hw_id())
}

#[cfg(feature = "smp")]
/// Record whether `cell` was claimed by a core on the cell's own NUMA node.
///
/// Called where the **cell** is known rather than inside the cursor, so the *steal*
/// path is counted too - a steal is a claim, and leaving it out would make the counters
/// disagree with the observed cell-to-core mapping, which is exactly the check that
/// makes them worth having.
fn count_claim(cell: usize) {
    let Some(mine) = this_node() else {
        return;
    };
    if crate::user::cell_node(cell) == mine {
        CLAIMS_LOCAL.fetch_add(1, Ordering::AcqRel);
    } else {
        CLAIMS_REMOTE.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(feature = "smp")]
/// Claim the next unclaimed slot for `cpu`, preferring this core's own NUMA node.
///
/// Returns the absolute slot index, or `None` when every group is exhausted. Own node
/// first, then the others in order - work-conserving, because an idle core beside a
/// runnable cell is a worse outcome than a remote memory access.
fn claim_next(n: usize) -> Option<usize> {
    // The *ordering* preference, which the toggle below can switch off so a proof can
    // measure the same round both ways in one binary (`set_node_affinity`).
    let mine = if NODE_AFFINITY.load(Ordering::Acquire) {
        this_node()
    } else {
        None
    };
    let groups = crate::mm::frames::nodes_known().max(1);
    // Own node first, then every other group. With one group `mine` is `None` and this
    // is one `fetch_add` on cursor 0 - exactly the pre-NUMA path.
    let first = mine.map(|m| m as usize).unwrap_or(0).min(groups - 1);
    for step in 0..groups {
        let g = if step == 0 {
            first
        } else {
            let cand = (first + step) % groups;
            if cand == first {
                continue;
            }
            cand
        };
        let start = if g == 0 {
            0
        } else {
            PLACE_GROUP_END[g - 1].load(Ordering::Acquire)
        };
        let end = PLACE_GROUP_END[g].load(Ordering::Acquire).min(n);
        if start >= end {
            continue;
        }
        let k = PLACE_NEXT_NODE[g].fetch_add(1, Ordering::AcqRel);
        // The cursor is monotonic and may run past the group; that is how exhaustion is
        // detected, and re-trying the same group would spin.
        if start + k >= end {
            continue;
        }
        // Was this a crossing, and did it have to be one?
        //
        // Judged from the **group actually taken** against `this_node()`, not from the
        // loop's `step`: `step` counts distance from `first`, and `first` comes from the
        // preference - so with the preference off every core's `first` is group 0 and a
        // node-1 core taking group 0 would look like `step == 0`, i.e. local. That is
        // how the second version of this check passed with the preference disabled
        // (docs/ENGINEERING.md 1: two vacuous proofs before this one).
        //
        // `this_node()` and not `mine`, for the same reason at one remove: the detector
        // must not share a binding with the thing it detects.
        if let Some(m) = this_node()
            .map(|m| (m as usize).min(groups - 1))
            .filter(|&m| m != g)
        {
            {
                let ms = if m == 0 {
                    0
                } else {
                    PLACE_GROUP_END[m - 1].load(Ordering::Acquire)
                };
                let me = PLACE_GROUP_END[m].load(Ordering::Acquire).min(n);
                // A group only shrinks, so "my own group still had room" cannot become
                // false later - there is no race to lose here.
                if ms + PLACE_NEXT_NODE[m].load(Ordering::Acquire) < me {
                    CLAIMS_AVOIDABLE.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
        return Some(start + k);
    }
    None
}

#[cfg(feature = "smp")]
/// How many runnable cells were rebalanced out of a busy core's claim in the last
/// round. Zero means claiming alone happened to divide the work evenly, which for an
/// uneven workload is evidence that the stealing is not working.
pub fn steals() -> usize {
    STEALS.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// How many cores are inside a drain. Zero means the round is genuinely over and its
/// cells can be torn down.
static BUSY: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// Decrements [`BUSY`] however `drain_cells` leaves - including the early returns.
struct Busy;
#[cfg(feature = "smp")]
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::AcqRel);
    }
}
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
/// How many cells a core claims at once **on a hybrid machine** (docs/SCHEDULING.md 12).
///
/// **One**, and that is a design statement rather than a test convenience. A batch is a core
/// holding work it has not started; on a hybrid machine some of that work may not suit the core's
/// tier, while a core that *does* suit it sits idle beside it. Claiming one at a time is what makes
/// the tier preference mean anything - a batch of two would let a fast core take a compute cell it
/// suits and a bursty cell it does not, in one indivisible step.
///
/// The cost is the one [`CLAIM_BATCH`]'s own note names: a core holding a single cell has nothing
/// to preempt *to*. That trade is the right way round here, because a mis-tiered cell runs slowly
/// for its whole life where a missed preemption costs one slice.
pub const CLAIM_BATCH_HYBRID: usize = 1;

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
    // **Mark this core busy for the whole round, not just for the cells it runs.**
    // `PLACE_DONE` says every cell finished; it does not say every core has stopped
    // *touching* them. The core that completed the last cell is still unwinding its
    // address space and its bookkeeping, and the primary - which returns the instant
    // the count is reached - would go on to `user::reset()` the cell table out from
    // under it. That is a use-after-free across cores, and it presented as kernel-mode
    // page faults on secondaries with no obvious connection to the phase that caused
    // them (docs/SMP.md 10.0).
    BUSY.fetch_add(1, Ordering::AcqRel);
    let _quiesce = Busy;
    let preemptive = PLACE_PREEMPT.load(Ordering::Acquire) == 1;
    if preemptive {
        enable_preemption_here();
    }
    loop {
        // Take a batch off the queue.
        let mut slot = [usize::MAX; CLAIM_BATCH];
        let mut cell = [usize::MAX; CLAIM_BATCH];
        // Whether this core has already set a slot's run-mark. A stolen slot is marked
        // by the steal itself (that exchange *is* the claim), so the run loop below
        // must not re-check it - it would find its own mark and drop the cell it just
        // took, which is a lost cell rather than a lost race.
        let mut marked = [false; CLAIM_BATCH];
        let mut got = 0;
        // One at a time on a hybrid machine - see `CLAIM_BATCH_HYBRID`.
        let batch = if crate::sched::hetero::is_hybrid() {
            CLAIM_BATCH_HYBRID
        } else {
            CLAIM_BATCH
        };
        while got < batch {
            // This core's own NUMA node first (docs/SUBSTRATE.md pillar 6), then any
            // other - work-conserving, since an idle core beside a runnable cell is a
            // worse outcome than a remote memory access. One `fetch_add` per group, so
            // exactly one core can obtain each slot exactly as before.
            // On a hybrid machine, work whose class suits this core's tier first
            // (docs/SCHEDULING.md 12). Inert on a uniform machine, where `is_hybrid()` is
            // false and this is one comparison before the pre-existing cursor.
            if let Some(k) = claim_matching_tier(cpu, n) {
                slot[got] = k;
                cell[got] = PLACE_CELLS.lock()[k];
                marked[got] = true;
                got += 1;
                continue;
            }
            let Some(k) = claim_next(n) else {
                break;
            };
            slot[got] = k;
            cell[got] = PLACE_CELLS.lock()[k];
            PLACE_OWNER[k].store(cpu, Ordering::Release);
            got += 1;
        }
        if got == 0 {
            // The shared cursor is empty, but a **peer may still be holding a cell it
            // has not started** - the second half of a batch it claimed while this core
            // was busy. Take it. This is the balancing that a claim alone cannot do:
            // claiming divides work by arrival, and once divided it stays divided, so a
            // core that drew a long cell and a short one finishes late while another
            // idles (docs/SMP.md 10.0).
            match steal(cpu, n) {
                Some(k) => {
                    slot[0] = k;
                    cell[0] = PLACE_CELLS.lock()[k];
                    marked[0] = true;
                    got = 1;
                    STEALS.fetch_add(1, Ordering::AcqRel);
                    // Measured, not prevented: a dry core takes what there is and the
                    // tier crossing is counted (docs/SCHEDULING.md 12).
                    crate::sched::hetero::steal_is_matched(cpu, class_of_slot(k));
                }
                None => return,
            }
        }
        // Run until every cell of the batch has exited. `run` returns the cell that
        // actually ended the run - under preemption that is whichever of the batch
        // finished first, not necessarily the one entered.
        let mut left = got;
        while left > 0 {
            let Some(pos) = (0..got).find(|&i| slot[i] != usize::MAX) else {
                break;
            };
            // Mark the cell **started**, which is what takes it out of reach of a
            // stealer. A slot already marked was taken by a peer that ran dry while
            // this core was inside an earlier cell of the batch: drop it and move on -
            // that is the balancing working, not an error.
            if !preemptive && !marked[pos] && PLACE_RUN[slot[pos]].swap(1, Ordering::AcqRel) == 1 {
                slot[pos] = usize::MAX;
                left -= 1;
                continue;
            }
            // Past the run-mark this core *will* run this cell and no peer can take it,
            // so this - not the claim - is where node locality is recorded. A claim can
            // be lost to a stealer, which would count a cell twice and make the
            // counters disagree with where the cell actually ran (observed: 9 claims
            // counted for 8 cells, one of them stolen).
            count_claim(crate::user::entity_pair(cell[pos]).map_or(0, |p| p.0));
            // **Stamp ownership here, not at claim time.** Past the run-mark this core
            // *will* run this vcore and no peer can take it; a batch-time stamp is a
            // claim a stealer can invalidate, and the previous owner has no way to learn
            // it lost one until it reaches the slot. That was harmless while entry was
            // gated by the run-mark alone, and became a real race once a *sibling-vcore
            // yield* started trusting the stamp: a core holding two vcores of one cell in
            // one batch would enter the sibling from inside the first, while the stealer
            // was already inside it. Caught by name by `user::double_entries`
            // (docs/SUBSTRATE.md pillar 3) - the same reasoning `count_claim` above
            // already carries for the counters.
            claim_vcore_id(cell[pos], cpu);
            if preemptive {
                // Under preemption *both* cells of a batch are live at once - the timer
                // enters the sibling without passing through this loop - so neither can
                // be stolen and both are marked here.
                for i in 0..got {
                    if slot[i] != usize::MAX {
                        PLACE_RUN[slot[i]].store(1, Ordering::Release);
                        // Both are live at once under preemption, so both must be owned
                        // before either runs - the timer enters the sibling without
                        // passing back through this loop.
                        claim_vcore_id(cell[i], cpu);
                    }
                }
            }
            // The caller's contract holds here: a present native cell this core owns
            // exclusively, because the `fetch_add` handed its index to nobody else.
            let (rc, rv) = crate::user::entity_pair(cell[pos]).expect("a claimed entity");
            let (ec, ev, outcome) = crate::user::run_vcore(rc, rv);
            let exited = crate::user::entity_of(ec, ev);
            let code = code_of(outcome);
            // Attribute the outcome to whichever vcore of the batch ended the run.
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
/// Give the context a queue entry names to CPU `cpu`.
fn claim_vcore_id(vid: usize, cpu: usize) {
    if let Some((c, v)) = crate::user::entity_pair(vid) {
        crate::user::claim_vcore(c, v, cpu);
    }
}

#[cfg(feature = "smp")]
/// Claim an **unclaimed** slot whose work suits this core's tier, on a hybrid machine
/// (docs/SCHEDULING.md 12).
///
/// A scan rather than a cursor, because a preference cannot be expressed as a monotonic counter -
/// and the safety comes from the same place [`steal`]'s does: the `PLACE_RUN` exchange. Exactly
/// one core can turn a slot's run-mark 0 -> 1, and only that core may enter the cell, so the scan
/// cannot hand one slot to two cores however many of them are scanning.
///
/// It claims **unclaimed** work only (`PLACE_OWNER` unset). Taking work a peer has already claimed
/// is a *steal*, which has its own path and its own counter; conflating the two would report a
/// preference as a rebalance.
///
/// The slot is returned already run-marked, so the caller must record it as marked - exactly as it
/// does for a steal. The cursor may later hand the same slot to another core, which finds the mark
/// set and drops it; that is the pre-existing "a peer took it" path, so nothing is stranded and
/// work conservation is unchanged.
///
/// `None` when nothing unclaimed matches, and the caller then falls through to the ordinary
/// cursor - so a core never idles beside work of the wrong tier.
fn claim_matching_tier(cpu: usize, n: usize) -> Option<usize> {
    use crate::sched::hetero;
    if !hetero::is_hybrid() {
        return None;
    }
    for k in 0..n.min(MAX_PLACED_CELLS) {
        if PLACE_OWNER[k].load(Ordering::Acquire) != usize::MAX {
            continue;
        }
        let want = class_of_slot(k);
        if !hetero::tier_suits(cpu, want) {
            continue;
        }
        if PLACE_RUN[k]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            PLACE_OWNER[k].store(cpu, Ordering::Release);
            TIER_CLAIMS.fetch_add(1, Ordering::AcqRel);
            return Some(k);
        }
    }
    None
}

#[cfg(feature = "smp")]
/// The class of the work published at slot `k`.
///
/// **Indexed through `PLACE_ORIGIN`, not by the slot.** The queue is republished *grouped by home
/// node* (docs/SUBSTRATE.md pillar 6), so slot `k` is not the caller's cell `k` - and reading
/// `PLACE_CLASS[k]` directly gives another cell's class. That was a real defect and it presented
/// exactly as a broken preference: a compute cell placed on an efficiency core with the mechanism
/// working perfectly, because it had been told the wrong thing about the cell.
///
/// **Honest about the proof**: it was found by the phase failing on its first run, and a
/// re-inserted control does *not* reliably fire - whether slot order differs from caller order
/// depends on the home nodes the four cells happen to draw, and when they all land on one node the
/// grouping is the identity and reading by slot is accidentally right. So the fix is proven by the
/// observation that produced it, not by a control the phase can reproduce on demand.
fn class_of_slot(k: usize) -> crate::sched::hetero::ThreadClass {
    use crate::sched::hetero::ThreadClass;
    let origin = PLACE_ORIGIN[k]
        .load(Ordering::Acquire)
        .min(MAX_PLACED_CELLS - 1);
    match PLACE_CLASS[origin].load(Ordering::Acquire) {
        1 => ThreadClass::Compute,
        2 => ThreadClass::Bursty,
        _ => ThreadClass::Unknown,
    }
}

#[cfg(feature = "smp")]
/// Claims made through the tier preference. Zero on a uniform machine, which is every machine
/// QEMU models - the preference is gated on `hetero::is_hybrid()`.
static TIER_CLAIMS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "smp")]
/// How many claims were made because the work's class suited the claiming core's tier.
pub fn tier_claims() -> usize {
    TIER_CLAIMS.load(Ordering::Acquire)
}

#[cfg(feature = "smp")]
/// Take one **claimed but not yet started** cell away from whichever peer holds it.
///
/// The exchange on `PLACE_RUN[k]` is the whole protocol: exactly one core can turn a
/// slot from 0 to 1, and only the core that did may run it. The owner discovers the
/// loss when it reaches that slot and finds the mark already set - no message, no
/// lock, and no window in which two cores could both enter the cell.
///
/// A cell that is already *running* is deliberately not stealable. Migrating one means
/// moving a live trap frame, an FP save area and an address space between cores while
/// the cell is mid-instruction; that is a different capability and is named as not
/// done (docs/SMP.md 10.0), where this one is just "the work had not begun yet".
fn steal(cpu: usize, n: usize) -> Option<usize> {
    for k in 0..n.min(MAX_PLACED_CELLS) {
        let owner = PLACE_OWNER[k].load(Ordering::Acquire);
        if owner == usize::MAX || owner == cpu {
            continue;
        }
        if PLACE_RUN[k]
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            PLACE_OWNER[k].store(cpu, Ordering::Release);
            return Some(k);
        }
    }
    None
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
/// Each entry installed, present, listed once, and either **native** or a **Linux cell
/// with no process tree**. The Linux case is admissible because such a cell's exit
/// reaches `linux::proc`, which with no children ends the run exactly as a native cell's
/// does; its global personality state is serialised by `linux::plock`. A Linux cell that
/// forks, pipes or signals across cores is a different question and is not covered here
/// (docs/SMP.md 10.2).
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
/// [`place_cells`], with a [`crate::sched::hetero::ThreadClass`] per cell so the claim can prefer
/// a core whose tier suits the work (docs/SCHEDULING.md 12).
///
/// On a uniform machine - every machine QEMU models - this is byte-for-byte [`place_cells`]: the
/// preference is gated on `hetero::is_hybrid()`. The classes are published for the round and
/// cleared by the next one.
///
/// # Safety
/// As [`place_cells`].
pub unsafe fn place_cells_classed(
    cells: &[usize],
    classes: &[crate::sched::hetero::ThreadClass],
    out: &mut [(u64, usize)],
) -> bool {
    use crate::sched::hetero::ThreadClass;
    for (k, slot) in PLACE_CLASS.iter().enumerate() {
        let c = classes.get(k).copied().unwrap_or(ThreadClass::Unknown);
        slot.store(
            match c {
                ThreadClass::Compute => 1,
                ThreadClass::Bursty => 2,
                ThreadClass::Unknown => 0,
            },
            Ordering::Release,
        );
    }
    // SAFETY: the caller's contract.
    let r = unsafe { place_cells_inner(cells, out, false) };
    for slot in PLACE_CLASS.iter() {
        slot.store(0, Ordering::Release);
    }
    r
}

#[cfg(feature = "smp")]
/// # Safety
/// As [`place_cells`].
unsafe fn place_cells_inner(cells: &[usize], out: &mut [(u64, usize)], preempt: bool) -> bool {
    // Publishing a cell is publishing its vcore 0 - one queue, one drain loop
    // (docs/SUBSTRATE.md pillar 3). The queue carries **entity ids**: it used to carry
    // `cell * MAX_VCORES + vcore`, a second copy of the id arithmetic `user::entity_of`
    // also had, which is the drift that derivation was supposed to prevent
    // (docs/EXECUTION-MODEL.md 9.4).
    let mut vids = [0usize; MAX_PLACED_CELLS];
    let k = cells.len().min(MAX_PLACED_CELLS);
    for (d, &c) in vids.iter_mut().zip(cells.iter()).take(k) {
        *d = crate::user::entity_of(c, 0);
    }
    // SAFETY: the caller's contract.
    unsafe { place_vcores_inner(&vids[..k], out, preempt) }
}

#[cfg(feature = "smp")]
/// [`place_cells`], over **entity ids** rather than cells.
///
/// This is what lets one cell occupy several cores: publish two vcores of the same cell
/// and each is claimed, run and reported independently, because the frame, the kernel
/// stack, the FP area and the ownership claim are all per vcore.
///
/// # Safety
/// As [`place_cells`], per vcore: each `(cell, vcore)` installed, present, native, and
/// listed once.
pub unsafe fn place_vcores(vids: &[usize], out: &mut [(u64, usize)]) -> bool {
    // SAFETY: the caller's contract.
    unsafe { place_vcores_inner(vids, out, false) }
}

#[cfg(feature = "smp")]
/// # Safety
/// As [`place_vcores`].
unsafe fn place_vcores_inner(cells: &[usize], out: &mut [(u64, usize)], preempt: bool) -> bool {
    let n = cells.len().min(MAX_PLACED_CELLS).min(out.len());
    // **Publish the queue grouped by each cell's home node** (docs/SUBSTRATE.md pillar
    // 6), so a core taking work from its own node's cursor gets cells whose pages are
    // on its own memory controller. The caller's order is kept in `PLACE_ORIGIN` and
    // restored when results are reported, so grouping is invisible above this seam.
    //
    // With fewer than two nodes every cell falls in group 0 and the order is the
    // caller's, which is the pre-NUMA behaviour exactly.
    let groups = crate::mm::frames::nodes_known().max(1);
    {
        let mut q = PLACE_CELLS.lock();
        let mut w = 0usize;
        for (g, end) in PLACE_GROUP_END.iter().enumerate().take(groups) {
            for (i, &c) in cells[..n].iter().enumerate() {
                let node = crate::user::cell_node(crate::user::entity_pair(c).map_or(0, |p| p.0));
                // A cell with no node - every cell on a single-node machine - belongs
                // to group 0, the only group there is.
                let cg = if (node as usize) < groups {
                    node as usize
                } else {
                    0
                };
                if cg == g {
                    q[w] = c;
                    PLACE_ORIGIN[w].store(i, Ordering::Release);
                    w += 1;
                }
            }
            end.store(w, Ordering::Release);
        }
        debug_assert_eq!(w, n, "every published cell must land in exactly one group");
        for e in PLACE_GROUP_END.iter().skip(groups) {
            e.store(w, Ordering::Release);
        }
    }
    for c in PLACE_NEXT_NODE.iter() {
        c.store(0, Ordering::Release);
    }
    CLAIMS_LOCAL.store(0, Ordering::Release);
    CLAIMS_REMOTE.store(0, Ordering::Release);
    CLAIMS_AVOIDABLE.store(0, Ordering::Release);
    for k in 0..MAX_PLACED_CELLS {
        PLACE_CODE[k].store(usize::MAX, Ordering::Release);
        PLACE_CPU[k].store(usize::MAX, Ordering::Release);
        PLACE_OWNER[k].store(usize::MAX, Ordering::Release);
        PLACE_RUN[k].store(0, Ordering::Release);
    }
    STEALS.store(0, Ordering::Release);
    TIER_CLAIMS.store(0, Ordering::Release);
    for c in 0..MAX_CPUS {
        // SAFETY: between rounds; no core is draining.
        unsafe { PLACE_TAKEN.get(c) }.store(0, Ordering::Release);
    }
    PLACE_DONE.store(0, Ordering::Release);
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
            let q = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
            while BUSY.load(Ordering::Acquire) > 0 && arch::timer_now_ns() < q {
                core::hint::spin_loop();
            }
            return false;
        }
        core::hint::spin_loop();
    }
    PLACE_COUNT.store(0, Ordering::Release);
    // Wait for every core to leave its drain before reporting the round finished: the
    // caller's next act is usually to tear these cells down.
    let quiesce = arch::timer_now_ns().wrapping_add(RV_TIMEOUT_NS);
    while BUSY.load(Ordering::Acquire) > 0 && arch::timer_now_ns() < quiesce {
        core::hint::spin_loop();
    }
    // Back into the caller's order: `out[i]` is about `cells[i]`, whatever group the
    // cell was published in.
    for k in 0..n {
        let i = PLACE_ORIGIN[k].load(Ordering::Acquire);
        if let Some(slot) = out.get_mut(i) {
            *slot = (
                PLACE_CODE[k].load(Ordering::Acquire) as u64,
                PLACE_CPU[k].load(Ordering::Acquire),
            );
        }
    }
    true
}

#[cfg(feature = "smp")]
/// The hardware id CPU registry slot `cpu` recorded for itself at bring-up.
///
/// Each core read this from a register and stored it in `set_online`; nothing tells a
/// core its identity. Exposed so a proof can map "the CPU that ran this cell" back to
/// the inventory - which keys CPUs by hardware id, since a registry index is an
/// artefact of the order bring-up claimed them in.
pub fn cpu_hw_id_of(cpu: usize) -> u32 {
    if cpu >= MAX_CPUS {
        return u32::MAX;
    }
    CPUS[cpu].hw_id()
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
    // Every core that came up has classified itself by now, so the graph can learn what the
    // boot CPU could not read about its siblings (docs/RESOURCE-GRAPH.md 2.4b). One writer,
    // here, with every secondary parked in its work loop.
    crate::hw::graph_build::refresh_cpu_classes(crate::hw::inventory());
    crate::sched::hetero::load_from_inventory(crate::hw::inventory());
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
