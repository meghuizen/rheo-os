//! crt0 for rheo-os std programs (docs/USERLAND.md M4). The kernel enters a
//! loaded ELF at `_start` with the stack set up; `_start` calls the
//! compiler-generated C `main(argc, argv)` (which runs `std::rt::lang_start`)
//! and leaves U-mode via `SYS_EXIT_GROUP` with its return code. argv is empty
//! for now (no exec/argv path yet).
#![no_std]

use core::arch::asm;

const SYS_EXIT_GROUP: u64 = 22;

unsafe extern "C" {
    fn main(argc: i32, argv: *const *const u8) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // SAFETY: `main` is the std program's entry; argv is empty (argc 0).
    let code = unsafe { main(0, core::ptr::null()) };
    exit(code as u64)
}

fn exit(code: u64) -> ! {
    // SAFETY: SYS_EXIT_GROUP never returns.
    unsafe {
        #[cfg(target_arch = "riscv64")]
        asm!("ecall", in("a7") SYS_EXIT_GROUP, in("a0") code, options(noreturn, nostack));
        #[cfg(target_arch = "aarch64")]
        asm!("svc #0", in("x8") SYS_EXIT_GROUP, in("x0") code, options(noreturn, nostack));
        #[cfg(target_arch = "x86_64")]
        asm!("syscall", in("rax") SYS_EXIT_GROUP, in("rdi") code, options(noreturn, nostack));
    }
}
