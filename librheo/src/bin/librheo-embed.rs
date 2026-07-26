//! `librheo-embed` - the **embedded** proof (docs/LIBRHEO.md Phase F). It links
//! only librheo's spine (`--no-default-features`: `cap`+`rt`+`mem`+`sys`, no
//! `term`/`io`/`proc`/`rng`/...) and does a real queue round-trip **directly**
//! over the ring - no strand executor, no `BTreeMap`, no async machinery - so
//! its code section is substantially smaller than a full librheo binary's. It
//! proves librheo scales down to a minimal cell that still reaches the kernel
//! through the one true interface, the queue pair.
//!
//! (The async reactor is still available in the spine; this cell just doesn't
//! use it, so the dead-code eliminator drops the executor + map. The
//! `librheoproc` test asserts this binary's code section is much smaller than a
//! full-featured librheo binary's, and that it runs.)

#![no_std]
#![no_main]

extern crate alloc;

use librheo::sys::{self, Qp};

/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // Bind this cell's queue pair (mapped by the kernel, discovered by _start).
    let Some(info) = sys::queue_info() else {
        return 31;
    };
    // SAFETY: `qp_va` is this cell's mapped, kernel-initialised ring region.
    let qp = unsafe { Qp::attach(info.qp_va as *mut u8) };
    let cap_id = info.cap_id as u32;

    let val = 0xBEEF_1234u32;
    let mut args = [0u8; 24];
    args[..4].copy_from_slice(&val.to_le_bytes());

    // Submit OP_ECHO, ring the doorbell, reap the completion - the whole
    // round-trip by hand, no reactor.
    while !qp.submit(sys::OP_ECHO, 0, cap_id, 0, 1, &args) {
        sys::doorbell();
    }
    sys::doorbell();
    loop {
        if let Some(cqe) = qp.reap() {
            return if cqe.status == sys::STATUS_OK && cqe.result == val {
                OK_CODE
            } else {
                30
            };
        }
        sys::doorbell();
    }
}
