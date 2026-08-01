//! A minimal flattened-device-tree parser (ARM64 / RISC-V firmware). It
//! walks the FDT the bootloader handed us and pulls out exactly what the
//! inventory needs - the memory map, CPU list (with NUMA node and, on
//! RISC-V, the ISA string), the PCIe ECAM base - rather than exposing a
//! general tree API. The FDT is big-endian; all multibyte reads go through
//! the be helpers.
//!
//! Format reference: the Devicetree Specification. Struct-block tokens:
//! BEGIN_NODE=1, END_NODE=2, PROP=3, NOP=4, END=9.

use super::{Inventory, MemKind};

const MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

fn be32(p: *const u8, off: usize) -> u32 {
    // SAFETY: callers keep off within the mapped blob.
    unsafe {
        let b = |i: usize| p.add(off + i).read() as u32;
        (b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3)
    }
}

fn be64(p: *const u8, off: usize) -> u64 {
    ((be32(p, off) as u64) << 32) | be32(p, off + 4) as u64
}

/// A byte slice starting at `off` up to a NUL, as &str (best effort).
fn cstr(p: *const u8, off: usize, max: usize) -> &'static str {
    unsafe {
        let mut n = 0;
        while n < max && p.add(off + n).read() != 0 {
            n += 1;
        }
        let bytes = core::slice::from_raw_parts(p.add(off), n);
        core::str::from_utf8(bytes).unwrap_or("")
    }
}

/// Does a node name start with `prefix` (before any '@unit')?
fn name_is(name: &str, prefix: &str) -> bool {
    name == prefix || name.starts_with(prefix) && name.as_bytes().get(prefix.len()) == Some(&b'@')
}

/// The RISC-V ISA string from the first cpu node, captured for the CPU
/// feature decode (which is portable string work, not an ISA register).
static mut RISCV_ISA: [u8; 64] = [0; 64];
static mut RISCV_ISA_LEN: usize = 0;

pub fn riscv_isa() -> &'static str {
    unsafe {
        let len = *core::ptr::addr_of!(RISCV_ISA_LEN);
        let p = core::ptr::addr_of!(RISCV_ISA) as *const u8;
        core::str::from_utf8(core::slice::from_raw_parts(p, len)).unwrap_or("")
    }
}

pub fn parse(dtb: usize, inv: &mut Inventory) {
    let p = dtb as *const u8;
    if be32(p, 0) != MAGIC {
        return;
    }
    let off_struct = be32(p, 8) as usize;
    let off_strings = be32(p, 12) as usize;

    // Root #address-cells / #size-cells default to 2/2 on QEMU virt; track
    // the values seen at the current node's parent for reg decoding.
    let mut addr_cells = 2u32;
    let mut size_cells = 2u32;

    // Walk the struct block with a depth stack of node kinds, so a node's
    // own properties are attributed to it and not to a nested child.
    const MAX_DEPTH: usize = 24;
    let mut kind = [NodeKind::Other; MAX_DEPTH];
    let mut numa = [0u8; MAX_DEPTH]; // per-depth numa-node-id (inherited)
    let mut cpu_hwid = 0u32;
    let mut depth = 0usize;

    let mut pos = off_struct;
    loop {
        let token = be32(p, pos);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                let name = cstr(p, pos, 64);
                pos += (name.len() + 1 + 3) & !3;
                let k = if name_is(name, "cpu") {
                    NodeKind::Cpu
                } else if name_is(name, "memory") {
                    NodeKind::Memory
                } else if name_is(name, "pci") || name_is(name, "pcie") {
                    NodeKind::Pci
                } else {
                    NodeKind::Other
                };
                if depth < MAX_DEPTH {
                    kind[depth] = k;
                    numa[depth] = if depth > 0 { numa[depth - 1] } else { 0 };
                }
                if k == NodeKind::Cpu {
                    cpu_hwid = 0;
                }
                depth += 1;
            }
            FDT_END_NODE => {
                depth = depth.saturating_sub(1);
                if depth < MAX_DEPTH && kind[depth] == NodeKind::Cpu {
                    inv.add_cpu(cpu_hwid, numa[depth]);
                }
            }
            FDT_PROP => {
                let len = be32(p, pos) as usize;
                let nameoff = be32(p, pos + 4) as usize;
                let data = pos + 8;
                let pname = cstr(p, off_strings + nameoff, 64);
                pos += 8 + ((len + 3) & !3);

                let d = depth.saturating_sub(1);
                let here = if d < MAX_DEPTH {
                    kind[d]
                } else {
                    NodeKind::Other
                };

                if depth == 1 {
                    if pname == "#address-cells" {
                        addr_cells = be32(p, data);
                    } else if pname == "#size-cells" {
                        size_cells = be32(p, data);
                    }
                }
                if pname == "numa-node-id" && len >= 4 && d < MAX_DEPTH {
                    numa[d] = be32(p, data) as u8;
                }
                // The device-tree analogue of ACPI's SLIT (`numa-distance-map-v1`): a flat
                // array of `(from, to, distance)` u32 triples under a `distance-map` node.
                // Matched on the property rather than on the node's `compatible`, because the
                // property name is what carries the meaning and this walk already visits
                // every property once - so no second pass and no node-name matching.
                //
                // This is what gives riscv64 real distances. Without it that ISA discovered
                // its *nodes* from the device tree and had no *distances*, which the `numa`
                // kernel caught the moment it started asserting them
                // (docs/RESOURCE-GRAPH.md 2.4).
                if pname == "distance-matrix" {
                    parse_distance_matrix(p, data, len, inv);
                }
                match here {
                    NodeKind::Cpu => {
                        if pname == "reg" && len >= 4 {
                            cpu_hwid = be32(p, data);
                        } else if pname == "riscv,isa" {
                            save_riscv_isa(p, data, len);
                        }
                    }
                    NodeKind::Memory => {
                        if pname == "reg" {
                            let node = numa[d];
                            decode_reg(p, data, len, addr_cells, size_cells, |base, size| {
                                inv.add_mem(base, size, MemKind::Ram, node);
                            });
                        }
                    }
                    NodeKind::Pci => {
                        if pname == "reg" && inv.ecam_base == 0 {
                            decode_reg(p, data, len, addr_cells, size_cells, |base, _size| {
                                if inv.ecam_base == 0 {
                                    inv.ecam_base = base;
                                }
                            });
                        }
                    }
                    NodeKind::Other => {}
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    // The CPU topology needs every `cpu` node's phandle before it can resolve one, so it is
    // its own walk. See [`parse_cpu_map`].
    parse_cpu_map(p, off_struct, off_strings, inv);
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum NodeKind {
    Other,
    Cpu,
    Memory,
    Pci,
}

/// Decode a `reg` property as (address, size) pairs and call `f` per pair.
fn decode_reg(
    p: *const u8,
    data: usize,
    len: usize,
    addr_cells: u32,
    size_cells: u32,
    mut f: impl FnMut(u64, u64),
) {
    let pair_cells = (addr_cells + size_cells) as usize;
    if pair_cells == 0 {
        return;
    }
    let pairs = (len / 4) / pair_cells;
    let mut off = data;
    for _ in 0..pairs {
        let base = read_cells(p, off, addr_cells);
        off += addr_cells as usize * 4;
        let size = read_cells(p, off, size_cells);
        off += size_cells as usize * 4;
        f(base, size);
    }
}

fn read_cells(p: *const u8, off: usize, cells: u32) -> u64 {
    match cells {
        1 => be32(p, off) as u64,
        2 => be64(p, off),
        _ => be64(p, off),
    }
}

fn save_riscv_isa(p: *const u8, data: usize, len: usize) {
    unsafe {
        let dst = core::ptr::addr_of_mut!(RISCV_ISA) as *mut u8;
        let n = (len.saturating_sub(1)).min(64);
        for i in 0..n {
            dst.add(i).write(p.add(data + i).read());
        }
        *core::ptr::addr_of_mut!(RISCV_ISA_LEN) = n;
    }
}

/// `numa-distance-map-v1`'s `distance-matrix`: `(from, to, distance)` u32 triples, big-endian.
///
/// Refuses the same way `parse_slit` does and for the same reason: a locality beyond
/// [`crate::hw::MAX_DIST_NODES`] sets `slit_truncated` and stores **nothing**, because a
/// partly-filled distance matrix is worse than an empty one - a caller would read a real
/// answer for some pairs and a fabricated one for the rest with nothing to tell them apart.
///
/// The device tree, unlike ACPI, does not define a minimum distance; the local value is 10 by
/// the same convention, so anything below it is not stored and "0 means unreported" stays
/// unambiguous.
/// Read `/cpus/cpu-map` and fill each CPU's core and cache-domain id
/// (docs/RESOURCE-GRAPH.md 2.4a).
///
/// A second walk of the struct block rather than more state in [`parse`], for one reason: a
/// `cpu-map` entry names a CPU by **phandle**, and a phandle is only resolvable once every
/// `cpu` node has been seen. The device-tree specification does not order `cpu-map` after the
/// `cpu` nodes, so depending on QEMU's ordering would be depending on a coincidence. This
/// collects both tables and resolves at the end.
///
/// The map's shape is a nesting: `cluster` nodes contain `core` nodes, which either name a
/// CPU directly (one thread) or contain `thread` nodes that each do. So:
///
/// - **cache domain** = the enclosing cluster. Counted rather than read from the node name,
///   because the specification allows a `socket` level above cluster and two sockets may each
///   have a `cluster0` - a name index would merge them.
/// - **core** = the enclosing core node, counted the same way, so a core id is unique across
///   the machine, which is what an SMT-sibling test needs.
///
/// Both shapes of core (with and without `thread` children) fall out of that without a special
/// case: the id in force when a `cpu` property is met is the answer.
///
/// Fills nothing and reports nothing when there is no `cpu-map`, which is the honest result -
/// the caller then leaves the topology unknown rather than defaulting it.
fn parse_cpu_map(p: *const u8, off_struct: usize, off_strings: usize, inv: &mut Inventory) {
    /// What the walk is inside, one entry per depth.
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum In {
        Other,
        CpuNode,
        CpuMap,
    }

    const MAX_DEPTH: usize = 24;
    let mut stack = [In::Other; MAX_DEPTH];
    let mut depth = 0usize;

    // cpu nodes: phandle -> hardware id.
    let mut ph = [0u32; crate::hw::MAX_CPUS];
    let mut ph_hwid = [0u32; crate::hw::MAX_CPUS];
    let mut nph = 0usize;
    let mut cur_ph = 0u32;
    let mut cur_hwid = 0u32;

    // cpu-map: phandle -> (cache domain, core).
    let mut m_ph = [0u32; crate::hw::MAX_CPUS];
    let mut m_llc = [0u16; crate::hw::MAX_CPUS];
    let mut m_core = [0u16; crate::hw::MAX_CPUS];
    let mut nmap = 0usize;
    let mut nclusters = 0u16;
    let mut ncores = 0u16;

    let mut pos = off_struct;
    loop {
        let token = be32(p, pos);
        pos += 4;
        match token {
            FDT_BEGIN_NODE => {
                let name = cstr(p, pos, 64);
                pos += (name.len() + 1 + 3) & !3;
                let inside_map = depth > 0 && depth <= MAX_DEPTH && {
                    // Any depth at or below the `cpu-map` node counts as inside it.
                    stack[..depth.min(MAX_DEPTH)].contains(&In::CpuMap)
                };
                let here = if name == "cpu-map" {
                    In::CpuMap
                } else if inside_map {
                    if name.starts_with("cluster") {
                        nclusters += 1;
                    } else if name.starts_with("core") {
                        ncores += 1;
                    }
                    In::CpuMap
                } else if name_is(name, "cpu") {
                    cur_ph = 0;
                    cur_hwid = 0;
                    In::CpuNode
                } else {
                    In::Other
                };
                if depth < MAX_DEPTH {
                    stack[depth] = here;
                }
                depth += 1;
            }
            FDT_END_NODE => {
                depth = depth.saturating_sub(1);
                if depth < MAX_DEPTH
                    && stack[depth] == In::CpuNode
                    && cur_ph != 0
                    && nph < crate::hw::MAX_CPUS
                {
                    ph[nph] = cur_ph;
                    ph_hwid[nph] = cur_hwid;
                    nph += 1;
                }
            }
            FDT_PROP => {
                let len = be32(p, pos) as usize;
                let nameoff = be32(p, pos + 4) as usize;
                let data = pos + 8;
                let pname = cstr(p, off_strings + nameoff, 64);
                pos += 8 + ((len + 3) & !3);
                let d = depth.saturating_sub(1);
                let here = if d < MAX_DEPTH { stack[d] } else { In::Other };
                match here {
                    In::CpuNode if len >= 4 => {
                        if pname == "phandle" {
                            cur_ph = be32(p, data);
                        } else if pname == "reg" {
                            cur_hwid = be32(p, data);
                        }
                    }
                    In::CpuMap if pname == "cpu" && len >= 4 && nmap < crate::hw::MAX_CPUS => {
                        m_ph[nmap] = be32(p, data);
                        m_llc[nmap] = nclusters.saturating_sub(1);
                        m_core[nmap] = ncores.saturating_sub(1);
                        nmap += 1;
                    }
                    _ => {}
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    if nmap == 0 {
        return;
    }
    // Resolve: phandle -> hardware id -> the inventory's CPU.
    let mut filled = 0usize;
    for i in 0..nmap {
        let Some(j) = (0..nph).find(|&j| ph[j] == m_ph[i]) else {
            continue;
        };
        let hwid = ph_hwid[j];
        for c in inv.cpus[..inv.ncpus].iter_mut() {
            if c.hw_id == hwid {
                c.core_id = m_core[i];
                c.llc_id = m_llc[i];
                filled += 1;
            }
        }
    }
    // Only claim the device tree as the source if every CPU got an answer. A partly-filled
    // topology is the `slit_truncated` situation again: a caller would read a real grouping
    // for some CPUs and the unknown sentinel for others, and the arch fallback - which
    // answers for all of them or none - is the better result.
    if filled == inv.ncpus {
        inv.topo = super::TopoSource::DeviceTree;
    } else {
        for c in inv.cpus[..inv.ncpus].iter_mut() {
            c.core_id = super::TOPO_UNKNOWN;
            c.llc_id = super::TOPO_UNKNOWN;
        }
    }
}

fn parse_distance_matrix(p: *const u8, data: usize, len: usize, inv: &mut Inventory) {
    let triples = len / 12;
    // First pass: does everything fit? Deciding after storing would leave a partial matrix.
    for i in 0..triples {
        let from = be32(p, data + i * 12) as usize;
        let to = be32(p, data + i * 12 + 4) as usize;
        if from >= crate::hw::MAX_DIST_NODES || to >= super::MAX_DIST_NODES {
            inv.slit_truncated = true;
            return;
        }
    }
    for i in 0..triples {
        let from = be32(p, data + i * 12) as usize;
        let to = be32(p, data + i * 12 + 4) as usize;
        let d = be32(p, data + i * 12 + 8);
        if d >= 10 && d <= u8::MAX as u32 {
            inv.dist[from][to] = d as u8;
            // The device tree records one direction of a symmetric pair; ACPI records both.
            // Filling the reverse keeps `cost(b, a)` answerable on both ISAs, which is what
            // lets one assertion cover them - and it is a *convention*, so it never
            // overwrites a distance the firmware stated explicitly.
            if inv.dist[to][from] == 0 {
                inv.dist[to][from] = d as u8;
            }
        }
        inv.nnodes = inv.nnodes.max(from + 1).max(to + 1);
    }
}
