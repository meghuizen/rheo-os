//! In-QEMU test kernel: **linuxclaude on a SECONDARY core** (docs/SMP.md 10.0e).
//!
//! `linuxclaude` proves this runtime on the boot CPU. This is the same binary, the same disk,
//! the same JIT authority and the same preemptive dispatch, held to the same strict
//! gate - the only difference is `on_secondary`, which publishes the installed cell to
//! a secondary through `smp::run_cell_on_secondary` and has the primary wait.
//!
//! Why it is worth its own kernel rather than a phase: the primary-CPU proof is the
//! baseline every claim about this runtime rests on, and a boot that runs it somewhere
//! else must not be able to weaken it. Two kernels, two independent results.
//!
//! x86-64 only, as its counterpart: there is no arm64/riscv64 build of this runtime
//! here, so those ISAs get no drive and the harness skips with a reason.
//!
//! What this does **not** show: parallel execution of the runtime's own threads. Its
//! contexts are scheduled cooperatively within whichever core runs the cell, exactly as
//! on the primary; running them on several cores at once needs the per-cell locking
//! docs/SMP.md 10.2 describes and is not built.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "disk_runtime.rs"]
mod disk_runtime;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    disk_runtime::prove(
        "linuxclaudesmp",
        "/bin/claude",
        &[b"claude", b"--version"],
        &[
            b"LD_LIBRARY_PATH=/lib:/lib64",
            b"PATH=/bin",
            b"HOME=/",
            // Claude Code checks for a terminal and for update channels; neither is
            // reachable here, and CI=1 is the documented way to tell a tool it is not
            // being run interactively. Not a workaround for a missing capability - the
            // cell genuinely has no tty and no network.
            b"CI=1",
        ],
        b"2.1.220 (Claude Code)\n",
        // Held to the strict gate: it prints its version and exits 0.
        false,
        // Preemptive, as for Node and Bun.
        true,
        // The W^X exception capability, so JavaScriptCore's JIT can map its code pages
        // (docs/ARCHITECTURE.md 5.1).
        true,
        None,
        // **On a secondary.** The whole point of this kernel (docs/SMP.md 10.0e).
        true,
    )
}
