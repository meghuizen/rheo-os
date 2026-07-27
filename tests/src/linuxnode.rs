//! In-QEMU test kernel: the **real Node.js binary** runs unmodified under the
//! Linux personality (GOAL-NODE, docs/LINUX-COMPAT.md).
//!
//! The actual production `node` (v22, dynamic, ~124 MB, V8 + libuv) is streamed
//! off a live ext4 disk (`ext4fs`/`ext4plus` + the block cache, GOAL-DISK-2b),
//! demand-paged, and asked to evaluate JavaScript - touching nothing of Node's
//! own code. `--jitless` runs V8's Ignition interpreter, needing no
//! writable-executable code page (W^X is structural, docs/ARCHITECTURE.md 5 - the
//! one `mprotect(RWX)` V8 would issue is refused). Per-context blocking
//! (docs/LINUX-COMPAT.md L4) lets its V8 + libuv threads coordinate, so it prints
//! exactly `rheo:42` and exits 0. **x86-64 only** (no arm64/riscv64 node build -
//! those skip-with-reason). The whole proof lives in the shared
//! [`disk_runtime`] harness; this bin is the `node`-specific launch.

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
        "linuxnode",
        "/bin/node",
        &[
            b"node",
            b"--jitless",
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
        // **Preemptive dispatch** (docs/SUBSTRATE.md 15, S3'): Node runs to its
        // correct answer with the CPU genuinely being taken away from it mid-run,
        // which is the useful half of that migration's proof - a preemption kernel
        // that only ever preempts a purpose-built spinner has not been tested by
        // anything.
        true,
    )
}
