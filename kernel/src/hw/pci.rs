//! PCIe enumeration and engine classification (docs/ACCELERATORS.md 1).
//! Walks configuration space and records every function found, mapping its
//! PCI class code to an EngineKind - which is how a GPU, NIC, NVMe drive,
//! or a processing accelerator (NPU/TPU, PCI base class 0x12) is
//! recognised. Config access is per-ISA (x86 uses the CF8/CFC I/O ports;
//! ARM64/RISC-V use the memory-mapped ECAM window), behind
//! `arch::pci_cfg_read32`.

use super::{EngineKind, Inventory, PciDevice};
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
    // QEMU's default machines expose a single root bus; a full driver would
    // recurse through bridges (bus-range from firmware). One bus covers the
    // virtio/VGA/accelerator devices these tests care about.
    for dev in 0u8..32 {
        enumerate_function(ecam_base, 0, dev, 0, inv);
        // Multi-function device? Scan the other functions.
        let header = arch::pci_cfg_read32(ecam_base, 0, dev, 0, 0x0C);
        let multifunction = (header >> 16) & 0x80 != 0;
        if multifunction {
            for func in 1u8..8 {
                enumerate_function(ecam_base, 0, dev, func, inv);
            }
        }
    }
}

fn enumerate_function(ecam_base: u64, bus: u8, dev: u8, func: u8, inv: &mut Inventory) {
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

    inv.add_pci(PciDevice {
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
    });
}
