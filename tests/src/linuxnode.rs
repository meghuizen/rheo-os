//! In-QEMU test kernel: the **real Node.js binary** runs unmodified under the
//! Linux personality (GOAL-NODE, docs/LINUX-COMPAT.md).
//!
//! The actual production `node` (v22, dynamic, ~124 MB, V8 + libuv) is streamed
//! off a live ext4 disk (`ext4fs`/`ext4plus` + the block cache, GOAL-DISK-2b),
//! demand-paged, and asked to evaluate JavaScript - touching nothing of Node's
//! own code. **JIT enabled**: the cell is minted the W^X exception capability (see
//! the note at the `prove` call), and the run log shows V8 taking it - `mprotect
//! PROT_WRITE|PROT_EXEC granted`. W^X is still structural (docs/ARCHITECTURE.md 5):
//! every other kernel in the suite mints nothing of the sort and is refused, which
//! is what makes this a capability rather than a setting. Per-context blocking
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
    )
}
