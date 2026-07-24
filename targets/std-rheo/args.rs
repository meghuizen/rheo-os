//! rheo-os command-line arguments (installed into std as `sys/args/rheo.rs` by
//! targets/patch-std.py; docs/USERLAND.md M5). The crt0 (`rheo-rt`) passes the
//! kernel-provided argc/argv to `main`, `std::rt::lang_start` stores them here
//! via `init`, and `args()` reads them back - the same design as the unix
//! impl, but without the `os::unix` dependency (rheo is not
//! `target_family = "unix"`, so `OsStringExt` is unavailable).
#![allow(dead_code)]

pub use super::common::Args;
use crate::ffi::{CStr, OsStr};
use crate::ptr;
use crate::sync::atomic::{Atomic, AtomicIsize, AtomicPtr, Ordering};

// The kernel-provided argc/argv, stored here at startup so the work of
// building the argument list is deferred until `args()` is called. Never
// mutated after `init`.
static ARGC: Atomic<isize> = AtomicIsize::new(0);
static ARGV: Atomic<*mut *const u8> = AtomicPtr::new(ptr::null_mut());

/// One-time global initialization from crt0 via `std::rt::lang_start`.
pub unsafe fn init(argc: isize, argv: *const *const u8) {
    ARGC.store(argc, Ordering::Relaxed);
    ARGV.store(argv as *mut _, Ordering::Relaxed);
}

/// Returns the command line arguments.
pub fn args() -> Args {
    let argv = ARGV.load(Ordering::Relaxed);
    let argc = if argv.is_null() { 0 } else { ARGC.load(Ordering::Relaxed) };

    let mut vec = Vec::with_capacity(argc as usize);
    for i in 0..argc {
        // SAFETY: `argv` is non-null with at least `argc` entries, each a valid
        // C string (the kernel builds it in load.rs `setup_stack`).
        let ptr = unsafe { argv.offset(i).read() };
        if ptr.is_null() {
            break;
        }
        let cstr = unsafe { CStr::from_ptr(ptr.cast()) };
        // rheo's `OsStr` is the raw-bytes representation, so the C-string bytes
        // are already a valid platform encoding.
        vec.push(unsafe { OsStr::from_encoded_bytes_unchecked(cstr.to_bytes()) }.to_os_string());
    }

    Args::new(vec)
}
