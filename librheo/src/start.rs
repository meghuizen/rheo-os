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

#[unsafe(no_mangle)]
pub extern "C" fn _start(arg: u64) -> ! {
    // SAFETY: runs once at process start, before any allocation, on a fresh
    // stack the kernel set up; `main` is provided by the linked program. `arg`
    // is the initial-stack pointer (the SysV `argc` block) the kernel entered
    // with (docs/LIBRHEO.md Phase F); 0 for a top cell with no arguments.
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
