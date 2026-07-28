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
    // The script exercises the runtime surface npm/Claude Code actually lean on -
    // not just `console.log`: `require` (Node's module loader), the `path` and `fs`
    // builtin modules, `fs.existsSync('/bin/node')` (a real `stat` through the VFS
    // onto the live disk the binary itself streams from), and a
    // `JSON.parse(JSON.stringify(...))` round-trip over an object with an array
    // reduced with an arrow closure. The arithmetic is chosen to still print exactly
    // `rheo:42`: nums reduce to 60, minus `basename('/bin/node')`.length (4), minus
    // 14 when `/bin/node` is found via the VFS = 42. So a broader slice of Node's
    // runtime is proven while the assertion shape is unchanged.
    disk_runtime::prove(
        "linuxnode",
        "/bin/node",
        &[
            b"node",
            b"--jitless",
            b"--no-expose-wasm",
            b"-e",
            b"const fs=require('fs'),path=require('path');const data={nums:[10,20,30],name:path.basename('/bin/node'),node:fs.existsSync('/bin/node')};const r=JSON.parse(JSON.stringify(data));console.log('rheo:'+(r.nums.reduce((a,b)=>a+b,0)-r.name.length-(r.node?14:0)));",
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
    )
}
