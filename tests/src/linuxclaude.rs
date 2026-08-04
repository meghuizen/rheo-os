//! In-QEMU test kernel: the **real Claude Code binary** runs unmodified under the
//! Linux personality (GOAL-CLAUDE, docs/LINUX-COMPAT.md).
//!
//! This is the workload docs/ARCHITECTURE-DEBT.md 4.0 measured this tree against and
//! named as the target: `/opt/claude-code/bin/claude`, ~275 MB, a **Bun-compiled
//! single-file executable** - so it is the same JavaScriptCore runtime `linuxbun`
//! proves, at nearly three times the size and with an entire application bundled into
//! it. It streams off a live ext4 disk over virtio-blk-pci (`ext4fs`/`ext4plus` + the
//! block cache), demand-pages, and links its glibc set (`librt` on top of bun's).
//!
//! It runs `claude --version`, which prints the version and exits 0. That choice is
//! deliberate and is the honest limit of what can be asserted here: it exercises the
//! whole load path, JSC bring-up, the bundled application's startup and its argument
//! handling, and it needs **no network and no credentials** - so the result is
//! deterministic and the test asserts an exact transcript. Driving a *conversation*
//! would need outbound TLS to an API from inside a cell, which is the N3b/N5a stack
//! wired into a cell rather than anything about running the binary, and is not claimed.
//!
//! Its JIT is enabled (the capability-gated W^X exception, docs/ARCHITECTURE.md 5.1)
//! and it runs under preemption, as `linuxbun` and `linuxnode` do. **x86-64 only** (the
//! binary is an x86-64 ELF; arm64/riscv64 skip-with-reason). The proof lives in the
//! shared [`disk_runtime`] harness; this bin is the `claude`-specific launch.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "disk_runtime.rs"]
mod disk_runtime;

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    disk_runtime::prove(disk_runtime::Proof {
        name: "linuxclaude",
        path: "/bin/claude",
        argv: &[b"claude", b"--version"],
        envp: &[
            b"LD_LIBRARY_PATH=/lib:/lib64",
            b"PATH=/bin",
            b"HOME=/",
            // Claude Code checks for a terminal and for update channels; neither is
            // reachable here, and CI=1 is the documented way to tell a tool it is not
            // being run interactively. Not a workaround for a missing capability - the
            // cell genuinely has no tty and no network.
            b"CI=1",
        ],
        // The version the **installed binary** reports, recorded on the host by xtask
        // at fixture-build time (`write_claude_version`) rather than hardcoded here -
        // a literal drifts every Claude Code release and turns a green gate red with
        // nothing wrong. Still an exact byte-for-byte match on the cell's whole stdout,
        // and still a value the cell cannot influence.
        want: include_bytes!("../linux-fixtures/build/claude-version.txt"),
        // Held to the strict gate: it prints its version and exits 0.
        thread_abort_partial: false,
        // Preemptive, as for Node and Bun.
        preemptive: true,
        // The W^X exception capability, so JavaScriptCore's JIT can map its code pages
        // (docs/ARCHITECTURE.md 5.1).
        wx_authority: true,
        second: None,
        // Not on a secondary: this kernel is the boot-CPU proof (docs/SMP.md 10.0e).
        on_secondary: false,
    })
}
