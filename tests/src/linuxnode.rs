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
    //
    // Run a real **multi-file** program off the disk - `/app/main.js`, which
    // `require`s a sibling `./lib.js` (CommonJS `module.exports`), reads config with
    // `JSON.parse`, and uses the `path` builtin. This is the shape every npm package
    // has (an entry module resolving and reading its dependencies off the
    // filesystem), so it proves the runtime surface npm and Claude Code are built on
    // - Node's module resolver reading a second file off the live ext4 disk, not
    // just an inline `-e` script. `main.js` prints `path.basename('/bin/rheo')` +
    // ':' + `lib.compute([10,20,12])` = `rheo:42`, so the assertion is unchanged.
    // The `/app` files are seeded into the disk image by xtask's
    // `build_node_disk_fixture`.
    disk_runtime::prove(
        "linuxnode",
        "/bin/node",
        &[b"node", b"--jitless", b"--no-expose-wasm", b"/app/main.js"],
        &[
            b"LD_LIBRARY_PATH=/lib:/lib64",
            b"PATH=/bin",
            b"UV_THREADPOOL_SIZE=1",
        ],
        b"rheo:42\n",
        // Node completes fully (prints rheo:42, exits 0), so it is held to the strict
        // exit-0 gate - no thread-abort partial.
        false,
    )
}
