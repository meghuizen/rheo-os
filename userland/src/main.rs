//! `hello` - the first separately-compiled native program to run on rheo-os
//! (docs/USERLAND.md M1). It is a freestanding `no_std` ELF: the kernel loads
//! its segments into a cell and jumps to `_start`. It writes a line through a
//! syscall and exits with a known code, which the `elfrun` test kernel checks.
//!
//! This is deliberately tiny and hand-rolled (raw syscalls, no heap, no libc);
//! it exists to prove the loader + address-space path end to end. The real
//! libc + std port (M3/M4) replace this hand-rolled ABI with the C/POSIX one.

#![no_std]
#![no_main]
// SYS_EXIT never returns, so the `loop {}` after it (and after a panic) is
// unreachable, but the syscall stub's return type is not `!`.
#![allow(clippy::empty_loop)]

use core::arch::asm;

// Must match kernel/src/abi.rs.
const SYS_EXIT: u64 = 3;
const SYS_DEBUG_WRITE: u64 = 20;

/// Exit code the program returns - the `elfrun` test asserts exactly this,
/// which proves the loaded code actually ran (not just the entry stub).
const EXIT_CODE: u64 = 42;

/// The kernel reads this at the VA passed to `SYS_DEBUG_WRITE` (little-endian
/// `{ptr, len}`), mirroring how the shell hands the kernel its I/O buffer VA.
#[repr(C)]
struct WriteReq {
    ptr: u64,
    len: u64,
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") arg => ret, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") arg => ret, options(nostack));
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") arg,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

fn write(msg: &[u8]) {
    let req = WriteReq {
        ptr: msg.as_ptr() as u64,
        len: msg.len() as u64,
    };
    // SAFETY: the kernel reads a `{ptr,len}` at this VA and writes `len` bytes
    // from `ptr` to the console; both VAs are in this cell's mapped pages.
    unsafe {
        syscall(SYS_DEBUG_WRITE, &req as *const WriteReq as u64);
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start(_arg: u64) -> ! {
    write(b"hello from a loaded ELF (userland)\n");
    // SAFETY: SYS_EXIT never returns.
    unsafe {
        syscall(SYS_EXIT, EXIT_CODE);
    }
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // No unwinding, no libc: a panic just exits with a sentinel code.
    unsafe {
        syscall(SYS_EXIT, 0xFF);
    }
    loop {}
}
