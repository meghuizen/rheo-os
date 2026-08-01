//! Panic = print location + message to serial, then exit QEMU with a
//! failure code so CI goes red immediately instead of hanging
//! (DEVELOPMENT.md 7).

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // **Raw, not `println!`.** A panic can be taken while this core already holds the
    // console lock - a panic comes from a fault path, and fault paths print - and acquiring
    // it again would deadlock, turning a reported failure into a 120-second timeout with no
    // output at all. The message is the only thing left that matters, so it goes straight to
    // the wire. Interleaving with another core is a real risk and the right trade: a garbled
    // panic is readable, an absent one is not.
    crate::console::write_raw(format_args!("KERNEL PANIC: {info}\n"));
    // Then whatever was buffered and not yet drained. A boot with console buffering on has
    // its history in the ring, and losing it because the run ended abnormally would make
    // buffering a way to hide the failures it exists to help diagnose. This takes the lock,
    // so it comes *after* the message that has to survive the lock being held.
    crate::console::flush();
    crate::arch::exit(crate::arch::ExitCode::Failure)
}
