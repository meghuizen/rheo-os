//! `librheo-pmem` - the proof that a **typed** memory grant reaches the physical
//! pool its kind names (docs/MEMORY.md 2.1, ARCHITECTURE.md 3 object 5,
//! docs/ARCHITECTURE-DEBT.md 3.6).
//!
//! Object 5 was implemented twice. `mm/grant.rs` is the typed implementation and
//! it does consult the pmem allocator - but it is reachable only from a test
//! kernel. The path a **cell** takes (`SYS_GRANT` -> `grant_create`) recorded the
//! kind and then committed through the DDR allocator regardless, so a cell asking
//! for `Pmem` got DDR with nothing said. That is the failure mode
//! docs/ENGINEERING.md 7 exists to forbid, and it made "PMEM real where a QEMU
//! nvdimm is exposed" true of a test-only type and false of the syscall.
//!
//! This cell asks for `MemKind::Pmem` through that syscall, commits it, and
//! round-trips a pattern through the committed pages. It exits `0x42` on success.
//!
//! The cell **cannot** check which pool it got - that is the point. The
//! unfakeable half is asserted by the `pmem` test kernel on the other side of the
//! trap: the kernel's pmem free-frame count must drop by exactly the pages this
//! cell committed, and every committed frame must fall inside the nvdimm's
//! firmware-reported physical range. A cell has no way to move either number.

#![no_std]
#![no_main]

use librheo::mem::{Grant, MemKind};
use librheo::println;

/// Pages to commit. Small and exact, because the test kernel asserts the pmem
/// allocator's free count fell by precisely this many.
const PAGES: usize = 4;
const LEN: usize = PAGES * 4096;

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// A deterministic pattern, so a zeroed or aliased page is visible as a mismatch.
fn word(i: usize) -> u64 {
    (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xA5A5_5A5A_DEAD_BEEF
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // Reserve + commit `LEN` bytes of **persistent** memory. `alloc` is
    // reserve-then-commit-whole, so on return every page is backed.
    let Some(grant) = Grant::alloc(MemKind::Pmem, LEN) else {
        println!("librheo-pmem: SYS_GRANT(Pmem) refused");
        return 1;
    };
    println!(
        "librheo-pmem: Pmem grant at {:#x}, {} pages committed",
        grant.base(),
        PAGES
    );

    // Write, then read back, through the grant's own pages. This is what proves
    // the frames are genuinely mapped and writable, whichever pool they came
    // from; the *which pool* half is the kernel's assertion.
    let n = LEN / 8;
    // SAFETY: `base()..base()+len` is this cell's committed grant - the kernel
    // mapped every page RW before `alloc` returned.
    let p = grant.base() as *mut u64;
    for i in 0..n {
        unsafe { p.add(i).write_volatile(word(i)) };
    }
    for i in 0..n {
        let got = unsafe { p.add(i).read_volatile() };
        if got != word(i) {
            println!(
                "librheo-pmem: word {i} read back {got:#x}, expected {:#x}",
                word(i)
            );
            return 2;
        }
    }
    println!("librheo-pmem: {n} words round-tripped through the grant");

    // Leave the grant committed: the test kernel reads the pmem allocator's free
    // count *after* the cell exits, and a `Drop` that released the frames would
    // erase the evidence. The cell's address space is torn down by the kernel.
    core::mem::forget(grant);
    OK_CODE
}
