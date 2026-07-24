// Baseline: Rust std::thread - the default Rust/Linux threading (one OS
// thread per spawn, a clone(2) and a kernel stack each). This is what the
// strand model replaces for the common "many small tasks" case.

use std::thread;
use std::time::Instant;

fn main() {
    // OS threads are heavy; use fewer than the strand count.
    const N: usize = 20_000;
    // Warm up the allocator/thread machinery.
    {
        let mut hs = Vec::with_capacity(1000);
        for _ in 0..1000 {
            hs.push(thread::spawn(|| {}));
        }
        for h in hs {
            h.join().unwrap();
        }
    }

    let t = Instant::now();
    let mut hs = Vec::with_capacity(N);
    for _ in 0..N {
        hs.push(thread::spawn(|| {}));
    }
    for h in hs {
        h.join().unwrap();
    }
    let e = t.elapsed();
    println!(
        "std::thread spawn+join    : {:>7.1} ns/thread {:>7.3} M threads/s",
        e.as_nanos() as f64 / N as f64,
        N as f64 / e.as_secs_f64() / 1e6
    );
}
