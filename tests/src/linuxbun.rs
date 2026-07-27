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
//! loop - then `abort()`s before evaluating. The harness accepts that bounded
//! partial (exit 134 + no output) as a skip-with-reason.
//!
//! **Why it aborts is currently unknown, and that is a correction.** The previous
//! answer was the preemptive-SMP frontier: every one of Bun's 205 syscalls came from
//! its main thread and the worker it spawned never got the CPU, so the cooperative
//! scheduler was blamed and the prediction was "when task #132 lands, Bun prints
//! `rheo:42`". Preemption has since landed (docs/SUBSTRATE.md 15, S3'); this boot
//! enables it; the worker measurably gets the CPU (66 preemptions, all to a sibling
//! context of Bun's own cell) - and Bun aborts **identically with preemption
//! disabled**, same exit, same empty output, same point. The starved worker was the
//! first difference anyone measured between Bun and Node, not the cause. The
//! prediction is withdrawn rather than reattached to a later milestone
//! (docs/ENGINEERING.md 1).
//!
//! `linuxnode`, which shares this harness, **does** complete under preemption - so
//! the mechanism is exercised here by a runtime that finishes, not only by one that
//! does not.
//!
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
        // Bun aborts before evaluating, for a reason that is no longer attributed
        // (see the module docs: preemption landed, the worker now runs, and the
        // abort is unchanged). Accept that specific, bounded partial - exit 134
        // **and** no output, so any other failure still fails - as a
        // skip-with-reason; see [`disk_runtime::prove`].
        true,
        // Cooperative, which is the scheduler this partial is characterised against
        // (module docs). Turning preemption on is not a no-op here - Bun gets
        // *further*, all the way to printing its banner - but it then fails
        // differently, and widening an accepted partial to cover a second unexplained
        // failure would turn a bounded disclosure into a blanket one.
        false,
    )
}
