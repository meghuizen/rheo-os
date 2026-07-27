//! `librheo-child` - a tiny native coreutil built on librheo (docs/LIBRHEO.md
//! Phase F) that turns its argument into work + a result: it parses `argv[1]` as
//! a number `n`, prints `child <n>`, and **exits with code `n`**. An
//! orchestrator spawns several of these with different arguments and sums their
//! exit codes - a genuine map (argv fan-out) / reduce (exit-code aggregation)
//! over real processes.
//!
//! It also carries the **spawn-authority** check (docs/ARCHITECTURE-DEBT.md
//! 2.3). A spawned child used to share its parent's capability table, so it
//! inherited the parent's `ObjectKind::Cell` capability and could spawn cells of
//! its own - making `abi.rs`'s claim that spawn authority is "not minted into a
//! spawned child by default" false, and §8.2 property 4 (disjoint capability
//! sets) inapplicable to every parent/child pair in the system. A child now gets
//! its own table holding only what it was explicitly given, so this attempt must
//! fail. It prints one line from a fixed set either way, so the transcript says
//! which happened rather than the test merely passing.

#![no_std]
#![no_main]

extern crate alloc;

use librheo::{println, proc};

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let args = proc::args();
    let n: i32 = args.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
    println!("child {n}");

    // No ambient authority: this cell was never handed a cell-spawn capability,
    // so it cannot create cells. `/bin/echo` exists and the parent can spawn it,
    // which is what makes the refusal about *authority* and not about the path.
    // Neither line starts with "child", so the test can count child *runs* and
    // refusals independently and require them equal - an oracle that does not
    // have to know how many times the orchestrator happens to spawn one (it
    // runs a spawn benchmark too).
    match proc::spawn("/bin/echo", &["echo", "nope"], &[]) {
        Ok(_) => println!("SPAWNED WITHOUT AUTHORITY"),
        Err(_) => println!("no cell capability, spawn refused"),
    }
    n
}
