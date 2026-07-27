// The **Linux side** of the scheduler-responsiveness axis, measured on this host.
//
// This is the axis a CachyOS-class build is tuned for, and the one docs/SUBSTRATE.md
// pillar 3 adopts the same frontier for (EEVDF + BORE burstiness): when every core is
// saturated by CPU-bound work, how long does an interactive task wait between being
// **woken** and actually **running**?
//
// The measurement is a wakeup, not a sleep. An earlier version timed
// `thread::sleep` overshoot and measured mostly hrtimer slack - a ~76 us floor that
// barely moved with load, which is a property of the timer subsystem and not of the
// scheduler. So this pairs two threads instead:
//
//   * a **waker** publishes a timestamp and signals a condition variable;
//   * a **sleeper** blocked on that variable wakes and reads the clock.
//
// The difference is wake-to-run: the time from "this task became runnable" to "this
// task is executing", which is exactly what a run queue's ordering decides. N
// CPU-bound hog threads saturate the machine around them; each hog never blocks, so
// the only way the sleeper runs promptly is if the scheduler puts it in front.
//
// Reported as a distribution, because a mean hides exactly the tail the axis is
// about. Jitter is P95 - P50, the one definition this tree uses everywhere
// (docs/SUBSTRATE.md pillar 8).
//
// It is a *host* measurement with real caches, real TLBs and a real scheduler, so the
// nanoseconds are real nanoseconds - unlike anything QEMU TCG can produce. It says
// nothing at all about rheo-os; the rheo side of this axis is `rheo_sched.rs`, which
// is deterministic and in different units. Read comparison/linux/README.md before
// putting the two side by side.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Wakeups sampled per round. Enough for a P99 to mean something.
const SAMPLES: usize = 5000;

fn main() {
    let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("host: {cpus} CPUs, Linux scheduler under test");
    println!("metric: wake-to-run delay (condvar signal -> woken thread executing)");
    for hogs in [0usize, cpus, cpus * 2] {
        let delays = run(hogs);
        report(hogs, cpus, &delays);
    }
}

/// One rendezvous slot: the waker's timestamp plus a "there is work" flag.
struct Slot {
    stamp: Mutex<(bool, Instant)>,
    cv: Condvar,
    done: Mutex<bool>,
    done_cv: Condvar,
}

/// Run one round with `hogs` CPU-bound threads and return the woken thread's
/// wake-to-run delays, in nanoseconds.
fn run(hogs: usize) -> Vec<u64> {
    let stop = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(AtomicU64::new(0));
    let mut hog_handles = Vec::new();
    for _ in 0..hogs {
        let stop = stop.clone();
        let sink = sink.clone();
        hog_handles.push(thread::spawn(move || {
            let mut acc = 1u64;
            while !stop.load(Ordering::Relaxed) {
                for k in 0..100_000u64 {
                    acc = acc.wrapping_add(k ^ 0x5f).wrapping_mul(3);
                }
            }
            sink.fetch_add(acc, Ordering::Relaxed);
        }));
    }

    let slot = Arc::new(Slot {
        stamp: Mutex::new((false, Instant::now())),
        cv: Condvar::new(),
        done: Mutex::new(false),
        done_cv: Condvar::new(),
    });

    // The sleeper: block, wake, measure, hand the sample back. It does no work
    // between wakeups, so it is the interactive task in this picture.
    let s = slot.clone();
    let sleeper = thread::spawn(move || {
        let mut out = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let mut g = s.stamp.lock().unwrap();
            while !g.0 {
                g = s.cv.wait(g).unwrap();
            }
            let sent = g.1;
            g.0 = false;
            drop(g);
            out.push(sent.elapsed().as_nanos() as u64);
            let mut d = s.done.lock().unwrap();
            *d = true;
            s.done_cv.notify_one();
        }
        out
    });

    // Let the hogs actually get onto the CPUs before sampling.
    thread::sleep(Duration::from_millis(50));

    for _ in 0..SAMPLES {
        {
            let mut g = slot.stamp.lock().unwrap();
            // Stamped under the lock, immediately before the notify, so the delay
            // measured is the wakeup and not the bookkeeping around it.
            g.1 = Instant::now();
            g.0 = true;
        }
        slot.cv.notify_one();
        let mut d = slot.done.lock().unwrap();
        while !*d {
            d = slot.done_cv.wait(d).unwrap();
        }
        *d = false;
    }

    let delays = sleeper.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    for h in hog_handles {
        let _ = h.join();
    }
    delays
}

fn report(hogs: usize, cpus: usize, delays: &[u64]) {
    let mut v = delays.to_vec();
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    let (p50, p95, p99, max) = (p(0.50), p(0.95), p(0.99), v[v.len() - 1]);
    let load = if hogs == 0 {
        "idle".to_string()
    } else {
        format!("{hogs} hogs on {cpus} CPUs")
    };
    println!(
        "linux wake-to-run [{load}]: P50 {p50} ns, P95 {p95} ns, P99 {p99} ns, \
         max {max} ns, jitter (P95-P50) {} ns",
        p95.saturating_sub(p50)
    );
}
