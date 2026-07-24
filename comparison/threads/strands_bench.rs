// rheo-os strand runtime, measured on the host with the EXACT executor the
// OS ships (included verbatim from runtime/src/strand.rs). Same framing as
// comparison/rng: the strand model is a userspace mechanism, so running its
// real logic natively measures the mechanism's cost against Linux/Go/Python.
//
// Two numbers: (1) spawn+run+teardown of a trivial strand - the "light
// thread, quick to set up and break down" claim; (2) context switch cost via
// yield_now. Both are pure userspace: no syscall, no kernel stack.

extern crate alloc;
use std::time::Instant;

// Wrap the verbatim executor in a module so its `//!` header stays legal
// under include!, and the exact OS code is what runs.
mod strand {
    include!("../../runtime/src/strand.rs");
}
use strand::{reset, run, spawn, yield_now};

fn main() {
    // ---- spawn + run + teardown throughput ----
    const ROUNDS: usize = 40;
    const N: usize = 100_000;
    // Warm up (size the slab; steady state reuses freed slots).
    for _ in 0..N {
        let _ = spawn(async {});
    }
    run();

    let t = Instant::now();
    for _ in 0..ROUNDS {
        for _ in 0..N {
            let _ = spawn(async {});
        }
        run();
    }
    let e = t.elapsed();
    let total = (ROUNDS * N) as f64;
    println!(
        "strand spawn+run+teardown : {:>7.1} ns/task   {:>7.2} M tasks/s",
        e.as_nanos() as f64 / total,
        total / e.as_secs_f64() / 1e6
    );

    // ---- context switch (cooperative yield) ----
    reset();
    const K: usize = 2_000_000;
    let _ = spawn(async {
        for _ in 0..K {
            yield_now().await;
        }
    });
    let _ = spawn(async {
        for _ in 0..K {
            yield_now().await;
        }
    });
    let t = Instant::now();
    run();
    let e = t.elapsed();
    let switches = (2 * K) as f64;
    println!(
        "strand context switch     : {:>7.1} ns/switch {:>7.2} M switch/s",
        e.as_nanos() as f64 / switches,
        switches / e.as_secs_f64() / 1e6
    );
}
