//! `linuxhello` - a bare program speaking the **raw Linux syscall ABI**
//! (docs/LINUX-COMPAT.md 5, milestone L0). It is built for the same bare
//! targets as the other userland programs, but its syscalls use Linux
//! numbers and the Linux register convention - so it only runs in a cell
//! tagged `Personality::Linux` (the `linuxrun` test). Because riscv64 has no
//! Linux libc cross toolchain in every environment, this bare fixture is the
//! permanent riscv64 coverage floor for the personality dispatch + loader.
//!
//! It writes a marker line via Linux `write(1, ...)` and leaves via Linux
//! `exit_group`. The exit code asserts the write path too: 42 only if
//! `write` returned the full byte count, 1 otherwise.
//!
//! Syscall numbers differ per ISA (x86-64 legacy table vs the asm-generic
//! table shared by arm64/riscv64) - mirrored from kernel/src/arch/*/
//! linux_abi*.rs.

#![no_std]
#![no_main]
#![allow(clippy::empty_loop)]

use core::arch::asm;

#[cfg(target_arch = "x86_64")]
mod sys {
    pub const WRITE: u64 = 1;
    pub const EXIT_GROUP: u64 = 231;
}
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
mod sys {
    pub const WRITE: u64 = 64;
    pub const EXIT_GROUP: u64 = 94;
}

const MSG: &[u8] = b"linux-abi: hello via Linux write\n";

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret, in("a1") a1, in("a2") a2,
             options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, in("x1") a1, in("x2") a2,
             options(nostack));
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[unsafe(no_mangle)]
extern "C" fn _start(_arg: u64) -> ! {
    // SAFETY: Linux write(1, MSG, len); the buffer is in this cell's pages.
    let n = unsafe { syscall3(sys::WRITE, 1, MSG.as_ptr() as u64, MSG.len() as u64) };
    let code = if n == MSG.len() as u64 { 42 } else { 1 };
    // SAFETY: exit_group never returns.
    unsafe {
        syscall3(sys::EXIT_GROUP, code, 0, 0);
    }
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: exit_group never returns.
    unsafe {
        syscall3(sys::EXIT_GROUP, 0xFF, 0, 0);
    }
    loop {}
}
