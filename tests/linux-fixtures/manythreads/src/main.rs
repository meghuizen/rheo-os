//! **More execution contexts than the old fixed ceiling** - the end-to-end proof
//! that a Linux cell's context table grows (docs/SUBSTRATE.md pillar 1).
//!
//! The personality used to hold a cell's contexts in a fixed
//! `[[Thread; MAX_THREADS]; MAX_CELLS]` array with `MAX_THREADS = 8`, so a
//! program could have at most 7 threads besides its main one and the 8th
//! `pthread_create` failed with `EAGAIN`. That is not an exotic limit: Node's
//! libuv threadpool is 4 by default, V8 adds helpers, and any `worker_threads`
//! come on top - so an ordinary program crossed it.
//!
//! This fixture spawns [`WORKERS`] = 12 threads and keeps them all alive
//! **simultaneously**, which is what makes the count a real concurrency
//! requirement rather than 12 sequential reuses of one slot: each worker
//! announces its arrival and then refuses to finish until every sibling has
//! arrived too, so at the rendezvous all 12 contexts exist at once. Under the old
//! ceiling it could not have run.
//!
//! The rendezvous is a **spin on an atomic with `yield_now`**, deliberately, not a
//! `Barrier`. A `Barrier` across 13 parties needs broadcast futex wake-ups with
//! more fidelity than this personality's cooperative FIFO `FUTEX_WAKE` provides
//! (docs/LINUX-COMPAT.md, the futex row: wake is FIFO and priority inheritance is
//! a documented TODO), so a barrier here would be testing that limitation instead
//! of the context count. `sched_yield` *is* real, and a cooperative scheduler is
//! exactly where an explicit yield is the right rendezvous primitive.
//!
//! Built for `<arch>-unknown-linux-gnu` with `+crt-static` (static glibc,
//! ET_EXEC) and run as a `Personality::Linux` cell, exactly like the 4-thread L4
//! fixture beside it - which is left untouched, so the pre-existing proof still
//! holds unedited.
//!
//! The result is scheduling-independent (the per-thread sums commute), so the
//! kernel test asserts exact stdout and exit code.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

/// Threads spawned, chosen to exceed the old `MAX_THREADS = 8` unambiguously.
const WORKERS: u64 = 12;

fn main() {
    let counter = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(Mutex::new(0u64));
    // Arrivals at the rendezvous. Every worker must be live at the same time, or
    // the test would pass with a context table of one reused slot.
    let arrived = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();

    let mut handles = Vec::new();
    for id in 1..=WORKERS {
        let counter = Arc::clone(&counter);
        let total = Arc::clone(&total);
        let arrived = Arc::clone(&arrived);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            // Triangular sum 1..=id*10 - distinct per thread, order-independent.
            let local: u64 = (1..=id * 10).sum();
            counter.fetch_add(1, Ordering::SeqCst);
            *total.lock().unwrap() += local;
            tx.send(local).unwrap();
            // Hold this context open until every sibling has arrived, yielding so a
            // cooperative scheduler can run them. All WORKERS contexts are live
            // simultaneously at the moment the last one arrives.
            arrived.fetch_add(1, Ordering::SeqCst);
            while arrived.load(Ordering::SeqCst) < WORKERS as usize {
                thread::yield_now();
            }
        }));
    }
    drop(tx); // so the receive loop ends once every worker has sent

    let channel_sum: u64 = rx.iter().sum();
    for h in handles {
        h.join().unwrap();
    }

    let threads = counter.load(Ordering::SeqCst);
    let total = *total.lock().unwrap();
    println!("contexts {threads} total {total} channel {channel_sum}");
    std::process::exit(threads as i32);
}
