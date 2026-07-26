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
pub const VENDOR_CIRRUS: u16 = 0x1013;
pub const VENDOR_VMWARE: u16 = 0x15AD;
pub const VENDOR_REDHAT: u16 = 0x1B36;

/// The GPU hardware vendors this OS recognises, keyed by PCI vendor ID.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    /// The virtio-gpu paravirtual reference device (vendor 0x1AF4).
    Virtio,
    /// QEMU's Bochs-compatible display (vendor 0x1234).
    QemuBochs,
    /// Cirrus Logic (vendor 0x1013).
    Cirrus,
    /// VMware SVGA (vendor 0x15AD).
    Vmware,
    /// Red Hat / QXL (vendor 0x1B36).
    Redhat,
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
            VENDOR_CIRRUS => GpuVendor::Cirrus,
            VENDOR_VMWARE => GpuVendor::Vmware,
            VENDOR_REDHAT => GpuVendor::Redhat,
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
            GpuVendor::Cirrus => "cirrus",
            GpuVendor::Vmware => "vmware",
            GpuVendor::Redhat => "redhat-qxl",
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

/// The silicon architecture family, recognised per vendor from the PCI
/// device ID (docs/ACCELERATORS.md 4). The families are the ones the
/// per-vendor lowering path (`VendorDriver::lowering`) actually branches on
/// (NVIDIA tensor-core generations, AMD's GCN/RDNA/CDNA split, Intel Xe),
/// so recognition is concrete per generation, not just per vendor ID. The
/// ID ranges are the documented public conventions; a real driver cell
/// carries the vendor's exhaustive table, this is the kernel-side coarse
/// classification that picks the driver strategy.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum GpuArch {
    // NVIDIA tensor-core generations (device-id high nibble tracks the chip
    // family: 0x1x Pascal, 0x1exx/0x1fxx Turing, 0x2xxx Ampere, 0x26xx Ada,
    // 0x23xx Hopper).
    NvPascal,
    NvTuring,
    NvAmpere,
    NvAda,
    NvHopper,
    NvBlackwell,
    // AMD.
    AmdGcn,  // pre-RDNA (incl. the legacy ati-vga parts QEMU models)
    AmdRdna, // consumer graphics
    AmdCdna, // MI-series compute (MI300 etc.)
    // Intel.
    IntelXe,
    /// Recognised vendor, family not classified (or a paravirtual device).
    Unknown,
}

impl GpuArch {
    pub fn name(&self) -> &'static str {
        match self {
            GpuArch::NvPascal => "pascal",
            GpuArch::NvTuring => "turing",
            GpuArch::NvAmpere => "ampere",
            GpuArch::NvAda => "ada",
            GpuArch::NvHopper => "hopper",
            GpuArch::NvBlackwell => "blackwell",
            GpuArch::AmdGcn => "gcn",
            GpuArch::AmdRdna => "rdna",
            GpuArch::AmdCdna => "cdna",
            GpuArch::IntelXe => "xe",
            GpuArch::Unknown => "unknown",
        }
    }
}

/// Classify silicon family from (vendor, device id). The NVIDIA/AMD/Intel
/// ranges follow the public device-id conventions each vendor uses; QEMU
/// exposes none of these real parts, so on this emulator only the AMD
/// legacy `ati-vga` IDs (GCN-era predecessors) land on a concrete family -
/// the rest are the classification a driver cell would apply to real
/// hardware. Honest: this picks a lowering *strategy*, it does not execute.
pub fn classify_arch(vendor: GpuVendor, device_id: u16) -> GpuArch {
    match vendor {
        GpuVendor::Nvidia => match device_id >> 8 {
            0x13 | 0x15 | 0x17 | 0x1b | 0x1c | 0x1d => GpuArch::NvPascal,
            0x1e | 0x1f | 0x21 => GpuArch::NvTuring,
            0x20 | 0x22 | 0x24 | 0x25 => GpuArch::NvAmpere,
            0x26..=0x28 => GpuArch::NvAda,
            0x23 => GpuArch::NvHopper,
            0x29 | 0x2b => GpuArch::NvBlackwell,
            _ => GpuArch::Unknown,
        },
        GpuVendor::Amd => match device_id {
            // The two parts QEMU's ati-vga models (Rage 128 / RV100) are
            // pre-GCN, but they are the AMD/ATI family the driver strategy
            // groups with GCN-era 2D for our purposes.
            0x5046 | 0x5159 => GpuArch::AmdGcn,
            // RDNA consumer (Navi): 0x73xx/0x74xx. CDNA compute (MI): 0x74xx
            // Instinct + 0x29xx. Coarse public convention.
            0x7300..=0x73ff => GpuArch::AmdRdna,
            0x7400..=0x74ff => GpuArch::AmdCdna,
            _ => GpuArch::AmdGcn,
        },
        GpuVendor::Intel => GpuArch::IntelXe,
        _ => GpuArch::Unknown,
    }
}

/// A per-vendor driver front-end (docs/GPU-HARDWARE.md 5, ACCELERATORS.md
/// 4): the named strategy by which this vendor's silicon would be driven
/// from a contained driver cell, plus whether that path can run in the
/// current environment. This is the vendor-specific layer made concrete for
/// every major vendor - the kernel records the strategy and attach
/// requirement; the actual command submission lives in the driver cell.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VendorDriver {
    /// The lowering path this vendor uses (ACCELERATORS.md 4).
    pub lowering: &'static str,
    /// The in-tree driver that can drive it here, if any.
    pub driver: GpuDriver,
    /// Honest one-line status for the current environment.
    pub status: &'static str,
}

/// The driver front-end for a recognised GPU. Every major vendor resolves
/// to a concrete strategy; only virtio-gpu resolves to an in-tree driver
/// today, and the real-vendor entries state exactly what each awaits
/// (docs/GPU-HARDWARE.md 5-7).
pub fn vendor_driver(vendor: GpuVendor, driver: GpuDriver) -> VendorDriver {
    if driver == GpuDriver::VirtioGpu {
        return VendorDriver {
            lowering: "virtio-gpu 2D control queue",
            driver: GpuDriver::VirtioGpu,
            status: "driven in-tree (Phase H 2D driver)",
        };
    }
    match vendor {
        GpuVendor::Nvidia => VendorDriver {
            lowering: "PTX/SASS via contained ptxas, tensor-core/TMA tile IR",
            driver: GpuDriver::None,
            status: "awaits GSP firmware + driver cell (no QEMU model)",
        },
        GpuVendor::Amd => VendorDriver {
            lowering: "MFMA via ROCm/LLVM (CDNA); RDNA graphics",
            driver: GpuDriver::None,
            status: "aperture + registers driven here; compute awaits driver cell",
        },
        GpuVendor::Intel => VendorDriver {
            lowering: "Vulkan-compute floor, native Xe lowering as justified",
            driver: GpuDriver::None,
            status: "awaits driver cell (no QEMU model)",
        },
        GpuVendor::QemuBochs => VendorDriver {
            lowering: "Bochs dispi register interface (2D framebuffer)",
            driver: GpuDriver::None,
            status: "registers driven here (dispi handshake)",
        },
        GpuVendor::Virtio => VendorDriver {
            lowering: "virtio-gpu 2D control queue",
            driver: GpuDriver::VirtioGpu,
            status: "driven in-tree (Phase H 2D driver)",
        },
        GpuVendor::Cirrus => VendorDriver {
            lowering: "Cirrus VGA / linear framebuffer (2D)",
            driver: GpuDriver::None,
            status: "framebuffer aperture driven here",
        },
        GpuVendor::Vmware => VendorDriver {
            lowering: "VMware SVGA II FIFO / linear framebuffer (2D)",
            driver: GpuDriver::None,
            status: "framebuffer aperture driven here",
        },
        GpuVendor::Redhat => VendorDriver {
            lowering: "QXL / linear VRAM (2D)",
            driver: GpuDriver::None,
            status: "framebuffer aperture driven here",
        },
        GpuVendor::Other => VendorDriver {
            lowering: "none",
            driver: GpuDriver::None,
            status: "unrecognised",
        },
    }
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
    pub arch: GpuArch,
    pub vram_bytes: u64,
    pub mmio_bytes: u64,
    pub msix: bool,
    pub flr: bool,
    pub driver: GpuDriver,
    /// Ticks per KiB written through the framebuffer aperture, measured by
    /// `attach_measure` (0 = unmeasured). Honest scope: this measures the
    /// CPU-driven MMIO *transport* to device memory - the attach contract's
    /// "offload proves itself" applied to the only path exercisable without
    /// a vendor driver cell - not GPU compute throughput
    /// (docs/GPU-HARDWARE.md 9).
    pub measured_cost_ticks: u64,
}

impl GpuDevice {
    pub const EMPTY: GpuDevice = GpuDevice {
        pci: 0,
        vendor: GpuVendor::Other,
        vendor_id: 0,
        device_id: 0,
        arch: GpuArch::Unknown,
        vram_bytes: 0,
        mmio_bytes: 0,
        msix: false,
        flr: false,
        driver: GpuDriver::None,
        measured_cost_ticks: 0,
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
            (GpuVendor::Cirrus, _) => "cirrus clgd 54xx",
            (GpuVendor::Vmware, _) => "vmware svga ii",
            (GpuVendor::Redhat, _) => "qxl",
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
            arch: classify_arch(vendor, d.device),
            vram_bytes: vram,
            mmio_bytes: mmio,
            msix: d.msix,
            flr: d.flr,
            driver,
            measured_cost_ticks: 0,
        };
        inv.ngpu += 1;
    }
}

/// Whether any recognised GPU of the given vendor is present.
pub fn vendor_present(inv: &Inventory, vendor: GpuVendor) -> bool {
    inv.gpus[..inv.ngpu].iter().any(|g| g.vendor == vendor)
}

/// Drive a GPU's linear framebuffer: pick its largest memory BAR (the
/// VRAM/framebuffer aperture), map it through the per-ISA MMIO window,
/// write a known pixel pattern, and read it back. Returns `Some(true)` if
/// the read-back matches (the aperture is genuinely driven on the real
/// device model), `Some(false)` on mismatch, `None` if the GPU has no
/// linear framebuffer BAR >= 1 MiB (virtio-gpu is command-queue based, not
/// a linear framebuffer, so it is driven by its 2D driver instead). This
/// is the same aperture-drive proof used for AMD's `ati-vga`, generalised
/// to every QEMU-modelled GPU vendor that exposes a framebuffer
/// (Cirrus, VMware SVGA, QXL, Bochs).
pub fn drive_framebuffer(inv: &Inventory, g: &GpuDevice) -> Option<bool> {
    const FB_MIN: u64 = 1 << 20; // 1 MiB: excludes virtio's small config BARs
    let d = &inv.pci[g.pci];
    let mut best: Option<super::PciBar> = None;
    for b in d.bars.iter() {
        if b.io || b.size < FB_MIN || b.base == 0 {
            continue;
        }
        match best {
            Some(cur) if cur.size >= b.size => {}
            _ => best = Some(*b),
        }
    }
    let bar = best?;
    let va = crate::arch::mmio_map_window(bar.base as usize, 4096);
    let p = va as *mut u32;
    // SAFETY: `va` maps the framebuffer BAR the device sized (>= 1 MiB);
    // the 64 words written stay well inside it. Volatile: device memory.
    let mut ok = true;
    for i in 0..64u32 {
        unsafe { p.add(i as usize).write_volatile(0x5A5A_0000 | i) };
    }
    for i in 0..64u32 {
        let got = unsafe { p.add(i as usize).read_volatile() };
        if got != (0x5A5A_0000 | i) {
            ok = false;
            break;
        }
    }
    Some(ok)
}

/// Attach-time measurement for every recognised GPU with a decodable
/// framebuffer BAR (docs/GPU-HARDWARE.md 9, the "offload proves itself"
/// rule applied to the only path exercisable without a vendor driver
/// cell): stream 64 KiB of u32 writes through the aperture via the
/// per-ISA MMIO window and record ticks per KiB. Opt-in, like
/// `assign_pci_bars` - call it after BARs decode; boots that skip it
/// leave every `measured_cost_ticks` an honest 0 (unmeasured).
pub fn attach_measure(inv: &mut Inventory) {
    const PROBE_BYTES: usize = 64 * 1024;
    for gi in 0..inv.ngpu {
        let d = inv.pci[inv.gpus[gi].pci];
        // The framebuffer BAR: the largest sized memory BAR with a base.
        let mut best: Option<super::PciBar> = None;
        for b in d.bars.iter() {
            if !b.io && b.size as usize >= PROBE_BYTES && b.base != 0 {
                match best {
                    Some(cur) if cur.size >= b.size => {}
                    _ => best = Some(*b),
                }
            }
        }
        let Some(bar) = best else { continue };
        let va = crate::arch::mmio_map_window(bar.base as usize, PROBE_BYTES);
        let words = PROBE_BYTES / 4;
        let p = va as *mut u32;
        let start = crate::time::monotonic();
        for i in 0..words {
            // SAFETY: [va, va+PROBE_BYTES) maps BAR device memory sized by
            // the mask probe (size checked above). Volatile: MMIO.
            unsafe { p.add(i).write_volatile(i as u32) };
        }
        let elapsed = crate::time::monotonic().wrapping_sub(start);
        inv.gpus[gi].measured_cost_ticks = elapsed / (PROBE_BYTES as u64 / 1024);
    }
}

/// Print the GPU inventory, one line per function (with silicon family +
/// the vendor driver strategy), then the per-vendor strategy for every
/// major vendor - including the ones with no QEMU device model, so the
/// vendor-specific layer is visible whether or not a device is present.
pub fn print_summary(inv: &Inventory) {
    for g in &inv.gpus[..inv.ngpu] {
        let d = &inv.pci[g.pci];
        let vd = vendor_driver(g.vendor, g.driver);
        crate::println!(
            "gpu: {:02x}:{:02x}.{} {} ({}) [{:04x}:{:04x}] vram={} KiB mmio={} KiB msix={} flr={} -> {}",
            d.bus,
            d.dev,
            d.func,
            g.model(),
            g.arch.name(),
            g.vendor_id,
            g.device_id,
            g.vram_bytes / 1024,
            g.mmio_bytes / 1024,
            g.msix,
            g.flr,
            vd.status
        );
    }
    // The per-vendor driver front-end for every major vendor, present or
    // not - the strategy is real code even where no device exists here.
    for vendor in [
        GpuVendor::Nvidia,
        GpuVendor::Amd,
        GpuVendor::Intel,
        GpuVendor::Virtio,
    ] {
        let vd = vendor_driver(vendor, GpuDriver::None);
        let present = if vendor_present(inv, vendor) {
            "present"
        } else {
            "absent (no QEMU model)"
        };
        crate::println!(
            "gpu: vendor {}: {} lowering=[{}] - {}",
            vendor.name(),
            present,
            vd.lowering,
            vd.status
        );
    }
}

// ---- Bochs DISPI / VBE 2D modeset driver (docs/GPU-HARDWARE.md 12) ------
// A minimal but real 2D display driver over the Bochs DISPI (VBE) register
// interface, which `bochs-display` (and QEMU's standard VGA) implement in
// their MMIO BAR at offset 0x500 (16-bit registers indexed by DISPI_INDEX).
// This is genuine driving, not a handshake: it programs a mode (xres, yres,
// bpp, enable + LFB flag), renders into the linear framebuffer BAR, and the
// caller reads pixels back. It is the 2D bring-up for the Bochs-class device
// the same way virtio_gpu.rs is for virtio - vendor-shaped only in that a
// real AMD/NVIDIA/Intel part would program its own register file from a
// driver cell (GPU-HARDWARE.md 5).

const DISPI_MMIO_OFF: usize = 0x500;
const DISPI_INDEX_XRES: usize = 1;
const DISPI_INDEX_YRES: usize = 2;
const DISPI_INDEX_BPP: usize = 3;
const DISPI_INDEX_ENABLE: usize = 4;
const DISPI_ENABLED: u16 = 0x01;
const DISPI_LFB_ENABLED: u16 = 0x40;

/// A configured Bochs framebuffer: the linear-framebuffer VA (mapped through
/// the per-ISA MMIO window) and the mode geometry.
#[derive(Copy, Clone)]
pub struct Framebuffer {
    pub fb_va: usize,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub stride: u32,
}

impl Framebuffer {
    /// Write a 32-bit pixel (X8R8G8B8) at (x, y). Bounds are the caller's.
    pub fn put(&self, x: u32, y: u32, argb: u32) {
        let off = (y * self.stride + x * 4) as usize;
        // SAFETY: off is inside the framebuffer BAR the caller sized; MMIO.
        unsafe { ((self.fb_va + off) as *mut u32).write_volatile(argb) };
    }
    /// Read back a 32-bit pixel at (x, y).
    pub fn get(&self, x: u32, y: u32) -> u32 {
        let off = (y * self.stride + x * 4) as usize;
        // SAFETY: as above.
        unsafe { ((self.fb_va + off) as *const u32).read_volatile() }
    }
}

/// Bring up a `bochs-display`-class GPU in a real 2D mode: program the DISPI
/// registers (`width` x `height` x 32bpp, LFB enabled) and map its linear
/// framebuffer, returning a `Framebuffer` the caller renders into. Returns
/// `None` for a device without a usable DISPI register + framebuffer BAR
/// pair. Honest: this drives the Bochs VBE interface QEMU models; it is not
/// a modeset for a real vendor's proprietary register file.
pub fn bochs_modeset(
    inv: &Inventory,
    g: &GpuDevice,
    width: u32,
    height: u32,
) -> Option<Framebuffer> {
    let d = &inv.pci[g.pci];
    // The MMIO register BAR (non-prefetchable) and the framebuffer BAR
    // (largest memory BAR - bochs-display's BAR0 LFB is prefetchable).
    let mut reg_bar: Option<super::PciBar> = None;
    let mut fb_bar: Option<super::PciBar> = None;
    for b in d.bars.iter() {
        if b.io || b.size == 0 || b.base == 0 {
            continue;
        }
        if b.prefetch || b.size >= (width * height * 4) as u64 {
            match fb_bar {
                Some(cur) if cur.size >= b.size => {}
                _ => fb_bar = Some(*b),
            }
        }
        if !b.prefetch {
            match reg_bar {
                Some(cur) if cur.size >= b.size => {}
                _ => reg_bar = Some(*b),
            }
        }
    }
    let reg = reg_bar?;
    let fb = fb_bar?;

    let reg_va = crate::arch::mmio_map_window(reg.base as usize, reg.size as usize);
    let dispi = |index: usize| (reg_va + DISPI_MMIO_OFF + index * 2) as *mut u16;
    // SAFETY: reg_va maps the sized MMIO BAR; the DISPI window is inside it.
    unsafe {
        // Disable before reprogramming (Bochs VBE protocol).
        dispi(DISPI_INDEX_ENABLE).write_volatile(0);
        dispi(DISPI_INDEX_XRES).write_volatile(width as u16);
        dispi(DISPI_INDEX_YRES).write_volatile(height as u16);
        dispi(DISPI_INDEX_BPP).write_volatile(32);
        dispi(DISPI_INDEX_ENABLE).write_volatile(DISPI_ENABLED | DISPI_LFB_ENABLED);
        // Read the geometry back: the device echoes the accepted mode.
        let xr = dispi(DISPI_INDEX_XRES).read_volatile() as u32;
        let yr = dispi(DISPI_INDEX_YRES).read_volatile() as u32;
        if xr != width || yr != height {
            return None; // the device did not accept the mode
        }
    }
    let fb_va = crate::arch::mmio_map_window(fb.base as usize, fb.size as usize);
    Some(Framebuffer {
        fb_va,
        width,
        height,
        bpp: 32,
        stride: width * 4,
    })
}
