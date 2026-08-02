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
pub mod graph_build;
pub mod iommu;
pub mod nvme;
pub mod pci;
pub mod smmuv3;
pub mod virtio_blk;
pub mod virtio_gpu;
pub mod virtio_net;
pub mod virtio_rng;

use crate::arch;

pub const MAX_CPUS: usize = 64;

/// Memory-locality ceiling for the **distance matrix** only.
///
/// Not a limit on nodes - `nnodes` is whatever firmware reports. It bounds the SLIT matrix
/// this Inventory carries, and a machine with more localities than this keeps its nodes and
/// loses only its *distances*, degrading to "everything equally near" with the loss reported
/// (`slit_truncated`) rather than to a wrong answer. 8 covers a four-socket machine with
/// HBM and CXL localities beside DDR.
pub const MAX_DIST_NODES: usize = 8;
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

/// The id a CPU carries when nothing discovered its topology.
///
/// A sentinel rather than `0`, because `0` is a perfectly good core id and a caller reading
/// it would believe every CPU shared one core (docs/ENGINEERING.md 11: a field left constant
/// is a field that lies). Every consumer must check [`TopoSource`] or this value.
pub const TOPO_UNKNOWN: u16 = u16::MAX;

/// Where a CPU's core and cache ids came from - so a caller can tell a discovered topology
/// from an absent one, and no test can assert a grouping that was never read.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TopoSource {
    /// Nothing described the topology. Every `core_id`/`llc_id` is [`TOPO_UNKNOWN`].
    None,
    /// Decoded from the hardware id by an architectural rule: x86-64 CPUID leaves, ARM64
    /// MPIDR affinity levels. No firmware table involved.
    Architectural,
    /// Read from the device tree's `cpu-map`.
    DeviceTree,
}

/// Where a CPU's core class and capacity came from (docs/RESOURCE-GRAPH.md 2.4b).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ClassSource {
    /// Nothing described them. Every core is `Unknown` at [`graph::CAPACITY_FULL`], which is
    /// the correct description of a machine whose cores are all the same.
    None,
    /// x86-64 CPUID leaf `0x1A`, read **by each core about itself**.
    Cpuid,
    /// A device tree's `capacity-dmips-mhz`.
    DeviceTree,
    /// Set by a caller rather than discovered - [`declare_core_class`]. Kept as its own value
    /// so nothing can mistake a declared asymmetry for a measured one.
    Declared,
}

#[derive(Copy, Clone)]
pub struct CpuInfo {
    /// Hardware id (APIC id / MPIDR affinity / hart id).
    pub hw_id: u32,
    pub node: u8,
    pub online: bool,
    /// What kind of core this is, on a hybrid part.
    pub class: graph::CoreClass,
    /// **This core's own** instruction-set features, as a mask over
    /// [`arch::cpu_feature_names`]. Read by the core itself, because that is the only core
    /// that can: CPUID answers about whoever executed it (docs/RESOURCE-GRAPH.md 2.4b).
    ///
    /// 0 means this core has not reported yet - distinguishable from "no features", which no
    /// real CPU has.
    pub features: u64,
    /// Throughput relative to the fastest core **on this host**, out of
    /// [`graph::CAPACITY_FULL`]. Every core starts at full, which is what a machine with one
    /// kind of core genuinely is.
    pub capacity: u16,
    /// The physical core this CPU is a thread of. Two CPUs with the same `core_id` are SMT
    /// siblings: they share execution resources, so co-scheduling two compute-bound entities
    /// on them is slower than spreading them. [`TOPO_UNKNOWN`] when undiscovered.
    pub core_id: u16,
    /// The last-level-cache domain this CPU belongs to. Two CPUs with the same `llc_id`
    /// share a cache, which is what makes stealing work from one of them cheap.
    /// [`TOPO_UNKNOWN`] when undiscovered.
    pub llc_id: u16,
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
    /// Where `cpus[..].core_id`/`llc_id` came from, or [`TopoSource::None`].
    pub topo: TopoSource,
    /// Where `cpus[..].class`/`capacity` came from, or [`ClassSource::None`].
    pub class_src: ClassSource,
    /// Per-CPU model id, filled by each core about itself (0 = has not reported).
    pub cpu_model: [u64; MAX_CPUS],
    /// Features **every** reporting core has - the intersection. See [`Inventory::features_any`]
    /// for why this, and not the union, is what the machine may advertise.
    pub features_common: u64,
    /// Features **some** core has - the union.
    ///
    /// The difference between this and [`Inventory::features_common`] is the set of features
    /// that exist on part of the machine, and the rule for them is not a preference:
    ///
    /// > A feature present on some cores and not others must either restrict placement to those
    /// > cores, or not be advertised at all.
    ///
    /// This is what Intel did on early Alder Lake, where AVX-512 existed only on the P-cores:
    /// they disabled it **chip-wide** rather than ship a machine where a thread using it could
    /// not be migrated. `CpuReport::features` therefore carries the *intersection*, so a program
    /// that asks the machine what it can do gets an answer that stays true wherever the scheduler
    /// puts it, while the per-CPU `IsaSet` in the graph keeps each core's real set for a
    /// placement that is *pinned* (docs/RESOURCE-GRAPH.md 2.4b).
    pub features_any: u64,
    /// True once two cores have reported **different** model ids. The asymmetry a machine can
    /// have that this kernel cannot name: reporting it as asymmetry is strictly better than
    /// reporting it as uniformity.
    pub model_divergence: bool,
    pub nmem: usize,
    pub mem: [MemRegion; MAX_MEM_REGIONS],
    pub nnodes: usize,
    /// SLIT node-to-node distances, `dist[from][to]`, ACPI's relative units where 10 is
    /// "local". 0 means "not reported", which is distinguishable from any real distance
    /// because SLIT forbids a distance below 10.
    pub dist: [[u8; MAX_DIST_NODES]; MAX_DIST_NODES],
    /// True when firmware reported distances for more localities than `MAX_DIST_NODES`
    /// holds. The matrix is then **not** used, because a partly-filled distance matrix is
    /// worse than none: a caller would read a real answer for some pairs and a fabricated
    /// one for others (docs/ENGINEERING.md 11 - a field left constant is a field that lies).
    pub slit_truncated: bool,
    /// HMAT access latency in **nanoseconds**, `lat[initiator][target]`, 0 = not reported.
    /// HMAT states picoseconds; converted here so every consumer reads one unit.
    pub lat_ns: [[u32; MAX_DIST_NODES]; MAX_DIST_NODES],
    /// HMAT access bandwidth in **MB/s**, `bw[initiator][target]`, 0 = not reported.
    pub bw_mbs: [[u32; MAX_DIST_NODES]; MAX_DIST_NODES],
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
                class: graph::CoreClass::Unknown,
                capacity: graph::CAPACITY_FULL,
                features: 0,
                core_id: TOPO_UNKNOWN,
                llc_id: TOPO_UNKNOWN,
            }; MAX_CPUS],
            topo: TopoSource::None,
            class_src: ClassSource::None,
            cpu_model: [0; MAX_CPUS],
            features_common: 0,
            features_any: 0,
            model_divergence: false,
            nmem: 0,
            mem: [MemRegion {
                base: 0,
                len: 0,
                kind: MemKind::Reserved,
                node: 0,
            }; MAX_MEM_REGIONS],
            nnodes: 0,
            dist: [[0; MAX_DIST_NODES]; MAX_DIST_NODES],
            slit_truncated: false,
            lat_ns: [[0; MAX_DIST_NODES]; MAX_DIST_NODES],
            bw_mbs: [[0; MAX_DIST_NODES]; MAX_DIST_NODES],
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
                class: graph::CoreClass::Unknown,
                capacity: graph::CAPACITY_FULL,
                features: 0,
                core_id: TOPO_UNKNOWN,
                llc_id: TOPO_UNKNOWN,
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

    // CPU topology - which CPUs are threads of one core, which share a last-level cache
    // (docs/RESOURCE-GRAPH.md 2.4a). Two sources, in this order:
    //
    // 1. A device tree's `cpu-map`, already read by `fdt::parse_cpu_map` if there was one.
    //    It is preferred because it is a *statement* by firmware rather than a decoding of
    //    ids.
    // 2. Failing that, the architectural rule for taking the hardware id apart -
    //    `arch::cpu_topology_bits`. x86-64 has CPUID for it and ARM64 has MPIDR's MT bit;
    //    RISC-V has neither and says so, which is why this is the fallback and not the
    //    only path.
    //
    // A machine that offers neither keeps `TOPO_UNKNOWN` and `TopoSource::None`. That is the
    // whole reason for the sentinel: defaulting the ids to 0 would tell a scheduler that
    // every CPU is a thread of one core, which is worse than telling it nothing.
    // The boot CPU's core class, read by the boot CPU about itself. See
    // `classify_this_cpu` for why only this one can be filled here.
    classify_this_cpu(0);

    if let (TopoSource::None, Some((smt_bits, llc_bits))) = (inv.topo, arch::cpu_topology_bits()) {
        for c in inv.cpus[..inv.ncpus].iter_mut() {
            c.core_id = (c.hw_id >> smt_bits) as u16;
            c.llc_id = (c.hw_id >> llc_bits) as u16;
        }
        inv.topo = TopoSource::Architectural;
    }

    // If firmware surfaced a persistent-memory region (a real QEMU nvdimm - the
    // NFIT SPA range on x86-64), bring up the separate pmem frame allocator over
    // it so a `MemKind::Pmem` grant is genuinely nvdimm-backed (docs/MEMORY.md
    // real-PMEM path). Inert on every machine without an nvdimm, so the DDR path
    // is unchanged.
    crate::mm::frames_pmem::init_from_inventory(inv);
}

/// Record the core class of the CPU **currently executing**, at inventory index `cpu`
/// (docs/RESOURCE-GRAPH.md 2.4b).
///
/// **Only the calling core can answer**, which is why this is a function every core calls for
/// itself rather than a loop the boot CPU runs: x86-64's CPUID leaf `0x1A` reports the class of
/// whoever executed it, and ARM64's `MIDR_EL1` the part *that* core implements. So the boot CPU
/// fills index 0 from `detect`, and `smp::secondary_run` fills each secondary's index as it
/// comes up. A machine whose secondaries never start keeps `Unknown` for them - correct, since
/// nothing has asked them.
///
/// Also records the core's **model id** and raises `class_src` to `Cpuid` the moment two cores
/// disagree about it. That is the part worth keeping: a machine can be asymmetric in a way this
/// kernel cannot name - a big.LITTLE ARM part, where turning a `MIDR` into a class would need a
/// table of every part ever shipped - and "these cores are not the same" is a fact that needs no
/// table. An unnameable asymmetry reported as asymmetry is strictly better than one reported as
/// uniformity.
pub fn classify_this_cpu(cpu: usize) {
    let inv = inventory_mut();
    if cpu >= MAX_CPUS {
        return;
    }
    if let Some((class, capacity)) = arch::cpu_class_this_cpu() {
        inv.cpus[cpu].class = class;
        inv.cpus[cpu].capacity = capacity;
        inv.class_src = ClassSource::Cpuid;
    }
    // **This core's own** feature set, read by this core. `cpu_report` is a pure decode of the
    // executing core's registers (CPUID / ID_AA64* / the device-tree ISA string), so calling it
    // here answers about *this* CPU rather than about the machine.
    inv.cpus[cpu].features = arch::cpu_report(inv).features;
    recompute_feature_sets(inv);

    let model = arch::cpu_model_this_cpu();
    if cpu < MAX_CPUS {
        inv.cpu_model[cpu] = model;
    }
    // Divergence against any core that has already reported. Compared against *reported* cores
    // only (a model of 0 is "has not reported"), so a machine bringing cores up one at a time
    // does not see a false difference against a slot nobody has filled.
    if model != 0 {
        for other in 0..inv.ncpus.min(MAX_CPUS) {
            if other != cpu && inv.cpu_model[other] != 0 && inv.cpu_model[other] != model {
                inv.model_divergence = true;
            }
        }
    }
}

/// Recompute the machine-wide feature intersection and union from the cores that have reported.
///
/// **The intersection is what the machine advertises** (`inv.cpu.features`), and that is the rule
/// rather than a conservative choice: a thread can be migrated to any core, so a feature only some
/// cores have is a promise the machine cannot keep. Early Alder Lake is the precedent - AVX-512 on
/// the P-cores only, disabled chip-wide rather than shipped as a migration hazard.
///
/// A core that has not reported (`features == 0`) is skipped, so a machine bringing cores up one at
/// a time does not intersect against an empty set and advertise nothing.
fn recompute_feature_sets(inv: &mut Inventory) {
    let mut common = 0u64;
    let mut any = 0u64;
    let mut first = true;
    for c in &inv.cpus[..inv.ncpus.min(MAX_CPUS)] {
        if c.features == 0 {
            continue;
        }
        any |= c.features;
        if first {
            common = c.features;
            first = false;
        } else {
            common &= c.features;
        }
    }
    if first {
        return; // nothing has reported yet - leave what discovery put there
    }
    inv.features_common = common;
    inv.features_any = any;
    inv.cpu.features = common;
}

/// Declare that a CPU has exactly `features`, rather than discovering it.
///
/// The feature twin of [`declare_core_class`], and it exists for the same measured reason: no
/// emulator here models a machine whose cores differ, so the rule above - *a feature some cores
/// lack must restrict placement or not be advertised* - could not be exercised at all. QEMU builds
/// every CPU of a machine from one model, checked in its source.
///
/// It recomputes the intersection, so the advertised set drops the divergent feature immediately,
/// which is the behaviour under test.
pub fn declare_cpu_features(cpu: usize, features: u64) {
    let inv = inventory_mut();
    if cpu >= MAX_CPUS {
        return;
    }
    inv.cpus[cpu].features = features;
    recompute_feature_sets(inv);
    graph_build::refresh_cpu_classes(inv);
}

/// Declare a CPU's core class and capacity rather than discovering them.
///
/// **This exists because no emulator this tree runs on models a hybrid part** - QEMU 11
/// implements neither x86-64's hybrid flag nor CPUID leaf `0x1A`, and never emits
/// `capacity-dmips-mhz` (both checked in its source). A capacity-aware scheduler whose
/// asymmetry can never be exercised is a scheduler nobody has run, so the asymmetry is
/// *declared* and every decision that reads it is then a decision under test.
///
/// It sets `class_src` to [`ClassSource::Declared`], which is the whole discipline: a declared
/// asymmetry can never be mistaken for a measured one, by a test or by a reader, and any
/// consumer that wants to distinguish them can. The precedent is the deliberately synthetic
/// asymmetry docs/RESOURCE-GRAPH.md 6.4d specifies for the no-FPU case.
pub fn declare_core_class(cpu: usize, class: graph::CoreClass, capacity: u16) {
    let inv = inventory_mut();
    if cpu >= MAX_CPUS {
        return;
    }
    inv.cpus[cpu].class = class;
    inv.cpus[cpu].capacity = capacity.min(graph::CAPACITY_FULL);
    inv.class_src = ClassSource::Declared;
    // One source, two readers: the graph learns it in the same call, so nothing can read a
    // capacity from the graph that disagrees with the inventory.
    graph_build::refresh_cpu_classes(inv);
    crate::sched::hetero::load_from_inventory(inv);
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
    // CPU topology, printed as it was discovered - `?` for a CPU whose grouping nothing
    // described, so a reader can tell "no topology" from "one core" at a glance.
    crate::print!("hw: cpu topology ({:?}):", inv.topo);
    for c in &inv.cpus[..inv.ncpus] {
        if c.core_id == TOPO_UNKNOWN {
            crate::print!(" id{}=?", c.hw_id);
        } else {
            crate::print!(" id{}=core{}/llc{}", c.hw_id, c.core_id, c.llc_id);
        }
    }
    crate::println!();
    // Core classes and capacities. `-` for a machine whose cores are all the same, which is what
    // every profile here is: no emulator models a hybrid part (docs/RESOURCE-GRAPH.md 2.4b).
    crate::print!(
        "hw: cpu classes ({:?}, divergent models {}, thread director {}):",
        inv.class_src,
        inv.model_divergence,
        arch::thread_director_present()
    );
    for c in &inv.cpus[..inv.ncpus] {
        crate::print!(" id{}={:?}/{}", c.hw_id, c.class, c.capacity);
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
