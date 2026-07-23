//! The U-mode programs. These run in user mode inside a cell's address
//! space; their code and data live in the `.user` section (linker script),
//! mapped with the U bit. They are deliberately self-contained: no panics,
//! no indexing, no calls into kernel `.text` (which is not mapped in
//! U-mode). Everything is register work, raw-pointer volatile access, the
//! inlined queue helpers, and syscalls.
//!
//! Entry convention matches `arch::trapframe_new`: the entry is called as
//! `extern "C" fn(params_va)`, so the cell's Params page arrives in the
//! first argument register.

// SYS_EXIT never returns, so the trailing `loop {}` after it is
// unreachable; the empty-loop lint would rather see a body, but there is
// nothing to spin on in code that cannot be reached.
#![allow(clippy::empty_loop)]

use crate::abi::{
    Params, SYS_CYCLES, SYS_DOORBELL, SYS_EXIT, SYS_SWITCH, WORKLOAD_CROSSCELL, WORKLOAD_ROUNDTRIP,
    WORKLOAD_SYSCALL,
};
use crate::queue::{OP_NOP, QueuePair};

// ------------------------------------------------------- syscall + counter

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        core::arch::asm!("ecall", in("a7") nr, inlateout("a0") arg => ret, options(nostack));
    }
    ret
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn rdcycle() -> u64 {
    let v;
    unsafe { core::arch::asm!("csrr {0}, cycle", out(reg) v, options(nostack, nomem)) };
    v
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") arg => ret, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn rdcycle() -> u64 {
    let v;
    unsafe { core::arch::asm!("mrs {0}, cntvct_el0", out(reg) v, options(nostack, nomem)) };
    v
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall(nr: u64, arg: u64) -> u64 {
    let ret;
    unsafe {
        // System V-ish: nr in rax, arg in rdi, result in rax. syscall
        // clobbers rcx and r11 (return address + flags).
        core::arch::asm!(
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

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdcycle() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nostack, nomem))
    };
    ((hi as u64) << 32) | lo as u64
}

// -------------------------------------------------------------- programs

/// Benchmark worker. Runs the workload named in Params, timing it with the
/// U-mode cycle counter, and writes ticks/ops back before exiting.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_worker(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    let workload = unsafe { (*p).workload };
    let iters = unsafe { (*p).iters };
    let qp = unsafe { (*p).qp_addr } as *const QueuePair;
    let cap_id = unsafe { (*p).cap_id } as u32;

    let start = rdcycle();
    let mut ops = 0u64;
    let mut i = 0u64;
    while i < iters {
        match workload {
            WORKLOAD_ROUNDTRIP => {
                // Submit one op, ring the doorbell, reap the completion:
                // a full queue round trip across the real syscall boundary.
                unsafe {
                    (*qp).submit(OP_NOP, cap_id, 0, 0);
                    syscall(SYS_DOORBELL, 0);
                    (*qp).reap();
                }
            }
            WORKLOAD_SYSCALL => unsafe {
                syscall(SYS_CYCLES, 0);
            },
            WORKLOAD_CROSSCELL => {
                // One directed switch to the peer cell and back (the peer
                // runs user_pong, which switches straight back).
                unsafe {
                    syscall(SYS_SWITCH, 0);
                }
            }
            _ => {}
        }
        ops += 1;
        i += 1;
    }
    let end = rdcycle();
    unsafe {
        (*p).ticks = end.wrapping_sub(start);
        (*p).ops = ops;
        (*p).status = 0;
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The peer for the cross-cell benchmark: bounce every switch straight
/// back. Never exits; the client cell ends the run.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_pong(_params_va: usize) -> ! {
    loop {
        unsafe {
            syscall(SYS_SWITCH, 0);
        }
    }
}

/// Isolation prober. `workload` selects the action, `iters` carries the
/// target user VA. If the action is allowed the prober reports status 1
/// and exits (a test failure - it should have faulted); if the MMU faults,
/// the kernel records the fault and the test passes.
pub const PROBE_READ: u64 = 0;
pub const PROBE_WRITE: u64 = 1;
pub const PROBE_EXEC: u64 = 2;

#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_prober(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    let mode = unsafe { (*p).workload };
    let target = unsafe { (*p).iters } as usize;

    match mode {
        PROBE_READ => unsafe {
            // Volatile so the read is emitted (it may fault); the result
            // is stored back to our own params page so nothing is a
            // non-inlined call into unmapped kernel .text.
            let v = (target as *const u64).read_volatile();
            (*p).ticks = v;
        },
        PROBE_WRITE => unsafe {
            (target as *mut u64).write_volatile(0xDEAD_BEEF);
        },
        PROBE_EXEC => {
            // Call the target as a function - an instruction fetch from a
            // page with no execute permission must fault.
            let f: extern "C" fn(usize) -> ! = unsafe { core::mem::transmute(target) };
            f(params_va);
        }
        _ => {}
    }

    // Reached only if the access was (wrongly) permitted.
    unsafe {
        (*p).status = 1;
        syscall(SYS_EXIT, 1);
    }
    loop {}
}
