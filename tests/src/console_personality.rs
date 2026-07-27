//! The console-only POSIX personality: a `svc::FileOps` whose `write` reaches
//! the serial line and whose other operations refuse.
//!
//! Twenty-two test kernels load a cell that only ever prints - the crypto
//! vectors, the HTTP codec, the tile GEMMs, the terminal discipline, every
//! network kernel - and each one had hand-written the same eight stubs. The
//! `write` arm was **byte-identical in all twenty-two**; only the errno each
//! stub returned differed, which is why two functions live here rather than one
//! (see below). The full-filesystem sibling is `vfs_personality`, used by the
//! eleven kernels whose cell reads real files.
//!
//! Kept in the test crate and included with `#[path]`, like `harness` and
//! `vfs_personality`: which personality a test installs is a per-test concern.

// Shared across several test bins via #[path]; each uses only a subset.
#![allow(dead_code)]

use kernel::arch;
use kernel::svc::FileOps;

/// Errnos, spelled out because a bare `-38` in a stub says nothing.
const ENOENT: i64 = -2;
const EBADF: i64 = -9;
const ENOSYS: i64 = -38;

/// The one arm that is real, and was identical in all twenty-two copies: fd 1
/// and 2 go to the serial line a byte at a time, anything else is `EBADF`.
///
/// The buffer is a raw VA in the calling cell's address space, which is active
/// during the trap. It has already been range-checked by the dispatcher
/// (`svc`'s `FileOps` forwards validate before calling), so this reads the
/// cell's own mapped memory - docs/ENGINEERING.md 12.
fn write(fd: u64, buf_va: u64, len: u64) -> i64 {
    if fd == 1 || fd == 2 {
        // SAFETY: `buf_va..buf_va+len` was validated against the calling cell's
        // user VA range before this handler ran, and its address space is active.
        let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
        for &b in buf {
            arch::serial_write_byte(b);
        }
        len as i64
    } else {
        EBADF
    }
}

fn nosys_open(_p: u64, _l: u64, _f: u64) -> i64 {
    ENOSYS
}
fn nosys_close(_fd: u64) -> i64 {
    ENOSYS
}
fn nosys_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    ENOSYS
}
fn nosys_lseek(_fd: u64, _o: i64, _w: u64) -> i64 {
    ENOSYS
}
fn nosys_stat(_p: u64, _l: u64, _s: u64) -> i64 {
    ENOSYS
}
fn nosys_fstat(_fd: u64, _s: u64) -> i64 {
    ENOSYS
}
fn nosys_getdents(_p: u64, _l: u64, _b: u64, _bl: u64) -> i64 {
    ENOSYS
}

fn empty_open(_p: u64, _l: u64, _f: u64) -> i64 {
    ENOENT
}
fn empty_close(_fd: u64) -> i64 {
    0
}
fn empty_read(_fd: u64, _b: u64, _l: u64) -> i64 {
    EBADF
}
fn empty_lseek(_fd: u64, off: i64, _w: u64) -> i64 {
    off
}

/// **Only the console exists.** Every file operation reports `ENOSYS`: this
/// kernel installed no filesystem at all, so the call itself is unimplemented.
/// The shape eight kernels used (librheo Phases A/C/D/E, the tile kernels,
/// `netlocal`).
pub fn console_only() -> FileOps {
    FileOps {
        open: nosys_open,
        close: nosys_close,
        read: nosys_read,
        write,
        lseek: nosys_lseek,
        stat: nosys_stat,
        fstat: nosys_fstat,
        getdents: nosys_getdents,
    }
}

/// **The console plus an empty filesystem.** `open` reports `ENOENT` (the file
/// is not there, rather than the call not existing), `close` succeeds, a
/// non-console `read` is `EBADF`, and `lseek` echoes the offset; `stat`/`fstat`/
/// `getdents` stay `ENOSYS`. The shape fourteen kernels used (every `net*`
/// kernel plus `librheodata`/`librheogpu`/`librheonet`).
///
/// This exists as a **second** function rather than being merged with
/// [`console_only`] because the two disagree on what a cell that *did* make a
/// file call would observe. None of the twenty-two cells makes one today - but
/// "no caller exercises this difference" is an inference about the cells, not a
/// property of the table, and collapsing the two would silently change the
/// answer for the first cell that did (docs/ENGINEERING.md 1, 8). Keeping both
/// costs four one-line functions and preserves every kernel's behaviour exactly.
pub fn console_and_empty_fs() -> FileOps {
    FileOps {
        open: empty_open,
        close: empty_close,
        read: empty_read,
        write,
        lseek: empty_lseek,
        stat: nosys_stat,
        fstat: nosys_fstat,
        getdents: nosys_getdents,
    }
}
