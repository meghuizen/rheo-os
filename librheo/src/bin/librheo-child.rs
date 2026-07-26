//! `librheo-child` - a tiny native coreutil built on librheo (docs/LIBRHEO.md
//! Phase F) that turns its argument into work + a result: it parses `argv[1]` as
//! a number `n`, prints `child <n>`, and **exits with code `n`**. An
//! orchestrator spawns several of these with different arguments and sums their
//! exit codes - a genuine map (argv fan-out) / reduce (exit-code aggregation)
//! over real processes.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::{println, proc};

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let args = proc::args();
    let n: i32 = args.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
    println!("child {n}");
    n
}
