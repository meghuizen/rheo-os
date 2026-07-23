//! Panic = print location + message to serial, then exit QEMU with a
//! failure code so CI goes red immediately instead of hanging
//! (DEVELOPMENT.md 7).

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("KERNEL PANIC: {info}");
    crate::arch::exit(crate::arch::ExitCode::Failure)
}
