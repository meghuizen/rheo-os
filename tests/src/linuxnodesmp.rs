//! In-QEMU test kernel: **linuxnode on a SECONDARY core** (docs/SMP.md 10.0e).
//!
//! `linuxnode` proves this runtime on the boot CPU. This is the same binary, the same disk,
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
    // `--no-expose-wasm` silences the otherwise stderr "conflicting flags" warning
    // so the captured transcript is exact; UV_THREADPOOL_SIZE=1 keeps libuv's lazy
    // pool minimal (the cell holds up to 8 contexts, node uses ~7).
    disk_runtime::prove(
        "linuxnodesmp",
        "/bin/node",
        &[
            b"node",
            b"--no-expose-wasm",
            b"-e",
            b"console.log(\"rheo:\"+(40+2))",
        ],
        &[
            b"LD_LIBRARY_PATH=/lib:/lib64",
            b"PATH=/bin",
            b"UV_THREADPOOL_SIZE=1",
        ],
        b"rheo:42\n",
        // Node completes fully (prints rheo:42, exits 0), so it is held to the strict
        // exit-0 gate - no thread-abort partial.
        false,
        // **Preemptive dispatch** (docs/SUBSTRATE.md 15, S3'). This is the useful half
        // of that migration's proof: a preemption kernel that only ever preempts a
        // purpose-built spinner has not been tested by anything, and here a real
        // 124 MB V8 + libuv runtime is preempted 17-31 times mid-run and still reaches
        // its exact answer.
        //
        // It was intermittent first - about one run in eight died with SIGSEGV and no
        // output - and rather than being left off, the cause was found: on x86-64 a
        // preempted frame was being resumed through `SYSRET`, which consumes RCX and
        // R11 (docs/LINUX-COMPAT.md). Fixed by frame provenance, and the fix is proven
        // in both directions by the `preempt` kernel's scratch-register phase, which
        // fails deterministically when reverted. Twelve consecutive clean runs here
        // after it; the property, not the run count, is what makes it shippable.
        true,
        // The **W^X exception capability** (docs/ARCHITECTURE.md 5.1), so this
        // runtime's JIT can map its code pages writable-and-executable. Every other
        // kernel in the suite mints nothing of the sort and is refused exactly as
        // before, which is what makes this a capability rather than a setting.
        true,
        None,
        // **On a secondary.** The whole point of this kernel (docs/SMP.md 10.0e).
        true,
    )
}
