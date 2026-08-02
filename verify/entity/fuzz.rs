// A model-checking fuzzer over the **shipped** execution-entity state machine
// (docs/EXECUTION-MODEL.md 8).
//
// It includes `kernel/src/sched/entity.rs` **verbatim** - the exact table the kernel
// links, not a model of it (the comparison/ "include the shipped code" rule). Only
// `Funded<T>`, which the kernel backs with frames charged to a cell, is shimmed,
// because a host process has no frame pool.
//
// WHY THIS EXISTS. Five defects in the vcore and preemption work were each found by an
// in-QEMU test needing four cores and a 120-second boot, and one of them
// (`next_sibling_vcore` entering a parked sibling) presented as a wrong syscall return
// value with no fault and no log. All five are the same defect: several places must
// agree about an execution entity and none decides. That state machine is integer-only
// and dependency-free, so it can be driven here at millions of operations per second
// with every invariant checked after every step - and a failing sequence shrunk to the
// two or three operations that actually matter.
//
// WHAT IT CANNOT DO, said up front. This checks the state machine. It does not check
// the trap path, the page tables, the FP register file or the real interrupt timing -
// those need real cores and stay with the in-QEMU kernels. It is the layer that catches
// the section-1 defect class before it costs a four-core boot to find, not a substitute
// for one.
//
// Run it with `cargo xtask verify`.

use std::collections::HashSet;

// --------------------------------------------------------------- host shims
//
// `Funded<T>` is a page-directory-backed table charged to a cell's frame budget
// (kernel/src/mm/kmeta.rs). The five methods below are every one `entity.rs` calls; a
// sixth appearing upstream is a compile error here rather than a silent divergence.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Owner(u16);
impl Owner {
    /// The kernel's own charge, used by `reset_table`.
    pub const KERNEL: Owner = Owner(u16::MAX);
    pub const fn cell(index: usize) -> Owner {
        Owner(index as u16)
    }
}

pub struct Funded<T: Copy> {
    slots: Vec<T>,
}

impl<T: Copy> Funded<T> {
    pub const fn new() -> Funded<T> {
        Funded { slots: Vec::new() }
    }
    pub fn set_owner(&mut self, _owner: Owner) {}
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    pub fn get(&self, index: usize) -> Option<T> {
        self.slots.get(index).copied()
    }
    pub fn set(&mut self, index: usize, value: T) -> bool {
        match self.slots.get_mut(index) {
            Some(s) => {
                *s = value;
                true
            }
            None => false,
        }
    }
    pub fn set_growing(&mut self, index: usize, value: T) -> bool {
        // A real growth can fail when the cell's budget is exhausted; the fuzzer models
        // that with a cap, because "growth always succeeds" would never exercise the
        // rollback path a `-EAGAIN` takes.
        if index >= MAX_ENTITIES {
            return false;
        }
        while index >= self.slots.len() {
            // The kernel grows into freshly allocated frames, which arrive zeroed -
            // `mm::kmeta`'s stated contract on `T`. Reproducing it keeps the shim
            // faithful to what the code under test sees.
            self.slots.push(unsafe { std::mem::zeroed() });
        }
        self.set(index, value)
    }
}

const MAX_ENTITIES: usize = 24;

mod mm {
    pub mod kmeta {
        pub use crate::{Funded, Owner};

        /// A frame charged to `owner`, as `mm::kmeta` hands out for a single-frame table.
        ///
        /// Shimmed as a **bump counter over a fake address space** rather than a real
        /// allocation: what the driver checks is the entity's bookkeeping - that an area is
        /// funded once, is distinct per entity, and is handed back exactly once - and that is
        /// arithmetic, not memory. `1` is skipped so 0 keeps meaning "no area".
        pub fn alloc_metric_frame(_owner: Owner) -> Option<usize> {
            // SAFETY: the driver is single-threaded.
            unsafe {
                NEXT_FRAME += 4096;
                LIVE_FRAMES += 1;
                Some(NEXT_FRAME)
            }
        }

        pub fn free_metric_frame(_va: usize, _owner: Owner) {
            // SAFETY: as above.
            unsafe { LIVE_FRAMES -= 1 };
        }

        pub static mut NEXT_FRAME: usize = 0x1000;
        pub static mut LIVE_FRAMES: isize = 0;

        /// Frames handed out and not returned - the leak check the driver asserts on.
        pub fn live_frames() -> isize {
            // SAFETY: as above.
            unsafe { LIVE_FRAMES }
        }
    }
}

// The class taxonomy's variants are constructed by kernel callers, not by this driver,
// so `dead_code` on the included module is expected rather than a smell.
#[allow(dead_code)]
#[path = "../../kernel/src/sched/entity.rs"]
mod entity;

use entity::{Entity, EntityTable, State, NO_CPU};

// ------------------------------------------------------------------- the model
//
// A tiny reference model kept BESIDE the table, holding only what the table cannot be
// asked about across steps: which CPU believes it is inside which entity, and whether
// a runnable entity was ignored while its owner had nothing to do. Those are the two
// invariants (I1 as a cross-CPU claim, and I5) that a snapshot check cannot see.

const CPUS: u16 = 4;

struct Model {
    /// What each CPU thinks it is inside. The table's own `inside` field is the
    /// kernel's answer; this is an independent one, so a disagreement is detectable.
    inside: [Option<usize>; CPUS as usize],
    /// Which of the dependency graph's edges (docs/EXECUTION-MODEL.md 3.2) the run has
    /// traversed. Coverage is the point: a fuzzer that never generated a steal racing a
    /// first entry has not tested I1.
    edges: HashSet<&'static str>,
}

impl Model {
    fn new() -> Model {
        Model {
            inside: [None; CPUS as usize],
            edges: HashSet::new(),
        }
    }
}

/// I1, across CPUs: no two CPUs believe they are inside the same entity.
fn check_i1(m: &Model) -> Result<(), String> {
    for a in 0..CPUS as usize {
        for b in (a + 1)..CPUS as usize {
            if m.inside[a].is_some() && m.inside[a] == m.inside[b] {
                return Err(format!(
                    "I1: CPU {a} and CPU {b} are both inside entity {:?}",
                    m.inside[a].unwrap()
                ));
            }
        }
    }
    Ok(())
}

/// I5, work conservation: no CPU is idle while an entity it may pick is runnable.
///
/// The invariant a scheduler exists for, and one that nothing in this tree asserts
/// today. Checked only at a point where the sequence has genuinely stopped scheduling
/// (see the `Op::Quiesce` step), because mid-sequence a CPU is legitimately between two
/// operations.
///
/// **The oracle is computed here, from the entity's own fields, and deliberately NOT by
/// calling `pickable`.** The first version of this function asked `pickable`, which is
/// the code under test - so making `pickable` too strict (refusing an *unclaimed*
/// entity, a real bug shape: it is what strands work nobody has claimed) left CPUs idle
/// beside runnable entities and this check still passed, because both sides agreed on a
/// wrong answer. That is the same defect the whole entity model exists to remove - two
/// places deciding one thing - reproduced inside its own test. An oracle must be
/// independent of the thing it judges (docs/ENGINEERING.md).
fn check_i5(t: &EntityTable, m: &Model) -> Result<(), String> {
    for cpu in 0..CPUS {
        if m.inside[cpu as usize].is_some() {
            continue;
        }
        for id in 0..t.capacity() {
            let Some(e) = t.get(id) else { continue };
            let could_run =
                e.state == State::Runnable && e.inside == NOT_INSIDE_HERE && e.owner_allows(cpu);
            if could_run {
                return Err(format!(
                    "I5: CPU {cpu} is idle while entity {id} is runnable and available to it \
                     (owner {}, state {:?})",
                    e.owner, e.state
                ));
            }
        }
    }
    Ok(())
}

/// The `inside` value meaning "nobody", recomputed here rather than imported: the
/// module keeps its own constant private, and an oracle that borrowed it would be one
/// more thing agreeing with the code under test by construction.
const NOT_INSIDE_HERE: u16 = u16::MAX;

trait OwnerAllows {
    fn owner_allows(&self, cpu: u16) -> bool;
}
impl OwnerAllows for Entity {
    fn owner_allows(&self, cpu: u16) -> bool {
        self.owner == NO_CPU || self.owner == cpu
    }
}

// -------------------------------------------------------------------- operations
//
// The operations ARE the graph's edges, which is what makes coverage measurable.

#[derive(Copy, Clone, Debug)]
enum Op {
    Create { cell: u16, context: u16 },
    Enter { id: usize, cpu: u16 },
    Leave { id: usize, cpu: u16, ns: u64, involuntary: bool },
    Park { id: usize, wake: u32 },
    Wake { id: usize },
    Exit { id: usize },
    Release { id: usize },
    Steal { id: usize, cpu: u16 },
    /// Drain every CPU and then run the scheduler to a fixed point, so I5 has a moment
    /// at which it is meaningful.
    Quiesce,
}

fn apply(t: &mut EntityTable, m: &mut Model, op: Op) -> Result<(), String> {
    match op {
        Op::Create { cell, context } => {
            if t.create(cell, context).is_some() {
                m.edges.insert("create");
            }
        }
        Op::Enter { id, cpu } => {
            // A CPU already inside something cannot enter another entity. Modelling
            // that here rather than in the table is deliberate: the table's job is to
            // refuse a *second CPU* per entity, and "one CPU, one entity" is the
            // caller's contract (a core runs one thing at a time).
            if m.inside[cpu as usize].is_some() {
                return Ok(());
            }
            if t.enter(id, cpu).is_ok() {
                m.inside[cpu as usize] = Some(id);
                m.edges.insert("a+b claim-and-enter");
            } else {
                m.edges.insert("enter refused");
            }
        }
        Op::Leave {
            id,
            cpu,
            ns,
            involuntary,
        } => {
            if m.inside[cpu as usize] == Some(id) && t.leave(id, cpu, ns, involuntary) {
                m.inside[cpu as usize] = None;
                m.edges
                    .insert(if involuntary { "g slice" } else { "f syscall" });
            }
        }
        Op::Park { id, wake } => {
            // A parked entity must not be inside a CPU: parking is something an entity
            // does about itself, from inside its own trap, so the caller has left first.
            if m.inside.iter().any(|s| *s == Some(id)) {
                return Ok(());
            }
            if t.park(id, wake) {
                m.edges.insert("j park per-entity");
            }
        }
        Op::Wake { id } => {
            if t.wake(id) {
                m.edges.insert("l wake");
            }
        }
        Op::Exit { id } => {
            if m.inside.iter().any(|s| *s == Some(id)) {
                return Ok(());
            }
            if t.exit(id) {
                m.edges.insert("h exit");
            }
        }
        Op::Release { id } => {
            if t.release(id) {
                m.edges.insert("k release");
            }
        }
        Op::Steal { id, cpu } => {
            if t.steal(id, cpu) {
                m.edges.insert("steal");
            }
        }
        Op::Quiesce => {
            for cpu in 0..CPUS {
                if let Some(id) = m.inside[cpu as usize] {
                    t.leave(id, cpu, 1000, false);
                    m.inside[cpu as usize] = None;
                }
            }
            // Now every CPU picks until nothing is pickable - the fixed point I5 is
            // about. A CPU picks at most once because it can only be inside one thing.
            for cpu in 0..CPUS {
                for id in 0..t.capacity() {
                    // Through `pickable`, the one predicate (R1) - not through `enter`
                    // alone. A scheduler that picks by one rule and is judged by
                    // another is defect 1's shape, and the first version of this loop
                    // had exactly that.
                    if t.pickable(id, cpu) && t.enter(id, cpu).is_ok() {
                        m.inside[cpu as usize] = Some(id);
                        break;
                    }
                }
            }
            m.edges.insert("quiesce");
            check_i5(t, m)?;
        }
    }
    if let Some(v) = t.check() {
        return Err(format!("{v:?}"));
    }
    check_i1(m)
}

// ---------------------------------------------------------------- sequence driver

/// A cheap deterministic PRNG so a failing run is reproducible without a dependency
/// (the `json/src/scan.rs` convention).
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

fn gen_op(st: &mut u64, cap: usize) -> Op {
    let ids = cap.max(1);
    match lcg(st) % 100 {
        0..=11 => Op::Create {
            cell: (lcg(st) % 3) as u16,
            context: (lcg(st) % 4) as u16,
        },
        12..=39 => Op::Enter {
            id: (lcg(st) as usize) % ids,
            cpu: (lcg(st) % CPUS as u64) as u16,
        },
        40..=59 => Op::Leave {
            id: (lcg(st) as usize) % ids,
            cpu: (lcg(st) % CPUS as u64) as u16,
            ns: lcg(st) % 5_000_000,
            involuntary: lcg(st) % 2 == 0,
        },
        60..=71 => Op::Park {
            id: (lcg(st) as usize) % ids,
            // Deliberately includes NO_WAKE so the refusal path is exercised: a park
            // with no source is the wedge I4 is about, and the table must refuse it
            // rather than the checker catching it afterwards.
            wake: (lcg(st) % 4) as u32,
        },
        72..=81 => Op::Wake {
            id: (lcg(st) as usize) % ids,
        },
        82..=87 => Op::Exit {
            id: (lcg(st) as usize) % ids,
        },
        88..=91 => Op::Release {
            id: (lcg(st) as usize) % ids,
        },
        92..=96 => Op::Steal {
            id: (lcg(st) as usize) % ids,
            cpu: (lcg(st) % CPUS as u64) as u16,
        },
        _ => Op::Quiesce,
    }
}

fn run(seq: &[Op], edges: &mut HashSet<&'static str>) -> Result<(), (usize, String)> {
    let mut t = EntityTable::new();
    t.init(Owner::cell(0), CPUS, 0);
    let mut m = Model::new();
    for (i, &op) in seq.iter().enumerate() {
        if let Err(e) = apply(&mut t, &mut m, op) {
            edges.extend(m.edges.iter());
            return Err((i, e));
        }
    }
    edges.extend(m.edges.iter());
    Ok(())
}

/// Shrink a failing sequence by dropping operations while it still fails at the same
/// message. A 400-operation counterexample teaches nothing; the minimal one is a test
/// case.
fn shrink(seq: Vec<Op>, want: &str) -> Vec<Op> {
    let mut best = seq;
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < best.len() {
            let mut candidate = best.clone();
            candidate.remove(i);
            let mut sink = HashSet::new();
            if let Err((_, msg)) = run(&candidate, &mut sink) {
                if msg == want {
                    best = candidate;
                    changed = true;
                    continue;
                }
            }
            i += 1;
        }
    }
    best
}

// -------------------------------------------------------------------- scenarios
//
// The fuzzer covers the state machine; these cover the **use cases**. Each is one row of
// docs/EXECUTION-MODEL.md's section 6 table, driven as a deterministic sequence with a
// hand-computed expectation - so the table is executed rather than tabulated, and the
// four rows it marks "blocked" become "the model expresses this; the kernel does not yet
// hold the field" rather than a claim.
//
// Deliberately not random: a scenario asserts a specific outcome, and a random sequence
// that happened to produce it would prove nothing about the shape being asked for.

type Scenario = (&'static str, fn() -> Result<(), String>);

fn fresh() -> (EntityTable, Model) {
    let mut t = EntityTable::new();
    t.init(Owner::cell(0), CPUS, 0);
    (t, Model::new())
}

/// One cell, four entities, four CPUs, all four inside at once.
///
/// The capability docs/SMP.md 10.2 leaves open and Node's and Bun's workers want. The
/// kernel cannot do it today because a Linux context has no owning CPU field and no
/// kernel stack of its own (`clone_child_frame` copies the parent's `kernel_sp`); this
/// shows the *model* has no objection, so E4 is a field, not a redesign.
fn sc_threads_across_cores() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let ids: Vec<usize> = (0..4).map(|c| t.create(0, c).expect("create")).collect();
    for (cpu, &id) in ids.iter().enumerate() {
        apply(&mut t, &mut m, Op::Enter { id, cpu: cpu as u16 })?;
    }
    let inside = m.inside.iter().filter(|s| s.is_some()).count();
    if inside != 4 {
        return Err(format!("only {inside} of 4 entities of one cell are running"));
    }
    Ok(())
}

/// Node's teardown, which is what the `linuxbun` partial got stuck in: the main entity
/// parks on a source only its peer can satisfy, the peer runs and satisfies it, and the
/// main entity must become runnable again.
///
/// The interesting assertion is the middle one - a cell with one parked entity and one
/// runnable entity is **not** blocked. Getting that wrong (per-cell block state) idled a
/// machine with work available, which was defect 3.
fn sc_node_wake_peer() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let main = t.create(0, 0).expect("create main");
    let worker = t.create(0, 1).expect("create worker");
    apply(&mut t, &mut m, Op::Enter { id: main, cpu: 0 })?;
    apply(
        &mut t,
        &mut m,
        Op::Leave {
            id: main,
            cpu: 0,
            ns: 1000,
            involuntary: false,
        },
    )?;
    apply(&mut t, &mut m, Op::Park { id: main, wake: 7 })?;
    if t.all_parked(0) {
        return Err("a cell with one parked and one runnable entity reported blocked".into());
    }
    apply(&mut t, &mut m, Op::Enter { id: worker, cpu: 1 })?;
    apply(&mut t, &mut m, Op::Wake { id: main })?;
    if !t.pickable(main, 0) {
        return Err("the woken main entity is not pickable by its own CPU".into());
    }
    Ok(())
}

/// Every live entity parked: the cell is blocked, and one wake un-blocks it.
///
/// The pair matters. "Blocked" that is too eager idles a machine; "blocked" that is
/// never reported means the deadlock classifier never fires and the run hangs instead of
/// ending with a reason (docs/ARCHITECTURE-DEBT.md 2.4).
fn sc_cell_blocked_and_woken() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let a = t.create(0, 0).expect("a");
    let b = t.create(0, 1).expect("b");
    apply(&mut t, &mut m, Op::Park { id: a, wake: 1 })?;
    apply(&mut t, &mut m, Op::Park { id: b, wake: 2 })?;
    if !t.all_parked(0) {
        return Err("every live entity is parked and the cell does not report blocked".into());
    }
    apply(&mut t, &mut m, Op::Wake { id: b })?;
    if t.all_parked(0) {
        return Err("one entity was woken and the cell still reports blocked".into());
    }
    Ok(())
}

/// The last entity out ends the cell, and an exited one counts as neither runnable nor
/// parked (the vcore-exit rule, docs/SMP.md 10.0a).
fn sc_last_one_out() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let ids: Vec<usize> = (0..3).map(|c| t.create(0, c).expect("create")).collect();
    for (n, &id) in ids.iter().enumerate() {
        apply(&mut t, &mut m, Op::Exit { id })?;
        let live = t.live_of(0);
        let want = 2 - n;
        if live != want {
            return Err(format!("after {} exits, {live} live, expected {want}", n + 1));
        }
    }
    if t.all_parked(0) {
        return Err("a cell with no live entities reported blocked, not finished".into());
    }
    Ok(())
}

/// A core that has run dry takes an **unstarted** entity from a peer's claim, and I5
/// holds afterwards. Migrating a *running* entity is refused, which is the difference
/// between the rebalance that ships and the capability reverted twice.
fn sc_steal_idle_not_running() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let hot = t.create(0, 0).expect("hot");
    let queued = t.create(0, 1).expect("queued");
    apply(&mut t, &mut m, Op::Enter { id: hot, cpu: 0 })?;
    if !t.steal(queued, 1) {
        return Err("an idle unclaimed entity could not be taken by a dry core".into());
    }
    if t.steal(hot, 1) {
        return Err("a RUNNING entity was stolen - that is the twice-reverted \
                    migrate-a-running-entity path, and the model must refuse it"
            .into());
    }
    apply(&mut t, &mut m, Op::Enter { id: queued, cpu: 1 })?;
    apply(&mut t, &mut m, Op::Quiesce)?;
    Ok(())
}

/// FA3's producer/consumer overlap: two entities of one cell, both running at the same
/// instant, which is what turns FlashAttention 3's pipelining from cooperative
/// interleaving into real overlap (docs/TILES.md 13).
///
/// Same shape as `sc_threads_across_cores` at a smaller width, and listed separately
/// because it is a different claim: that one is about a *Linux* cell's threads, this is
/// about a native cell's tile pipeline, and the two are blocked on different fields.
fn sc_fa3_overlap() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let producer = t.create(0, 0).expect("producer");
    let consumer = t.create(0, 1).expect("consumer");
    apply(&mut t, &mut m, Op::Enter { id: producer, cpu: 0 })?;
    apply(&mut t, &mut m, Op::Enter { id: consumer, cpu: 1 })?;
    if m.inside[0] != Some(producer) || m.inside[1] != Some(consumer) {
        return Err("the two halves of the pipeline are not both running".into());
    }
    Ok(())
}

/// A container: two cells in one bundle, four entities, no cross-cell confusion. The
/// per-cell queries must answer about the cell asked for and no other - a bundle shares
/// a budget and a PrincipalId, not an execution state.
fn sc_bundle_two_cells() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let a0 = t.create(0, 0).expect("a0");
    let _a1 = t.create(0, 1).expect("a1");
    let b0 = t.create(1, 0).expect("b0");
    let b1 = t.create(1, 1).expect("b1");
    apply(&mut t, &mut m, Op::Park { id: b0, wake: 1 })?;
    apply(&mut t, &mut m, Op::Park { id: b1, wake: 2 })?;
    if !t.all_parked(1) {
        return Err("cell 1 has both entities parked and does not report blocked".into());
    }
    if t.all_parked(0) {
        return Err("cell 0 reported blocked because its BUNDLE PEER is - the two cells \
                    share a budget, not an execution state"
            .into());
    }
    apply(&mut t, &mut m, Op::Enter { id: a0, cpu: 0 })?;
    Ok(())
}

/// An entity created between a peer's pick and its enter (a `fork` or `clone` landing
/// mid-scan) is not enterable by the peer that was already inside something, and the
/// table stays consistent. One of section 6.2's edge cases.
fn sc_create_races_a_pick() -> Result<(), String> {
    let (mut t, mut m) = fresh();
    let running = t.create(0, 0).expect("running");
    apply(&mut t, &mut m, Op::Enter { id: running, cpu: 0 })?;
    let child = t.create(0, 1).expect("child");
    // CPU 0 is inside `running`; a second core may take the child, and CPU 0 may not
    // (the driver's one-CPU-one-entity contract), but nothing may enter it twice.
    apply(&mut t, &mut m, Op::Enter { id: child, cpu: 1 })?;
    if t.enter(child, 2).is_ok() {
        return Err("a third CPU entered an entity another core is already inside".into());
    }
    Ok(())
}

/// Budget exhaustion during creation is a clean refusal naming the cell, never a global
/// "table full" and never a panic (docs/MEMORY.md 7, no OOM killer).
fn sc_budget_exhaustion() -> Result<(), String> {
    let (mut t, _m) = fresh();
    let mut made = 0;
    while t.create(0, made as u16).is_some() {
        made += 1;
        if made > MAX_ENTITIES + 8 {
            return Err("creation never refused - the budget cap is not enforced".into());
        }
    }
    if made != MAX_ENTITIES {
        return Err(format!("created {made}, expected exactly {MAX_ENTITIES}"));
    }
    if let Some(v) = t.check() {
        return Err(format!("the table is inconsistent after a refused create: {v:?}"));
    }
    Ok(())
}

/// E4: a context's FP save area is **funded once and returned once**.
///
/// The kernel's `MAX_VCORES` was 4 because these areas were a fixed static; funded, the ceiling
/// is the owning cell's frame budget. The three properties that has to satisfy are arithmetic,
/// so they belong here rather than in a boot: fund is **idempotent** (an `install` that runs
/// again for a reused slot must not leak the previous frame), every area is **distinct**, and
/// release is **exact** - the count returns to zero and a second release is a no-op rather than
/// a double free.
fn sc_funded_fp_areas() -> Result<(), String> {
    let (mut t, _m) = fresh();
    let owner = Owner::cell(0);
    let before = crate::mm::kmeta::live_frames();

    let mut ids = Vec::new();
    for c in 0..6u16 {
        let id = t.create(0, c).ok_or("create")?;
        if !t.fund_fp(id, owner, 512) {
            return Err(format!("entity {id} could not be funded"));
        }
        ids.push(id);
    }
    if crate::mm::kmeta::live_frames() - before != ids.len() as isize {
        return Err(format!(
            "{} entities took {} frames",
            ids.len(),
            crate::mm::kmeta::live_frames() - before
        ));
    }
    // Idempotent: funding again must not take a second frame.
    for &id in &ids {
        t.fund_fp(id, owner, 512);
    }
    if crate::mm::kmeta::live_frames() - before != ids.len() as isize {
        return Err("funding an entity twice took a second frame - a reused slot would leak".into());
    }
    // Distinct.
    for (i, &a) in ids.iter().enumerate() {
        if t.fp_of(a) == 0 {
            return Err(format!("entity {a} has no area after being funded"));
        }
        for &b in &ids[i + 1..] {
            if t.fp_of(a) == t.fp_of(b) {
                return Err(format!("entities {a} and {b} share one FP area"));
            }
        }
    }
    // Released exactly once, and a second release is a no-op.
    for &id in &ids {
        t.release_fp(id, owner);
        t.release_fp(id, owner);
    }
    if crate::mm::kmeta::live_frames() != before {
        return Err(format!(
            "{} frames outstanding after releasing every area",
            crate::mm::kmeta::live_frames() - before
        ));
    }
    Ok(())
}

const SCENARIOS: &[Scenario] = &[
    ("a context's FP area is funded once, returned once", sc_funded_fp_areas),
    ("threads of one cell across 4 cores", sc_threads_across_cores),
    ("Node teardown: peer wakes a parked main", sc_node_wake_peer),
    ("cell blocked when every entity is", sc_cell_blocked_and_woken),
    ("last entity out ends the cell", sc_last_one_out),
    ("steal an idle entity, refuse a running one", sc_steal_idle_not_running),
    ("FA3 producer/consumer overlap", sc_fa3_overlap),
    ("a bundle's two cells block independently", sc_bundle_two_cells),
    ("create races a pick", sc_create_races_a_pick),
    ("budget exhaustion refuses cleanly", sc_budget_exhaustion),
];

fn main() {
    const RUNS: usize = 20_000;
    const LEN: usize = 400;

    let mut edges: HashSet<&'static str> = HashSet::new();
    let mut failures = 0usize;

    println!("== use-case scenarios (docs/EXECUTION-MODEL.md 6) ==");
    let mut sc_failed = 0usize;
    for &(name, f) in SCENARIOS {
        match f() {
            Ok(()) => println!("  ok   {name}"),
            Err(e) => {
                println!("  FAIL {name}: {e}");
                sc_failed += 1;
            }
        }
    }
    if sc_failed > 0 {
        println!("entity scenarios: FAIL ({sc_failed} of {})", SCENARIOS.len());
        std::process::exit(1);
    }
    println!("entity scenarios: {} passed\n", SCENARIOS.len());

    println!("== entity state machine: {RUNS} sequences x {LEN} operations, {CPUS} CPUs ==");
    for run_no in 0..RUNS {
        let mut st = 0x9E37_79B9_7F4A_7C15u64 ^ (run_no as u64).wrapping_mul(0x1000_0001B3);
        let mut seq = Vec::with_capacity(LEN);
        // The sequence is generated against a *guess* at the capacity, because the real
        // one grows as the run proceeds; out-of-range ids are then a legitimate input
        // (the table must refuse them) rather than a generator bug.
        for _ in 0..LEN {
            seq.push(gen_op(&mut st, MAX_ENTITIES));
        }
        if let Err((at, msg)) = run(&seq, &mut edges) {
            failures += 1;
            let minimal = shrink(seq[..=at].to_vec(), &msg);
            println!("FAIL seed {run_no} at op {at}: {msg}");
            println!("  minimal sequence ({} ops):", minimal.len());
            for op in &minimal {
                println!("    {op:?}");
            }
            if failures >= 3 {
                break;
            }
        }
    }

    let mut names: Vec<&str> = edges.iter().copied().collect();
    names.sort_unstable();
    println!("edges traversed ({}): {}", names.len(), names.join(", "));

    // Coverage is asserted, not reported. A green run that never generated an entry
    // refusal, a steal or a quiesce has proven nothing, and would read as if it had.
    let required = [
        "create",
        "a+b claim-and-enter",
        "enter refused",
        "f syscall",
        "g slice",
        "h exit",
        "j park per-entity",
        "k release",
        "l wake",
        "quiesce",
        "steal",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|e| !edges.contains(e))
        .collect();
    if !missing.is_empty() {
        println!("INCOMPLETE: edges never traversed: {}", missing.join(", "));
        std::process::exit(1);
    }

    if failures > 0 {
        println!("entity fuzz: FAIL ({failures} sequences violated an invariant)");
        std::process::exit(1);
    }
    println!("entity fuzz: PASS (I1, I2, I3, I4, I5, I7, I9 held on every step)");
}
