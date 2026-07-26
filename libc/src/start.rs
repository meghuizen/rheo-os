//! crt0: the ELF entry point (docs/USERLAND.md M3). Initialises the heap,
//! calls the program's `main`, and exits with its return code. argc/argv/envp
//! are not wired yet (argv arrives when a shell/exec passes it - future work).

unsafe extern "C" {
    fn main() -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(_arg: u64) -> ! {
    // SAFETY: runs once at process start, before any allocation, on a fresh
    // stack the kernel set up; `main` is provided by the linked program.
    unsafe {
        crate::mem::init_heap();
        let code = main();
        crate::sys::exit(code as u32 as u64);
    }
}
