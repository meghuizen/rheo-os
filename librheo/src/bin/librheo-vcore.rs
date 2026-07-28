//! `librheo-vcore` - **a loaded cell running the multi-vcore strand executor**
//! (docs/CONCURRENCY.md, docs/SUBSTRATE.md pillar 3).
//!
//! This is the assembly of pieces proven separately: per-vcore trap frames and FP
//! areas, a per-vcore queue ring, a per-vcore strand executor with a `Send`-bounded
//! shared injector, a multi-core-safe allocator, and `SYS_VCORE_INFO` so a context
//! can key all of it on its own index. Every one of those has its own proof; what
//! had never been shown is a real loaded ELF using them together.
//!
//! **One binary, two contexts, one entry point.** Both vcores enter at `_start`;
//! librheo's crt0 branches on `sys::vcore_index()` so the secondary does not redo
//! one-time process setup. `main` then branches the same way, which is the whole
//! point of the verb: the cell is not *told* its role by its launcher, it asks.
//!
//! The claim is deliberately the **deterministic** one. Vcore 0 spawns the shared
//! work and does **not** drain it; only vcore 1 runs the executor. So if the run
//! succeeds, every strand was executed by a context that did not create it - which
//! is the crossing itself, not a probability. (Both draining concurrently is proven
//! in kernel context by the `smp` kernel, where the split can be reported without
//! being asserted; a loaded cell adds nothing to that argument and would only make
//! this exit code a coin flip.)
//!
//! Exit: `0x42` from vcore 0 via `SYS_EXIT_GROUP` once it has verified everything.
//! `SYS_EXIT_GROUP` rather than `SYS_EXIT` so the cell's status is decided by the
//! context that did the checking, whichever vcore happens to finish last.

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};
use librheo::sys;

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: u64 = 0x42;

/// Shared strands to run. Small: the point is that they cross vcores, not throughput.
const WORK: usize = 32;

/// Set by vcore 0 once the injector is filled. Vcore 1 yields until it sees this -
/// yields rather than spins, so the CPU goes back to the kernel while it waits.
static READY: AtomicUsize = AtomicUsize::new(0);
/// Times each strand ran. Every entry must end at exactly 1.
static RAN: [AtomicUsize; WORK] = [const { AtomicUsize::new(0) }; WORK];
/// Strands finished, so vcore 0 knows when to check.
static DONE: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let Some(info) = sys::vcore_info() else {
        return 31;
    };
    if info.count < 2 {
        // Launched with one vcore: nothing to cross to. Report it rather than
        // pretending the phase ran.
        return 32;
    }
    if info.index == 0 { vcore0() } else { vcore1() }
}

/// Vcore 0: fill the injector, release vcore 1, wait, verify, end the cell.
///
/// It never calls `run`, so its own executor stays empty - which is what makes
/// "every strand ran on the other vcore" a fact about this run rather than a
/// likely outcome.
fn vcore0() -> i32 {
    for i in 0..WORK {
        librheo::rt::spawn_shared(async move {
            librheo::rt::yield_now().await;
            RAN[i].fetch_add(1, Ordering::AcqRel);
            DONE.fetch_add(1, Ordering::AcqRel);
        });
    }
    READY.store(1, Ordering::Release);

    // Hand the CPU on until the work is done. A bounded number of yields, so a
    // sibling that never runs ends the cell with a distinct code instead of hanging.
    let mut spins = 0u32;
    while DONE.load(Ordering::Acquire) < WORK {
        sys::yield_cell();
        spins += 1;
        if spins > 200_000 {
            return 33;
        }
    }

    for c in RAN.iter() {
        if c.load(Ordering::Acquire) != 1 {
            return 34;
        }
    }
    // The crossing, checked rather than assumed: this vcore took none of them and
    // the other took all of them.
    if librheo::rt::shared_taken(0) != 0 {
        return 35;
    }
    if librheo::rt::shared_taken(1) != WORK as u64 {
        return 36;
    }
    // End the whole cell, so the status is this context's verdict.
    sys::exit(OK_CODE);
}

/// Vcore 1: wait for the injector to be filled, then drain it.
fn vcore1() -> i32 {
    let mut spins = 0u32;
    while READY.load(Ordering::Acquire) == 0 {
        sys::yield_cell();
        spins += 1;
        if spins > 200_000 {
            return 37;
        }
    }
    // Runs every shared strand this cell has, including the `yield_now` inside each
    // one - so the executor genuinely interleaves them rather than completing each
    // in its first poll.
    librheo::rt::run();
    // Nothing left for this context. Ending it does **not** end the cell: the last
    // vcore out does that, and vcore 0 is still checking (docs/SMP.md 10.0a).
    0
}
