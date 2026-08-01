//! Hardware discovery and the machine inventory (docs/TARGET-
//! ARCHITECTURES.md, docs/ACCELERATORS.md 1). At boot the kernel builds a
//! single `Inventory` describing what it is running on: CPUs and their
//! instruction-set features, the physical memory map with typed regions,
//! NUMA topology, and the PCIe device tree classified into engine kinds.
//!
//! Firmware is discovered from whatever the platform provides - ACPI on
//! x86-64 (RSDP handed over by the PVH boot info), a flattened device tree
//! on ARM64 and RISC-V - and normalised into the same portable inventory.
//! The parsers (fdt, acpi, pci) are portable byte-walkers; only the raw
//! register reads (CPUID / ID_AA64* / the boot pointer) live in `arch`.
//!
//! What is real in QEMU: CPU features, the memory map, CPU count, NUMA
//! nodes (with `-numa`), and PCIe enumeration of the virtio/VGA devices
//! present. What a real machine adds - more memory tiers, accelerators on
//! the PCIe bus - flows through the same tables; a GPU/NPU/TPU shows up as
//! a PCIe device classified by its class code (see EngineKind).

pub mod acpi;
pub mod block;
pub mod fdt;
pub mod gpu;
pub mod graph;
pub mod iommu;
pub mod nvme;
pub mod pci;
pub mod smmuv3;
pub mod virtio_blk;
pub mod virtio_gpu;
pub mod virtio_net;

use crate::arch;

pub const MAX_CPUS: usize = 64;
pub const MAX_MEM_REGIONS: usize = 24;
pub const MAX_NUMA_NODES: usize = 8;
pub const MAX_PCI_DEVICES: usize = 48;
pub const MAX_GPUS: usize = 8;

/// Typed physical memory (docs/MEMORY.md). QEMU mostly reports DDR; the
/// other tiers appear on hardware that has them, classified from the
/// firmware memory map.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemKind {
    Ram,
    Reserved,
    AcpiReclaim,
    AcpiNvs,
    Pmem,
}

#[derive(Copy, Clone)]
pub struct MemRegion {
    pub base: u64,
    pub len: u64,
    pub kind: MemKind,
    pub node: u8,
}

#[derive(Copy, Clone)]
pub struct CpuInfo {
    /// Hardware id (APIC id / MPIDR affinity / hart id).
    pub hw_id: u32,
    pub node: u8,
    pub online: bool,
}

/// The engine class a PCIe device maps to (docs/ACCELERATORS.md 1).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum EngineKind {
    Display,
    Gpu,
    Nic,
    Storage,
    Nvme,
    Accelerator,
    Bridge,
    Other,
}

/// One sized Base Address Register (docs/GPU-HARDWARE.md 3). `base` is what
/// the register held at enumeration (0 on PVH boots, where no firmware
/// programs BARs) or what `pci::assign_bars` later wrote; `size` comes from
/// the write-ones mask probe. `size == 0` means the BAR is unimplemented.
#[derive(Copy, Clone)]
pub struct PciBar {
    pub base: u64,
    pub size: u64,
    pub io: bool,
    pub is64: bool,
    pub prefetch: bool,
}

impl PciBar {
    pub const EMPTY: PciBar = PciBar {
        base: 0,
        size: 0,
        io: false,
        is64: false,
        prefetch: false,
    };
}

#[derive(Copy, Clone)]
pub struct PciDevice {
    pub seg: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub engine: EngineKind,
    /// Header type (0 = endpoint, 1 = PCI-PCI bridge), multifunction bit
    /// stripped.
    pub header: u8,
    /// The six type-0 BARs, sized by the mask probe (bridges: first two).
    pub bars: [PciBar; 6],
    /// Capabilities found on the standard capability list.
    pub msi: bool,
    pub msix: bool,
    /// Config-space offset of the MSI-X capability (0 when absent). A driver
    /// wiring an interrupt needs the capability itself, not just the fact that
    /// there is one: the table's BAR and offset live in its next two dwords.
    pub msix_cap: u16,
    /// PCIe capability present; `flr` is its DevCap FLR bit.
    pub pcie: bool,
    pub flr: bool,
}

/// Where the machine description came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Firmware {
    Acpi,
    DeviceTree,
    /// ARM64 PSCI: the CPU list came from `AFFINITY_INFO`, the only enumeration
    /// source a bare-ELF `virt` boot has (QEMU passes no device tree there).
    /// Distinct from `Builtin` on purpose - it is the difference between having
    /// asked the platform and having assumed a profile.
    Psci,
    /// A built-in platform profile (used where the emulator hands a bare
    /// ELF no firmware table; the layout is the fixed QEMU machine model).
    Builtin,
    None,
}

/// CPU instruction-set report. `features` is a bitmask over
/// `arch::cpu_feature_names()`; `vendor` is ASCII, NUL-padded.
#[derive(Copy, Clone)]
pub struct CpuReport {
    pub vendor: [u8; 16],
    pub features: u64,
}

impl CpuReport {
    pub const EMPTY: CpuReport = CpuReport {
        vendor: [0; 16],
        features: 0,
    };
}

/// The whole machine, as discovered at boot.
pub struct Inventory {
    pub firmware: Firmware,
    pub cpu: CpuReport,
    pub ncpus: usize,
    pub cpus: [CpuInfo; MAX_CPUS],
    pub nmem: usize,
    pub mem: [MemRegion; MAX_MEM_REGIONS],
    pub nnodes: usize,
    pub ecam_base: u64,
    /// VT-d remapping-hardware register base (first DRHD in the ACPI DMAR
    /// table), 0 if no IOMMU was discovered (docs/GPU-HARDWARE.md 4).
    pub iommu_base: u64,
    pub npci: usize,
    pub pci: [PciDevice; MAX_PCI_DEVICES],
    pub ngpu: usize,
    pub gpus: [gpu::GpuDevice; MAX_GPUS],
}

impl Inventory {
    const fn new() -> Inventory {
        Inventory {
            firmware: Firmware::None,
            cpu: CpuReport::EMPTY,
            ncpus: 0,
            cpus: [CpuInfo {
                hw_id: 0,
                node: 0,
                online: false,
            }; MAX_CPUS],
            nmem: 0,
            mem: [MemRegion {
                base: 0,
                len: 0,
                kind: MemKind::Reserved,
                node: 0,
            }; MAX_MEM_REGIONS],
            nnodes: 0,
            ecam_base: 0,
            iommu_base: 0,
            npci: 0,
            pci: [PciDevice {
                seg: 0,
                bus: 0,
                dev: 0,
                func: 0,
                vendor: 0,
                device: 0,
                class: 0,
                subclass: 0,
                prog_if: 0,
                engine: EngineKind::Other,
                header: 0,
                bars: [PciBar::EMPTY; 6],
                msi: false,
                msix: false,
                msix_cap: 0,
                pcie: false,
                flr: false,
            }; MAX_PCI_DEVICES],
            ngpu: 0,
            gpus: [gpu::GpuDevice::EMPTY; MAX_GPUS],
        }
    }

    pub(crate) fn add_cpu(&mut self, hw_id: u32, node: u8) {
        if self.ncpus < MAX_CPUS {
            self.cpus[self.ncpus] = CpuInfo {
                hw_id,
                node,
                online: false,
            };
            self.ncpus += 1;
            if (node as usize) + 1 > self.nnodes {
                self.nnodes = node as usize + 1;
            }
        }
    }

    pub(crate) fn add_mem(&mut self, base: u64, len: u64, kind: MemKind, node: u8) {
        if self.nmem < MAX_MEM_REGIONS && len > 0 {
            self.mem[self.nmem] = MemRegion {
                base,
                len,
                kind,
                node,
            };
            self.nmem += 1;
            if (node as usize) + 1 > self.nnodes {
                self.nnodes = node as usize + 1;
            }
        }
    }

    pub(crate) fn add_pci(&mut self, d: PciDevice) {
        if self.npci < MAX_PCI_DEVICES {
            self.pci[self.npci] = d;
            self.npci += 1;
        }
    }

    /// Total usable RAM in bytes across all RAM regions.
    pub fn ram_bytes(&self) -> u64 {
        self.mem[..self.nmem]
            .iter()
            .filter(|r| r.kind == MemKind::Ram)
            .map(|r| r.len)
            .sum()
    }

    /// RAM bytes on a given NUMA node.
    pub fn node_ram_bytes(&self, node: u8) -> u64 {
        self.mem[..self.nmem]
            .iter()
            .filter(|r| r.kind == MemKind::Ram && r.node == node)
            .map(|r| r.len)
            .sum()
    }

    /// The NUMA node the CPU with hardware id `hw_id` sits on, or `None` if this
    /// machine reported no such CPU (docs/SUBSTRATE.md pillar 6).
    ///
    /// By **hardware id** and not by registry index, because a core knows its own
    /// hardware id from a register before it knows anything else - that is how the
    /// secondaries identify themselves - while a registry index is an artefact of
    /// the order bring-up happened to claim them in.
    pub fn cpu_node(&self, hw_id: u32) -> Option<u8> {
        self.cpus[..self.ncpus]
            .iter()
            .find(|c| c.hw_id == hw_id)
            .map(|c| c.node)
    }
}

static mut INVENTORY: Inventory = Inventory::new();

/// The machine inventory (valid after `detect`).
pub fn inventory() -> &'static Inventory {
    // SAFETY: built once during single-threaded init, read-only after.
    unsafe { &*core::ptr::addr_of!(INVENTORY) }
}

fn inventory_mut() -> &'static mut Inventory {
    unsafe { &mut *core::ptr::addr_of_mut!(INVENTORY) }
}

/// Discover the machine: firmware tables, CPU features, PCIe. Called once
/// early in kernel init, before the frame allocator (which uses the
/// detected memory map).
pub fn detect() {
    let inv = inventory_mut();

    // Each ISA fills firmware/CPUs/memory/ECAM from whatever it has: ACPI
    // (x86), a device tree (RISC-V), or a built-in platform profile (ARM64
    // on QEMU, which hands a bare ELF no firmware table).
    arch::discover(inv);

    // CPU vendor/features from the ISA's own registers (or, on RISC-V,
    // from the device-tree ISA string filled in by discovery).
    inv.cpu = arch::cpu_report(inv);

    if inv.ecam_base != 0 {
        pci::enumerate(inv.ecam_base, inv);
        // Classify every display-class function into the GPU inventory
        // (vendor recognition + BAR topology; docs/GPU-HARDWARE.md).
        gpu::probe(inv);
    }

    // Fallback: if firmware gave us no CPU, we are at least running on one.
    if inv.ncpus == 0 {
        inv.add_cpu(0, 0);
    }
    if inv.nnodes == 0 {
        inv.nnodes = 1;
    }
    // The boot CPU is online by definition.
    inv.cpus[0].online = true;

    // If firmware surfaced a persistent-memory region (a real QEMU nvdimm - the
    // NFIT SPA range on x86-64), bring up the separate pmem frame allocator over
    // it so a `MemKind::Pmem` grant is genuinely nvdimm-backed (docs/MEMORY.md
    // real-PMEM path). Inert on every machine without an nvdimm, so the DDR path
    // is unchanged.
    crate::mm::frames_pmem::init_from_inventory(inv);
}

/// Opt-in BAR assignment (docs/GPU-HARDWARE.md 3): write addresses from
/// the host bridge's MMIO window into every sized, unassigned memory BAR
/// and enable decode. Called by the kernels that need reachable BARs (the
/// `enable_uart_rx_irq` precedent); everything else leaves devices exactly
/// as firmware (or nobody, on PVH) left them. Returns BARs assigned.
pub fn assign_pci_bars() -> usize {
    pci::assign_bars(inventory_mut())
}

/// Opt-in GPU attach measurement (docs/GPU-HARDWARE.md 9): stream writes
/// through each recognised GPU's framebuffer aperture and record ticks
/// per KiB in its inventory record. Call after BARs decode
/// (`assign_pci_bars`); `SYS_ENGINE_INFO` reports the result live.
pub fn gpu_attach_measure() {
    gpu::attach_measure(inventory_mut())
}

/// Print the discovered inventory to the console.
pub fn print_summary() {
    let inv = inventory();
    crate::println!(
        "hw: firmware={:?} cpus={} nodes={} ram={} MiB pci={}",
        inv.firmware,
        inv.ncpus,
        inv.nnodes,
        inv.ram_bytes() / (1024 * 1024),
        inv.npci
    );
    let names = arch::cpu_feature_names();
    crate::print!("hw: cpu features:");
    for (i, name) in names.iter().enumerate() {
        if inv.cpu.features & (1 << i) != 0 {
            crate::print!(" {name}");
        }
    }
    crate::println!();
    for r in &inv.mem[..inv.nmem] {
        crate::println!(
            "hw: mem [{:#012x}..{:#012x}] {:?} node {}",
            r.base,
            r.base + r.len,
            r.kind,
            r.node
        );
    }
    for d in &inv.pci[..inv.npci] {
        crate::println!(
            "hw: pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}:{:02x} -> {:?}",
            d.bus,
            d.dev,
            d.func,
            d.vendor,
            d.device,
            d.class,
            d.subclass,
            d.engine
        );
    }
}
