//! crt0 for rheo-os std programs (docs/USERLAND.md M4/M5). The kernel enters a
//! loaded ELF at `_start` with the stack holding the System V initial process
//! block (`argc`, the `argv` pointer array, then `envp`), SP pointing at
//! `argc` (kernel/src/load.rs `setup_stack`). `_start` captures SP, reads
//! `argc`/`argv`, calls the compiler-generated C `main` (which runs
//! `std::rt::lang_start` and stores argc/argv so `std::env::args` works), then
//! leaves U-mode via `SYS_EXIT_GROUP` with `main`'s return code.
#![no_std]

use core::arch::{asm, naked_asm};

const SYS_EXIT_GROUP: u64 = 22;

unsafe extern "C" {
    fn main(argc: i32, argv: *const *const u8) -> i32;
}

/// ELF entry point. Naked so it reads the initial SP before any prologue
/// touches the stack: it passes SP to `rust_entry` in the first argument
/// register (System V: rdi / x0 / a0).
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    #[cfg(target_arch = "x86_64")]
    naked_asm!("mov rdi, rsp", "and rsp, -16", "call {e}", e = sym rust_entry);
    #[cfg(target_arch = "aarch64")]
    naked_asm!("mov x0, sp", "b {e}", e = sym rust_entry);
    #[cfg(target_arch = "riscv64")]
    naked_asm!("mv a0, sp", "call {e}", e = sym rust_entry);
}

/// Read `argc`/`argv` from the initial stack and hand off to `main`.
extern "C" fn rust_entry(sp: *const usize) -> ! {
    // SAFETY: the kernel guarantees `sp` points at `argc`, followed by an
    // `argc`-long, NULL-terminated `argv` array (load.rs `setup_stack`).
    let argc = unsafe { sp.read() } as i32;
    let argv = unsafe { sp.add(1) } as *const *const u8;
    let code = unsafe { main(argc, argv) };
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
