//! In-QEMU test kernel for hardware discovery. Boots, runs the discovery
//! pass, prints the machine inventory (firmware source, CPU features,
//! memory map, NUMA nodes, PCIe devices), and asserts the basics the
//! platform must report.

#![no_std]
#![no_main]

use kernel::hw;
use kernel::{arch, println};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("hwinfo: start on {}", arch::NAME);

    hw::print_summary();

    let inv = hw::inventory();
    assert!(inv.firmware != hw::Firmware::None, "no firmware discovered");
    assert!(inv.ncpus >= 1, "no CPUs discovered");
    assert!(
        inv.ram_bytes() >= 64 * 1024 * 1024,
        "too little RAM discovered"
    );
    assert!(inv.nnodes >= 1, "no NUMA nodes");
    assert!(inv.cpu.features != 0, "no CPU features detected");

    println!("hwinfo: PASS");
    arch::exit(arch::ExitCode::Success)
}
