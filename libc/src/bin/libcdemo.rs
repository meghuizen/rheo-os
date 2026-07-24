//! `libcdemo` - a native program written against rheo-libc, not raw syscalls
//! (docs/USERLAND.md M3). It uses the Rust heap (`Vec`/`String` via the libc's
//! global allocator), C `malloc`/`free`, formatted output (`println!`), and
//! fd-based file I/O. The `libcrun` test kernel checks its exit code and the
//! file it echoes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use rheo_libc as libc;

/// The program's entry (the libc's `_start` calls this, then exits with the
/// return value).
#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // Rust `alloc` works because the libc installs a global allocator.
    let mut v: Vec<u32> = Vec::new();
    for i in 0..50 {
        v.push(i);
    }
    let sum: u32 = v.iter().sum(); // 0..49 -> 1225
    let s = alloc::format!("sum={sum}");
    libc::println!("libcdemo: {s} ({} items)", v.len());

    // C malloc/free round-trip.
    // SAFETY: p is a fresh 128-byte allocation we own until free.
    unsafe {
        let p = libc::malloc(128);
        if p.is_null() {
            return 1;
        }
        *p = 0xAB;
        let readback = *p;
        libc::free(p);
        if readback != 0xAB {
            return 2;
        }
    }

    // Read a file through the VFS and echo it to stdout.
    let fd = libc::open("/greeting.txt", libc::O_RDONLY);
    if fd < 0 {
        return 3;
    }
    let mut buf = [0u8; 128];
    let n = libc::read(fd, &mut buf);
    if n < 0 {
        return 4;
    }
    libc::write(1, &buf[..n as usize]);
    libc::close(fd);

    // Exit with the number of bytes read (the test asserts the file length).
    n as i32
}
