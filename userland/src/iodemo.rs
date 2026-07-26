//! `iodemo` - exercises the POSIX personality syscalls (docs/USERLAND.md M2):
//! mmap a buffer, open a file, read it, write the bytes to stdout, and exit
//! with the byte count. A separately-compiled native program using the
//! multi-argument fd-based ABI (still raw syscalls - the libc arrives in M3).

#![no_std]
#![no_main]
// exit never returns, but the syscall stub's return type is not `!`.
#![allow(clippy::empty_loop)]

use core::arch::asm;

const SYS_MMAP: u64 = 21;
const SYS_EXIT_GROUP: u64 = 22;
const SYS_OPEN: u64 = 23;
const SYS_CLOSE: u64 = 24;
const SYS_READ: u64 = 25;
const SYS_WRITE_FD: u64 = 26;

const O_RDONLY: u64 = 0;
const STDOUT: u64 = 1;

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
             in("a1") a1, in("a2") a2, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
             in("x1") a1, in("x2") a2, options(nostack));
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret,
             in("rdi") a0, in("rsi") a1, in("rdx") a2,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

fn exit(code: u64) -> ! {
    unsafe {
        syscall3(SYS_EXIT_GROUP, code, 0, 0);
    }
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn _start(_arg: u64) -> ! {
    let path = b"/hello.txt";
    // SAFETY: raw syscalls; the kernel validates/serves each.
    let fd = unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, O_RDONLY) };
    if (fd as i64) < 0 {
        exit(200); // open failed
    }

    // A fresh anonymous page to read into (proves mmap gives usable RW memory).
    let buf = unsafe { syscall3(SYS_MMAP, 4096, 0, 0) };
    if buf == 0 {
        exit(201);
    }

    let n = unsafe { syscall3(SYS_READ, fd, buf, 4096) } as i64;
    if n < 0 {
        exit(202);
    }

    // Echo what we read to stdout (fd 1 -> console), then exit with the count.
    unsafe {
        syscall3(SYS_WRITE_FD, STDOUT, buf, n as u64);
        syscall3(SYS_CLOSE, fd, 0, 0);
    }
    exit(n as u64);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    exit(0xFF)
}
