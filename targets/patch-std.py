#!/usr/bin/env python3
"""Patch the toolchain's vendored rust-src `std` to add `target_os = "rheo"`
(docs/USERLAND.md M4). rheo is single-core (SMP is deferred), so std uses the
single-threaded `no_threads` sync/TLS impls - which are sound here; we only
lift the guard that assumes such targets have no atomics. Everything else
routes rheo to the portable fallbacks (`generic` errors, `unsupported`
random). Real capabilities (stdio/fs/process/time) live in the rheo `sys`
arms added by this repo, not here.

Idempotent. Run via `cargo xtask std-patch` (or directly). Finds the active
toolchain's std sys dir automatically unless a path is given.
"""
import subprocess
import sys
from pathlib import Path


def std_sys_dir() -> Path:
    if len(sys.argv) > 1:
        return Path(sys.argv[1])
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"]).decode().strip()
    return Path(sysroot) / "lib/rustlib/src/rust/library/std/src/sys"


# (relative path, anchor that must be present, replacement of the anchor).
# Each anchor is unique within its file; applying twice is a no-op because the
# replacement no longer contains the bare anchor.
EDITS = [
    # Route rheo's io error helpers to the portable `generic` impl.
    (
        "io/error/mod.rs",
        '        target_os = "trusty",\n    ) => {\n        mod generic;',
        '        target_os = "trusty",\n        target_os = "rheo",\n    ) => {\n        mod generic;',
    ),
    # Route rheo random to the `unsupported` fill (HashMap seeding only).
    (
        "random/mod.rs",
        '        target_os = "xous",\n        target_os = "vexos",\n    ) => {\n        // FIXME: finally remove std support',
        '        target_os = "xous",\n        target_os = "vexos",\n        target_os = "rheo",\n    ) => {\n        // FIXME: finally remove std support',
    ),
    (
        "random/mod.rs",
        '    target_os = "xous",\n    target_os = "vexos",\n)))]\npub fn hashmap_random_keys',
        '    target_os = "xous",\n    target_os = "vexos",\n    target_os = "rheo",\n)))]\npub fn hashmap_random_keys',
    ),
    # rheo uses the single-threaded (static) thread-local storage.
    (
        "thread_local/mod.rs",
        '        target_os = "vexos",\n    ) => {\n        mod no_threads;',
        '        target_os = "vexos",\n        target_os = "rheo",\n    ) => {\n        mod no_threads;',
    ),
    # rheo's TLS destructor guard: std is the only runtime (like hermit/xous).
    (
        "thread_local/mod.rs",
        '            target_os = "hermit",\n            target_os = "xous",\n        ) => {\n            // `std` is the only runtime,',
        '            target_os = "hermit",\n            target_os = "xous",\n            target_os = "rheo",\n        ) => {\n            // `std` is the only runtime,',
    ),
    # rheo allocator arm (the module file is copied in by COPIES below).
    (
        "alloc/mod.rs",
        '    target_os = "zkvm" => {\n        mod zkvm;\n        use zkvm as imp;\n    }\n}\n\npub use imp::{alloc, dealloc, realloc};',
        '    target_os = "zkvm" => {\n        mod zkvm;\n        use zkvm as imp;\n    }\n    target_os = "rheo" => {\n        mod rheo;\n        use rheo as imp;\n    }\n}\n\npub use imp::{alloc, dealloc, realloc};',
    ),
    # rheo stdio arm (real fd 0/1/2; module file copied in by COPIES).
    (
        "stdio/mod.rs",
        '    target_os = "zkvm" => {\n        mod zkvm;\n        pub use zkvm::*;\n    }\n    _ => {\n        mod unsupported;\n        pub use unsupported::*;\n    }',
        '    target_os = "zkvm" => {\n        mod zkvm;\n        pub use zkvm::*;\n    }\n    target_os = "rheo" => {\n        mod rheo;\n        pub use rheo::*;\n    }\n    _ => {\n        mod unsupported;\n        pub use unsupported::*;\n    }',
    ),
    # rheo process::exit -> SYS_EXIT_GROUP (instead of aborting).
    (
        "exit.rs",
        '        target_os = "xous" => {\n            crate::os::xous::ffi::exit(code as u32)\n        }\n        _ => {\n            let _ = code;\n            crate::intrinsics::abort()\n        }',
        '        target_os = "xous" => {\n            crate::os::xous::ffi::exit(code as u32)\n        }\n        target_os = "rheo" => {\n            // rheo-os: leave U-mode via SYS_EXIT_GROUP (docs/USERLAND.md M4).\n            unsafe {\n                #[cfg(target_arch = "riscv64")]\n                core::arch::asm!("ecall", in("a7") 22u64, in("a0") code as u64, options(noreturn, nostack));\n                #[cfg(target_arch = "aarch64")]\n                core::arch::asm!("svc #0", in("x8") 22u64, in("x0") code as u64, options(noreturn, nostack));\n                #[cfg(target_arch = "x86_64")]\n                core::arch::asm!("syscall", in("rax") 22u64, in("rdi") code as u64, options(noreturn, nostack));\n            }\n        }\n        _ => {\n            let _ = code;\n            crate::intrinsics::abort()\n        }',
    ),
]

# rheo `sys` module files this repo owns, copied into the std tree.
# (repo-relative source, std-sys-relative destination)
COPIES = [
    ("std-rheo/alloc.rs", "alloc/rheo.rs"),
    ("std-rheo/stdio.rs", "stdio/rheo.rs"),
]

# The no_threads sync/TLS impls compile_error on targets that "have threads"
# (i.e. have atomics). rheo has atomics but no preemptive threads, so the
# Cell-based impls are sound - exempt rheo from the guard.
GUARD_FILES = [
    "sync/condvar/no_threads.rs",
    "sync/once/no_threads.rs",
    "sync/rwlock/no_threads.rs",
    "sync/mutex/no_threads.rs",
    "thread_local/no_threads.rs",
]
GUARD_OLD = "#[cfg(target_has_threads)]"
GUARD_NEW = '#[cfg(all(target_has_threads, not(target_os = "rheo")))]'


def main() -> int:
    root = std_sys_dir()
    if not root.is_dir():
        print(f"std sys dir not found: {root}", file=sys.stderr)
        return 1
    here = Path(__file__).resolve().parent
    changed = 0
    for src, dst in COPIES:
        content = (here / src).read_text()
        p = root / dst
        if not p.exists() or p.read_text() != content:
            p.write_text(content)
            changed += 1
    for rel, old, new in EDITS:
        p = root / rel
        s = p.read_text()
        if new in s:
            continue
        if old not in s:
            print(f"anchor missing in {rel} - std layout changed?", file=sys.stderr)
            return 2
        p.write_text(s.replace(old, new, 1))
        changed += 1
    for rel in GUARD_FILES:
        p = root / rel
        s = p.read_text()
        if GUARD_NEW in s:
            continue
        if GUARD_OLD not in s:
            print(f"guard missing in {rel} - std layout changed?", file=sys.stderr)
            return 2
        p.write_text(s.replace(GUARD_OLD, GUARD_NEW, 1))
        changed += 1
    print(f"std rheo patch: {changed} edit(s) applied at {root}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
