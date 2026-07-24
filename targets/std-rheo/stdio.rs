//! rheo-os std stdio (installed into std as `sys/stdio/rheo.rs` by
//! targets/patch-std.py; docs/USERLAND.md M4). stdout/stderr write to fds 1/2
//! and stdin reads fd 0, via the rheo syscalls (SYS_WRITE_FD/SYS_READ). The
//! kernel routes those fds to the console personality.
//!
//! Non-blocking by construction: `read` returns whatever input is available
//! right now (0 if none - never waits for a keypress), and writes go to the
//! console with only a bounded local FIFO drain (no wait on external events).
//! So `println!`/logging and stdin reads can never park the cell. (Fully
//! async stdio parked on a queue-pair completion is the design direction,
//! docs/CONCURRENCY.md; this is the synchronous, non-blocking baseline.)
use crate::io::{self, IoSlice};

const SYS_READ: u64 = 25;
const SYS_WRITE_FD: u64 = 26;

unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
            in("a1") a1, in("a2") a2, options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
            in("x1") a1, in("x2") a2, options(nostack));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0,
            in("rsi") a1, in("rdx") a2, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

fn write_fd(fd: u64, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe { syscall3(SYS_WRITE_FD, fd, buf.as_ptr() as u64, buf.len() as u64) };
    if n < 0 {
        Err(io::Error::from_raw_os_error((-n) as i32))
    } else {
        Ok(n as usize)
    }
}

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as u64, buf.len() as u64) };
        if n < 0 {
            Err(io::Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_fd(1, buf)
    }
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for b in bufs {
            total += write_fd(1, b)?;
        }
        Ok(total)
    }
    fn is_write_vectored(&self) -> bool {
        true
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

impl io::Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_fd(2, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;

pub fn is_ebadf(err: &io::Error) -> bool {
    err.raw_os_error() == Some(9)
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}
