//! In-QEMU test kernel: **the real Bun binary on a SECONDARY core** (docs/SMP.md 10.0e).
//!
//! `linuxbun` proves Bun on the boot CPU - streamed off a live ext4 disk, JSC brought up
//! with its JIT behind the W^X exception, evaluating JavaScript and exiting 0. What it does
//! not say is whether any of that works on a core that did not boot the machine, and that
//! is the question a multi-core kernel has to answer about its own workloads.
//!
//! It was worth asking rather than reasoning about. The prediction written down before
//! running it was that this would need *threads of one Linux cell across cores* - the finer
//! per-cell locking docs/SMP.md 10.2 describes - because Bun spawns a worker. That
//! prediction was **wrong in an instructive way**: Bun's contexts are scheduled
//! cooperatively *within* whichever core runs the cell, exactly as they are on the primary,
//! so nothing about them has to change for the cell to sit on a secondary. Parallel
//! execution of those contexts is a separate capability, and this needs none of it.
//!
//! Same binary, same disk, same JIT authority, same preemptive dispatch as `linuxbun`, and
//! the same strict gate - exact stdout, exit 0. The only difference is `on_secondary`, which
//! publishes the installed cell to a secondary through `smp::run_cell_on_secondary` and has
//! the primary wait. x86-64 only, as `linuxbun`: there is no arm64/riscv64 bun build here,
//! so those ISAs get no drive and the harness skips with a reason.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "disk_runtime.rs"]
mod disk_runtime;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    disk_runtime::prove(
        "linuxbunsmp",
        "/bin/bun",
        &[b"bun", b"-e", b"console.log(\"rheo:\"+(40+2))"],
        &[b"LD_LIBRARY_PATH=/lib:/lib64", b"PATH=/bin", b"TMPDIR=/tmp"],
        b"rheo:42\n",
        // No partial accepted, exactly as `linuxbun`.
        false,
        // Preemptive dispatch, so the worker is scheduled the same way it is on the primary.
        true,
        // The W^X exception capability, so JSC's JIT can map its code pages.
        true,
        // One invocation. The `bun:ffi` tile call is `linuxbun`'s; repeating it here would
        // add time without adding a claim about which core ran it.
        None,
        // **On a secondary.** The whole point of this kernel.
        true,
    )
}
