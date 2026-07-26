//! GPU recognition across the major hardware vendors
//! (docs/GPU-HARDWARE.md 2-3, docs/ACCELERATORS.md 4). Every PCI function
//! with display class 0x03 (or 3D-controller subclass) is classified by
//! vendor ID into a `GpuDevice` record: NVIDIA, AMD/ATI, Intel, the
//! virtio-gpu reference device, and QEMU's Bochs display. The record
//! carries the device's BAR topology - the VRAM aperture (largest
//! prefetchable BAR) and the register window (largest non-prefetchable
//! BAR) - plus the MSI-X/FLR capability bits enumeration found.
//!
//! Honesty (docs/GPU-HARDWARE.md 14): recognition + BAR topology + engine
//! registration is what this module does. Command submission to a vendor
//! GPU is the contained driver cell's job (GPU-HARDWARE.md 5), and no
//! vendor driver exists in-tree; `driver` reports which in-tree driver, if
//! any, can drive the function (today: virtio-gpu only). QEMU models an
//! AMD/ATI part (`ati-vga`), the Bochs display, and virtio-gpu; it has no
//! NVIDIA or Intel GPU model, so those vendors are recognised by ID with
//! nothing to attach to in CI - the per-vendor probe still runs and
//! reports honestly.

use super::{EngineKind, Inventory};

pub const VENDOR_NVIDIA: u16 = 0x10DE;
pub const VENDOR_AMD: u16 = 0x1002;
pub const VENDOR_INTEL: u16 = 0x8086;
pub const VENDOR_VIRTIO: u16 = 0x1AF4;
pub const VENDOR_QEMU: u16 = 0x1234;

/// The major GPU vendors, keyed by PCI vendor ID.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    /// The virtio-gpu paravirtual reference device (vendor 0x1AF4).
    Virtio,
    /// QEMU's Bochs-compatible display (vendor 0x1234).
    QemuBochs,
    Other,
}

impl GpuVendor {
    pub fn from_id(vendor: u16) -> GpuVendor {
        match vendor {
            VENDOR_NVIDIA => GpuVendor::Nvidia,
            VENDOR_AMD => GpuVendor::Amd,
            VENDOR_INTEL => GpuVendor::Intel,
            VENDOR_VIRTIO => GpuVendor::Virtio,
            VENDOR_QEMU => GpuVendor::QemuBochs,
            _ => GpuVendor::Other,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            GpuVendor::Nvidia => "nvidia",
            GpuVendor::Amd => "amd",
            GpuVendor::Intel => "intel",
            GpuVendor::Virtio => "virtio",
            GpuVendor::QemuBochs => "qemu-bochs",
            GpuVendor::Other => "unknown",
        }
    }
}

/// Which in-tree driver can drive this function. `VirtioGpu` is the Phase H
/// 2D driver; every real-vendor GPU is `None` until its contained driver
/// cell exists (docs/GPU-HARDWARE.md 5) - recognised, sized, registered as
/// an engine, and honestly not driven.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GpuDriver {
    VirtioGpu,
    None,
}

/// One recognised GPU function (docs/GPU-HARDWARE.md 2). `pci` indexes the
/// inventory's PCI table; `vram_bytes`/`mmio_bytes` come from the BAR mask
/// probe (aperture sizes, not necessarily full VRAM - resizable BAR is the
/// case where they coincide, GPU-HARDWARE.md 6).
#[derive(Copy, Clone)]
pub struct GpuDevice {
    pub pci: usize,
    pub vendor: GpuVendor,
    pub vendor_id: u16,
    pub device_id: u16,
    pub vram_bytes: u64,
    pub mmio_bytes: u64,
    pub msix: bool,
    pub flr: bool,
    pub driver: GpuDriver,
}

impl GpuDevice {
    pub const EMPTY: GpuDevice = GpuDevice {
        pci: 0,
        vendor: GpuVendor::Other,
        vendor_id: 0,
        device_id: 0,
        vram_bytes: 0,
        mmio_bytes: 0,
        msix: false,
        flr: false,
        driver: GpuDriver::None,
    };

    /// A short human name for the recognised silicon, per vendor. This is
    /// the per-vendor recognition front-end: QEMU's `ati-vga` models the
    /// two listed ATI parts; virtio-gpu is its own spec; NVIDIA/Intel IDs
    /// are recognised by vendor with no device table (nothing to probe in
    /// QEMU - a real driver cell would carry the vendor's own tables).
    pub fn model(&self) -> &'static str {
        match (self.vendor, self.device_id) {
            (GpuVendor::Amd, 0x5046) => "ati rage 128 pro",
            (GpuVendor::Amd, 0x5159) => "ati rv100 (radeon 7000)",
            (GpuVendor::Amd, _) => "amd/ati",
            (GpuVendor::Nvidia, _) => "nvidia",
            (GpuVendor::Intel, _) => "intel",
            (GpuVendor::Virtio, 0x1050) => "virtio-gpu",
            (GpuVendor::Virtio, _) => "virtio (display)",
            (GpuVendor::QemuBochs, _) => "bochs display",
            (GpuVendor::Other, _) => "unknown display",
        }
    }
}

/// Classify every display-class PCI function into the GPU inventory.
/// Called from `hw::detect` after PCIe enumeration; pure classification
/// over already-read config state, no device access.
pub fn probe(inv: &mut Inventory) {
    inv.ngpu = 0;
    for i in 0..inv.npci {
        let d = inv.pci[i];
        if d.engine != EngineKind::Display && d.engine != EngineKind::Gpu {
            continue;
        }
        if inv.ngpu >= super::MAX_GPUS {
            break;
        }
        let vendor = GpuVendor::from_id(d.vendor);

        // BAR topology: the VRAM aperture is the largest prefetchable
        // memory BAR; the register window the largest non-prefetchable one.
        let mut vram = 0u64;
        let mut mmio = 0u64;
        for b in d.bars.iter() {
            if b.io || b.size == 0 {
                continue;
            }
            if b.prefetch {
                if b.size > vram {
                    vram = b.size;
                }
            } else if b.size > mmio {
                mmio = b.size;
            }
        }
        // Devices that put the framebuffer in a non-prefetchable BAR
        // (ati-vga's 32-bit BAR 0 is one): fall back to "largest BAR is
        // the aperture" when no prefetchable BAR exists.
        if vram == 0 && mmio != 0 {
            let mut largest = 0u64;
            for b in d.bars.iter() {
                if !b.io && b.size > largest {
                    largest = b.size;
                }
            }
            if largest > mmio {
                vram = largest;
            }
        }

        let driver = match (vendor, d.device) {
            // virtio-gpu (modern ID 0x1050 = 0x1040 + device type 16).
            (GpuVendor::Virtio, 0x1050) => GpuDriver::VirtioGpu,
            _ => GpuDriver::None,
        };

        inv.gpus[inv.ngpu] = GpuDevice {
            pci: i,
            vendor,
            vendor_id: d.vendor,
            device_id: d.device,
            vram_bytes: vram,
            mmio_bytes: mmio,
            msix: d.msix,
            flr: d.flr,
            driver,
        };
        inv.ngpu += 1;
    }
}

/// Whether any recognised GPU of the given vendor is present.
pub fn vendor_present(inv: &Inventory, vendor: GpuVendor) -> bool {
    inv.gpus[..inv.ngpu].iter().any(|g| g.vendor == vendor)
}

/// Print the GPU inventory, one line per function, with the honest
/// skip-with-reason lines for the major vendors QEMU cannot model.
pub fn print_summary(inv: &Inventory) {
    for g in &inv.gpus[..inv.ngpu] {
        let d = &inv.pci[g.pci];
        crate::println!(
            "gpu: {:02x}:{:02x}.{} {} [{:04x}:{:04x}] vram={} KiB mmio={} KiB msix={} flr={} driver={:?}",
            d.bus,
            d.dev,
            d.func,
            g.model(),
            g.vendor_id,
            g.device_id,
            g.vram_bytes / 1024,
            g.mmio_bytes / 1024,
            g.msix,
            g.flr,
            g.driver
        );
    }
    for (vendor, why) in [
        (GpuVendor::Nvidia, "no QEMU device model"),
        (
            GpuVendor::Intel,
            "no QEMU device model (igd is passthrough-only)",
        ),
    ] {
        if !vendor_present(inv, vendor) {
            crate::println!(
                "gpu: {}: no device present ({}) - recognised by ID, skip-with-reason",
                vendor.name(),
                why
            );
        }
    }
}
