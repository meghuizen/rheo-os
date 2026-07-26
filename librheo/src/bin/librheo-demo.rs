//! `librheo-demo` - the Phase A proof program (docs/LIBRHEO.md). It exercises
//! the foundation spine: the growable heap (`Vec`/`String`), the per-cell RNG
//! (a library call, no syscall on the fast path), a capability-typed handle
//! (compile-time attenuation), and - the headline - an **async queue round
//! trip**: strands that submit `OP_ECHO` over the cell's real queue pair, park
//! on the completion token, and are woken by the reactor. It exits with a
//! distinctive code only if every echo came back correct; the `librhearun`
//! test asserts that code.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use librheo::cap::{Cap, READ, ReadOnly, ReadWrite, WRITE};
use librheo::{cap, rng, rt, sys};

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// Number of async echo strands.
const N: u32 = 8;

static DONE: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);

/// Pack a u32 into the 24-byte submission payload (little-endian, low 4 bytes).
fn u32_arg(v: u32) -> [u8; 24] {
    let mut a = [0u8; 24];
    a[..4].copy_from_slice(&v.to_le_bytes());
    a
}

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // --- heap: alloc collections work over the growable heap ---
    let mut v: Vec<u32> = Vec::new();
    for i in 0..1000 {
        v.push(i);
    }
    let sum: u32 = v.iter().sum(); // 0..999 -> 499500
    if sum != 499_500 {
        return 10;
    }
    let mut s = String::new();
    let _ = write!(s, "sum={sum}");
    if s != "sum=499500" {
        return 11;
    }

    // --- rng: the per-cell DRBG as a library call ---
    let a = rng::next_u64();
    let b = rng::next_u64();
    if a == b || (a == 0 && b == 0) {
        return 12; // degenerate output
    }

    // --- cap: a capability-typed handle; attenuation is a type operation ---
    let caps = cap::cap_set();
    let q = caps.queue();
    if !(q.allows(READ) && q.allows(WRITE)) {
        return 13;
    }
    // Narrowing type-checks; widening would not compile (SubsetOf).
    let rw: Cap<sys::Qp, ReadWrite> = Cap::from_handle(caps.queue_cap_id() as u64);
    let ro: Cap<sys::Qp, ReadOnly> = rw.attenuate::<ReadOnly>();
    if !(ro.allows(READ) && !ro.allows(WRITE)) {
        return 14;
    }

    // --- the headline: an async queue round trip across N strands ---
    rt::block_on(async {
        let mut handles = Vec::new();
        for i in 0..N {
            handles.push(rt::spawn(async move {
                let val = 0xA000_0000u32 + i;
                let cqe = rt::submit_and_await(sys::OP_ECHO, u32_arg(val)).await;
                if cqe.status == sys::STATUS_OK && cqe.result == val {
                    DONE.fetch_add(1, Ordering::Relaxed);
                } else {
                    FAIL.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().await;
        }
    });

    if FAIL.load(Ordering::Relaxed) != 0 || DONE.load(Ordering::Relaxed) != N {
        librheo::println!("librheo-demo: async echo FAILED");
        return 15;
    }

    // The human-readable marker (routed to the console by the test kernel).
    librheo::println!("librheo-demo: heap+rng+cap+async-echo {N}/{N} OK");
    OK_CODE
}
