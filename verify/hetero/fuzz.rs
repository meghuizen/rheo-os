// Model checking over the **shipped** heterogeneous-core placement
// (kernel/src/sched/hetero.rs and kernel/src/sched/bore.rs), included verbatim
// (docs/RESOURCE-GRAPH.md 2.4b, docs/SCHEDULING.md 12).
//
// WHY HERE AND NOT ONLY IN A BOOT TEST. No emulator this tree runs on models a hybrid part -
// QEMU 11 implements neither x86-64's hybrid flag nor CPUID leaf 0x1A and never emits
// `capacity-dmips-mhz`, checked in its source. So the *interesting* machines cannot be booted at
// all, and a boot test can only exercise a declared asymmetry of one fixed shape. Here every
// shape is reachable: three tiers, one core of each, a machine whose slowest core is also its
// only idle core, a uniform machine, an empty idle set.
//
// The oracle is arithmetic over the declared table, never the module's own accessors - the
// `entity` fuzzer's first I5 check asked the code under test whether work existed and passed
// while stranding work, because both sides agreed on a wrong answer (verify/README.md).

// ------------------------------------------------------------------ shims
//
// Only what the two included files reach for. `smp::MAX_CPUS` is a constant, `hw::graph`'s two
// items are a constant and an enum, `hw::Inventory` is read for its CPU list, and
// `arch::thread_director_present` is a probe that is false on every machine either way.

pub mod smp {
    pub const MAX_CPUS: usize = 64;
}

pub mod hw {
    pub mod graph {
        pub const CAPACITY_FULL: u16 = 1024;
        #[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
        #[repr(u8)]
        pub enum CoreClass {
            #[default]
            Unknown = 0,
            Performance = 1,
            Efficiency = 2,
            LowPower = 3,
        }
    }
    #[derive(Copy, Clone)]
    pub struct CpuInfo {
        pub class: graph::CoreClass,
        pub capacity: u16,
    }
    pub struct Inventory {
        pub ncpus: usize,
        pub cpus: [CpuInfo; super::smp::MAX_CPUS],
    }
    impl Inventory {
        pub fn uniform(n: usize) -> Inventory {
            Inventory {
                ncpus: n,
                cpus: [CpuInfo {
                    class: graph::CoreClass::Unknown,
                    capacity: graph::CAPACITY_FULL,
                }; super::smp::MAX_CPUS],
            }
        }
    }
}

pub mod arch {
    pub fn thread_director_present() -> bool {
        false
    }
}

// The two shipped files, verbatim, **at the crate root**: `hetero` reaches its sibling as
// `super::bore`, and for a root-level module `super` is the crate root - so declaring both here
// gives the shipped file the module neighbour it expects without a wrapper module.
#[allow(dead_code)]
#[path = "../../kernel/src/sched/bore.rs"]
pub mod bore;
#[allow(dead_code)]
#[path = "../../kernel/src/sched/hetero.rs"]
pub mod hetero;

use hw::graph::{CAPACITY_FULL, CoreClass};
use bore::Burst;
use hetero::{HintSource, ThreadClass};

/// Build an inventory from `(class, capacity)` pairs and load it into the module under test.
fn load(cpus: &[(CoreClass, u16)]) {
    let mut inv = hw::Inventory::uniform(cpus.len());
    for (i, &(class, cap)) in cpus.iter().enumerate() {
        inv.cpus[i].class = class;
        inv.cpus[i].capacity = cap;
    }
    hetero::load_from_inventory(&inv);
    hetero::reset_stats();
}

const P: (CoreClass, u16) = (CoreClass::Performance, CAPACITY_FULL);
const E: (CoreClass, u16) = (CoreClass::Efficiency, 640);
const LP: (CoreClass, u16) = (CoreClass::LowPower, 320);

// --------------------------------------------------------- deterministic properties

/// A uniform machine must behave exactly as it did before this module existed: the picker
/// returns the lowest-numbered available CPU whatever the work is, and nothing is ever a
/// mismatch. This is the additivity property, and it is the reason the whole thing can land
/// without touching an existing boot.
fn uniform_is_unchanged() -> Result<(), String> {
    load(&[P, P, P, P]);
    if hetero::is_hybrid() {
        return Err("four identical cores reported as hybrid".into());
    }
    for want in [ThreadClass::Compute, ThreadClass::Bursty, ThreadClass::Unknown] {
        for (idle, expect) in [(0b1111u64, 0usize), (0b1110, 1), (0b1000, 3), (0b0100, 2)] {
            let got = hetero::pick_cpu(want, idle);
            if got != Some(expect) {
                return Err(format!(
                    "uniform machine, {want:?}, idle {idle:#b}: picked {got:?}, want {expect}"
                ));
            }
        }
    }
    let (_, misplaced, _) = hetero::stats();
    if misplaced != 0 {
        return Err(format!("{misplaced} mismatches on a machine with one tier"));
    }
    Ok(())
}

/// A machine whose cores differ only in *capacity* is still hybrid: the class is a label and the
/// capacity is the number decisions are made from, so two cores both labelled `Unknown` at
/// different capacities must not be treated as interchangeable.
fn capacity_alone_makes_it_hybrid() -> Result<(), String> {
    load(&[
        (CoreClass::Unknown, CAPACITY_FULL),
        (CoreClass::Unknown, 500),
    ]);
    if !hetero::is_hybrid() {
        return Err("two cores of different capacity reported as uniform".into());
    }
    if hetero::pick_cpu(ThreadClass::Compute, 0b11) != Some(0) {
        return Err("compute work did not take the faster core".into());
    }
    if hetero::pick_cpu(ThreadClass::Bursty, 0b11) != Some(1) {
        return Err("bursty work did not take the slower core".into());
    }
    Ok(())
}

/// Compute and unknown work take the fastest available core; bursty work takes the slowest.
/// Checked against the *declared* table rather than against `capacity_of`.
fn tiers_are_respected() -> Result<(), String> {
    load(&[P, P, E, E, LP]);
    if !hetero::is_hybrid() {
        return Err("a P/E/LP machine reported as uniform".into());
    }
    let cases: &[(ThreadClass, u64, usize)] = &[
        // Everything idle.
        (ThreadClass::Compute, 0b11111, 0),
        (ThreadClass::Unknown, 0b11111, 0),
        (ThreadClass::Bursty, 0b11111, 4),
        // No P core available: compute work must take the *best remaining*, not refuse.
        (ThreadClass::Compute, 0b11100, 2),
        // No slow core available: bursty work must take a P core rather than refuse.
        (ThreadClass::Bursty, 0b00011, 0),
        // One core, and it is the wrong tier for the work: still the answer.
        (ThreadClass::Compute, 0b10000, 4),
    ];
    for &(want, idle, expect) in cases {
        let got = hetero::pick_cpu(want, idle);
        if got != Some(expect) {
            return Err(format!(
                "{want:?} with idle {idle:#b}: picked {got:?}, want {expect}"
            ));
        }
    }
    // Ties go to the lowest CPU number, so the answer is reproducible.
    if hetero::pick_cpu(ThreadClass::Compute, 0b00011) != Some(0) {
        return Err("a tie between two equal cores is not resolved to the lowest".into());
    }
    if hetero::pick_cpu(ThreadClass::Bursty, 0b01100) != Some(2) {
        return Err("a tie between two equal slow cores is not resolved to the lowest".into());
    }
    Ok(())
}

/// An empty or impossible idle set answers `None` rather than a plausible CPU, and counts no
/// placement. A picker that returned CPU 0 for "nothing is free" would put work on a busy core.
fn no_candidate_is_none() -> Result<(), String> {
    load(&[P, E]);
    if hetero::pick_cpu(ThreadClass::Compute, 0) != None {
        return Err("an empty idle set produced a CPU".into());
    }
    // Bits for CPUs that are not present.
    if hetero::pick_cpu(ThreadClass::Compute, 0b1111_0000) != None {
        return Err("absent CPUs produced a CPU".into());
    }
    let (placements, _, _) = hetero::stats();
    if placements != 0 {
        return Err(format!("{placements} placements counted for 0 answers"));
    }
    Ok(())
}

/// Mismatches are **counted, never prevented** - the work-conservation rule. A steal onto the
/// wrong tier still succeeds and shows up in the counter.
fn mismatches_are_counted_not_prevented() -> Result<(), String> {
    load(&[P, E]);
    // Compute work onto the E core, bursty onto the P core: both wrong, both allowed.
    if hetero::steal_is_matched(1, ThreadClass::Compute) {
        return Err("compute work on an E core reported as a match".into());
    }
    if hetero::steal_is_matched(0, ThreadClass::Bursty) {
        return Err("bursty work on a P core reported as a match".into());
    }
    if !hetero::steal_is_matched(0, ThreadClass::Compute) {
        return Err("compute work on a P core reported as a mismatch".into());
    }
    let (_, _, mismatched) = hetero::stats();
    if mismatched != 2 {
        return Err(format!("{mismatched} mismatched steals counted, want 2"));
    }
    // And on a uniform machine nothing can be a mismatch, because there is one tier.
    load(&[P, P]);
    for cpu in 0..2 {
        for want in [ThreadClass::Compute, ThreadClass::Bursty] {
            if !hetero::steal_is_matched(cpu, want) {
                return Err("a mismatch on a machine with one tier".into());
            }
        }
    }
    Ok(())
}

/// Classification comes from **observed** relinquish behaviour, and an entity that has never
/// relinquished is `Unknown` however long it has run - a long first burst is what every
/// program's startup looks like.
fn classify_needs_an_observation() -> Result<(), String> {
    let mut b = Burst::new();
    if hetero::classify(&b).class != ThreadClass::Unknown {
        return Err("a brand-new entity is not Unknown".into());
    }
    // A very long first burst, still never relinquished.
    b.charge(1 << 40);
    let hint = hetero::classify(&b);
    if hint.class != ThreadClass::Unknown {
        return Err(format!(
            "an entity that ran {} ns without ever yielding is {:?}, want Unknown",
            1u64 << 40,
            hint.class
        ));
    }
    if hint.source != HintSource::Observed {
        return Err("a hint derived from our own accounting is not labelled Observed".into());
    }
    // Now it yields: the history term makes it compute-bound.
    b.relinquish();
    if hetero::classify(&b).class != ThreadClass::Compute {
        return Err("a long burst that then yielded is not Compute".into());
    }
    // A short-burst entity that yields is bursty.
    let mut q = Burst::new();
    q.charge(1000);
    q.relinquish();
    if hetero::classify(&q).class != ThreadClass::Bursty {
        return Err("a 1 us burst that yielded is not Bursty".into());
    }
    Ok(())
}

/// The threshold is a statement about the score, so it is checked at the boundary from both
/// sides rather than at a value comfortably inside one class.
fn threshold_is_exact() -> Result<(), String> {
    // Find the smallest burst whose score reaches the threshold, by the module's own arithmetic
    // - `score_of` is the shipped function, so this is the boundary the classifier will use.
    let mut ns = 1u64;
    while bore::score_of(ns) < hetero::COMPUTE_SCORE && ns < u64::MAX / 2 {
        ns *= 2;
    }
    if bore::score_of(ns) < hetero::COMPUTE_SCORE {
        return Err("no burst length reaches the compute threshold".into());
    }
    let mut at = Burst::new();
    at.charge(ns);
    at.relinquish();
    if hetero::classify(&at).class != ThreadClass::Compute {
        return Err(format!("a burst of {ns} ns scores at the threshold but is not Compute"));
    }
    // One bit below: the score must be under the threshold, so the class must not be Compute.
    let below = ns / 2;
    if bore::score_of(below) >= hetero::COMPUTE_SCORE {
        return Err("halving the burst did not drop the score below the threshold".into());
    }
    let mut under = Burst::new();
    under.charge(below);
    under.relinquish();
    if hetero::classify(&under).class == ThreadClass::Compute {
        return Err(format!("a burst of {below} ns scores below the threshold but is Compute"));
    }
    Ok(())
}

/// Thread Director is absent here, and the fallback is what answers - so a hint's source must
/// say `Observed` and never claim hardware that is not there.
fn thread_director_absent_is_honest() -> Result<(), String> {
    if hetero::thread_director() {
        return Err("Thread Director reported present on a machine that has none".into());
    }
    let mut b = Burst::new();
    b.charge(5000);
    b.relinquish();
    if hetero::classify(&b).source != HintSource::Observed {
        return Err("a hint claims Thread Director as its source".into());
    }
    Ok(())
}

// --------------------------------------------------------------- randomised

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// Over random machines and random idle sets, three invariants must hold whatever the shape:
///
/// 1. The chosen CPU is **present and in the idle set** - a picker that answers with a busy or
///    absent CPU puts work where it cannot run.
/// 2. On a hybrid machine, compute work gets the **maximum** capacity in the idle set and bursty
///    work the **minimum** - computed here by scanning the declared table, not by asking the
///    module.
/// 3. `None` if and only if the idle set selects no present CPU.
fn random_machine(seed: u64) -> Result<(), String> {
    let mut st = seed;
    let n = 1 + (lcg(&mut st) % 8) as usize;
    let mut cpus = Vec::new();
    for _ in 0..n {
        // Capacity 1..=1024 so ties, near-ties and extremes all occur.
        let cap = 1 + (lcg(&mut st) % CAPACITY_FULL as u64) as u16;
        let class = match lcg(&mut st) % 4 {
            0 => CoreClass::Unknown,
            1 => CoreClass::Performance,
            2 => CoreClass::Efficiency,
            _ => CoreClass::LowPower,
        };
        cpus.push((class, cap));
    }
    load(&cpus);
    let idle = lcg(&mut st) & 0xff;

    // The oracle: scan the declared table.
    let present: Vec<(usize, u16)> = cpus
        .iter()
        .enumerate()
        .filter(|(i, _)| idle & (1u64 << i) != 0)
        .map(|(i, &(_, cap))| (i, cap))
        .collect();
    let hybrid = cpus.iter().any(|&c| c != cpus[0]);

    for want in [ThreadClass::Compute, ThreadClass::Bursty, ThreadClass::Unknown] {
        let got = hetero::pick_cpu(want, idle);
        if present.is_empty() {
            if got.is_some() {
                return Err(format!("seed {seed}: answered {got:?} with nothing idle"));
            }
            continue;
        }
        let Some(cpu) = got else {
            return Err(format!(
                "seed {seed}: answered None with {} idle CPUs",
                present.len()
            ));
        };
        if !present.iter().any(|&(i, _)| i == cpu) {
            return Err(format!("seed {seed}: answered CPU {cpu}, which is not idle"));
        }
        if !hybrid {
            let lowest = present[0].0;
            if cpu != lowest {
                return Err(format!(
                    "seed {seed}: uniform machine answered {cpu}, want the lowest idle {lowest}"
                ));
            }
            continue;
        }
        let want_cap = if want == ThreadClass::Bursty {
            present.iter().map(|&(_, c)| c).min().unwrap()
        } else {
            present.iter().map(|&(_, c)| c).max().unwrap()
        };
        let got_cap = present.iter().find(|&&(i, _)| i == cpu).unwrap().1;
        if got_cap != want_cap {
            return Err(format!(
                "seed {seed}: {want:?} took capacity {got_cap}, want {want_cap}"
            ));
        }
    }
    Ok(())
}

fn main() {
    let mut failures = 0usize;
    println!("== heterogeneous cores: deterministic properties ==");
    for (name, r) in [
        ("a uniform machine behaves exactly as before", uniform_is_unchanged()),
        ("differing capacity alone makes a machine hybrid", capacity_alone_makes_it_hybrid()),
        ("compute takes the fastest, bursty the slowest", tiers_are_respected()),
        ("no candidate answers None, not CPU 0", no_candidate_is_none()),
        ("a mismatch is counted, never prevented", mismatches_are_counted_not_prevented()),
        ("classification needs an observed relinquish", classify_needs_an_observation()),
        ("the compute threshold holds at its boundary", threshold_is_exact()),
        ("Thread Director's absence is reported honestly", thread_director_absent_is_honest()),
    ] {
        match r {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                failures += 1;
            }
        }
    }

    println!("== heterogeneous cores: randomised machines ==");
    let mut bad = 0;
    for run in 0..20_000u64 {
        if let Err(e) = random_machine(0x5EED ^ run.wrapping_mul(0x9E3779B97F4A7C15)) {
            if bad == 0 {
                println!("  FAIL {e}");
            }
            bad += 1;
        }
    }
    if bad == 0 {
        println!("  ok   20000 random machines, 1..8 cores");
    } else {
        failures += 1;
    }

    if failures > 0 {
        println!("hetero fuzz: FAIL ({failures} properties)");
        std::process::exit(1);
    }
    println!("hetero fuzz: PASS");
}
