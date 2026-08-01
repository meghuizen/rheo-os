//! Building the [resource graph](super::graph) from what firmware and architectural
//! discovery reported.
//!
//! **Separate from `hw::graph` on purpose.** That module is the model and the queries and is
//! deliberately dependency-free - no `arch`, no ACPI, no `Inventory` - which is what lets it
//! be model-checked on the host in `verify/graph/`. This half reaches into
//! [`super::Inventory`], so it cannot be, and the first version of it lived in `graph.rs` and
//! broke the host driver's compile immediately. That was the design telling the truth about
//! itself: the split is not stylistic, and the compile error is the guard.

use super::graph::{
    Arch, CapClass, Capability, Cost, IsaSet, NodeId, NodeKind, Reach, Source, Trust, graph,
};
use crate::mm::kmeta::Owner;

/// Build the graph from what firmware and architectural discovery reported
/// ([`super::Inventory`]).
///
/// Called from the boot sequencer **after** `hw::detect`, for the same reason
/// `frames::init_numa` is: the inventory is populated inside `arch::init` only after a
/// firmware table has been read.
///
/// **Nothing is invented.** A machine whose firmware describes no localities gets one
/// `MemoryNode` and no edges, which is exactly correct for it - every `cost` query then
/// answers `LOCAL` for the node to itself and `None` between distinct nodes, and
/// [`Graph::source`] reports [`Source::None`] so no test can claim a distance that was never
/// read. That degraded shape is asserted, not assumed (docs/RESOURCE-GRAPH.md 5).
pub fn build(inv: &super::Inventory) {
    // SAFETY: boot, single-threaded, before any secondary runs.
    let g = unsafe { graph() };
    g.init(Owner::KERNEL);

    // One node per memory locality, in locality order, so `NodeId(n)` is locality `n` for
    // the whole of the rest of this function. Recording the mapping rather than relying on
    // it would be safer; relying on it is why this loop is first and unconditional.
    let nnodes = inv.nnodes.max(1);
    let mut mem_node = [NodeId::NONE; super::MAX_DIST_NODES];
    for (n, slot) in mem_node
        .iter_mut()
        .enumerate()
        .take(nnodes.min(super::MAX_DIST_NODES))
    {
        let Some(id) = g.add_node(NodeKind::MemoryNode, n as u64) else {
            break;
        };
        *slot = id;
        // A locality is its own locality - so `in_locality` finds it, and so a device tagged
        // with it resolves to something.
        g.set_locality(id, id);
    }

    // One node per CPU, tagged with its locality, carrying the ISA the whole machine
    // reports. Per-CPU feature divergence (a hybrid part) is a later refinement and would
    // arrive here rather than anywhere else.
    let isa = IsaSet {
        arch: this_arch(),
        baseline: 0,
        features: 0,
    };
    for c in &inv.cpus[..inv.ncpus] {
        let Some(id) = g.add_node(NodeKind::Cpu, c.hw_id as u64) else {
            break;
        };
        g.set_isa(id, isa);
        let n = c.node as usize;
        if n < super::MAX_DIST_NODES && mem_node[n] != NodeId::NONE {
            g.set_locality(id, mem_node[n]);
        }
        // Floating point and vector arithmetic, **inline** - no queue, no DMA, no edge. This
        // is the entry that explains why `Reach::Inline` exists at all.
        //
        // **This is a machine-wide claim asserted per CPU, and that is a real limitation
        // rather than a simplification.** `Inventory` discovers features for the *machine*
        // (`inv.cpu`), not per CPU, so on a hybrid part - or any machine where some cores
        // lack an FPU or a vector width the others have - this would claim a capability a
        // core does not have. That is the shape docs/ENGINEERING.md 11 names: a field left
        // constant is a field that lies. It is true of every machine this kernel runs on
        // today (all three ISAs' QEMU profiles are homogeneous and all have FP), which is
        // why it is written rather than omitted - but per-CPU feature discovery is the
        // prerequisite for the heterogeneous-FPU handling in
        // docs/RESOURCE-GRAPH.md 6.4d, and it arrives here.
        g.add_capability(
            id,
            Capability {
                class: CapClass::FloatSimd,
                reach: Reach::Inline,
                rate: 0,
                trust: Trust::Performance,
                detail: 0,
            },
        );
    }

    // The SLIT matrix, as edges. `hops` carries the ACPI distance directly: it is a
    // *relative* number in ACPI's units, which is exactly what `hops` is for - the component
    // every firmware source can supply. Latency and bandwidth stay unknown because SLIT does
    // not report them; HMAT does, and is not parsed yet, so claiming them here would be
    // fabricating the two fields a caller is most likely to rank by.
    let mut edges = 0usize;
    if !inv.slit_truncated {
        for from in 0..nnodes.min(super::MAX_DIST_NODES) {
            for to in 0..nnodes.min(super::MAX_DIST_NODES) {
                let d = inv.dist[from][to];
                // A pair with neither a distance nor a magnitude has nothing to record.
                // `from == to` is skipped because `cost()` answers `LOCAL` for it directly -
                // recording a self-edge would give two answers to one question.
                if from == to {
                    continue;
                }
                if d == 0 && inv.lat_ns[from][to] == 0 && inv.bw_mbs[from][to] == 0 {
                    continue;
                }
                if mem_node[from] == NodeId::NONE || mem_node[to] == NodeId::NONE {
                    continue;
                }
                // SLIT gives `hops` - a relative number, good for ordering. HMAT gives the
                // magnitudes, and each field is filled **only if HMAT reported it**: a 0 here
                // means "not reported", which is what lets a caller tell a missing latency
                // from a fast one. Nothing is derived from `hops`, because a relative
                // distance is not a latency and converting one to the other would be
                // inventing a number in the field a caller is most likely to rank by.
                let cost = Cost {
                    latency_ns: inv.lat_ns[from][to],
                    bandwidth_mbs: inv.bw_mbs[from][to],
                    hops: d,
                    energy: 0,
                };
                if g.add_edge(mem_node[from], mem_node[to], cost) {
                    edges += 1;
                }
            }
        }
    }

    g.set_source(if edges > 0 {
        source_of(inv.firmware)
    } else {
        Source::None
    });
}

/// The graph's source, from the firmware that described the machine.
fn source_of(f: super::Firmware) -> Source {
    match f {
        super::Firmware::Acpi => Source::Acpi,
        super::Firmware::DeviceTree => Source::DeviceTree,
        _ => Source::CpuidOnly,
    }
}

/// The architecture this kernel is built for. A constant rather than a probe: a node's *arch*
/// is not discoverable at run time on a single-ISA machine, and a cluster's remote nodes
/// carry theirs from the host that published them.
fn this_arch() -> Arch {
    #[cfg(target_arch = "x86_64")]
    return Arch::X86_64;
    #[cfg(target_arch = "aarch64")]
    return Arch::Aarch64;
    #[cfg(target_arch = "riscv64")]
    return Arch::Riscv64;
}
