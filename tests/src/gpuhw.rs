//! In-QEMU test kernel for real-GPU hardware plumbing
//! (docs/GPU-HARDWARE.md 3 and 12 stage 1): PCIe bridge recursion with
//! kernel-programmed bus numbers, BAR sizing by the mask probe, the
//! capability walk, opt-in BAR assignment from the host bridge's MMIO
//! window, vendor recognition across the major GPU vendors, and GPU
//! engine registration.
//!
//! It also drives every GPU device model QEMU provides, one per vendor:
//! AMD (`ati-vga`), Bochs, Cirrus Logic, VMware SVGA and Red Hat/QXL by a
//! framebuffer-aperture MMIO write + read-back, and virtio-gpu (behind a
//! `pcie-root-port`) by its 2D command driver. x86-64 gets up to six;
//! arm/riscv `virt` up to four (VMware and QXL are x86-only in QEMU). NVIDIA
//! and Intel have no QEMU GPU device model, so their recognition front-ends
//! report skip-with-reason (docs/GPU-HARDWARE.md 12).
//!
//! **"Up to", not "exactly", and the difference is the point.** Which models a
//! QEMU build actually has is a property of that build - QXL needs SPICE compiled
//! in, and a QEMU without it *refuses to launch*, which used to be a zero-byte
//! serial log naming no cause. So `xtask` attaches only the models
//! `-device help` lists and this kernel asserts a property of the bus rather than
//! a copy of QEMU's catalogue: every display function present is recognised, and
//! every one whose vendor has a linear framebuffer is driven. A vendor QEMU cannot
//! model is handled the way NVIDIA and Intel already are - classified from its PCI
//! ID directly, reported absent from the inventory - so recognition stays proven
//! with no such device on the bus.

#![no_std]
#![no_main]

use kernel::hw::{self, gpu};
use kernel::{arch, println, svc};

#[unsafe(no_mangle)]
extern "C" fn kernel_main() -> ! {
    kernel::boot::init();
    println!("gpuhw: start on {}", arch::NAME);

    hw::print_summary();
    let inv = hw::inventory();
    gpu::print_summary(inv);

    // --- Vendor recognition across every QEMU-modelled GPU vendor ------
    // Present on all ISAs: AMD, Bochs, virtio-gpu, Cirrus. x86-64 adds
    // VMware SVGA and QXL (both x86-only in QEMU).
    for (vendor, what) in [
        (gpu::GpuVendor::Amd, "ati-vga 0x1002"),
        (gpu::GpuVendor::QemuBochs, "bochs 0x1234"),
        (gpu::GpuVendor::Virtio, "virtio-gpu 0x1AF4"),
        (gpu::GpuVendor::Cirrus, "cirrus 0x1013"),
    ] {
        assert!(gpu::vendor_present(inv, vendor), "{} not recognised", what);
    }
    // VMware SVGA and Red Hat/QXL are x86-only in QEMU, and QXL additionally needs
    // a QEMU built with SPICE - so they are *reported*, not asserted present.
    // Recognition is still proven either way, because it is a pure function of the
    // PCI vendor id: the ID classifier is checked directly, which is exactly how
    // NVIDIA and Intel are covered with no such device anywhere.
    //
    // No `cfg(target_arch)` here, which is the point: this loop reads identically on
    // all three ISAs and reports what each one has. The previous version asserted
    // these two present under `cfg(target_arch = "x86_64")` - a per-ISA branch
    // outside `arch/` (docs/TARGET-ARCHITECTURES.md 4) that existed only because the
    // vendor set was a hardcoded catalogue. Observing the set removed the need for
    // it.
    for (vendor, id, what) in [
        (gpu::GpuVendor::Vmware, gpu::VENDOR_VMWARE, "vmware-svga"),
        (gpu::GpuVendor::Redhat, gpu::VENDOR_REDHAT, "qxl"),
    ] {
        assert_eq!(
            gpu::GpuVendor::from_id(id),
            vendor,
            "{what} vendor id not classified"
        );
        if gpu::vendor_present(inv, vendor) {
            println!("gpuhw: {what} present and recognised");
        } else {
            println!("gpuhw: skip {what} - this QEMU build has no such device model");
        }
    }
    // No QEMU device model exists for these vendors on any ISA: recognised by ID,
    // honestly absent here (skip-with-reason printed above).
    assert_eq!(
        gpu::GpuVendor::from_id(gpu::VENDOR_NVIDIA),
        gpu::GpuVendor::Nvidia
    );
    assert_eq!(
        gpu::GpuVendor::from_id(gpu::VENDOR_INTEL),
        gpu::GpuVendor::Intel
    );
    assert!(!gpu::vendor_present(inv, gpu::GpuVendor::Nvidia));
    assert!(!gpu::vendor_present(inv, gpu::GpuVendor::Intel));

    // --- Silicon-family classification (per-vendor, per generation) ----
    // The AMD part QEMU models classifies to a concrete family; the
    // NVIDIA/Intel classifiers are exercised directly against known IDs so
    // the per-generation recognition is proven even with no such device.
    let amd = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::Amd)
        .unwrap();
    assert_eq!(amd.arch, gpu::GpuArch::AmdGcn, "ati-vga -> GCN-era family");
    // NVIDIA: an Ampere ID (A100 = 0x20B0) and an Ada ID (RTX 4090 = 0x2684).
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Nvidia, 0x20B0),
        gpu::GpuArch::NvAmpere
    );
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Nvidia, 0x2684),
        gpu::GpuArch::NvAda
    );
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Nvidia, 0x2330),
        gpu::GpuArch::NvHopper
    );
    // AMD CDNA (MI300 = 0x74a0) and RDNA (Navi 0x73bf).
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Amd, 0x74A0),
        gpu::GpuArch::AmdCdna
    );
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Amd, 0x73BF),
        gpu::GpuArch::AmdRdna
    );
    // Intel is Xe.
    assert_eq!(
        gpu::classify_arch(gpu::GpuVendor::Intel, 0x56A0),
        gpu::GpuArch::IntelXe
    );

    // --- Per-vendor driver front-end: a concrete strategy for each -----
    // Every major vendor resolves to a named lowering path (ACCELERATORS.md
    // 4); only virtio resolves to an in-tree driver today.
    for v in [
        gpu::GpuVendor::Nvidia,
        gpu::GpuVendor::Amd,
        gpu::GpuVendor::Intel,
        gpu::GpuVendor::Virtio,
    ] {
        let vd = gpu::vendor_driver(v, gpu::GpuDriver::None);
        assert!(!vd.lowering.is_empty(), "vendor has no lowering strategy");
        assert!(!vd.status.is_empty());
    }
    assert_eq!(
        gpu::vendor_driver(gpu::GpuVendor::Virtio, gpu::GpuDriver::VirtioGpu).driver,
        gpu::GpuDriver::VirtioGpu
    );

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

    // --- Drive every framebuffer-capable GPU: MMIO write + read-back ---
    // The aperture-drive proof (enumeration -> BAR -> mapping window ->
    // MMIO decode -> device memory), generalised to EVERY QEMU-modelled
    // GPU vendor with a linear framebuffer: AMD (ati-vga), Bochs, Cirrus
    // Logic, VMware SVGA, and Red Hat/QXL. virtio-gpu has no linear
    // framebuffer (it is command-queue based, driven by its 2D driver), so
    // `drive_framebuffer` returns None for it. Memory decode is forced on
    // per device so the proof does not depend on who enabled it.
    let inv = hw::inventory();
    // Every vendor here has a linear framebuffer aperture, so an enumerated
    // function of one of these vendors MUST be driven. Counted before driving
    // anything, so the expectation is derived from the bus rather than from the
    // result it is about to check - the difference between an invariant and a
    // restatement.
    let expect_driven = inv.gpus[..inv.ngpu]
        .iter()
        .filter(|g| {
            matches!(
                g.vendor,
                gpu::GpuVendor::Amd
                    | gpu::GpuVendor::QemuBochs
                    | gpu::GpuVendor::Cirrus
                    | gpu::GpuVendor::Vmware
                    | gpu::GpuVendor::Redhat
            )
        })
        .count();
    let mut driven = 0;
    for g in &inv.gpus[..inv.ngpu] {
        let d = &inv.pci[g.pci];
        let cmd = arch::pci_cfg_read32(inv.ecam_base, d.bus, d.dev, d.func, 0x04);
        arch::pci_cfg_write32(inv.ecam_base, d.bus, d.dev, d.func, 0x04, cmd | 0x6);
        match gpu::drive_framebuffer(inv, g) {
            Some(true) => {
                driven += 1;
                println!(
                    "gpuhw: {} ({}) framebuffer MMIO write/read-back OK",
                    g.model(),
                    g.vendor.name()
                );
            }
            Some(false) => panic!("{} framebuffer read-back mismatch", g.model()),
            None => {} // no linear framebuffer (virtio-gpu)
        }
    }
    println!(
        "gpuhw: {} GPU framebuffers driven (write + read-back)",
        driven
    );
    // **Every** aperture-bearing function on the bus was driven - not "at least
    // five", which was a copy of QEMU's device catalogue and went stale the moment
    // a build shipped without one of them. virtio-gpu is excluded because it has no
    // linear framebuffer; it is driven separately by its 2D command driver below.
    assert_eq!(
        driven, expect_driven,
        "{expect_driven} aperture-bearing GPUs enumerated but {driven} driven"
    );
    // A floor, so an empty or barely-populated bus cannot satisfy the equality
    // above trivially. AMD (`ati-vga`), Bochs and Cirrus are in every QEMU build
    // with PCI on all three ISAs; VMware and QXL are the optional ones.
    assert!(
        driven >= 3,
        "expected >= 3 framebuffer-driven vendors, drove {driven}"
    );

    // --- Attach measurement: offload proves itself (transport) ---------
    // Ticks per KiB streamed through each GPU's framebuffer aperture -
    // the attach contract's measurement applied to the only path
    // exercisable without a vendor driver cell. SYS_ENGINE_INFO reports
    // it live (the engine table IS the inventory).
    hw::gpu_attach_measure();
    let inv = hw::inventory();
    for g in &inv.gpus[..inv.ngpu] {
        println!(
            "gpuhw: {} aperture measured {} ticks/KiB",
            g.model(),
            g.measured_cost_ticks
        );
    }
    let amd = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::Amd)
        .unwrap();
    assert!(
        amd.measured_cost_ticks > 0,
        "AMD aperture should have a measured cost"
    );

    // --- Bochs dispi handshake: a second vendor's registers driven -----
    // The bochs-display exposes the Bochs VBE "dispi" interface in its
    // MMIO BAR (QEMU docs/specs/standard-vga.txt: 16-bit registers at
    // 0x500 + index*2). Register 0 is the interface ID: 0xB0C0..0xB0C5.
    let bochs = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::QemuBochs)
        .unwrap();
    let bdev = &inv.pci[bochs.pci];
    let mmio_bar = bdev
        .bars
        .iter()
        .find(|b| !b.io && !b.prefetch && b.size > 0 && b.base != 0)
        .expect("bochs-display MMIO BAR");
    let mmio_va = arch::mmio_map_window(mmio_bar.base as usize, mmio_bar.size as usize);
    // SAFETY: mmio_va maps the sized MMIO BAR; 0x500 is inside its 4 KiB.
    let dispi_id = unsafe { ((mmio_va + 0x500) as *const u16).read_volatile() };
    println!("gpuhw: bochs dispi id = {:#06x}", dispi_id);
    assert_eq!(
        dispi_id & 0xFFF0,
        0xB0C0,
        "bochs dispi interface ID handshake failed"
    );

    // --- Real 2D modeset: drive the Bochs display to a mode + render ---
    // Beyond the register handshake: program a real VBE mode via the DISPI
    // interface, render a pattern into the linear framebuffer, and read it
    // back - a working 2D driver bring-up on a real QEMU device model
    // (docs/GPU-HARDWARE.md 12).
    let bochs = inv.gpus[..inv.ngpu]
        .iter()
        .find(|g| g.vendor == gpu::GpuVendor::QemuBochs)
        .unwrap();
    let fb = gpu::bochs_modeset(inv, bochs, 640, 480).expect("bochs 640x480 modeset");
    println!(
        "gpuhw: bochs modeset {}x{}x{} stride={}",
        fb.width, fb.height, fb.bpp, fb.stride
    );
    // Render a diagonal of known pixels and read them back through the LFB.
    for i in 0..64u32 {
        fb.put(i, i, 0x00FF_0000 | i); // red channel + a marker in blue
    }
    for i in 0..64u32 {
        assert_eq!(
            fb.get(i, i),
            0x00FF_0000 | i,
            "bochs framebuffer pixel read-back mismatch"
        );
    }
    println!("gpuhw: bochs 2D framebuffer render + read-back OK");

    // --- Engine registration: CPU + every recognised GPU ---------------
    let n = svc::engine_count();
    println!("gpuhw: engines registered = {}", n);
    assert!(
        n > inv.ngpu,
        "engine table should hold the CPU plus every recognised GPU"
    );

    println!("gpuhw: PASS");
    arch::exit(arch::ExitCode::Success)
}
