//! crt0: the ELF entry point (docs/LIBRHEO.md). Brings the cell's foundation
//! up before `main`: the growable heap (so `alloc` works), the per-cell DRBG
//! (seeded once over `SYS_RANDOM`), the capability set + the async reactor
//! bound to the cell's queue pair (discovered via `SYS_QUEUE_INFO`). Then it
//! calls the program's `main` and exits with its return code.

use crate::cap::{self, CapSet};
use crate::{mem, rt, sys};

unsafe extern "C" {
    fn main() -> i32;
}

/// Set by vcore 0 once the cell's foundation is up (heap, DRBG, capability set,
/// reactor). A secondary vcore waits on it before touching any of them.
///
/// In the cell's own `.data`, which every vcore shares by construction - one address
/// space - so waiting needs no allocation.
static PRIMARY_READY: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

// x86-64: the SysV ABI enters `_start` with RSP **16-byte aligned** (pointing
// at argc), but a compiled C function assumes the 8-byte-offset alignment of a
// post-`CALL` stack. A hard-float cell spills SSE registers with `movaps`,
// which faults (`#GP`) on a misaligned address - invisible while the cell was
// soft-float. This asm entry aligns RSP and `call`s the Rust body, so it sees
// the alignment the compiler assumed. The initial-stack pointer is already in
// RDI (the kernel's arg0), untouched here. ARM64/RISC-V put the return address
// in a register (no `CALL` push) and keep SP 16-aligned, so they enter the Rust
// body directly.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global _start",
    "_start:",
    "and rsp, -16",
    "call {start_rust}",
    start_rust = sym start_rust,
);

#[cfg(not(target_arch = "x86_64"))]
#[unsafe(no_mangle)]
pub extern "C" fn _start(arg: u64) -> ! {
    start_rust(arg)
}

/// The Rust crt0 body. `arg` is the initial-stack pointer (the SysV `argc`
/// block) the kernel entered with (docs/LIBRHEO.md Phase F); 0 for a top cell
/// with no arguments.
extern "C" fn start_rust(arg: u64) -> ! {
    // **A secondary vcore enters here too**, and must not redo one-time process setup
    // (docs/SUBSTRATE.md pillar 3, docs/CONCURRENCY.md). Both contexts start at the ELF
    // entry point, because the loader resolves no symbols and asking a cell to have two
    // entry points would make the loader read its symbol table; so the branch belongs
    // here, in the code that knows what "once per process" means. Re-running
    // `init_heap` would reset the allocator's free list under a sibling that is already
    // using it - no fault, wrong memory - and re-seeding the DRBG would hand two
    // contexts the same stream.
    //
    // What a secondary needs is its own reactor bound to its **own** ring, which
    // `SYS_QUEUE_INFO` reports per vcore, and the vcore hook so the strand executor
    // keys its per-vcore state correctly. Everything else is already up.
    if sys::vcore_index() != 0 {
        // **Wait for the primary to have brought the cell up.** The launcher publishes
        // every vcore as runnable at once, so whichever core claims first enters first -
        // a secondary can reach this line before vcore 0 has executed one instruction.
        // Found exactly that way: the first version assumed the primary ran first, and
        // the cell never finished because this context read a capability set that had not
        // been installed. `PRIMARY_READY` lives in the cell's own `.data`, so waiting on
        // it needs no heap and no syscall but the yield.
        //
        // Yields rather than spins: the CPU goes back to the kernel, which is the whole
        // point of having a scheduler. Bounded, so a primary that never comes up ends
        // this context with a distinct code instead of hanging.
        let mut spins = 0u32;
        while PRIMARY_READY.load(core::sync::atomic::Ordering::Acquire) == 0 {
            sys::yield_cell();
            spins += 1;
            if spins > 1_000_000 {
                sys::exit_vcore(38);
            }
        }
        // SAFETY: the cell's foundation was brought up by vcore 0 before this context
        // was ever entered (the launcher installs vcore 0 first and a secondary is
        // only dispatched after); `main` is provided by the linked program.
        unsafe {
            runtime::strand::set_vcore_hook(sys::vcore_index);
            let info = sys::queue_info().expect("librheo: no queue pair for this vcore");
            rt::init(cap::cap_set(), info.qp_va);
            let code = main();
            // **This vcore**, not the cell: `sys::exit` is `SYS_EXIT_GROUP` and would
            // take the siblings down mid-work (docs/SMP.md 10.0a).
            sys::exit_vcore(code as u32 as u64);
        }
    }
    // SAFETY: runs once at process start, before any allocation, on a fresh
    // stack the kernel set up; `main` is provided by the linked program.
    unsafe {
        mem::init_heap();
        // Before any `spawn`: the executor is per vcore and keys on this index, and the
        // hook is how the runtime is *told* it rather than inventing one.
        runtime::strand::set_vcore_hook(sys::vcore_index);
        rt::set_args(arg);
        // The per-cell DRBG is an extended-feature module; an embedded build
        // (no `full`) omits it to shrink the binary.
        #[cfg(feature = "full")]
        crate::rng::init();
        let info = sys::queue_info().expect("librheo: no queue pair mapped for this cell");
        cap::install(CapSet::new(info.cap_id as u32));
        rt::init(cap::cap_set(), info.qp_va);
        // The cell's foundation is up: a secondary vcore may now proceed. Released here
        // and not earlier, because everything above is what a secondary reads.
        PRIMARY_READY.store(1, core::sync::atomic::Ordering::Release);
        let code = main();
        sys::exit(code as u32 as u64);
    }
}
