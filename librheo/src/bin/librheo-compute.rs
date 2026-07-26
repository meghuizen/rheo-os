//! `librheo-compute` - the Phase C proof program (docs/LIBRHEO.md): parallel &
//! accelerated compute with QoS. It exercises the whole Phase C surface and
//! exits `0x42` only if every check passes:
//!
//! - **parallel aggregation across strands** (`compute::map_reduce`): a
//!   columnar `SUM WHERE odd` over an in-memory dataset, fanned across N
//!   strands and reduced - the exact aggregate is asserted.
//! - **strand primitives**: `compute::scan` (blocked parallel prefix sum) and
//!   `compute::parallel_for` (disjoint-block loop), each verified.
//! - **userspace dependency-graph submission** (`compute::GraphBuilder`): build
//!   `n0=const(6); n1=n0+1; n2=n1*n0` and submit it to the CPU engine over the
//!   async queue (`OP_GRAPH_SUBMIT`); the result (42) is asserted.
//! - **reservations / QoS** (`sched`): a feasible CPU reservation is admitted
//!   (committed ppm > 0), an infeasible one is cleanly rejected with a typed
//!   error (not a fault), and the `PeriodicTask`/`TimingReport` surface is used.
//! - **engine introspection** (`compute::Engine::info`): the engine kind +
//!   measured throughput are printed (visible, not asserted-exact).
//!
//! The `librheocompute` test kernel asserts the `0x42` exit on all three ISAs.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use librheo::compute::{self, Engine, EngineKind, GraphBuilder, In};
use librheo::sched::{PeriodicTask, Priority, Reservation, ReserveError, TimingReport};
use librheo::{println, rt};

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;
/// Dataset rows and the strand/partition fan-out.
const LEN: usize = 4096;
const N: usize = 8;

/// Async-stage failure code (0 = ok), read after `block_on`.
static CODE: AtomicI32 = AtomicI32::new(0);
/// The parallel aggregate, computed in the async root and checked after.
static AGG: AtomicU64 = AtomicU64::new(0);
/// The submitted graph's result.
static GRAPH_RESULT: AtomicU64 = AtomicU64::new(0);

fn fail(c: i32) {
    CODE.store(c, Ordering::Relaxed);
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // In-memory columnar dataset: col[i] = i (u32). The parallel scan sums the
    // odd values under a predicate, a mini analytical aggregation.
    let mut data: Vec<u32> = Vec::with_capacity(LEN);
    for i in 0..LEN {
        data.push(i as u32);
    }
    let base = data.as_ptr() as usize;

    // Async stages: map_reduce + scan + parallel_for + graph submit.
    rt::block_on(work(base));
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }

    // (a) the parallel aggregation is exact: SUM of odd i in [0, LEN) = (LEN/2)^2.
    let agg = AGG.load(Ordering::Relaxed);
    let half = (LEN / 2) as u64;
    if agg != half * half {
        return 40;
    }

    // (b) the graph computed (6 + 1) * 6 = 42.
    if GRAPH_RESULT.load(Ordering::Relaxed) != 42 {
        return 41;
    }

    // (c) reservations / QoS admission.
    // A feasible reservation (30% of the CPU) is admitted.
    let r = match Reservation::request(3, 10, 10, 16) {
        Ok(r) => r,
        Err(_) => return 50,
    };
    if r.committed_ppm() == 0 {
        return 51;
    }
    if TimingReport::read().committed_ppm != r.committed_ppm() {
        return 52;
    }
    // Infeasible params (budget > period) are cleanly rejected, not faulted.
    match Reservation::request(20, 10, 10, 0) {
        Err(ReserveError::BadParams) => {}
        _ => return 53,
    }
    // Over-committing the CPU (another 80% on top of the held 30%) is refused.
    match Reservation::request(8, 10, 10, 0) {
        Err(ReserveError::Overcommit) => {}
        _ => return 54,
    }
    // The lattice-rt PeriodicTask builder runs the same admission.
    let task = PeriodicTask::new(20)
        .budget(2)
        .deadline(20)
        .priority(Priority::High);
    if task.get_priority() != Priority::High {
        return 55;
    }
    let r2 = match task.build() {
        Ok(r) => r,
        Err(_) => return 56,
    };
    // Releasing both (RAII drop) returns the CPU to fully uncommitted.
    drop(r2);
    drop(r);
    if TimingReport::read().committed_ppm != 0 {
        return 57;
    }

    // (d) engine introspection: which executor am I on, measured?
    let info = Engine::info();
    println!(
        "librheo-compute: engine kind={:?} measured_cost_ticks={} preemption={:?}",
        info.kind, info.measured_cost_ticks, info.preemption
    );
    if info.kind != EngineKind::Cpu {
        return 60;
    }

    println!("librheo-compute: map_reduce SUM={agg} graph=42 reservations+engine OK ({N} strands)");
    OK_CODE
}

/// The async root: parallel aggregation, the scan/for primitives, and the graph
/// submission. Records failures via [`fail`]; the aggregate and graph result go
/// to statics read after `block_on`.
async fn work(base: usize) {
    // (a) parallel aggregation across strands: SUM(col) WHERE col value is odd.
    let agg = compute::map_reduce(
        LEN,
        N,
        move |lo, hi| {
            let mut s = 0u64;
            for i in lo..hi {
                // SAFETY: `[lo, hi)` indexes the live dataset at `base`.
                let v = unsafe { *((base as *const u32).add(i)) };
                if v & 1 == 1 {
                    s += v as u64;
                }
            }
            s
        },
        |a, b| a + b,
        0u64,
    )
    .await;
    AGG.store(agg, Ordering::Relaxed);

    // scan: inclusive prefix of [1; 16] must be [1, 2, ..., 16].
    let mut v: Vec<u64> = alloc::vec![1u64; 16];
    compute::scan(&mut v, 4).await;
    for (i, &x) in v.iter().enumerate() {
        if x != i as u64 + 1 {
            fail(30);
            return;
        }
    }

    // parallel_for: fill 64 elements across 8 disjoint blocks.
    let mut f: Vec<u64> = alloc::vec![0u64; 64];
    let fbase = f.as_mut_ptr() as usize;
    compute::parallel_for(64, 8, move |lo, hi| {
        for i in lo..hi {
            // SAFETY: disjoint block within `f`.
            unsafe {
                *((fbase as *mut u64).add(i)) = i as u64;
            }
        }
    })
    .await;
    for (i, &x) in f.iter().enumerate() {
        if x != i as u64 {
            fail(31);
            return;
        }
    }

    // (b) build + submit a real dependency graph: (6 + 1) * 6 = 42.
    let mut g = GraphBuilder::new();
    let n0 = g.constant(6);
    let n1 = g.add(In::Node(n0), In::Imm(1));
    let _n2 = g.mul(In::Node(n1), In::Node(n0));
    match g.submit().await {
        Ok(r) => GRAPH_RESULT.store(r, Ordering::Relaxed),
        Err(_) => fail(32),
    }
}
