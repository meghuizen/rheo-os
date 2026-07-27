// The **rheo-os side** of the scheduler-responsiveness axis, run on the host.
//
// This includes `kernel/src/sched/bore.rs` and `kernel/src/sched/vcore.rs`
// **verbatim** - the exact EEVDF + BORE run queue the OS ships, not a model of it
// (the comparison/tiles and comparison/threads "include the shipped code" rule). Only
// the storage the kernel funds from frames is shimmed, because a host process has no
// frame pool; the scheduling arithmetic, the eligibility gate, the burst score and the
// weight table are the kernel's own source.
//
// **The units are not nanoseconds and must never be printed as if they were.** rheo-os
// runs only under QEMU TCG in this repository, which models no caches, no TLB and no
// branch predictor, so no honest wall-clock number for it exists outside the hardware
// lab (docs/TOOLING.md 4). What *is* honest, deterministic and directly comparable to
// a real scheduler is the **decision**: when an interactive vcore wakes while CPU-bound
// vcores are runnable, how many of their slices run before it does, and what weight
// does the burst score give it?
//
// That is the same question `sched_latency.rs` asks Linux, expressed in the one unit
// both can answer in: **intervening slices**. Read comparison/linux/README.md before
// putting the two side by side.

use std::time::Instant;

// ------------------------------------------------------------------ host shims
//
// `Funded<T>` is a page-directory-backed table charged to a cell's frame budget
// (kernel/src/mm/kmeta.rs). On the host there are no frames, so it is a `Vec`. The
// six methods below are every one `vcore.rs` calls - checked by grep, so a seventh
// appearing upstream is a compile error here rather than a silent divergence.

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Owner(u16);
impl Owner {
    pub const KERNEL: Owner = Owner(u16::MAX);
}

pub struct Funded<T: Copy> {
    slots: Vec<T>,
}

impl<T: Copy> Funded<T> {
    pub const fn new() -> Funded<T> {
        Funded { slots: Vec::new() }
    }
    pub fn set_owner(&mut self, _owner: Owner) {}
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
        while index >= self.slots.len() {
            // The kernel grows into freshly-allocated frames, which arrive zeroed -
            // that is the module's stated contract on `T` (kernel/src/mm/kmeta.rs:
            // `Copy`, no drop glue, zero-initialised on growth). Reproducing it here
            // rather than using `Default` keeps the shim faithful to what the code
            // under test actually sees.
            // SAFETY: `T: Copy` with no drop glue, and every field of `Vcore` is an
            // integer or a `repr` enum whose zero pattern is valid - the same
            // guarantee the kernel relies on for a fresh frame.
            self.slots.push(unsafe { core::mem::zeroed() });
        }
        self.set(index, value)
    }
    pub fn release(&mut self) {
        self.slots.clear();
    }
    /// No frames on the host, so the kernel's storage accounting reads zero. Only
    /// `RunQueue::metadata_frames` surfaces it, and nothing here asserts on it.
    pub fn frames_held(&self) -> usize {
        0
    }
}

/// The kernel records a dispatch's queueing delay into its histogram pipeline. Here
/// it is captured instead, because the delay *is* the thing being reported.
mod metrics {
    use std::cell::RefCell;
    #[derive(Copy, Clone, PartialEq, Eq)]
    pub enum Metric {
        RunDelayNs,
    }
    thread_local! {
        pub static DELAYS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }
    pub fn record(_m: Metric, v: u64) {
        DELAYS.with(|d| d.borrow_mut().push(v));
    }
}

/// `crate::mm::kmeta` as `vcore.rs` names it.
mod mm {
    pub mod kmeta {
        pub use crate::{Funded, Owner};
    }
}

// The shipped scheduler, verbatim. `#[path]` rather than `include!` because these
// files carry `//!` module docs, which are only legal at the top of a real module -
// and keeping them is the point: the source is unedited.
#[allow(dead_code)]
#[path = "../../kernel/src/sched/bore.rs"]
mod bore;
#[allow(dead_code)]
#[path = "../../kernel/src/sched/vcore.rs"]
mod vcore;

use vcore::{Class, RunQueue};

// --------------------------------------------------------------- the scenario
//
// One interactive vcore and N CPU-bound ones, the same shape `sched_latency.rs`
// gives Linux: the hogs never relinquish and are charged a full slice every time
// they run; the interactive one runs for a sliver and blocks again.

/// Slice a hog consumes each time it is dispatched (ns, the queue's own domain).
///
/// 4 ms, and the warm-up below runs enough of them that a hog's accumulated burst
/// passes BORE's exemption (`PENALTY_OFFSET_BITS` = 24, so bursts under ~16.7 ms score
/// zero on purpose - a task that runs briefly is not greedy). A shorter slice would
/// leave every vcore at the base weight and the comparison would be measuring plain
/// EEVDF with the burst term switched off by accident.
const HOG_SLICE_NS: u64 = 4_000_000;
/// Work the interactive vcore does before blocking again - a keystroke's worth.
const INTERACTIVE_RUN_NS: u64 = 20_000;
/// Wakeups sampled.
const WAKEUPS: usize = 200;

fn main() {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!("rheo-os EEVDF+BORE run queue (kernel/src/sched, verbatim) on the host");
    println!("metric: intervening CPU-bound slices between wake and pick (deterministic)");
    for hogs in [cpus, cpus * 2] {
        scenario(hogs);
    }
    throughput();
}

fn scenario(hogs: usize) {
    let mut q = RunQueue::new();
    q.init(Owner::KERNEL);
    let mut now = 0u64;

    // Cell 0 context 0 is the interactive vcore; cells 1..=hogs are CPU-bound.
    let inter = q
        .admit(0, 0, Class::Fair, bore::Burst::new(), now)
        .expect("admit interactive");
    let mut hog_ids = Vec::new();
    for h in 0..hogs {
        hog_ids.push(
            q.admit((h + 1) as u16, 0, Class::Fair, bore::Burst::new(), now)
                .expect("admit hog"),
        );
    }

    // Warm the hogs up so their burst scores are what a CPU-bound task's really is
    // by the time the interactive vcore starts waking. Without this the comparison
    // is against three freshly-admitted peers, which is not the loaded case.
    q.block(inter, true).ok();
    for _ in 0..(hogs * 16) {
        if let Some((id, _)) = q.dispatch(now) {
            now += HOG_SLICE_NS;
            q.charge(id, HOG_SLICE_NS).ok();
            q.was_preempted(id);
        }
    }

    let mut intervening = Vec::with_capacity(WAKEUPS);
    for _ in 0..WAKEUPS {
        q.wake(inter, now).expect("wake");
        let mut waited = 0u64;
        loop {
            let Some((id, _)) = q.dispatch(now) else { break };
            if id.index() == inter.index() {
                now += INTERACTIVE_RUN_NS;
                q.charge(id, INTERACTIVE_RUN_NS).ok();
                q.relinquished(id);
                q.block(inter, true).ok();
                break;
            }
            now += HOG_SLICE_NS;
            q.charge(id, HOG_SLICE_NS).ok();
            q.was_preempted(id);
            waited += 1;
            if waited > 1000 {
                break; // never reached; a bound so a regression cannot hang the bench
            }
        }
        intervening.push(waited);
    }

    let mut v = intervening.clone();
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    let inter_w = q.get(inter).map(|c| c.weight()).unwrap_or(0);
    let hog_w = q.get(hog_ids[0]).map(|c| c.weight()).unwrap_or(0);
    let inter_s = q.get(inter).map(|c| c.burst.score()).unwrap_or(0);
    let hog_s = q.get(hog_ids[0]).map(|c| c.burst.score()).unwrap_or(0);
    let (_d, _pre, _y, defers) = q.counters();
    println!(
        "rheo wake-to-pick [{hogs} CPU-bound vcores]: P50 {} slices, P95 {} slices, \
         P99 {} slices, max {} slices",
        p(0.50),
        p(0.95),
        p(0.99),
        v[v.len() - 1]
    );
    println!(
        "  BORE: interactive score {inter_s} weight {inter_w}; CPU-bound score {hog_s} \
         weight {hog_w} ({}x)",
        if hog_w == 0 { 0 } else { inter_w / hog_w }
    );
    println!("  EEVDF eligibility deferred a nearer-deadline vcore {defers} times");
    assert!(q.invariant_holds(), "run-queue invariant broken");
}

/// How long a pick costs, in real host nanoseconds. This one *is* wall-clock, and it
/// is legitimate: the code under test is ordinary integer Rust compiled for the host,
/// not a guest under emulation. It says how expensive the ordering decision is, not
/// how fast the OS is.
fn throughput() {
    let mut q = RunQueue::new();
    q.init(Owner::KERNEL);
    for h in 0..16u16 {
        q.admit(h, 0, Class::Fair, bore::Burst::new(), 0)
            .expect("admit");
    }
    const N: usize = 200_000;
    let t0 = Instant::now();
    let mut sink = 0usize;
    for _ in 0..N {
        if let Some(id) = q.pick() {
            sink += id.index() as usize;
        }
    }
    let ns = t0.elapsed().as_nanos() as usize / N;
    println!("rheo pick() over a 16-vcore queue: {ns} ns per decision (host, real time) [{sink:x}]");
}
