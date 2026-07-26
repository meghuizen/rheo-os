//! PCIe enumeration and engine classification (docs/ACCELERATORS.md 1,
//! docs/GPU-HARDWARE.md 3). Walks configuration space and records every
//! function found, mapping its PCI class code to an EngineKind - which is
//! how a GPU, NIC, NVMe drive, or a processing accelerator (NPU/TPU, PCI
//! base class 0x12) is recognised. Config access is per-ISA (x86 uses the
//! CF8/CFC I/O ports; ARM64/RISC-V use the memory-mapped ECAM window),
//! behind `arch::pci_cfg_read32`/`arch::pci_cfg_write32`.
//!
//! Beyond bus 0, enumeration recurses through PCI-PCI bridges (root ports,
//! switches). PVH boots have no firmware to program bridge bus numbers, so
//! an unconfigured bridge (secondary == 0) is assigned the next free bus
//! number here - real bridge programming, which is what makes a device
//! behind a `pcie-root-port` reachable at all.
//!
//! Every function's BARs are sized by the standard write-ones mask probe
//! (original value restored), and the standard capability list is walked
//! for MSI / MSI-X / the PCIe capability (whose DevCap carries FLR). BAR
//! *assignment* - writing addresses from the host bridge's MMIO window and
//! enabling decode - is NOT done at boot; it is the opt-in
//! `assign_bars`, called by the kernels that need mapped BARs (the
//! `enable_uart_rx_irq` precedent), so the config-tunnel virtio drivers
//! and every existing test see unchanged devices.

use super::{EngineKind, Inventory, PciBar, PciDevice};
use crate::arch;

fn classify(class: u8, subclass: u8) -> EngineKind {
    match class {
        0x01 => {
            if subclass == 0x08 {
                EngineKind::Nvme
            } else {
                EngineKind::Storage
            }
        }
        0x02 => EngineKind::Nic,
        0x03 => {
            if subclass == 0x02 {
                EngineKind::Gpu // 3D controller
            } else {
                EngineKind::Display
            }
        }
        0x06 => EngineKind::Bridge,
        0x12 => EngineKind::Accelerator, // processing accelerators (NPU/TPU)
        _ => EngineKind::Other,
    }
}

pub fn enumerate(ecam_base: u64, inv: &mut Inventory) {
    // Bus numbers below `next_bus` are allocated; bridges found with no
    // firmware-programmed secondary bus get the next free one.
    let mut next_bus: u8 = 1;
    scan_bus(ecam_base, 0, inv, &mut next_bus, 0);
}

fn scan_bus(ecam_base: u64, bus: u8, inv: &mut Inventory, next_bus: &mut u8, depth: u8) {
    if depth > 8 {
        return; // malformed topology guard
    }
    for dev in 0u8..32 {
        let id = arch::pci_cfg_read32(ecam_base, bus, dev, 0, 0x00);
        if (id & 0xFFFF) as u16 == 0xFFFF || (id & 0xFFFF) == 0 {
            continue;
        }
        let header = arch::pci_cfg_read32(ecam_base, bus, dev, 0, 0x0C);
        let multifunction = (header >> 16) & 0x80 != 0;
        let nfunc = if multifunction { 8 } else { 1 };
        for func in 0..nfunc {
            enumerate_function(ecam_base, bus, dev, func, inv, next_bus, depth);
        }
    }
}

fn enumerate_function(
    ecam_base: u64,
    bus: u8,
    dev: u8,
    func: u8,
    inv: &mut Inventory,
    next_bus: &mut u8,
    depth: u8,
) {
    let id = arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x00);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF || vendor == 0 {
        return; // no device here
    }
    let device = (id >> 16) as u16;
    let class_reg = arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x08);
    let prog_if = ((class_reg >> 8) & 0xFF) as u8;
    let subclass = ((class_reg >> 16) & 0xFF) as u8;
    let class = ((class_reg >> 24) & 0xFF) as u8;
    let header = ((arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x0C) >> 16) & 0x7F) as u8;

    let mut d = PciDevice {
        seg: 0,
        bus,
        dev,
        func,
        vendor,
        device,
        class,
        subclass,
        prog_if,
        engine: classify(class, subclass),
        header,
        bars: [PciBar::EMPTY; 6],
        msi: false,
        msix: false,
        pcie: false,
        flr: false,
    };

    // Size the BARs (type-0 headers have 6; bridges have 2) and walk the
    // capability list. Sizing happens before any driver touches the device
    // and restores the original register value, so it is invisible.
    let nbars = if header == 1 { 2 } else { 6 };
    size_bars(ecam_base, bus, dev, func, nbars, &mut d.bars);
    walk_caps(ecam_base, bus, dev, func, &mut d);

    inv.add_pci(d);

    // A PCI-PCI bridge (root port, switch port) leads to another bus. With
    // firmware-programmed bus numbers, follow them; on a PVH boot they are
    // zero, so program the next free bus number ourselves (primary = this
    // bus, subordinate provisionally 0xFF, clamped after the walk).
    if header == 1 && class == 0x06 {
        let busreg = arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x18);
        let mut secondary = ((busreg >> 8) & 0xFF) as u8;
        if secondary == 0 {
            if *next_bus == 0xFF {
                return; // out of bus numbers
            }
            secondary = *next_bus;
            *next_bus += 1;
            let prog =
                (busreg & 0xFF00_0000) | 0x00FF_0000 | ((secondary as u32) << 8) | (bus as u32);
            arch::pci_cfg_write32(ecam_base, bus, dev, func, 0x18, prog);
            scan_bus(ecam_base, secondary, inv, next_bus, depth + 1);
            // Clamp the subordinate bus to the highest bus actually behind
            // this bridge, so sibling bridges can be programmed after it.
            let sub = *next_bus - 1;
            let clamped = (busreg & 0xFF00_0000)
                | ((sub as u32) << 16)
                | ((secondary as u32) << 8)
                | (bus as u32);
            arch::pci_cfg_write32(ecam_base, bus, dev, func, 0x18, clamped);
        } else if secondary > bus {
            if secondary >= *next_bus {
                *next_bus = secondary + 1;
            }
            scan_bus(ecam_base, secondary, inv, next_bus, depth + 1);
        }
    }
}

/// Size each BAR with the standard write-ones probe: save the original,
/// write all-ones, read back the mask, restore. The size is the two's
/// complement of the writable mask. 64-bit memory BARs consume two slots.
fn size_bars(ecam_base: u64, bus: u8, dev: u8, func: u8, nbars: usize, bars: &mut [PciBar; 6]) {
    let mut i = 0usize;
    while i < nbars {
        let off = 0x10 + (i as u16) * 4;
        let orig = arch::pci_cfg_read32(ecam_base, bus, dev, func, off);
        arch::pci_cfg_write32(ecam_base, bus, dev, func, off, 0xFFFF_FFFF);
        let mask = arch::pci_cfg_read32(ecam_base, bus, dev, func, off);
        arch::pci_cfg_write32(ecam_base, bus, dev, func, off, orig);

        if mask == 0 {
            i += 1;
            continue; // unimplemented BAR
        }

        if orig & 1 != 0 {
            // I/O space BAR (x86 legacy); low two bits are flags.
            let size = (!(mask & 0xFFFF_FFFC)).wrapping_add(1) & 0xFFFF;
            bars[i] = PciBar {
                base: (orig & 0xFFFF_FFFC) as u64,
                size: size as u64,
                io: true,
                is64: false,
                prefetch: false,
            };
            i += 1;
            continue;
        }

        let is64 = (orig >> 1) & 0x3 == 0x2;
        let prefetch = orig & 0x8 != 0;
        if is64 && i + 1 < 6 {
            let off_hi = off + 4;
            let orig_hi = arch::pci_cfg_read32(ecam_base, bus, dev, func, off_hi);
            arch::pci_cfg_write32(ecam_base, bus, dev, func, off_hi, 0xFFFF_FFFF);
            let mask_hi = arch::pci_cfg_read32(ecam_base, bus, dev, func, off_hi);
            arch::pci_cfg_write32(ecam_base, bus, dev, func, off_hi, orig_hi);

            let full_mask = ((mask_hi as u64) << 32) | (mask as u64 & 0xFFFF_FFF0);
            let size = (!full_mask).wrapping_add(1);
            bars[i] = PciBar {
                base: ((orig_hi as u64) << 32) | (orig as u64 & 0xFFFF_FFF0),
                size,
                io: false,
                is64: true,
                prefetch,
            };
            i += 2;
        } else {
            let size = ((!(mask & 0xFFFF_FFF0)) as u64).wrapping_add(1) & 0xFFFF_FFFF;
            bars[i] = PciBar {
                base: (orig & 0xFFFF_FFF0) as u64,
                size,
                io: false,
                is64: false,
                prefetch,
            };
            i += 1;
        }
    }
}

/// Walk the standard capability list (status bit 4 gates it) recording
/// MSI (0x05), MSI-X (0x11), and the PCIe capability (0x10), whose DevCap
/// dword carries the Function-Level-Reset bit.
fn walk_caps(ecam_base: u64, bus: u8, dev: u8, func: u8, d: &mut PciDevice) {
    let status = arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x04) >> 16;
    if status & 0x10 == 0 {
        return;
    }
    let mut ptr = (arch::pci_cfg_read32(ecam_base, bus, dev, func, 0x34) & 0xFC) as u16;
    let mut hops = 0;
    while ptr != 0 && hops < 48 {
        let cap = arch::pci_cfg_read32(ecam_base, bus, dev, func, ptr);
        let cap_id = (cap & 0xFF) as u8;
        match cap_id {
            0x05 => d.msi = true,
            0x11 => d.msix = true,
            0x10 => {
                d.pcie = true;
                let devcap = arch::pci_cfg_read32(ecam_base, bus, dev, func, ptr + 4);
                d.flr = devcap & (1 << 28) != 0;
            }
            _ => {}
        }
        ptr = ((cap >> 8) & 0xFC) as u16;
        hops += 1;
    }
}

/// Assign addresses to every sized, unassigned memory BAR from the host
/// bridge's MMIO window (`arch::pci_mmio_window`), naturally aligned, then
/// enable memory decode + bus mastering on the function. Opt-in: called by
/// kernels that need reachable BARs (docs/GPU-HARDWARE.md 3); boots that do
/// not call it leave every device exactly as firmware (or nobody, on PVH)
/// left it. Returns the number of BARs assigned.
pub fn assign_bars(inv: &mut Inventory) -> usize {
    let (window_base, window_len) = arch::pci_mmio_window();
    let ecam = inv.ecam_base;
    let mut cursor = window_base;
    let end = window_base + window_len;
    let mut assigned = 0usize;

    for d in inv.pci[..inv.npci].iter_mut() {
        if d.header != 0 {
            continue; // endpoints only; bridge windows are future work
        }
        let mut touched = false;
        let mut i = 0usize;
        while i < 6 {
            let bar = d.bars[i];
            let step = if bar.is64 { 2 } else { 1 };
            if bar.io || bar.size == 0 || bar.base != 0 {
                i += step;
                continue;
            }
            // Natural alignment: a BAR decodes on a multiple of its size.
            let align = bar.size.max(0x1000);
            let base = (cursor + align - 1) & !(align - 1);
            if base + bar.size > end {
                i += step;
                continue; // window exhausted; leave unassigned, honestly
            }
            cursor = base + bar.size;
            let off = 0x10 + (i as u16) * 4;
            arch::pci_cfg_write32(ecam, d.bus, d.dev, d.func, off, (base & 0xFFFF_FFF0) as u32);
            if bar.is64 {
                arch::pci_cfg_write32(ecam, d.bus, d.dev, d.func, off + 4, (base >> 32) as u32);
            }
            d.bars[i].base = base;
            assigned += 1;
            touched = true;
            i += step;
        }
        if touched {
            // Memory decode + bus master on, I/O decode untouched.
            let cmd = arch::pci_cfg_read32(ecam, d.bus, d.dev, d.func, 0x04);
            arch::pci_cfg_write32(ecam, d.bus, d.dev, d.func, 0x04, cmd | 0x6);
        }
    }
    assigned
}
