//! **Heterogeneous cores**: placing work on a machine whose CPUs are not equally fast
//! (docs/RESOURCE-GRAPH.md 2.4b, docs/SCHEDULING.md 12).
//!
//! # What a P-core and an E-core actually differ in
//!
//! Not the instruction set. Both execute the same x86-64 base, the same SSE through AVX2, the
//! same AES-NI, SHA, BMI, FMA and virtualization extensions; AMD's Zen 5 and Zen 5c are closer
//! still, differing mainly in cache size and frequency. They present the same architectural
//! state, so a thread migrates between them with an ordinary context switch - registers, program
//! counter, vector state saved and restored exactly as for any other switch - with no
//! recompilation and no emulation. **They differ in how fast and how efficiently they execute
//! the same code**, which is why everything here is about *capacity* and nothing here is about
//! capability.
//!
//! The one historical exception is the rule's proof. Early Alder Lake had AVX-512 on the P-cores
//! only, and Intel disabled it **chip-wide** rather than ship a machine where a running thread
//! could not be moved. So a feature present on some cores and not others is a *correctness*
//! constraint, not a placement hint, and it belongs in the per-CPU `IsaSet` where placement can
//! be restricted to the cores that have it - or the feature is not advertised at all. This module
//! deliberately says nothing about features.
//!
//! # What replaces Intel Thread Director when it is absent
//!
//! Thread Director is hardware that watches a running thread - IPC, cache misses, memory stalls,
//! vector-instruction mix, spin behaviour - and hands the OS a per-thread class hint. Where it
//! exists, it is the better source and this module defers to it ([`ClassHint::source`]).
//!
//! Where it does not, the substitute is not a heuristic: **this kernel already measures the
//! behavioural half exactly**. Every relinquish here is an explicit, counted transition through
//! a named call (`sched::bore`), so "how long does this entity run before it voluntarily gives
//! the CPU up" is an *observation* rather than the inference Linux's CFS had to make. That is
//! precisely the signal Thread Director's `compute intensive` versus `mostly sleeping` hints
//! carry. What is genuinely missing without the hardware is the *microarchitectural* half - IPC,
//! stalls, vector mix - which needs a PMU, is not modelled by any emulator here, and is named as
//! absent rather than approximated.
//!
//! # Uniform machines must be unaffected
//!
//! Every decision below reduces to the pre-existing one when the machine has one kind of core:
//! [`pick_cpu`] returns the lowest-numbered idle CPU, exactly as a capacity-unaware picker does,
//! and [`steal_is_matched`] is always true. That is checked rather than asserted in prose - it is
//! what lets this land without changing any existing boot.

use crate::hw::graph::{CAPACITY_FULL, CoreClass};
use crate::smp::MAX_CPUS;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicUsize, Ordering};

/// Per-CPU capacity, 0 = this CPU is not present.
static CAPACITY: [AtomicU16; MAX_CPUS] = [const { AtomicU16::new(0) }; MAX_CPUS];
/// Per-CPU class, as [`CoreClass`]'s discriminant.
static CLASS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
/// True when the loaded table holds more than one (class, capacity) pair.
static HYBRID: AtomicBool = AtomicBool::new(false);
/// Placements made through [`pick_cpu`], and how many landed on a CPU whose tier does not
/// match the work's class.
static PLACEMENTS: AtomicUsize = AtomicUsize::new(0);
static MISPLACED: AtomicUsize = AtomicUsize::new(0);
/// Steals that took work to a CPU of the wrong tier for it. Counted, never prevented - work
/// conservation wins and the crossing is reported, the rule the locality work already holds
/// (docs/RESOURCE-GRAPH.md 6.2).
static MISMATCHED_STEALS: AtomicUsize = AtomicUsize::new(0);

/// What kind of work an entity is, for the purpose of choosing a core.
///
/// Three values rather than Thread Director's four, because these are the ones this kernel can
/// *observe*: the hardware's classes separate vector from scalar work, which needs a PMU.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ThreadClass {
    /// Never relinquished voluntarily and has not yet run long enough to judge - a freshly
    /// created entity.
    ///
    /// Treated like [`ThreadClass::Compute`] for placement, deliberately: an unknown demand put
    /// on a fast core and later found modest costs a little energy, where an unknown demand put
    /// on a slow core and later found heavy costs a migration *and* the time already lost.
    #[default]
    Unknown,
    /// Runs for a long time before giving the CPU up. Wants throughput.
    Compute,
    /// Relinquishes often after short runs - an event loop, a shell, an I/O-bound strand. Its
    /// latency comes from being *dispatched* promptly, which the burst-weighted virtual deadline
    /// already gives it (a low burst score is a high weight is an earlier deadline), so it does
    /// not need the fastest core and can leave it for work that does.
    Bursty,
}

/// Where a class hint came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HintSource {
    /// Derived from this kernel's own relinquish accounting (`sched::bore`).
    Observed,
    /// Read from Intel Thread Director's per-thread feedback.
    ThreadDirector,
}

/// A class and where it came from, so a consumer can tell a hardware hint from our own
/// measurement and a test can assert which one was used.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ClassHint {
    pub class: ThreadClass,
    pub source: HintSource,
}

/// The burst score at or above which an entity counts as compute-bound.
///
/// `sched::bore`'s score is `bitlen(burst_ns >> 26) * 1280 / 1024` clamped to 39, so a score of
/// 8 is reached by a burst of roughly 2^32 ns - about 4 seconds of unbroken CPU on the tick
/// domain the score is fed - and any entity that gets there is not an event loop. Stated as
/// arithmetic rather than tuned by feel, which is the only way a threshold in a scheduler can be
/// defended.
pub const COMPUTE_SCORE: u8 = 8;

/// Classify an entity from its **observed** burst behaviour.
///
/// Two questions, in this order:
///
/// 1. Has it ever voluntarily relinquished? If not, nothing has been observed about how it waits,
///    so the answer is [`ThreadClass::Unknown`] however long it has run - a long first burst is
///    what *every* program's startup looks like.
/// 2. Is its score at or above [`COMPUTE_SCORE`]? The score already folds the in-flight burst
///    together with the smoothed history, so a thing that is *currently* running long is
///    classified as compute immediately rather than only after it finally yields.
pub fn classify(burst: &super::bore::Burst) -> ClassHint {
    let class = if burst.yields() == 0 {
        ThreadClass::Unknown
    } else if burst.score() >= COMPUTE_SCORE {
        ThreadClass::Compute
    } else {
        ThreadClass::Bursty
    };
    ClassHint {
        class,
        source: HintSource::Observed,
    }
}

/// Load the per-CPU capacity table from the machine inventory.
///
/// Called after bring-up (when every core has classified itself - only the calling core can read
/// its own class) and again whenever a caller declares an asymmetry, so the scheduler's table and
/// the inventory cannot drift apart.
pub fn load_from_inventory(inv: &crate::hw::Inventory) {
    let mut seen: Option<(u8, u16)> = None;
    let mut hybrid = false;
    for (i, c) in inv.cpus[..inv.ncpus.min(MAX_CPUS)].iter().enumerate() {
        let cap = if c.capacity == 0 {
            CAPACITY_FULL
        } else {
            c.capacity
        };
        CAPACITY[i].store(cap, Ordering::Release);
        CLASS[i].store(c.class as u8, Ordering::Release);
        match seen {
            None => seen = Some((c.class as u8, cap)),
            Some(first) if first != (c.class as u8, cap) => hybrid = true,
            Some(_) => {}
        }
    }
    for slot in CAPACITY.iter().skip(inv.ncpus.min(MAX_CPUS)) {
        slot.store(0, Ordering::Release);
    }
    HYBRID.store(hybrid, Ordering::Release);
}

/// True when this host holds cores of more than one class or capacity.
///
/// **The gate on every capacity-aware decision.** A machine with one kind of core must behave
/// exactly as it did before this module existed, and asking this first is how that is enforced
/// rather than hoped for.
pub fn is_hybrid() -> bool {
    HYBRID.load(Ordering::Acquire)
}

/// CPU `cpu`'s capacity out of [`CAPACITY_FULL`], or 0 if it is not present.
pub fn capacity_of(cpu: usize) -> u16 {
    if cpu >= MAX_CPUS {
        return 0;
    }
    CAPACITY[cpu].load(Ordering::Acquire)
}

/// CPU `cpu`'s core class.
pub fn class_of(cpu: usize) -> CoreClass {
    if cpu >= MAX_CPUS {
        return CoreClass::Unknown;
    }
    match CLASS[cpu].load(Ordering::Acquire) {
        1 => CoreClass::Performance,
        2 => CoreClass::Efficiency,
        3 => CoreClass::LowPower,
        _ => CoreClass::Unknown,
    }
}

/// Choose a CPU for work of class `want` from the set `idle` (bit `n` = CPU `n` is available).
///
/// **On a uniform machine this is the lowest set bit** - byte-for-byte what a picker that had
/// never heard of capacity returns. That is the whole additivity argument: enabling this changes
/// nothing until the machine is actually hybrid.
///
/// It comes out of the **tie rule** rather than out of a special case, and that is deliberate.
/// A first version checked `is_hybrid()` here and returned early; the check was then observed to
/// be *redundant* - with every capacity equal, "highest capacity, ties to the lowest index" is
/// already "the lowest index" - and a guard whose removal changes no answer is not proven to be
/// doing anything (the same finding as the sysfs CPU-index bound, docs/LINUX-COMPAT.md). One path
/// with no special case cannot drift away from the case it was meant to preserve.
///
/// On a hybrid machine:
///
/// - [`ThreadClass::Compute`] and [`ThreadClass::Unknown`] take the **highest** capacity
///   available, because that is where throughput is and because an unknown demand is cheaper to
///   over-serve than to under-serve.
/// - [`ThreadClass::Bursty`] takes the **lowest** capacity available, leaving the fast cores for
///   work that can use them. Its latency comes from the run queue's ordering, not from clock
///   speed.
///
/// Ties go to the lowest CPU number, so the answer is deterministic and a test has an oracle.
/// `None` only when `idle` selects no present CPU.
pub fn pick_cpu(want: ThreadClass, idle: u64) -> Option<usize> {
    let mut best: Option<(usize, u16)> = None;
    let prefer_fast = want != ThreadClass::Bursty;
    for cpu in 0..MAX_CPUS {
        if idle & (1u64 << (cpu % 64)) == 0 || cpu >= 64 {
            continue;
        }
        let cap = capacity_of(cpu);
        if cap == 0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, b)) => {
                if prefer_fast {
                    cap > b
                } else {
                    cap < b
                }
            }
        };
        if better {
            best = Some((cpu, cap));
        }
    }
    let chosen = best.map(|(c, _)| c);
    if let Some(c) = chosen {
        PLACEMENTS.fetch_add(1, Ordering::Relaxed);
        if !tier_matches(c, want) {
            MISPLACED.fetch_add(1, Ordering::Relaxed);
        }
    }
    chosen
}

/// Whether CPU `cpu`'s tier suits work of class `want`.
///
/// Always true on a uniform machine: there is one tier, so nothing can be the wrong one.
fn tier_matches(cpu: usize, want: ThreadClass) -> bool {
    if !is_hybrid() {
        return true;
    }
    let cap = capacity_of(cpu);
    match want {
        ThreadClass::Bursty => cap < CAPACITY_FULL,
        _ => cap == CAPACITY_FULL,
    }
}

/// Whether a steal of work classed `want` onto CPU `thief` is a good match, **counting the
/// mismatch either way**.
///
/// The steal itself is never refused. Work conservation wins and the crossing is counted - the
/// same rule the locality work already holds, and for the same reason: a thief that idles rather
/// than take mismatched work is a worse failure than one that takes it and says so.
///
/// This preference is real even for work that has **not started yet**, which is what
/// distinguishes it from the cache-domain preference docs/RESOURCE-GRAPH.md 6.3a refuses. A cache
/// domain is about moving a working set that an unstarted entity does not have; capacity is about
/// how fast the entity will run *for its whole life* once it does start.
pub fn steal_is_matched(thief: usize, want: ThreadClass) -> bool {
    let ok = tier_matches(thief, want);
    if !ok {
        MISMATCHED_STEALS.fetch_add(1, Ordering::Relaxed);
    }
    ok
}

/// `(placements, placements onto a mismatched tier, mismatched steals)`.
pub fn stats() -> (usize, usize, usize) {
    (
        PLACEMENTS.load(Ordering::Acquire),
        MISPLACED.load(Ordering::Acquire),
        MISMATCHED_STEALS.load(Ordering::Acquire),
    )
}

/// Zero the counters. For a test that measures one phase.
pub fn reset_stats() {
    PLACEMENTS.store(0, Ordering::Release);
    MISPLACED.store(0, Ordering::Release);
    MISMATCHED_STEALS.store(0, Ordering::Release);
}

/// Whether the hardware offers Intel Thread Director's per-thread feedback on this machine.
///
/// Probed, never assumed, and **absent on every machine this tree can run**: it needs
/// `CPUID.06H:EAX[19]` (the hardware feedback interface) and `EAX[23]` (Thread Director), and
/// QEMU 11 implements neither - it has no hybrid support at all, checked in its source. ARM64 and
/// RISC-V have no equivalent, so they answer false by construction rather than by probe.
///
/// When it *is* present the hints replace [`classify`]'s output and [`ClassHint::source`] says so;
/// that path is designed and not built, because a hint path that cannot be exercised is a hint
/// path nobody has run (docs/SCHEDULING.md 12).
pub fn thread_director() -> bool {
    crate::arch::thread_director_present()
}
