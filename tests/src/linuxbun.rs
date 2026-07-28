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
//! **Bun evaluates JavaScript and exits 0**, with its JIT enabled and under
//! preemption: it streams off ext4, demand-pages, dynamically links its whole library
//! set, brings up JavaScriptCore including the 128 GiB Gigacage, spawns a worker via
//! `clone3`, runs its libuv event loop, prints exactly `rheo:42`. Held to the same
//! strict gate as Node - no partial is accepted.
//!
//! ## Three wrong diagnoses, and the measurement that ended them
//!
//! Worth recording, because this abort was blamed on three different things and each
//! guess cost a full experiment:
//!
//! 1. **The scheduler.** Every one of Bun's 205 startup syscalls came from its main
//!    thread and the worker it spawned never got the CPU, so the cooperative scheduler
//!    was blamed and the prediction was "when preemption lands, Bun prints `rheo:42`".
//!    Preemption landed; the worker measurably got the CPU (66 preemptions to a sibling
//!    context); Bun aborted **identically with preemption disabled**.
//! 2. **The JIT.** W^X refused JavaScriptCore's RWX arena, so that was blamed next.
//!    The arena is now granted through the capability-gated exception
//!    (docs/ARCHITECTURE.md 5.1) - and Bun aborted at the same point again.
//! 3. **Nothing in particular.** After two eliminations the honest position was that
//!    the cause was unknown.
//!
//! What ended it was not a fourth guess but **evidence**: the personality now prints
//! the path of every refused `open` and dumps the last syscalls before a fatal signal.
//! The trace showed glibc's `abort()` preamble - `rt_sigprocmask`, `gettid`, `getpid`,
//! `tgkill` - preceded by a series of probes, and the refused-path log named them.
//! `/proc/self/maps` was the one that mattered: **JavaScriptCore reads its own memory
//! map**, and a JS engine that cannot find its own mappings cannot proceed. The Linux
//! personality now synthesizes it from the cell's real VMA list.
//!
//! The lesson is the cheapness of the fix relative to the guesses. Two large,
//! correctly-built mechanisms were driven to completion on the strength of a plausible
//! story, and a one-line "print the path that failed" would have answered it first
//! (docs/ENGINEERING.md 1 - observe, never infer).
//!
//! Still refused, and correctly: `/etc/localtime` (glibc falls back to UTC, which is
//! right - there is no timezone database and inventing one is worse), `bunfig.toml`
//! (no config), the `glibc-hwcaps` probes, `trace_marker`, and `/proc/self/statm`.
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
    // JSC's JIT is left **enabled**: the cell holds the W^X exception capability
    // (docs/ARCHITECTURE.md 5.1), so its 1 GiB RWX arena is grantable now.
    disk_runtime::prove(
        "linuxbun",
        "/bin/bun",
        &[b"bun", b"-e", b"console.log(\"rheo:\"+(40+2))"],
        &[b"LD_LIBRARY_PATH=/lib:/lib64", b"PATH=/bin", b"TMPDIR=/tmp"],
        b"rheo:42\n",
        // Bun aborts before evaluating, for a reason that is no longer attributed
        // (see the module docs: preemption landed, the worker now runs, and the
        // abort is unchanged). Accept that specific, bounded partial - exit 134
        // **and** no output, so any other failure still fails - as a
        // skip-with-reason; see [`disk_runtime::prove`].
        // **No partial accepted any more.** Bun evaluates its input and exits 0, so it
        // is held to the same strict gate as Node (see the module docs for what the
        // three withdrawn diagnoses cost, and what the fourth turned out to be).
        false,
        // **Preemptive dispatch** (docs/SUBSTRATE.md 15, S3'), as for Node: JSC's
        // worker and its main thread are now scheduled preemptively rather than only
        // at blocking points.
        true,
        // The **W^X exception capability** (docs/ARCHITECTURE.md 5.1), so this
        // runtime's JIT can map its code pages writable-and-executable. Every other
        // kernel in the suite mints nothing of the sort and is refused exactly as
        // before, which is what makes this a capability rather than a setting.
        true,
        // **JavaScript calling a tile kernel, from a real script file on the disk**
        // (docs/TILES.md 13.4d). `bun:ffi` is built into Bun, so the runtime opens
        // `/lib/libtileso.so`, generates a native trampoline for the declared signature,
        // and calls through it.
        //
        // A **file**, not `-e`, and getting there took five hypotheses. Running a file
        // makes Bun call `createFakeTemporaryNodeExecutable`, which aborted
        // `error.FileNotFound` naming nothing. Ruled out by observation: the read-only
        // root (a writable ramfs is mounted at `/tmp` now), `TMPDIR` being unset, the
        // legacy `stat`/`lstat` being unimplemented, and `/proc/self/exe` being absent
        // from the code. The fifth was the answer, and it needed a diagnostic rather than
        // a guess: a refused `readlink` now names its path, and it named
        // `/proc/self/exe` - which was set only by the `execve` *syscall*, so it worked
        // for a process another cell had exec'd and failed for the initial one.
        //
        // The expected value is the low 31 bits of the FNV-1a hash of the whole 32x32x32
        // int8 GEMM output, so it proves the *kernel ran* rather than that a symbol
        // resolved - the same number the librheo cells, the static `tilelinux` binary and
        // the `dlopentile` C probe produce. 31 bits because a JS number is exact only to
        // 2^53.
        Some((&[b"bun", b"/bin/tileffi.js"], b"tileffi: gemm 568708273\n")),
    )
}
