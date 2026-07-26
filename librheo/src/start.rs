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
    // SAFETY: runs once at process start, before any allocation, on a fresh
    // stack the kernel set up; `main` is provided by the linked program.
    unsafe {
        mem::init_heap();
        rt::set_args(arg);
        // The per-cell DRBG is an extended-feature module; an embedded build
        // (no `full`) omits it to shrink the binary.
        #[cfg(feature = "full")]
        crate::rng::init();
        let info = sys::queue_info().expect("librheo: no queue pair mapped for this cell");
        cap::install(CapSet::new(info.cap_id as u32));
        rt::init(cap::cap_set(), info.qp_va);
        let code = main();
        sys::exit(code as u32 as u64);
    }
}
