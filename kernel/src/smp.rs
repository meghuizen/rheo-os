//! SMP: per-CPU state, a kernel spinlock, and secondary-core bring-up
//! (docs/SMP.md, long-standing task #27).
//!
//! This module is **portable** - no `cfg(target_arch)` here. The per-ISA bits
//! (the secondary entry trampoline, the SBI/PSCI bring-up call, and the per-CPU
//! identity register) live in `kernel/src/arch/`. What is portable: the
//! `SpinLock<T>` mutual-exclusion primitive, a fixed per-CPU registry indexed by
//! CPU index, and the driver that asks the arch layer to start one secondary and
//! waits (bounded) for it to run kernel code.
//!
//! **Zero-impact on the single-CPU path.** Nothing here runs unless a kernel
//! opts in by calling [`init`] then [`bring_up_one`] (only the `smp` test does).
//! `cpu_index()` defaults to 0, so `this_cpu()` returns CPU 0 for any code that
//! never brought a secondary up - no behaviour or timing change for the 31
//! existing kernels.
//!
//! Honest per-ISA status (docs/SMP.md): the RISC-V secondary hart genuinely runs
//! kernel code (SBI HSM `hart_start`); ARM64 issues a real PSCI `CPU_ON` and
//! reports the observed blocker; x86-64 reports the real-mode-AP-trampoline gap.
//! A blocked ISA keeps single-core boot working and skips-with-reason.

use crate::arch;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Registry capacity: matches the machine inventory's CPU ceiling.
pub const MAX_CPUS: usize = crate::hw::MAX_CPUS;

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

/// Per-CPU kernel state. Kept minimal for the bring-up proof: the hardware CPU
/// id (hart id / MPIDR affinity / APIC id) and an online flag. A real SMP kernel
/// grows this block (per-CPU run queue, current cell, timer state); the point
/// here is that it is indexed by CPU index and reachable via [`this_cpu`].
pub struct PerCpu {
    hw_id: AtomicU32,
    online: AtomicBool,
}

impl PerCpu {
    const fn new() -> PerCpu {
        PerCpu {
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

static CPUS: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// The per-CPU block for CPU index `i`.
pub fn cpu(i: usize) -> &'static PerCpu {
    &CPUS[i]
}

/// The per-CPU block for the CPU this call runs on. Defaults to CPU 0 on the
/// single-CPU path (`arch::cpu_index()` returns 0 until a secondary establishes
/// its own identity), so callers that never opt into SMP always see CPU 0.
pub fn this_cpu() -> &'static PerCpu {
    &CPUS[arch::cpu_index()]
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

/// A shared counter guarded by the [`SpinLock`], written by a secondary core and
/// read back by the primary - the observable cross-core proof that the lock and
/// shared memory work between cores.
static SHARED: SpinLock<u64> = SpinLock::new(0);

/// Bumped (release) by a secondary once it has done its work, so the primary can
/// wait on it without holding the lock (no hold-and-wait deadlock under QEMU's
/// round-robin TCG).
static SECONDARY_UP: AtomicUsize = AtomicUsize::new(0);

/// The value a secondary adds to the shared counter, so the primary can assert
/// the exact result (a fixed magic, not 1, to catch a stuck/garbage write).
pub const SECONDARY_MARK: u64 = 0x5EC0;

/// Iterations each core performs in the two-core lock-contention proof.
pub const CONTENTION_ITERS: u64 = 20_000;

/// A counter the primary and a secondary increment **concurrently**, each under
/// the [`SpinLock`]. This is the proof the lock provides genuine mutual exclusion
/// across cores, not merely that a single cross-core write lands: a lock that did
/// not actually serialise the read-modify-write would lose updates and the final
/// value would fall short of `CONTENTION_ITERS * 2`. The single-write
/// [`SHARED`]/[`SECONDARY_MARK`] proof passes even with a broken lock (there is no
/// concurrent writer), which is exactly the gap this closes.
static CONTENDED: SpinLock<u64> = SpinLock::new(0);

/// A two-core rendezvous so both cores are inside their increment loops at the
/// same time - genuine contention, not two sequential runs. Best-effort: a core
/// whose peer is slow proceeds alone after the bounded wait, which still yields
/// the exact sum (the correctness oracle) but weaker overlap.
static CONTEND_READY: AtomicUsize = AtomicUsize::new(0);

/// Run this core's half of the contention proof: rendezvous with the peer, then
/// take the shared lock `CONTENTION_ITERS` times, adding 1 each time. Called from
/// both the primary ([`bring_up_one`]) and the secondary ([`secondary_run`]).
pub fn contend() {
    CONTEND_READY.fetch_add(1, Ordering::AcqRel);
    let mut budget = WAIT_BUDGET;
    while CONTEND_READY.load(Ordering::Acquire) < 2 && budget > 0 {
        core::hint::spin_loop();
        budget -= 1;
    }
    for _ in 0..CONTENTION_ITERS {
        let mut g = CONTENDED.lock();
        *g += 1;
    }
}

/// The contended counter (under the lock) - `CONTENTION_ITERS * 2` once both
/// cores have run [`contend`].
pub fn contended_value() -> u64 {
    *CONTENDED.lock()
}

/// Iterations of `alloc`+`free` each core performs in the frame-allocator
/// contention proof (docs/SMP.md 10, task #132). Smaller than [`CONTENTION_ITERS`]
/// because each iteration also zeroes a 4 KiB frame under the allocator's lock.
pub const FRAME_CONTENTION_ITERS: u64 = 4_000;

/// A separate two-core rendezvous for the frame-allocator phase, so it cannot
/// interfere with the [`CONTEND_READY`] barrier of the counter phase.
static FRAME_READY: AtomicUsize = AtomicUsize::new(0);

/// Both cores allocate a frame and immediately free it, `FRAME_CONTENTION_ITERS`
/// times, **concurrently** - the proof that [`crate::mm::frames`] is SMP-safe
/// (task #132). The frame allocator's internal lock must serialise the bitmap
/// read-modify-write: without it, two cores could observe the same bit clear and
/// both claim it (one frame handed out twice), lose a `USED` increment (the count
/// and the bitmap then disagree), or trip the double-free assertion when both free
/// the frame they both think they own. Each iteration is net-zero (alloc then
/// free), so the free-frame count returns to its baseline; the primary checks that
/// **and** `frames::used_matches_bitmap()` after the window (the test's oracle). A
/// broken lock shows up as a failed invariant or a double-free panic - either way
/// the test fails.
pub fn contend_frames() {
    FRAME_READY.fetch_add(1, Ordering::AcqRel);
    let mut budget = WAIT_BUDGET;
    while FRAME_READY.load(Ordering::Acquire) < 2 && budget > 0 {
        core::hint::spin_loop();
        budget -= 1;
    }
    for _ in 0..FRAME_CONTENTION_ITERS {
        if let Some(pa) = crate::mm::frames::alloc() {
            crate::mm::frames::free(pa);
        }
    }
}

/// The next free registry index to hand a secondary. The boot CPU is index 0
/// ([`init`]); secondaries take 1, 2, ... as they come up. This keeps the
/// registry index independent of the hardware CPU id (the boot hart id may be
/// nonzero - QEMU's RISC-V boot hart is often not hart 0).
static NEXT_INDEX: AtomicUsize = AtomicUsize::new(1);

/// Portable entry the arch secondary trampoline calls once it is running kernel
/// code on the shared address space. It claims a registry index, establishes its
/// per-CPU identity, records itself online, exercises the spinlock on shared
/// memory, then signals the primary. Runs on the **secondary** core.
pub fn secondary_run(hw_id: u32) {
    let idx = NEXT_INDEX.fetch_add(1, Ordering::AcqRel);
    // Establish this CPU's identity so this_cpu() resolves to its own block.
    arch::smp_set_this_cpu(idx);
    set_online(idx, hw_id);
    // this_cpu() must resolve to *this* secondary's block now that its identity
    // is set - a check that per-CPU addressing works off the boot core.
    debug_assert!(this_cpu().is_online());
    // The **first** secondary (registry index 1) runs the single-write and the
    // two-core contention proofs against the primary. Additional secondaries
    // (start-all, docs/SMP.md 10) only prove they come online with a distinct
    // identity - they do NOT touch SHARED/CONTENDED, so the primary's exact
    // `SECONDARY_MARK` and `CONTENTION_ITERS * 2` assertions stay true regardless
    // of how many cores are brought up. (A concurrent N-way contention proof needs
    // simultaneous bring-up + an IPI to re-summon parked cores; that is later.)
    if idx == 1 {
        {
            let mut g = SHARED.lock();
            *g += SECONDARY_MARK;
        }
        // Genuine cross-core contention: hammer a shared counter under the lock
        // while the primary does the same, proving mutual exclusion (not just a
        // single write). Runs before the completion signal so the primary's wait
        // on `SECONDARY_UP` implies this secondary finished contending.
        contend();
        // The same, but hammering the real frame allocator (task #132): both cores
        // alloc+free concurrently, proving `mm::frames` serialises its bitmap. Also
        // before the completion signal, so the primary's balance/invariant check
        // after the wait sees a finished phase.
        contend_frames();
    }
    SECONDARY_UP.fetch_add(1, Ordering::Release);
}

/// Read the shared counter (under the lock) - the primary's view of the
/// secondary's write.
pub fn shared_value() -> u64 {
    *SHARED.lock()
}

/// How many secondaries have signalled completion.
pub fn secondaries_up() -> usize {
    SECONDARY_UP.load(Ordering::Acquire)
}

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

/// Bounded spin budget for the primary waiting on a secondary to come online.
/// Large enough that QEMU's round-robin TCG schedules the secondary; on timeout
/// the caller skips-with-reason rather than hanging.
const WAIT_BUDGET: u64 = 200_000_000;

/// Initialise SMP on the **primary** CPU: establish CPU 0's identity and mark it
/// online. Idempotent-ish; call once before [`bring_up_one`].
pub fn init() {
    arch::smp_set_this_cpu(0);
    set_online(0, arch::boot_cpu_hw_id());
}

/// How many secondaries the current ISA brings up in the start-all proof (RISC-V:
/// two; ARM64/x86-64: one, docs/SMP.md 10). The portable [`bring_up_all`] loop and
/// the test read this so they stay ISA-agnostic.
pub fn secondary_count() -> usize {
    arch::smp_secondary_count()
}

/// Ask the arch layer to start the **first** secondary core (ordinal 0) and wait
/// (bounded) for it to run kernel code. This is the core that runs the two-core
/// contention proof against the primary. Returns the CPU index that came online,
/// or a [`StartError`] the caller turns into a skip-with-reason.
pub fn bring_up_one() -> Result<usize, StartError> {
    bring_up_nth(0)
}

/// Bring up secondary number `ordinal` (0-based) and wait (bounded) for it to come
/// online. `ordinal 0` is the first secondary (it runs the contention proof);
/// `ordinal >= 1` are additional cores that only prove they run with a distinct
/// identity (start-all, docs/SMP.md 10). Bring-up is **sequential**: the caller
/// brings up ordinal N and waits for it online before ordinal N+1, so the per-CPU
/// stack hand-off (`arch::smp_prepare_secondary`) has no race.
pub fn bring_up_nth(ordinal: usize) -> Result<usize, StartError> {
    let inv = crate::hw::inventory();
    let boot = arch::boot_cpu_hw_id();

    // Pick the `ordinal`-th firmware-enumerated non-boot CPU. The synthesized
    // fallback (boot + 1 + ordinal) lets an ISA whose firmware cannot enumerate
    // secondaries from the kernel's exception level (ARM64: PSCI enumeration needs
    // EL3, so only the boot CPU is in the inventory) still make a *genuine*
    // attempt and report the observed blocker, rather than doing nothing.
    let mut seen = 0usize;
    let mut target = None;
    for i in 0..inv.ncpus {
        if inv.cpus[i].hw_id != boot {
            if seen == ordinal {
                target = Some(inv.cpus[i].hw_id);
                break;
            }
            seen += 1;
        }
    }
    let target_hw = target.unwrap_or(boot + 1 + ordinal as u32);

    // Hand this secondary its own stack before releasing it (sequential bring-up,
    // so the shared `secondary_sp` word is written and consumed without a race).
    arch::smp_prepare_secondary(ordinal);

    let before = secondaries_up();
    match arch::smp_start_secondary(target_hw) {
        Ok(()) => {
            // The primary's half of the two-core contention proof runs **only for
            // the first secondary** (ordinal 0), in the window between the start
            // and the completion wait - the only window in which that secondary is
            // also in its own `contend` loop. Both rendezvous inside `contend`, so
            // the lock is genuinely contended by two cores at once (docs/SMP.md 10).
            if ordinal == 0 {
                contend();
                contend_frames();
            }
            let mut budget = WAIT_BUDGET;
            while budget > 0 {
                if secondaries_up() > before {
                    // The ordinal-th secondary claims registry index `ordinal + 1`
                    // (NEXT_INDEX increments 1,2,... in sequential bring-up order).
                    let slot = ordinal + 1;
                    if slot < MAX_CPUS && cpu(slot).is_online() {
                        return Ok(slot);
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
