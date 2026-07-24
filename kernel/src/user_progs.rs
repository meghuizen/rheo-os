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
    Params, SHELL_BUF, SYS_CAPS, SYS_CPUINFO, SYS_CYCLES, SYS_DOORBELL, SYS_EVENT_COUNT,
    SYS_EVENT_EMIT, SYS_EXIT, SYS_GRAPH, SYS_LEASE, SYS_LSPCI, SYS_MEMINFO, SYS_NUMA, SYS_PS,
    SYS_RANDOM, SYS_READLINE, SYS_RESERVE, SYS_SWITCH, SYS_UPTIME, SYS_WRITE, ShellIo,
    WORKLOAD_CROSSCELL, WORKLOAD_ROUNDTRIP, WORKLOAD_SYSCALL,
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

// ------------------------------------------------------------- lsh shell
//
// A freestanding shell running as a U-mode cell. Everything below runs in
// the cell's address space: helpers live in .user.text, string constants
// in .user.rodata (both shared, mapped into the cell), and all buffer
// access is through raw pointers so nothing calls into unmapped kernel
// .text. The shell reads a line from the PTY (SYS_READLINE), dispatches a
// builtin, and writes the response (SYS_WRITE); resource-touching builtins
// call the matching syscall and format the result here.

/// Place a byte-string constant in .user.rodata, sized to fit.
macro_rules! rostr {
    ($name:ident, $lit:literal) => {
        #[unsafe(link_section = ".user.rodata")]
        static $name: [u8; $lit.len()] = *$lit;
    };
}

rostr!(S_BANNER, b"\r\nlsh - the Lattice shell. type 'help'.\r\n");
rostr!(S_PROMPT, b"lsh> ");
rostr!(S_BYE, b"lsh: goodbye\r\n");
rostr!(S_NL, b"\r\n");
rostr!(
    S_HELP,
    b"builtins: help echo uptime rand meminfo ps caps event graph reserve lease cpuinfo lspci numa exit\r\n"
);
rostr!(S_UPTIME, b"uptime ticks: ");
rostr!(S_RAND, b"random: ");
rostr!(S_MEM_A, b"frames free ");
rostr!(S_MEM_B, b" of ");
rostr!(S_PS, b"cells running: ");
rostr!(S_CAPS, b"capabilities held: ");
rostr!(S_EV_A, b"events buffered ");
rostr!(S_EV_B, b" total ");
rostr!(S_GRAPH, b"graph (n+1)*n = ");
rostr!(S_RES_OK, b"reservation admitted; utilization ppm ");
rostr!(
    S_RES_NO,
    b"reservation refused (would overcommit the core)\r\n"
);
rostr!(S_LEASE, b"lease acquired; fencing token ");
rostr!(S_UNK, b"lsh: unknown command (try 'help')\r\n");

// -- output builder over a raw buffer (all in .user.text) --

#[unsafe(link_section = ".user.text")]
fn w_byte(buf: *mut u8, pos: &mut usize, b: u8) {
    if *pos < SHELL_BUF {
        unsafe { buf.add(*pos).write(b) };
        *pos += 1;
    }
}

#[unsafe(link_section = ".user.text")]
fn w_str(buf: *mut u8, pos: &mut usize, s: &[u8]) {
    let mut i = 0;
    while i < s.len() {
        w_byte(buf, pos, unsafe { *s.as_ptr().add(i) });
        i += 1;
    }
}

#[unsafe(link_section = ".user.text")]
fn w_u64(buf: *mut u8, pos: &mut usize, mut v: u64) {
    if v == 0 {
        w_byte(buf, pos, b'0');
        return;
    }
    let mut tmp = [0u8; 20];
    let tp = tmp.as_mut_ptr();
    let mut n = 0usize;
    while v > 0 {
        unsafe { tp.add(n).write(b'0' + (v % 10) as u8) };
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        w_byte(buf, pos, unsafe { *tp.add(n) });
    }
}

#[unsafe(link_section = ".user.text")]
fn w_hex(buf: *mut u8, pos: &mut usize, v: u64) {
    w_byte(buf, pos, b'0');
    w_byte(buf, pos, b'x');
    let mut shift = 60i32;
    let mut started = false;
    while shift >= 0 {
        let nib = ((v >> shift) & 0xF) as u8;
        if nib != 0 || started || shift == 0 {
            started = true;
            let c = if nib < 10 {
                b'0' + nib
            } else {
                b'a' + nib - 10
            };
            w_byte(buf, pos, c);
        }
        shift -= 4;
    }
}

// -- input tokenising over a raw buffer (all in .user.text) --

#[unsafe(link_section = ".user.text")]
fn byte_at(p: *const u8, i: usize) -> u8 {
    unsafe { *p.add(i) }
}

#[unsafe(link_section = ".user.text")]
fn tok_len(p: *const u8, len: usize) -> usize {
    let mut i = 0;
    while i < len && byte_at(p, i) != b' ' {
        i += 1;
    }
    i
}

#[unsafe(link_section = ".user.text")]
fn tok_eq(p: *const u8, len: usize, s: &[u8]) -> bool {
    let tl = tok_len(p, len);
    if tl != s.len() {
        return false;
    }
    let mut i = 0;
    while i < tl {
        if byte_at(p, i) != unsafe { *s.as_ptr().add(i) } {
            return false;
        }
        i += 1;
    }
    true
}

/// (pointer, length) of the region after the first token, spaces skipped.
#[unsafe(link_section = ".user.text")]
fn arg_of(p: *const u8, len: usize) -> (*const u8, usize) {
    let mut i = tok_len(p, len);
    while i < len && byte_at(p, i) == b' ' {
        i += 1;
    }
    (unsafe { p.add(i) }, len - i)
}

#[unsafe(link_section = ".user.text")]
fn parse_u64(p: *const u8, len: usize) -> u64 {
    let mut v = 0u64;
    let mut i = 0;
    while i < len {
        let c = byte_at(p, i);
        if !c.is_ascii_digit() {
            break;
        }
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as u64);
        i += 1;
    }
    v
}

/// Advance past a leading number and following spaces (for two-arg cmds).
#[unsafe(link_section = ".user.text")]
fn after_num(p: *const u8, len: usize) -> (*const u8, usize) {
    let mut i = 0;
    while i < len && byte_at(p, i).is_ascii_digit() {
        i += 1;
    }
    while i < len && byte_at(p, i) == b' ' {
        i += 1;
    }
    (unsafe { p.add(i) }, len - i)
}

#[unsafe(link_section = ".user.text")]
fn emit(io: *mut ShellIo, len: usize) {
    unsafe {
        (*io).out_len = len as u64;
        syscall(SYS_WRITE, io as usize as u64);
    }
}

/// Handle one command line; returns true if the shell should exit.
#[unsafe(link_section = ".user.text")]
fn dispatch(inp: *const u8, len: usize, out: *mut u8, p: &mut usize) -> bool {
    if len == 0 {
        return false;
    }
    if tok_eq(inp, len, b"help") {
        w_str(out, p, &S_HELP);
    } else if tok_eq(inp, len, b"echo") {
        let (a, al) = arg_of(inp, len);
        let mut i = 0;
        while i < al {
            w_byte(out, p, byte_at(a, i));
            i += 1;
        }
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"uptime") {
        let t = unsafe { syscall(SYS_UPTIME, 0) };
        w_str(out, p, &S_UPTIME);
        w_u64(out, p, t);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"rand") {
        let v = unsafe { syscall(SYS_RANDOM, 0) };
        w_str(out, p, &S_RAND);
        w_hex(out, p, v);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"meminfo") {
        let m = unsafe { syscall(SYS_MEMINFO, 0) };
        w_str(out, p, &S_MEM_A);
        w_u64(out, p, m >> 32);
        w_str(out, p, &S_MEM_B);
        w_u64(out, p, m & 0xFFFF_FFFF);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"ps") {
        let n = unsafe { syscall(SYS_PS, 0) };
        w_str(out, p, &S_PS);
        w_u64(out, p, n);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"caps") {
        let n = unsafe { syscall(SYS_CAPS, 0) };
        w_str(out, p, &S_CAPS);
        w_u64(out, p, n);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"event") {
        let (a, al) = arg_of(inp, len);
        let kind = parse_u64(a, al);
        unsafe { syscall(SYS_EVENT_EMIT, kind) };
        let c = unsafe { syscall(SYS_EVENT_COUNT, 0) };
        w_str(out, p, &S_EV_A);
        w_u64(out, p, c >> 32);
        w_str(out, p, &S_EV_B);
        w_u64(out, p, c & 0xFFFF_FFFF);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"graph") {
        let (a, al) = arg_of(inp, len);
        let n = parse_u64(a, al);
        let r = unsafe { syscall(SYS_GRAPH, n) };
        w_str(out, p, &S_GRAPH);
        w_u64(out, p, r);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"reserve") {
        let (a, al) = arg_of(inp, len);
        let budget = parse_u64(a, al);
        let (a2, al2) = after_num(a, al);
        let period = parse_u64(a2, al2);
        let arg = (budget << 32) | (period & 0xFFFF_FFFF);
        let r = unsafe { syscall(SYS_RESERVE, arg) };
        if r == u64::MAX {
            w_str(out, p, &S_RES_NO);
        } else {
            w_str(out, p, &S_RES_OK);
            w_u64(out, p, r);
            w_str(out, p, &S_NL);
        }
    } else if tok_eq(inp, len, b"lease") {
        let t = unsafe { syscall(SYS_LEASE, 0) };
        w_str(out, p, &S_LEASE);
        w_u64(out, p, t);
        w_str(out, p, &S_NL);
    } else if tok_eq(inp, len, b"cpuinfo") {
        // Kernel prints the report straight to the console; nothing to
        // format here (p stays 0, so the shell skips its own emit).
        unsafe { syscall(SYS_CPUINFO, 0) };
    } else if tok_eq(inp, len, b"lspci") {
        unsafe { syscall(SYS_LSPCI, 0) };
    } else if tok_eq(inp, len, b"numa") {
        unsafe { syscall(SYS_NUMA, 0) };
    } else if tok_eq(inp, len, b"exit") {
        return true;
    } else {
        w_str(out, p, &S_UNK);
    }
    false
}

/// The shell entry point. `io_va` is the ShellIo block mapped into the
/// cell (kernel fills in_buf on SYS_READLINE, reads out_buf on SYS_WRITE).
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_shell(io_va: usize) -> ! {
    let io = io_va as *mut ShellIo;
    let out = unsafe { core::ptr::addr_of_mut!((*io).out_buf).cast::<u8>() };
    let inp = unsafe { core::ptr::addr_of!((*io).in_buf).cast::<u8>() };

    let mut p = 0usize;
    w_str(out, &mut p, &S_BANNER);
    emit(io, p);

    loop {
        let mut p = 0usize;
        w_str(out, &mut p, &S_PROMPT);
        emit(io, p);

        let got = unsafe { syscall(SYS_READLINE, io_va as u64) };
        if got == 0 {
            break; // end of input
        }
        let len = unsafe { (*io).in_len } as usize;

        let mut p = 0usize;
        let done = dispatch(inp, len, out, &mut p);
        if p > 0 {
            emit(io, p);
        }
        if done {
            break;
        }
    }

    let mut p = 0usize;
    w_str(out, &mut p, &S_BYE);
    emit(io, p);
    unsafe { syscall(SYS_EXIT, 0) };
    loop {}
}
