//! A POSIX personality handler backed by the `posix` VFS, shared by the test
//! kernels that run native/std programs doing real file I/O (posixrun,
//! libcrun, coreutils). Included via `#[path]` so each bin gets its own copy;
//! `dead_code` is allowed because a given test uses only a subset.
//!
//! Fd convention: 0/1/2 are the console (stdin/stdout/stderr); 3+ map to the
//! `posix` fd table (offset by 3 so they never collide with the console fds).
//! Console I/O is **non-blocking**: stdout/stderr write to the serial UART,
//! and stdin drains whatever the serial RX FIFO holds right now (0 if none),
//! so a program's read/println logging can never park the cell.
#![allow(dead_code)]
#![allow(static_mut_refs)]

use kernel::abi::Stat;
use kernel::arch;
use kernel::svc::FileOps;
use posix::FileType;
use posix::sys::{self, Whence};

// Optional stdout capture, so a test can assert a program's exact stdout in
// addition to seeing it on the serial log. Writes to fd 1 are appended here
// (bounded); `clear_stdout` resets it before a run, `captured_stdout` reads it
// after. A fixed static buffer keeps this allocation-free.
const CAP_MAX: usize = 16 * 1024;
static mut STDOUT_CAP: [u8; CAP_MAX] = [0; CAP_MAX];
static mut STDOUT_LEN: usize = 0;

pub fn clear_stdout() {
    unsafe {
        STDOUT_LEN = 0;
    }
}

pub fn captured_stdout() -> &'static [u8] {
    unsafe { &STDOUT_CAP[..STDOUT_LEN] }
}

fn capture(bytes: &[u8]) {
    unsafe {
        for &b in bytes {
            if STDOUT_LEN < CAP_MAX {
                STDOUT_CAP[STDOUT_LEN] = b;
                STDOUT_LEN += 1;
            }
        }
    }
}

/// The `FileOps` this module implements, ready to hand to `svc::set_file_ops`.
pub fn ops() -> FileOps {
    FileOps {
        open: p_open,
        close: p_close,
        read: p_read,
        write: p_write,
        lseek: p_lseek,
        stat: p_stat,
        fstat: p_fstat,
        getdents: p_getdents,
    }
}

fn kind_code(k: FileType) -> u64 {
    match k {
        FileType::Regular => 0,
        FileType::Dir => 1,
        FileType::Symlink => 2,
        FileType::Other => 3,
    }
}

fn p_open(path_va: u64, path_len: u64, flags: u64) -> i64 {
    let bytes = unsafe { core::slice::from_raw_parts(path_va as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return -22; // EINVAL
    };
    match sys::open(path, flags as u32) {
        Ok(fd) => (fd + 3) as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_close(fd: u64) -> i64 {
    if fd < 3 {
        return 0;
    }
    match sys::close((fd - 3) as usize) {
        Ok(()) => 0,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_read(fd: u64, buf_va: u64, len: u64) -> i64 {
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, len as usize) };
    if fd == 0 {
        // Non-blocking stdin: drain the serial RX FIFO, return count (0 if none).
        let mut n = 0;
        while n < buf.len() {
            match arch::serial_read_byte() {
                Some(b) => {
                    buf[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        return n as i64;
    }
    if fd < 3 {
        return -9; // EBADF (stdout/stderr not readable)
    }
    match sys::read((fd - 3) as usize, buf) {
        Ok(n) => n as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_write(fd: u64, buf_va: u64, len: u64) -> i64 {
    let buf = unsafe { core::slice::from_raw_parts(buf_va as *const u8, len as usize) };
    if fd == 1 || fd == 2 {
        for &b in buf {
            arch::serial_write_byte(b);
        }
        if fd == 1 {
            capture(buf);
        }
        return len as i64;
    }
    if fd < 3 {
        return -9; // EBADF (stdin not writable)
    }
    match sys::write((fd - 3) as usize, buf) {
        Ok(n) => n as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_lseek(fd: u64, off: i64, whence: u64) -> i64 {
    if fd < 3 {
        return -9; // EBADF
    }
    let w = match whence {
        0 => Whence::Set,
        1 => Whence::Cur,
        _ => Whence::End,
    };
    match sys::lseek((fd - 3) as usize, off, w) {
        Ok(o) => o as i64,
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn write_stat(statbuf_va: u64, m: &posix::Metadata) {
    let st = Stat {
        size: m.len,
        kind: kind_code(m.kind),
    };
    unsafe {
        (statbuf_va as *mut Stat).write(st);
    }
}

fn p_stat(path_va: u64, path_len: u64, statbuf_va: u64) -> i64 {
    let bytes = unsafe { core::slice::from_raw_parts(path_va as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return -22;
    };
    match sys::stat(path) {
        Ok(m) => {
            write_stat(statbuf_va, &m);
            0
        }
        Err(e) => -(sys::errno(e) as i64),
    }
}

fn p_fstat(fd: u64, statbuf_va: u64) -> i64 {
    if fd < 3 {
        // Console fds: report a zero-length "other" so std's File::metadata on
        // them does not fault (coreutils only fstat real files).
        write_stat(
            statbuf_va,
            &posix::Metadata {
                kind: FileType::Other,
                len: 0,
                mode: 0,
            },
        );
        return 0;
    }
    match sys::fstat((fd - 3) as usize) {
        Ok(m) => {
            write_stat(statbuf_va, &m);
            0
        }
        Err(e) => -(sys::errno(e) as i64),
    }
}

/// Pack directory entries as `[u32 kind][u32 name_len][name bytes]` records
/// into the user buffer; returns bytes used or -errno. Kept in sync with the
/// std `fs` arm's `readdir` parser (targets/std-rheo/fs.rs).
fn p_getdents(path_va: u64, path_len: u64, buf_va: u64, buf_len: u64) -> i64 {
    let bytes = unsafe { core::slice::from_raw_parts(path_va as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return -22;
    };
    let entries = match sys::getdents(path) {
        Ok(e) => e,
        Err(e) => return -(sys::errno(e) as i64),
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_va as *mut u8, buf_len as usize) };
    let mut off = 0usize;
    for e in &entries {
        let name = e.name.as_bytes();
        let rec = 8 + name.len();
        if off + rec > buf.len() {
            break; // honest truncation: caller's buffer is full
        }
        buf[off..off + 4].copy_from_slice(&(kind_code(e.kind) as u32).to_ne_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(name.len() as u32).to_ne_bytes());
        buf[off + 8..off + rec].copy_from_slice(name);
        off += rec;
    }
    off as i64
}
