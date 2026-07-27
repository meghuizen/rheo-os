//! `include_bytes!` the cell images a test kernel loads, once.
//!
//! A test kernel embeds the program it runs, and the path differs per target
//! directory. Eighteen kernels had written that three-arm `cfg` by hand as a
//! `static DEMO`, and twelve more had written a `macro_rules!` for it - **ten
//! independently authored macros for one path lookup**, of which eight were
//! byte-identical, four more were byte-identical to each other, and two of those
//! landed on the same line number in different files
//! (docs/ARCHITECTURE-DEBT.md 5). This module is that lookup, spelled once.
//!
//! These macros are the documented `cfg(target_arch)` exemption for test kernels
//! (docs/TARGET-ARCHITECTURES.md 4.1): what varies is a **build-tree path**, not
//! an instruction, a register or a layout. Concentrating them here is what makes
//! the exemption auditable - it is now four `cfg` sites instead of a hundred and
//! thirty-eight.
//!
//! Usable from a bin via `#[path = "fixture.rs"] mod fixture;` and then
//! `fixture::cell!("librheo-demo")`.

// Shared across several test bins via #[path]; each uses only a subset, so the
// others are legitimately unused in that bin.
#![allow(unused_macros, unused_imports)]

/// A **native cell** image: a librheo/userland/net program built for this ISA's
/// bare target in release mode. `cell!("librheo-demo")`.
macro_rules! cell {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/x86_64-unknown-none/release/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/aarch64-unknown-none-softfloat/release/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/riscv64gc-unknown-none-elf/release/",
                $name
            ))
        }
    }};
}

/// A **Linux-personality** fixture built by xtask into `linux-fixtures/build/
/// <arch>/` - the static-glibc C programs, the bare-ABI programs, and the
/// installed uutils/coreutils tree. `linux!("chello")`,
/// `linux!("cu/bin/coreutils")`.
///
/// These are gitignored build products (docs/LINUX-COMPAT.md): no binary is
/// committed, so a missing one is a build error, not a silently stale image.
macro_rules! linux {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/x86_64/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/aarch64/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/build/riscv64/",
                $name
            ))
        }
    }};
}

/// A **cargo-built** Linux fixture: an unpatched Rust `std` program compiled for
/// the `*-unknown-linux-gnu` triple, living under its own crate's `target/`.
/// `linux_cargo!("rusthello")`, `linux_cargo!("rustthreads")` - the crate
/// directory and the binary share a name in every case so far.
macro_rules! linux_cargo {
    ($name:literal) => {{
        #[cfg(target_arch = "x86_64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/",
                $name,
                "/target/x86_64-unknown-linux-gnu/release/",
                $name
            ))
        }
        #[cfg(target_arch = "aarch64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/",
                $name,
                "/target/aarch64-unknown-linux-gnu/release/",
                $name
            ))
        }
        #[cfg(target_arch = "riscv64")]
        {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/linux-fixtures/",
                $name,
                "/target/riscv64gc-unknown-linux-gnu/release/",
                $name
            ))
        }
    }};
}

pub(crate) use {cell, linux, linux_cargo};
