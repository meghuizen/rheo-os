//! `linuxauxv` - a bare program (raw Linux ABI) that validates the initial
//! stack the Linux personality builds (docs/LINUX-COMPAT.md L1). Its naked
//! `_start` captures the initial SP and walks past argc / argv / envp to the
//! **auxiliary vector**, then checks the entries glibc's startup relies on:
//! AT_PAGESZ == 4096 and AT_RANDOM points at 16 nonzero bytes. It exits 42
//! only if the auxv is well-formed, so the `linuxrun` test's exit-code
//! assertion covers `load::load_elf_linux` + `linux::stack::setup_stack`.
//!
//! (FP/SIMD enablement is not exercised here: on these soft-float bare
//! targets Rust float ops become soft-float calls, not hardware FP. The real
//! FP proof is L2, when glibc's SIMD `memcpy` runs; L1's job is the auxv.)

#![no_std]
#![no_main]
#![allow(clippy::empty_loop)]

use core::arch::{asm, naked_asm};

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

const AT_NULL: u64 = 0;
const AT_PAGESZ: u64 = 6;
const AT_RANDOM: u64 = 25;

const MSG: &[u8] = b"linux-abi: auxv validated\n";

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
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// Naked entry: pass the initial SP (points at argc) to `rust_main`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    #[cfg(target_arch = "x86_64")]
    naked_asm!("mov rdi, rsp", "and rsp, -16", "call {m}", m = sym rust_main);
    #[cfg(target_arch = "aarch64")]
    naked_asm!("mov x0, sp", "b {m}", m = sym rust_main);
    #[cfg(target_arch = "riscv64")]
    naked_asm!("mv a0, sp", "call {m}", m = sym rust_main);
}

extern "C" fn rust_main(sp: *const usize) -> ! {
    let mut ok = validate(sp);
    if ok {
        // SAFETY: valid buffer in this cell's pages.
        let n = unsafe { syscall3(sys::WRITE, 1, MSG.as_ptr() as u64, MSG.len() as u64) };
        ok = n == MSG.len() as u64;
    }
    let code = if ok { 42 } else { 1 };
    // SAFETY: exit_group never returns.
    unsafe { syscall3(sys::EXIT_GROUP, code, 0, 0) };
    loop {}
}

/// Walk argc / argv / envp to the auxv and check AT_PAGESZ and AT_RANDOM.
fn validate(sp: *const usize) -> bool {
    // SAFETY: the kernel guarantees the System V layout at `sp`
    // (linux::stack::setup_stack).
    unsafe {
        let argc = sp.read();
        let mut p = sp.add(1 + argc + 1); // skip argc, argv[argc], NULL
        while p.read() != 0 {
            p = p.add(1); // skip envp
        }
        p = p.add(1); // skip envp NULL -> first auxv entry
        let mut pagesz = 0u64;
        let mut random_va = 0usize;
        loop {
            let t = p.read() as u64;
            let v = p.add(1).read();
            if t == AT_NULL {
                break;
            }
            if t == AT_PAGESZ {
                pagesz = v as u64;
            }
            if t == AT_RANDOM {
                random_va = v;
            }
            p = p.add(2);
        }
        if pagesz != 4096 || random_va == 0 {
            return false;
        }
        // AT_RANDOM must point at 16 bytes; require at least one nonzero
        // (the DRBG fill - a 16-zero draw is astronomically unlikely).
        let rnd = random_va as *const u8;
        (0..16).any(|i| rnd.add(i).read() != 0)
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { syscall3(sys::EXIT_GROUP, 0xFF, 0, 0) };
    loop {}
}
