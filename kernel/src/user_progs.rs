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
    Params, SHELL_BUF, SYS_ARM_TIMER, SYS_CAP_DERIVE, SYS_CAP_DROP, SYS_CAP_INFO, SYS_CAP_REVOKE,
    SYS_CAPS, SYS_CPUINFO, SYS_CYCLES, SYS_DOORBELL, SYS_EVENT_COUNT, SYS_EVENT_EMIT, SYS_EXIT,
    SYS_GRAPH, SYS_LEASE, SYS_LSPCI, SYS_MEMINFO, SYS_MMAP, SYS_MUNMAP, SYS_NUMA, SYS_PS,
    SYS_QUEUE_INFO, SYS_RANDOM, SYS_READLINE, SYS_RESERVE, SYS_SWITCH, SYS_UPTIME, SYS_WAIT_INPUT,
    SYS_WAIT_NET, SYS_WRITE, SYS_YIELD, ShellIo, WORKLOAD_CROSSCELL, WORKLOAD_ROUNDTRIP,
    WORKLOAD_SYSCALL,
};
use crate::capability::{
    DELEGATE as RIGHT_DELEGATE, READ as RIGHT_READ, REVOKE as RIGHT_REVOKE, WRITE as RIGHT_WRITE,
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

// A four-argument syscall, for the verbs whose ABI needs more than one
// register (`SYS_MUNMAP(va, len)`, `SYS_GRANT(out_va, len, kind, flags)`).
// Same shape and register convention as `syscall` above; unused arguments are
// passed as zero. Per-ISA because it is the U-mode syscall instruction itself.

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") nr,
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            options(nostack),
        );
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") nr,
            inlateout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            options(nostack),
        );
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
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

// ------------------------------------------- scheduler idle state (2.4 keystone)
//
// The two cells of the `schedidle` proof (docs/ARCHITECTURE-DEBT.md 2.4). One
// **blocks** on a wake source; the other must demonstrably **run while it is
// blocked**. The evidence is an ordering vector in a page mapped read-write into
// both cells: each cell appends its own marker, so neither can manufacture the
// other's - the `netservice` interleave-witness pattern (docs/ENGINEERING.md 1).
//
// Shared page layout: byte 0 is the append cursor, bytes 1..=ORDER_MAX the order
// vector, and `ORDER_MAX + 1` onward a scratch area the blocker hands to
// `SYS_WAIT_INPUT` / `SYS_WAIT_NET` as its destination buffer.

/// Order-vector capacity in the shared page.
pub const ORDER_MAX: usize = 60;
/// Offset of the blocker's I/O destination inside the shared page.
pub const ORDER_IO_OFF: usize = 64;

/// `Params.workload` selector for [`user_blocker`].
pub const BLOCK_TIMER: u64 = 0;
/// Block in `SYS_WAIT_INPUT` (console).
pub const BLOCK_CONSOLE: u64 = 1;
/// Block in `SYS_WAIT_NET` (a bounded receive).
pub const BLOCK_NET: u64 = 2;

/// Append one marker byte to the shared order vector. Hand-bounded (a raw compare,
/// no slice indexing) so it cannot call a panic path in unmapped kernel `.text`.
#[inline(always)]
unsafe fn order_append(shared: *mut u8, c: u8) {
    unsafe {
        let n = shared.read_volatile();
        if (n as usize) < ORDER_MAX {
            shared.add(1 + n as usize).write_volatile(c);
            shared.write_volatile(n + 1);
        }
    }
}

/// The **blocking** cell. `workload` picks the wait, `iters` its argument (a
/// nanosecond deadline for the timer and the bounded receive), and `ticks` carries
/// the shared page's VA in. Appends `b` before the wait and `B` after it, and
/// reports the syscall's return in `ops`.
///
/// Pre-fix, all three waits ran to completion **inside the trap**, so the peer could
/// not run at all and the order vector would read `b B` with no peer marker between.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_blocker(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    unsafe {
        let mode = (*p).workload;
        let arg = (*p).iters;
        let shared = (*p).ticks as *mut u8;
        let io = shared.add(ORDER_IO_OFF) as u64;
        order_append(shared, b'b');
        let r = match mode {
            BLOCK_CONSOLE => syscall4(SYS_WAIT_INPUT, io, 8, 0, 0),
            BLOCK_NET => syscall4(SYS_WAIT_NET, io, 1514, arg, 0),
            _ => syscall4(SYS_ARM_TIMER, arg, 0, 0, 0),
        };
        order_append(shared, b'B');
        (*p).ops = r;
        (*p).status = 1; // reached the end of the wait
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The **peer** cell: append `S` and yield, `iters` times, then park on a long
/// deadline of its own so the run ends on the blocker's wake rather than on this
/// cell's exit. `ticks` carries the shared page VA; `qp_addr` the parking deadline.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_peer(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    unsafe {
        let rounds = (*p).iters;
        let shared = (*p).ticks as *mut u8;
        let park_ns = (*p).qp_addr;
        let mut i = 0u64;
        while i < rounds {
            order_append(shared, b'S');
            (*p).ops = i + 1;
            syscall(SYS_YIELD, 0);
            i += 1;
        }
        (*p).status = 1;
        // Park far beyond the blocker's deadline: now neither cell is runnable, so
        // the scheduler must reach its idle state, and the blocker's (nearer)
        // deadline is what wakes the machine.
        syscall4(SYS_ARM_TIMER, park_ns, 0, 0, 0);
        (*p).status = 2; // only reached if this cell outlived the blocker
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The **spinner** cell: the proof that timer preemption exists
/// (docs/SUBSTRATE.md pillar 3).
///
/// It runs a bounded compute loop and **issues no syscall at all** until it is
/// finished. `workload` is its marker byte, `iters` the number of spin rounds, and
/// `ticks` the shared order-vector page. Each round appends its marker and then
/// burns a fixed amount of arithmetic, so the loop is long enough for a slice to
/// elapse inside it.
///
/// Why this is a proof rather than a demonstration: under **cooperative**
/// scheduling a cell that never traps cannot be stopped, so two of these cells run
/// strictly one after the other and the order vector is a run of one marker
/// followed by a run of the other. An interleaved vector is therefore only
/// producible if something took the CPU away mid-loop, and the only thing that can
/// is the preemption timer. The `preempt` kernel asserts both halves - the
/// uninterleaved control with dispatch off, and the interleave with it on - so the
/// claim is bounded by its own negative case (docs/ENGINEERING.md 1).
///
/// `status` is set to 1 only after the loop completes, so a cell that never got the
/// CPU back is distinguishable from one that finished.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_spinner(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: the cell's own mapped Params page (its entry argument) and the shared
    // order page mapped read-write into it.
    unsafe {
        let marker = (*p).workload as u8;
        let rounds = (*p).iters;
        let shared = (*p).ticks as *mut u8;
        let mut i = 0u64;
        let mut acc = 1u64;
        while i < rounds {
            order_append(shared, marker);
            // Burn work with no call and no memory the kernel owns. Three
            // constraints shape this loop, all from the `.user` window rule
            // (docs/TARGET-ARCHITECTURES.md 4.1): `wrapping_*` so there is no
            // overflow-panic path in kernel `.text`, the result stored so it is not
            // dead code, and **only small immediates** - a 64-bit multiplier cannot
            // be materialised inline on RISC-V, so LLVM puts it in a constant pool
            // in kernel `.rodata`, which a cell has no mapping for. That was not a
            // hypothetical: the first version of this loop used a 64-bit LCG
            // multiplier and faulted at a high-half address before its first round.
            let mut k = 0u64;
            while k < SPIN_WORK {
                acc = acc.wrapping_add(k ^ 0x5f).wrapping_mul(3);
                k += 1;
            }
            (*p).ops = acc;
            i += 1;
        }
        (*p).status = 1;
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The **scratch-register spinner** (x86-64 only): spin with known sentinels in the
/// two registers `sysretq` consumes, and report whether they survived.
///
/// `SYSRET` takes its RIP from RCX and its RFLAGS from R11 - it *consumes* them - which
/// is correct for returning from a `syscall` (the instruction is defined to clobber
/// both) and wrong for resuming a context stopped anywhere else. Timer preemption
/// creates exactly such frames, and if a *sibling's syscall* later switches to one, the
/// syscall trampoline is the path it comes back through. The symptom is not a fault: it
/// is the resumed cell computing with two wrong registers.
///
/// So this cell pins a sentinel in each, spins comparing them against copies held in
/// R8/R9, and writes a nonzero `status` the moment either differs. The whole loop -
/// sentinel load, compare, back-edge - is **one `asm!` block**, so the compiler cannot
/// spill and reload RCX/R11 around the interesting window; that is the same discipline
/// the FP/SIMD `SYS_YIELD` proof needed (docs/LIBRHEO.md), and for the same reason: a
/// register-preservation property is only observable if the register is genuinely live
/// across the switch.
///
/// `iters` is the spin count and `ticks` the shared page (a marker is appended before
/// and after, so the kernel can see the cell reached the loop). On the other ISAs
/// `eret`/`sret` restore the whole register file from the frame and consume nothing, so
/// there is nothing to check and this reports success immediately - stated rather than
/// silently skipped.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_scratch_spinner(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: the cell's own mapped Params page and the shared order page.
    unsafe {
        let rounds = (*p).iters;
        let shared = (*p).ticks as *mut u8;
        order_append(shared, b'C');
        (*p).status = spin_scratch(rounds);
        order_append(shared, b'D');
        (*p).ops = 1;
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// Sentinels chosen so a corrupted value is unmistakable: both halves nonzero, neither
/// a plausible RIP or RFLAGS, and different from each other. Small enough to be
/// materialised by a 32-bit immediate (a 64-bit constant would need a `.rodata` pool a
/// cell cannot reach - the defect the plain spinner hit first).
const SCRATCH_RCX: u64 = 0x5EC0_1234;
const SCRATCH_R11: u64 = 0x0BAD_5678;

/// Spin `rounds` times with the sentinels live in RCX and R11; return 0 if both
/// survived every iteration, 1 if either was seen changed.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn spin_scratch(rounds: u64) -> u64 {
    let bad: u64;
    // SAFETY: pure register arithmetic in the cell's own context; no memory touched.
    unsafe {
        core::arch::asm!(
            "2:",
            "cmp rcx, r8",
            "jne 3f",
            "cmp r11, r9",
            "jne 3f",
            "dec rdx",
            "jnz 2b",
            "xor eax, eax",
            "jmp 4f",
            "3:",
            "mov eax, 1",
            "4:",
            inout("rcx") SCRATCH_RCX => _,
            inout("r11") SCRATCH_R11 => _,
            in("r8") SCRATCH_RCX,
            in("r9") SCRATCH_R11,
            inout("rdx") rounds.max(1) => _,
            out("rax") bad,
            options(nostack, nomem),
        );
    }
    bad
}

/// Nothing to check off x86-64: `eret`/`sret` restore the whole register file from the
/// frame and consume no register, so no equivalent hazard exists. The loop still runs,
/// so the cell is preemptable and the phase's other assertions hold.
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn spin_scratch(rounds: u64) -> u64 {
    let mut acc = 1u64;
    let mut i = 0u64;
    while i < rounds {
        acc = acc.wrapping_add(i ^ 0x5f).wrapping_mul(3);
        i += 1;
    }
    // Consumed so the loop is not dead code; the result is not a property.
    if acc == 0 { 0 } else { 0 }
}

/// Arithmetic operations per spin round in [`user_spinner`].
///
/// Sized so the whole loop is **many** preemption slices long, not one or two. The
/// first value tried (20,000) gave a 24-round loop about 2.7 ms of CPU against a
/// 1 ms default slice - three slices for the entire run - and that was measurably
/// flaky: whether an interleave appeared depended on how QEMU's TCG happened to be
/// scheduled by the host. A proof whose outcome depends on host load is not a proof,
/// so the work is an order of magnitude larger, giving tens of slices per run.
/// Still far inside the 120 s boot budget.
pub const SPIN_WORK: u64 = 200_000;

// ------------------------------------------------------- security attacker
//
// The `security` test kernel's probes (docs/ENGINEERING.md 12). An
// **unprivileged** U-mode cell attempts each of the three audited attacks and
// reports what the kernel returned; the kernel then asserts both the return code
// and an invariant the cell cannot fake (a canary word it never mapped, the
// frame-pool free count, a still-working queue ring).
//
// Each attack is its **own entry point** rather than one function switching on
// `Params.workload`: a dense integer dispatch - `match` or an if/else chain -
// lowers to a jump table in kernel `.rodata`, which a cell cannot read, so the
// probe would fault before attacking anything. `Params.iters` carries the
// address the kernel wants probed; `ticks`/`ops`/`status` carry results back.

/// Call `SYS_QUEUE_INFO(out_va = Params.iters)` and report the return in
/// `status`. With a kernel VA (or a null / unaligned / out-of-range one) the call
/// must be refused; with the cell's own `Params.ticks` it must succeed, and the
/// 16-byte `QueueInfo` then lands in `ticks` (qp_va) and `ops` (cap_id).
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_attack_out(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: the cell's own mapped Params page (its entry argument).
    unsafe {
        let out = (*p).iters;
        (*p).status = syscall(SYS_QUEUE_INFO, out);
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The capability-surface probe (docs/ARCHITECTURE-DEBT.md 2.1): an
/// **unprivileged** cell derives, inspects, revokes and drops capabilities in
/// its own table, and reports which of seven checks held.
///
/// `Params.iters` carries the 32-bit id of a capability the kernel minted into
/// this cell with `READ|WRITE|DELEGATE|REVOKE`. On return: `status` is a bitmask
/// of the checks that passed (all seven = `0x7F`), `ticks` is the derived
/// child's id, `ops` the rights the kernel reported for that child.
///
/// The sequence is straight-line - each step's result feeds one bit - so it
/// compiles to compares and branches, never the dense jump table a
/// `match`-on-integer would put in kernel `.rodata` that this cell cannot read.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_cap_probe(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // A `CapInfo` is object:u32, kind:u32, rights:u32, pad:u32, budget:u64 -
    // read as three words so no struct literal (and no `memset` call into
    // kernel `.text`, which is not mapped here) is needed.
    let mut info: [u64; 3] = [0, 0, 0];
    let mut child: u32 = 0;
    // SAFETY: the cell's own mapped Params page and two stack locals, all
    // inside this cell's user VA range - which is exactly what `user_out`
    // requires of an out-parameter.
    unsafe {
        let info_va = info.as_mut_ptr() as u64;
        let child_va = &mut child as *mut u32 as u64;
        let parent = (*p).iters;
        let mut ok: u64 = 0;

        // 1. The parent reports the rights the kernel actually stored. Without
        //    this the rest proves nothing: every later comparison is against a
        //    number the cell would otherwise be assuming.
        if syscall4(SYS_CAP_INFO, parent, info_va, 0, 0) == 0 {
            let rights = info[1] & 0xFFFF_FFFF;
            if rights == (RIGHT_READ | RIGHT_WRITE | RIGHT_DELEGATE | RIGHT_REVOKE) as u64 {
                ok |= 1 << 0;
            }
        }

        // 2. Deriving a narrower capability succeeds.
        if syscall4(
            SYS_CAP_DERIVE,
            parent,
            RIGHT_READ as u64,
            u64::MAX,
            child_va,
        ) == 0
        {
            ok |= 1 << 1;
        }
        let kid = child as u64;

        // 3. The child carries exactly READ - and names the *same object*, so
        //    it is an attenuation of this capability and not some unrelated one.
        if syscall4(SYS_CAP_INFO, kid, info_va, 0, 0) == 0 {
            (*p).ops = info[1] & 0xFFFF_FFFF;
            if (info[1] & 0xFFFF_FFFF) == RIGHT_READ as u64 {
                ok |= 1 << 2;
            }
        }

        // 8. Drop releases a capability, and a *second* drop of the same one is
        //    refused rather than quietly succeeding - a double free that
        //    reported 0 would hide a real bug in whatever did it.
        let mut spare: u32 = 0;
        let spare_va = &mut spare as *mut u32 as u64;
        if syscall4(
            SYS_CAP_DERIVE,
            parent,
            RIGHT_READ as u64,
            u64::MAX,
            spare_va,
        ) == 0
        {
            let s = spare as u64;
            if syscall4(SYS_CAP_DROP, s, 0, 0, 0) == 0 && syscall4(SYS_CAP_DROP, s, 0, 0, 0) != 0 {
                ok |= 1 << 7;
            }
        }

        // 4. Widening is refused. The subset test runs against the parent's
        //    stored rights, so there is nothing the cell can pass to defeat it
        //    (ARCHITECTURE.md 8.2, monotonic attenuation).
        if syscall4(
            SYS_CAP_DERIVE,
            kid,
            (RIGHT_READ | RIGHT_WRITE) as u64,
            u64::MAX,
            child_va,
        ) != 0
        {
            ok |= 1 << 3;
        }

        // 5. The child cannot revoke: REVOKE is its own right and was not
        //    derived. Handing someone read access must not hand them the power
        //    to invalidate the object for everyone.
        if syscall4(SYS_CAP_REVOKE, kid, 0, 0, 0) != 0 {
            ok |= 1 << 4;
        }

        // 6. The parent can.
        if syscall4(SYS_CAP_REVOKE, parent, 0, 0, 0) == 0 {
            ok |= 1 << 5;
        }

        // 7. And the revoke killed the *derived* capability too, which is the
        //    whole promise of epoch revocation - one increment, every
        //    outstanding capability to that object, no table walked.
        if syscall4(SYS_CAP_INFO, kid, info_va, 0, 0) != 0 {
            ok |= 1 << 6;
        }

        (*p).ticks = kid;
        (*p).status = ok;
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// Call `SYS_MMAP(len = Params.iters)` and report the base VA in `ticks`
/// (0 = refused).
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_attack_mmap(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: as above.
    unsafe {
        let len = (*p).iters;
        (*p).ticks = syscall(SYS_MMAP, len);
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// The legitimate anon round trip librheo's `mem::Grant`/`Mapping` drop relies
/// on: map two pages, write one, read it back, unmap them. `ticks` = the base VA,
/// `status` = the value read back (1 if the mapping worked), `ops` = the
/// `SYS_MUNMAP` return (0 = accepted).
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_attack_mmap_roundtrip(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: as above; `base` is a mapping the kernel just made for this cell.
    unsafe {
        let base = syscall(SYS_MMAP, 8192);
        (*p).ticks = base;
        if base != 0 {
            (base as *mut u64).write_volatile(1);
            (*p).status = (base as *const u64).read_volatile();
            (*p).ops = syscall4(SYS_MUNMAP, base, 8192, 0, 0);
        }
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// Call `SYS_MUNMAP(Params.iters, 4096)` and report the return in `ticks`
/// (`u64::MAX` = refused). Used for a kernel VA, the cell's own `.user` stack,
/// and the channel / loaded-queue / unreserved-grant region bases.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_attack_munmap(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: as above.
    unsafe {
        let va = (*p).iters;
        (*p).ticks = syscall4(SYS_MUNMAP, va, 4096, 0, 0);
        syscall(SYS_EXIT, 0);
    }
    loop {}
}

/// `SYS_MUNMAP` of the cell's **own queue-pair region** (`Params.iters`), then a
/// full `OP_NOP` round trip over that ring. `ticks` = the munmap return,
/// `ops` = the completion status, `status` = 1 if a completion came back - so a
/// refused munmap is proven not to have broken the ring the kernel still holds an
/// overlay onto.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn user_attack_munmap_queue(params_va: usize) -> ! {
    let p = params_va as *mut Params;
    // SAFETY: as above; `qp_addr`/`cap_id` are the cell's own queue overlay and
    // capability, handed to it by the loader.
    unsafe {
        let va = (*p).iters;
        (*p).ticks = syscall4(SYS_MUNMAP, va, 4096, 0, 0);
        let qp = (*p).qp_addr as *const QueuePair;
        let cap = (*p).cap_id as u32;
        if (*qp).submit(OP_NOP, cap, 0, 0) {
            syscall(SYS_DOORBELL, 0);
            if let Some(st) = (*qp).reap() {
                (*p).ops = st as u64;
                (*p).status = 1;
            }
        }
        syscall(SYS_EXIT, 0);
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

// A per-cell RNG read as a pure library call (ChaCha20 over the cell's own
// state, no syscall) is the docs/TIME-IDENTITY.md 4 ideal. It was prototyped
// here but removed: non-trivial Rust in a U-mode cell can emit memcpy/memset/
// .rodata references into unmapped kernel sections, which fault, and keeping
// it hazard-free across codegen changes proved unmaintainable. The shell's
// `rand` uses the kernel DRBG via SYS_RANDOM (like every other builtin) until
// the runtime-in-U-mode work lands. The kernel DRBG and the host RNG
// comparison (comparison/rng) still show the library-call model's speed.

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
        // Draw from the kernel's per-cell DRBG via a syscall. The design's
        // fast path is a *library call* over the cell's own DRBG state
        // (TIME-IDENTITY.md 4) - `urng_next_u64` implements exactly that and
        // is validated in kernel context by the `rng` test - but running it in
        // U-mode is gated on the runtime-in-U-mode work: non-trivial Rust in a
        // cell can emit memcpy/memset/rodata references into unmapped kernel
        // sections, the same constraint the shell's hand-written code obeys.
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
