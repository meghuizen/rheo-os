//! Unpatched multi-threaded Rust `std` fixture for the Linux personality
//! milestone L4 (docs/LINUX-COMPAT.md). Built for `<arch>-unknown-linux-gnu`
//! with `+crt-static` (static glibc, ET_EXEC) and run as a `Personality::Linux`
//! cell. Exercises clone + futex + per-thread TLS + join through real
//! `std::thread`, `mpsc`, `Mutex`, and `Arc<AtomicUsize>`.
//!
//! The result is scheduling-independent (the per-thread sums commute), so the
//! kernel test asserts exact stdout and exit code.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

fn main() {
    let counter = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(Mutex::new(0u64));
    let (tx, rx) = mpsc::channel();

    let mut handles = Vec::new();
    for id in 1..=4u64 {
        let counter = Arc::clone(&counter);
        let total = Arc::clone(&total);
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            // Triangular sum 1..=id*10 - distinct per thread, order-independent.
            let local: u64 = (1..=id * 10).sum();
            counter.fetch_add(1, Ordering::SeqCst);
            *total.lock().unwrap() += local;
            tx.send(local).unwrap();
        }));
    }
    drop(tx); // so the receive loop ends once every worker has sent

    let channel_sum: u64 = rx.iter().sum();
    for h in handles {
        h.join().unwrap();
    }

    let threads = counter.load(Ordering::SeqCst);
    let total = *total.lock().unwrap();
    println!("threads {threads} total {total} channel {channel_sum}");
    std::process::exit(threads as i32);
}
