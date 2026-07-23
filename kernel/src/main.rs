//! The rheo-os kernel. BUILD-ORDER.md step 1: boot, print over serial,
//! exit clean through the QEMU test device on all three ISAs.

#![no_std]
#![no_main]

mod arch;
#[macro_use]
mod console;
mod panic;

/// Reached from the per-ISA assembly entry (kernel/arch/<isa>/boot.S) with
/// the stack set up and the BSS cleared.
#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::serial_init();
    println!("rheo-os: kernel booted on {}", arch::NAME);
    arch::exit(arch::ExitCode::Success)
}
