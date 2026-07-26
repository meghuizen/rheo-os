//! Raw syscalls and typed wrappers (docs/USERLAND.md M2 ABI). The numbers
//! match `kernel/src/abi.rs`; arguments go in the ISA's argument registers
//! (riscv a0.., arm x0.., x86 rdi/rsi/rdx), the number in the syscall-number
//! register, the result back in the first argument register.

use core::arch::asm;

pub const SYS_MMAP: u64 = 21;
pub const SYS_EXIT_GROUP: u64 = 22;
pub const SYS_OPEN: u64 = 23;
pub const SYS_CLOSE: u64 = 24;
pub const SYS_READ: u64 = 25;
pub const SYS_WRITE_FD: u64 = 26;
pub const SYS_LSEEK: u64 = 27;

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe { asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret, options(nostack)) };
    ret
}
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
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe { asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, options(nostack)) };
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
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

pub fn mmap(len: usize) -> usize {
    unsafe { syscall1(SYS_MMAP, len as u64) as usize }
}
pub fn exit(code: u64) -> ! {
    unsafe {
        syscall1(SYS_EXIT_GROUP, code);
    }
    loop {}
}
pub fn open(path_va: u64, len: u64, flags: u64) -> i64 {
    unsafe { syscall3(SYS_OPEN, path_va, len, flags) as i64 }
}
pub fn read(fd: u64, buf_va: u64, len: u64) -> i64 {
    unsafe { syscall3(SYS_READ, fd, buf_va, len) as i64 }
}
pub fn write(fd: u64, buf_va: u64, len: u64) -> i64 {
    unsafe { syscall3(SYS_WRITE_FD, fd, buf_va, len) as i64 }
}
pub fn close(fd: u64) -> i64 {
    unsafe { syscall1(SYS_CLOSE, fd) as i64 }
}
pub fn lseek(fd: u64, off: i64, whence: u64) -> i64 {
    unsafe { syscall3(SYS_LSEEK, fd, off as u64, whence) as i64 }
}
