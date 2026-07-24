//! rheo-libc: the C/POSIX ABI as a translation layer over rheo-os's native
//! syscalls (docs/USERLAND.md M3). This is what lets normal Rust (and later
//! C/C++) source build and run on the OS - `crt0`, a heap so `alloc` works,
//! `malloc`/`free`, and fd-based I/O - rather than the raw syscalls the M1/M2
//! bring-up programs use. (C string/`str*` helpers arrive with the C-program
//! work in M4/M5, when the runtime-symbol interaction is handled.)
//!
//! A binary that links this crate inherits its `_start`, global allocator,
//! and panic handler, and defines its own `extern "C" fn main() -> i32`.

#![no_std]
// `sys::exit` diverges but the syscall stub's type is not `!`.
#![allow(clippy::empty_loop)]

extern crate alloc;

pub mod io;
pub mod mem;
mod start;
pub mod sys;

pub use io::{
    FdWriter, O_APPEND, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY, close, lseek, open, read,
    stderr, stdout, write,
};
pub use mem::{calloc, free, malloc, realloc};
pub use sys::exit;

/// The process heap (see `mem`); initialised by `_start` before `main`.
#[global_allocator]
pub(crate) static HEAP: runtime::Heap = runtime::Heap::empty();

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // No unwinding: a panic exits with a sentinel code.
    sys::exit(0xFE)
}

/// Formatted write to stdout (fd 1), no trailing newline.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = core::write!($crate::io::stdout(), $($arg)*);
    }};
}

/// Formatted write to stdout with a trailing newline.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = core::writeln!($crate::io::stdout(), $($arg)*);
    }};
}

/// Formatted write to stderr with a trailing newline.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = core::writeln!($crate::io::stderr(), $($arg)*);
    }};
}
