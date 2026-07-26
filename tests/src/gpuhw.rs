//! In-QEMU test kernel for real-GPU hardware plumbing
//! (docs/GPU-HARDWARE.md 3 and 12 stage 1): PCIe bridge recursion with
//! kernel-programmed bus numbers, BAR sizing by the mask probe, the
//! capability walk, opt-in BAR assignment from the host bridge's MMIO
//! window, vendor recognition across the major GPU vendors, and GPU
//! engine registration.
//!
//! QEMU attaches, identically on all three ISAs: an `ati-vga` (a real
//! AMD/ATI vendor ID 0x1002 device model), a `bochs-display`, and a
//! `virtio-gpu-pci` placed BEHIND a `pcie-root-port` - reachable only if
//! enumeration programs the bridge's secondary bus (PVH boots have no
//! firmware to do it). NVIDIA and Intel have no QEMU GPU device model, so
//! their recognition front-ends report skip-with-reason (the honest
//! per-vendor table in docs/GPU-HARDWARE.md 12).

#![no_std]
#![no_main]

use kernel::hw::{self, gpu};
use kernel::{arch, println, svc};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    arch::init();
    println!("gpuhw: start on {}", arch::NAME);

    hw::print_summary();
    let inv = hw::inventory();
    gpu::print_summary(inv);

    // --- Vendor recognition across the major vendors -------------------
    assert!(
        inv.ngpu >= 3,
        "expected >= 3 GPU functions (ati, bochs, virtio)"
    );
    assert!(
        gpu::vendor_present(inv, gpu::GpuVendor::Amd),
        "AMD (ati-vga, vendor 0x1002) not recognised"
    );
    assert!(
        gpu::vendor_present(inv, gpu::GpuVendor::QemuBochs),
        "bochs-display (vendor 0x1234) not recognised"
    );
    assert!(
        gpu::vendor_present(inv, gpu::GpuVendor::Virtio),
        "virtio-gpu (vendor 0x1AF4) not recognised"
    );
    // No QEMU device model exists for these vendors: recognised by ID,
    // honestly absent here (skip-with-reason printed above).
    assert!(!gpu::vendor_present(inv, gpu::GpuVendor::Nvidia));
    assert!(!gpu::vendor_present(inv, gpu::GpuVendor::Intel));

    // --- Bridge recursion: the virtio GPU lives behind a root port -----
    let virtio = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::Virtio)
        .unwrap();
    let vdev = &inv.pci[virtio.pci];
    assert!(
        vdev.bus > 0,
        "virtio-gpu should sit on a secondary bus behind the root port"
    );
    assert!(virtio.msix, "virtio-gpu-pci should expose MSI-X");
    assert_eq!(virtio.driver, gpu::GpuDriver::VirtioGpu);

    // --- BAR sizing: the AMD part's apertures came from the mask probe -
    let amd = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::Amd)
        .unwrap();
    assert!(
        amd.vram_bytes >= 1024 * 1024,
        "ati-vga framebuffer aperture should size >= 1 MiB"
    );
    assert!(amd.mmio_bytes > 0, "ati-vga register window should size");
    assert!(
        amd.vram_bytes.is_power_of_two(),
        "a BAR decodes a power-of-two range"
    );

    // --- Opt-in BAR assignment from the per-ISA MMIO window ------------
    // On x86-64 q35 the `-kernel` loader path runs SeaBIOS, which programs
    // BARs before the kernel boots, so there may be nothing left to assign;
    // on the bare arm/riscv `virt` boots nobody has, and the kernel
    // assigns. Either way, every sized memory BAR of every GPU function
    // must decode somewhere real afterwards.
    let assigned = hw::assign_pci_bars();
    println!(
        "gpuhw: assigned {} BARs (rest were firmware-programmed)",
        assigned
    );
    let inv = hw::inventory();
    for g in &inv.gpus[..inv.ngpu] {
        let d = &inv.pci[g.pci];
        let mut i = 0;
        while i < 6 {
            let b = d.bars[i];
            let step = if b.is64 { 2 } else { 1 };
            if !b.io && b.size > 0 {
                assert!(
                    b.base != 0,
                    "an unassigned GPU BAR survived assign_pci_bars"
                );
            }
            i += step;
        }
    }
    // Read a register back: the device must decode on the recorded base.
    let amd = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::Amd)
        .unwrap();
    let adev = &inv.pci[amd.pci];
    let bar0 = adev.bars[0];
    assert!(bar0.size > 0);
    let readback = arch::pci_cfg_read32(inv.ecam_base, adev.bus, adev.dev, adev.func, 0x10);
    assert_eq!(
        (readback & 0xFFFF_FFF0) as u64,
        bar0.base & 0xFFFF_FFFF,
        "BAR0 read-back does not match the recorded base"
    );

    // --- Engine registration: CPU + every recognised GPU ---------------
    let n = svc::engine_count();
    println!("gpuhw: engines registered = {}", n);
    assert!(
        n >= 1 + inv.ngpu,
        "engine table should hold the CPU plus every recognised GPU"
    );

    println!("gpuhw: PASS");
    arch::exit(arch::ExitCode::Success)
}
