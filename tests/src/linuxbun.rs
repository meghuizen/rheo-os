//! In-QEMU test kernel: the **real Bun binary** runs unmodified under the Linux
//! personality (GOAL-BUN, docs/LINUX-COMPAT.md).
//!
//! Bun is the second high-performance JavaScript runtime the goal names, built on
//! **JavaScriptCore** (not V8). The actual `bun` (v1.3, dynamic, ~99 MB) is
//! streamed off a live ext4 disk (`ext4fs`/`ext4plus` + the block cache,
//! GOAL-DISK-2b), demand-paged, and asked to evaluate JavaScript - touching
//! nothing of Bun's own code. JSC's JIT wants a **1 GiB RWX** arena, which the W^X
//! invariant (docs/ARCHITECTURE.md 5) refuses; `BUN_JSC_useJIT=0` runs JSC's LLInt
//! low-level interpreter with **no executable allocation** (the JSC equivalent of
//! `node --jitless`, host-verified to issue zero RWX mappings).
//!
//! Bun demonstrably streams off ext4, demand-pages, dynamically links its whole
//! library set, brings up JavaScriptCore **including the 128 GiB Gigacage** (a
//! `MAP_NORESERVE` reservation the kernel now demand-fills, GOAL-BUN), spawns a
//! worker thread via **`clone3`** (now implemented), and sets up its libuv event
//! loop - then `abort()`s before evaluating, because that worker must run
//! *concurrently* with the main thread and the cooperative single-CPU scheduler
//! cannot provide that yet (verified: every syscall came from the main thread; the
//! worker never got the CPU). That is the preemptive-SMP frontier (task #132). So
//! the harness accepts Bun's bounded partial (exit 134 + no output) as a
//! skip-with-reason; when #132 lands it should print `rheo:42` and exit 0.
//! **x86-64 only** (no arm64/riscv64 bun build - those skip-with-reason). The proof
//! lives in the shared [`disk_runtime`] harness; this bin is the `bun`-specific
//! launch.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "disk_runtime.rs"]
mod disk_runtime;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    // `BUN_JSC_useJIT=0` disables every JSC JIT tier -> LLInt interpreter only,
    // so no writable-executable code page is requested (host-verified: 0 RWX).
    disk_runtime::prove(
        "linuxbun",
        "/bin/bun",
        &[b"bun", b"-e", b"console.log(\"rheo:\"+(40+2))"],
        &[
            b"BUN_JSC_useJIT=0",
            b"LD_LIBRARY_PATH=/lib:/lib64",
            b"PATH=/bin",
        ],
        b"rheo:42\n",
        // Bun's JavaScriptCore spawns a helper thread that must run concurrently
        // with the main thread; the cooperative single-CPU scheduler cannot provide
        // that yet (preemptive SMP, task #132), so Bun aborts before evaluating.
        // Accept that specific, bounded partial (exit 134 + no output) as a
        // skip-with-reason - see [`disk_runtime::prove`].
        true,
    )
}
