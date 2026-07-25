//! `librheo-orch` - the direct **spawn / wait / timer** proof (docs/LIBRHEO.md
//! Phase F). An orchestrator cell that, in order:
//!
//! - spawns `/bin/echo "hello world"`, awaits it, and checks it exited 0;
//! - **fans out** three `/bin/child` cells with arguments `3`, `4`, `5`, awaits
//!   each, and **reduces** their exit codes to a sum (map over argv, reduce over
//!   exit codes = 12) - a real process-level map/reduce;
//! - reads the monotonic clock, `time::sleep`s, and checks the clock advanced
//!   (the one-shot timer woke the parked strand).
//!
//! It exits `0x42` only if every stage passed; the `librheoproc` test asserts
//! that code and the spawned children's captured output. All async: each `wait`
//! parks a strand while the child runs cooperatively. It also emits `BENCH`
//! lines (deterministic under QEMU `-icount`) for the async round-trip and
//! spawn+wait path lengths.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use librheo::time::{self, Duration};
use librheo::{println, proc, rt, sys};

const OK_CODE: i32 = 0x42;

static FAIL: AtomicU32 = AtomicU32::new(0);
static ECHO_OK: AtomicU32 = AtomicU32::new(0);
static AGG: AtomicU64 = AtomicU64::new(0);
static SLEPT: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(async {
        // 1. spawn + wait a native coreutil.
        match proc::spawn("/bin/echo", &["echo", "hello", "world"], &[]) {
            Ok(child) => {
                if child.wait().await == 0 {
                    ECHO_OK.store(1, Ordering::Relaxed);
                } else {
                    FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                FAIL.fetch_add(1, Ordering::Relaxed);
            }
        }

        // 2. fan out N children with per-child arguments; reduce exit codes.
        let mut sum = 0u64;
        for n in [3u64, 4, 5] {
            let arg = format!("{n}");
            match proc::spawn("/bin/child", &["child", &arg], &[]) {
                Ok(child) => sum += child.wait().await,
                Err(_) => {
                    FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        AGG.store(sum, Ordering::Relaxed);

        // 3. the one-shot timer: the clock advances across a sleep.
        let start = time::now();
        time::sleep(Duration::from_micros(4)).await;
        if start.elapsed_ticks() > 0 {
            SLEPT.store(1, Ordering::Relaxed);
        }

        // --- Phase F micro-benchmarks. Under QEMU `-icount shift=0` the tick
        // counter advances with retired instructions, so these are deterministic
        // instruction *path lengths* (docs/TOOLING.md 4), not wall-clock; under a
        // normal boot the numbers are meaningless and ignored. Machine-readable
        // `BENCH` lines, like tests/src/bench_core.rs. ---
        const RT_OPS: u64 = 32;
        let t0 = time::now();
        for _ in 0..RT_OPS {
            let _ = rt::submit_and_await(sys::OP_ECHO, [0u8; 24]).await;
        }
        let rt_ticks = t0.elapsed_ticks();
        println!(
            "BENCH librheo_async_roundtrip ops={RT_OPS} ticks={rt_ticks} per_op={}",
            rt_ticks / RT_OPS
        );

        const SW_OPS: u64 = 8;
        let t1 = time::now();
        for _ in 0..SW_OPS {
            if let Ok(c) = proc::spawn("/bin/child", &["child", "0"], &[]) {
                let _ = c.wait().await;
            }
        }
        let sw_ticks = t1.elapsed_ticks();
        println!(
            "BENCH librheo_spawn_wait ops={SW_OPS} ticks={sw_ticks} per_op={}",
            sw_ticks / SW_OPS
        );
    });

    if FAIL.load(Ordering::Relaxed) != 0
        || ECHO_OK.load(Ordering::Relaxed) != 1
        || AGG.load(Ordering::Relaxed) != 12
        || SLEPT.load(Ordering::Relaxed) != 1
    {
        println!(
            "librheo-orch: FAIL (echo_ok={} agg={} slept={} fail={})",
            ECHO_OK.load(Ordering::Relaxed),
            AGG.load(Ordering::Relaxed),
            SLEPT.load(Ordering::Relaxed),
            FAIL.load(Ordering::Relaxed),
        );
        return 20;
    }
    println!("librheo-orch: spawn+wait OK, map/reduce agg=12, timer woke");
    OK_CODE
}
