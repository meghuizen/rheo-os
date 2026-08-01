//! ACPI + PVH discovery (x86-64). The memory map comes straight from the
//! PVH `hvm_start_info` the bootloader handed us; the ACPI RSDP address in
//! that same struct leads to the XSDT and from there to the MADT (CPU
//! list), MCFG (PCIe ECAM base), SRAT (NUMA affinities), and SLIT (the
//! node-to-node distance matrix), and HMAT (real latency and bandwidth per
//! initiator/target pair - docs/RESOURCE-GRAPH.md 2.4).
//!
//! Physical addresses are read through the kernel's high linear map
//! (`arch::phys_to_virt`; docs/MEMORY.md) - the MMU is on before discovery
//! runs, and the kernel no longer identity-maps RAM low - so ACPI tables placed
//! by QEMU in low RAM are reachable at their high alias.

use super::{Inventory, MemKind};
use crate::arch;

fn rd8(pa: u64) -> u8 {
    unsafe { (arch::phys_to_virt(pa as usize) as *const u8).read() }
}
fn rd16(pa: u64) -> u16 {
    unsafe { (arch::phys_to_virt(pa as usize) as *const u16).read_unaligned() }
}
fn rd32(pa: u64) -> u32 {
    unsafe { (arch::phys_to_virt(pa as usize) as *const u32).read_unaligned() }
}
fn rd64(pa: u64) -> u64 {
    unsafe { (arch::phys_to_virt(pa as usize) as *const u64).read_unaligned() }
}

fn sig4(pa: u64) -> [u8; 4] {
    [rd8(pa), rd8(pa + 1), rd8(pa + 2), rd8(pa + 3)]
}

pub fn parse(start_info: usize, inv: &mut Inventory) {
    if start_info == 0 {
        return;
    }
    let si = start_info as u64;
    // hvm_start_info: magic@0, version@4, ..., rsdp_paddr@32, memmap_paddr@40,
    // memmap_entries@48.
    let version = rd32(si + 4);
    let rsdp = rd64(si + 32);

    if version >= 1 {
        let memmap = rd64(si + 40);
        let entries = rd32(si + 48);
        parse_pvh_memmap(memmap, entries, inv);
    }
    if inv.nmem == 0 {
        // No memmap: assume the low RAM the boot stub mapped.
        inv.add_mem(0x10_0000, 512 * 1024 * 1024, MemKind::Ram, 0);
    }

    if rsdp != 0 {
        parse_rsdp(rsdp, inv);
    }
    if inv.ncpus == 0 {
        inv.add_cpu(0, 0);
    }
}

/// PVH memory map -> typed regions (E820-style type codes).
fn parse_pvh_memmap(memmap: u64, entries: u32, inv: &mut Inventory) {
    for i in 0..entries as u64 {
        let e = memmap + i * 24;
        let addr = rd64(e);
        let size = rd64(e + 8);
        let ty = rd32(e + 16);
        let kind = match ty {
            1 => MemKind::Ram,
            3 => MemKind::AcpiReclaim,
            4 => MemKind::AcpiNvs,
            7 => MemKind::Pmem,
            _ => MemKind::Reserved,
        };
        inv.add_mem(addr, size, kind, 0);
    }
}

fn parse_rsdp(rsdp: u64, inv: &mut Inventory) {
    // "RSD PTR " signature check.
    if &sig4(rsdp) != b"RSD " {
        return;
    }
    let revision = rd8(rsdp + 15);
    let (sdt, is_xsdt) = if revision >= 2 {
        let xsdt = rd64(rsdp + 24);
        if xsdt != 0 {
            (xsdt, true)
        } else {
            (rd32(rsdp + 16) as u64, false)
        }
    } else {
        (rd32(rsdp + 16) as u64, false)
    };
    walk_sdt(sdt, is_xsdt, inv);
}

/// Walk the (X)SDT and dispatch tables we care about.
fn walk_sdt(sdt: u64, is_xsdt: bool, inv: &mut Inventory) {
    let len = rd32(sdt + 4) as u64;
    let entry_size = if is_xsdt { 8 } else { 4 };
    let count = (len.saturating_sub(36)) / entry_size;
    for i in 0..count {
        let ptr_off = sdt + 36 + i * entry_size;
        let table = if is_xsdt {
            rd64(ptr_off)
        } else {
            rd32(ptr_off) as u64
        };
        if table == 0 {
            continue;
        }
        match &sig4(table) {
            b"APIC" => parse_madt(table, inv),
            b"MCFG" => parse_mcfg(table, inv),
            b"SRAT" => parse_srat(table, inv),
            b"SLIT" => parse_slit(table, inv),
            b"HMAT" => parse_hmat(table, inv),
            b"NFIT" => parse_nfit(table, inv),
            b"DMAR" => parse_dmar(table, inv),
            _ => {}
        }
    }
}

/// The ACPI GUID for "byte-addressable persistent memory" SPA ranges
/// (NFIT SPA Range Structure), stored in the mixed-endian GUID layout QEMU
/// writes for an nvdimm: {66F0D379-B4F3-4074-AC43-0D3318B78CDB}.
const NFIT_PM_GUID: [u8; 16] = [
    0x79, 0xD3, 0xF0, 0x66, 0xF3, 0xB4, 0x74, 0x40, 0xAC, 0x43, 0x0D, 0x33, 0x18, 0xB7, 0x8C, 0xDB,
];

/// NFIT -> persistent-memory regions (docs/MEMORY.md real-PMEM path). A real
/// QEMU nvdimm's physical span is reported **only** here (the SPA Range
/// Structure), not in the PVH E820 memmap, so this is the discovery path that
/// turns a `MemKind::Pmem` grant into genuinely nvdimm-backed frames rather than
/// DDR. Each SPA Range Structure (type 0) whose address-range-type GUID is the
/// persistent-memory GUID contributes its `[base, base+len)` as a `Pmem` region.
fn parse_nfit(nfit: u64, inv: &mut Inventory) {
    let len = rd32(nfit + 4) as u64;
    // Header(36) + reserved(4); sub-structures follow.
    let mut off = 40;
    while off + 4 <= len {
        let stype = rd8(nfit + off) as u16 | ((rd8(nfit + off + 1) as u16) << 8);
        let slen = (rd8(nfit + off + 2) as u16 | ((rd8(nfit + off + 3) as u16) << 8)) as u64;
        if slen == 0 {
            break;
        }
        // Type 0 = System Physical Address Range Structure: GUID@16, base@32,
        // length@40 (ACPI 6.x table 5-132).
        if stype == 0 && slen >= 48 {
            let mut guid = [0u8; 16];
            for (i, b) in guid.iter_mut().enumerate() {
                *b = rd8(nfit + off + 16 + i as u64);
            }
            if guid == NFIT_PM_GUID {
                let base = rd64(nfit + off + 32);
                let size = rd64(nfit + off + 40);
                inv.add_mem(base, size, MemKind::Pmem, 0);
            }
        }
        off += slen;
    }
}

/// DMAR -> the VT-d remapping-hardware register base (docs/GPU-HARDWARE.md
/// 4, the IOMMU containment mechanism). The table header (36) is followed
/// by host_address_width(1) + flags(1) + reserved(10), then remapping
/// structures; the first DRHD (type 0) carries the register base at offset
/// 8 within it. Only present when QEMU is started with `-device
/// intel-iommu`; absent otherwise, leaving `iommu_base` 0 (skip-with-reason).
fn parse_dmar(dmar: u64, inv: &mut Inventory) {
    let len = rd32(dmar + 4) as u64;
    let mut off = 48; // header(36) + haw(1) + flags(1) + reserved(10)
    while off + 4 <= len {
        let stype = rd8(dmar + off) as u16 | ((rd8(dmar + off + 1) as u16) << 8);
        let slen = (rd8(dmar + off + 2) as u16 | ((rd8(dmar + off + 3) as u16) << 8)) as u64;
        if slen == 0 {
            break;
        }
        // Type 0 = DRHD (DMA Remapping Hardware Unit Definition): the
        // register base is a u64 at offset 8 within the structure.
        if stype == 0 && slen >= 16 && inv.iommu_base == 0 {
            inv.iommu_base = rd64(dmar + off + 8);
        }
        off += slen;
    }
}

/// MADT -> CPU list (enabled processors only).
fn parse_madt(madt: u64, inv: &mut Inventory) {
    let len = rd32(madt + 4) as u64;
    let mut off = 44; // header(36) + local_apic_addr(4) + flags(4)
    while off < len {
        let etype = rd8(madt + off);
        let elen = rd8(madt + off + 1) as u64;
        if elen == 0 {
            break;
        }
        match etype {
            0 => {
                // Local APIC: acpi_proc_id, apic_id, flags. Bit0 = enabled.
                let apic_id = rd8(madt + off + 3) as u32;
                let flags = rd32(madt + off + 4);
                if flags & 1 != 0 {
                    inv.add_cpu(apic_id, 0);
                }
            }
            9 => {
                // x2APIC: reserved(2), x2apic_id(4), flags(4).
                let x2id = rd32(madt + off + 4);
                let flags = rd32(madt + off + 8);
                if flags & 1 != 0 {
                    inv.add_cpu(x2id, 0);
                }
            }
            _ => {}
        }
        off += elen;
    }
}

/// MCFG -> PCIe ECAM base (first segment).
fn parse_mcfg(mcfg: u64, inv: &mut Inventory) {
    let len = rd32(mcfg + 4) as u64;
    if len >= 44 + 16 {
        inv.ecam_base = rd64(mcfg + 44);
    }
}

/// SRAT -> NUMA affinities: tag CPUs and memory regions with their node.
fn parse_srat(srat: u64, inv: &mut Inventory) {
    let len = rd32(srat + 4) as u64;
    let mut off = 48; // header(36) + reserved(4) + reserved(8)
    while off < len {
        let etype = rd8(srat + off);
        let elen = rd8(srat + off + 1) as u64;
        if elen == 0 {
            break;
        }
        match etype {
            0 => {
                // Processor local APIC affinity.
                let prox_lo = rd8(srat + off + 2) as u32;
                let apic_id = rd8(srat + off + 3) as u32;
                let flags = rd32(srat + off + 4);
                let prox_hi = (rd8(srat + off + 9) as u32)
                    | ((rd8(srat + off + 10) as u32) << 8)
                    | ((rd8(srat + off + 11) as u32) << 16);
                let node = (prox_lo | (prox_hi << 8)) as u8;
                if flags & 1 != 0 {
                    for c in &mut inv.cpus[..inv.ncpus] {
                        if c.hw_id == apic_id {
                            c.node = node;
                        }
                    }
                }
            }
            1 => {
                // Memory affinity. The firmware memory map (PVH E820) is one
                // contiguous RAM blob with no node split, so we intersect
                // each affinity range with the RAM regions and split them at
                // the node boundary (apply_mem_node).
                let node = rd32(srat + off + 2) as u8;
                let base = (rd32(srat + off + 8) as u64) | ((rd32(srat + off + 12) as u64) << 32);
                let size = (rd32(srat + off + 16) as u64) | ((rd32(srat + off + 20) as u64) << 32);
                let flags = rd32(srat + off + 28);
                if flags & 1 != 0 && size > 0 {
                    apply_mem_node(inv, base, size, node);
                }
            }
            _ => {}
        }
        off += elen;
    }
    // Recompute node count from what SRAT assigned.
    let mut maxnode = 0u8;
    for c in &inv.cpus[..inv.ncpus] {
        maxnode = maxnode.max(c.node);
    }
    for r in &inv.mem[..inv.nmem] {
        maxnode = maxnode.max(r.node);
    }
    inv.nnodes = inv.nnodes.max(maxnode as usize + 1);
}

/// Tag the RAM in `[base, base+size)` with `node`, splitting any RAM region
/// that straddles the boundary so every region lies within a single node.
/// Leftover pieces keep their previous node; new pieces are appended (and
/// re-examined by later affinity ranges, which is harmless since they no
/// longer overlap this one).
fn apply_mem_node(inv: &mut Inventory, base: u64, size: u64, node: u8) {
    let rend = base.saturating_add(size);
    let mut i = 0;
    while i < inv.nmem {
        let r = inv.mem[i];
        if r.kind != MemKind::Ram {
            i += 1;
            continue;
        }
        let end = r.base + r.len;
        let os = r.base.max(base);
        let oe = end.min(rend);
        if os >= oe {
            i += 1; // no overlap
            continue;
        }
        // Shrink this region to the overlap and tag it; re-add the parts
        // that fall outside the affinity range under their old node.
        inv.mem[i].base = os;
        inv.mem[i].len = oe - os;
        inv.mem[i].node = node;
        if r.base < os {
            inv.add_mem(r.base, os - r.base, r.kind, r.node);
        }
        if oe < end {
            inv.add_mem(oe, end - oe, r.kind, r.node);
        }
        i += 1;
    }
}

/// SLIT -> the node-to-node distance matrix (ACPI 5.2.17).
///
/// Layout: the standard 36-byte table header, then a `u64` locality count, then
/// `count * count` bytes of relative distances in row-major order. ACPI defines 10 as
/// "local" and forbids anything below it, so a stored 0 unambiguously means **not
/// reported** - which is what lets a caller tell a missing distance from a real one without
/// a second flag.
///
/// This is what turns "same node or not" into a *distance*, which is the difference between
/// a two-node fallback and an eight-node-across-two-sockets fallback being the same code
/// (docs/RESOURCE-GRAPH.md 1).
///
/// **Refuses rather than truncates.** A machine with more localities than
/// [`super::MAX_DIST_NODES`] keeps its nodes and loses only its distances: the matrix is
/// left empty and `slit_truncated` is set. A partly-filled matrix would be worse than none,
/// because a caller would read a real answer for some pairs and a fabricated one for the
/// rest, with nothing to tell them apart.
fn parse_slit(base: u64, inv: &mut super::Inventory) {
    let len = rd32(base + 4) as usize;
    // Header (36) + the locality count (8). A table shorter than that is malformed.
    if len < 44 {
        return;
    }
    let count = rd64(base + 36) as usize;
    if count == 0 {
        return;
    }
    if count > super::MAX_DIST_NODES {
        inv.slit_truncated = true;
        return;
    }
    // Every byte the header claims must actually be there, or the matrix is short and the
    // tail would read whatever follows the table.
    if len < 44 + count * count {
        return;
    }
    for from in 0..count {
        for to in 0..count {
            let d = rd8(base + 44 + (from * count + to) as u64);
            // Below ACPI's local value is not a distance. Storing it would make the "0 means
            // unreported" contract meaningless.
            if d >= 10 {
                inv.dist[from][to] = d;
            }
        }
    }
    // A count above what SRAT described is legitimate - firmware may describe localities
    // with no memory or CPUs yet - so the node count rises to whichever is larger.
    inv.nnodes = inv.nnodes.max(count);
}

/// HMAT -> real latency and bandwidth per (initiator, target) pair (ACPI 6.x, 5.2.27).
///
/// This is what SLIT cannot give. A SLIT distance is a *relative* number in ACPI's own units,
/// useful for ordering and useless for a decision that needs a magnitude - and the two fields
/// a caller is most likely to rank by are exactly the two SLIT omits. HBM is lower-latency
/// **and** higher-bandwidth than DDR; CXL is similar-bandwidth and much higher-latency; a
/// scalar distance cannot express either (docs/RESOURCE-GRAPH.md 2.2), and until this parser
/// existed the graph reported both as 0 = unknown rather than guessing.
///
/// Layout: the 40-byte HMAT header (a standard 36-byte table header plus 4 reserved), then a
/// sequence of `(type u16, reserved u16, length u32)` structures. Type **1** is the System
/// Locality Latency and Bandwidth Information structure, and the only one read here:
///
/// ```text
///   +0  type u16 = 1        +8  flags u8         +10 reserved u16
///   +4  length u32          +9  data_type u8     +12 num_initiator u32
///   +16 num_target u32      +24 entry_base_unit u64
///   +32 initiator list: num_initiator * u32
///       target list:    num_target    * u32
///       entries:        num_initiator * num_target * u16
/// ```
///
/// `data_type` 0 is access latency and 3 is access bandwidth; the read/write variants (1, 2,
/// 4, 5) are **deliberately ignored** rather than averaged into the access figure, because a
/// read latency reported as an access latency is a fabricated number and the `Cost` vector has
/// no field that means "read". They are a later refinement with their own fields.
///
/// A value is `entry * entry_base_unit`, latency in picoseconds and bandwidth in MB/s. Entry
/// `0xFFFF` means "unreachable" and `0` means "unknown"; both are stored as 0, because a
/// consumer of this table treats them identically - there is no cost it could use either way -
/// and inventing a distinction here would be inventing information.
fn parse_hmat(base: u64, inv: &mut super::Inventory) {
    let total = rd32(base + 4) as usize;
    if total < 40 {
        return;
    }
    let mut off = 40usize;
    while off + 8 <= total {
        let kind = rd16(base + off as u64);
        let len = rd32(base + off as u64 + 4) as usize;
        // A zero or unaligned length would loop forever; a length past the table would read
        // whatever follows it.
        if len < 8 || off + len > total {
            return;
        }
        if kind == 1 && len >= 32 {
            let data_type = rd8(base + off as u64 + 9);
            let ni = rd32(base + off as u64 + 12) as usize;
            let nt = rd32(base + off as u64 + 16) as usize;
            let unit = rd64(base + off as u64 + 24);
            let need = 32 + ni * 4 + nt * 4 + ni * nt * 2;
            if ni > 0 && nt > 0 && len >= need && unit > 0 {
                let ilist = base + off as u64 + 32;
                let tlist = ilist + (ni * 4) as u64;
                let ents = tlist + (nt * 4) as u64;
                for i in 0..ni {
                    let from = rd32(ilist + (i * 4) as u64) as usize;
                    if from >= super::MAX_DIST_NODES {
                        continue;
                    }
                    for t in 0..nt {
                        let to = rd32(tlist + (t * 4) as u64) as usize;
                        if to >= super::MAX_DIST_NODES {
                            continue;
                        }
                        let e = rd16(ents + ((i * nt + t) * 2) as u64);
                        // 0 = unknown, 0xFFFF = unreachable. Neither is a usable cost.
                        if e == 0 || e == 0xFFFF {
                            continue;
                        }
                        let v = (e as u64).saturating_mul(unit);
                        match data_type {
                            // Access latency, HMAT's picoseconds -> the nanoseconds every
                            // consumer of `Cost` reads. Rounded up, so a sub-nanosecond
                            // latency reads as 1 rather than as "not reported".
                            0 => {
                                inv.lat_ns[from][to] =
                                    ((v + 999) / 1000).min(u32::MAX as u64) as u32
                            }
                            // Access bandwidth, already MB/s.
                            3 => inv.bw_mbs[from][to] = v.min(u32::MAX as u64) as u32,
                            _ => {}
                        }
                    }
                }
            }
        }
        off += len;
    }
}
