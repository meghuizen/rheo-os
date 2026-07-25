//! librheo: the greenfield native userspace foundation library for rheo-os
//! (docs/LIBRHEO.md). The role a libc plays, rebuilt for THIS kernel:
//! async-first, capability-native, built ON the strand runtime rather than a
//! POSIX threading port. It does not chase an existing ABI (that is `libc/`'s
//! job) - it expresses the kernel's own object model.
//!
//! Phase A ships the spine every program needs:
//! - [`mem`]: allocation over a growable heap (`SYS_MMAP`).
//! - [`rng`]: a per-cell ChaCha20 DRBG as a library call (no syscall on the
//!   fast path).
//! - [`cap`]: capability-typed handles (widening is a compile error).
//! - [`rt`]: the async strand executor + the userland queue reactor.
//! - [`sys`]: the raw syscall + on-wire queue ABI (a `repr(C)` mirror of the
//!   kernel, kept in sync by hand).
//!
//! A binary that links this crate inherits its `_start`, global allocator, and
//! panic handler, and defines its own `extern "C" fn main() -> i32`.

#![no_std]
// `sys::exit` diverges but the syscall stub's type is not `!`.
#![allow(clippy::empty_loop)]

extern crate alloc;

pub mod cap;
pub mod io;
pub mod mem;
pub mod rng;
pub mod rt;
mod start;
pub mod store;
pub mod sys;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // No unwinding: a panic exits with a sentinel code the test can spot.
    sys::exit(0xFE)
}

/// Formatted write to stdout (fd 1) with a trailing newline. A minimal
/// print for bring-up; the full `term` renderer arrives in a later phase.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = core::writeln!($crate::Stdout, $($arg)*);
    }};
}

/// Formatted write to stdout (fd 1), no trailing newline.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = core::write!($crate::Stdout, $($arg)*);
    }};
}

/// A `core::fmt::Write` sink over fd 1 (stdout).
pub struct Stdout;

impl core::fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let mut off = 0;
        while off < bytes.len() {
            let n = sys::write(1, bytes[off..].as_ptr() as u64, (bytes.len() - off) as u64);
            if n <= 0 {
                return Err(core::fmt::Error);
            }
            off += n as usize;
        }
        Ok(())
    }
}
