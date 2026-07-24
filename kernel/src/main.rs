//! The boot demo kernel. BUILD-ORDER.md step 1: boot, print over serial,
//! exit clean through the QEMU test device on all three ISAs.

#![no_std]
#![no_main]

use kernel::{arch, println};

/// Reached from the per-ISA assembly entry (kernel/arch/<isa>/boot.S) with
/// the stack set up and the BSS cleared.
#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("rheo-os: kernel booted on {}", arch::NAME);

    // Deliberate trap, caught and returned from (BUILD-ORDER.md step 2).
    arch::doorbell_trap();
    assert_eq!(
        arch::doorbell_count(),
        1,
        "doorbell trap did not round-trip"
    );
    println!("rheo-os: trap round trip OK");

    arch::exit(arch::ExitCode::Success)
}
