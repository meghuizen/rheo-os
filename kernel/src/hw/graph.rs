//! **The resource graph** - one typed model of the machine, queried rather than hardcoded
//! (docs/RESOURCE-GRAPH.md).
//!
//! # What this replaces
//!
//! [`super::Inventory`] knows *what* hardware exists and almost nothing about *how it
//! relates*, so placement is written as constants: the frame pool sits a fixed distance
//! into RAM, a cell's home node is round-robin, a driver's queues come from the pool at
//! large with no reference to the device's node. That is tolerable on one socket and wrong
//! on a real server, and unexpressible across hosts.
//!
//! # Three design decisions, each with a reason that has already cost something
//!
//! **1. Capabilities, not device kinds.** A fixed `EngineKind` enum is a list of what
//! someone thought of, and the resources that matter are the ones it did not: a hardware
//! RNG, a TPM, an NPU, the FPU inside every core, and the engines sitting *on* a NIC -
//! inline crypto, compression, DMA, a precision clock, packet pipelines. A node **offers
//! capabilities** ([`Capability`]) and a query asks for a capability class, never for a
//! product name. The precedent is docs/TILES.md, where an engine is *anything that declares
//! a TileContract*; this generalises that from matmul to every class.
//!
//! **2. Costs are a vector, not a distance.** HBM is lower-latency *and* higher-bandwidth
//! than DDR; CXL is similar-bandwidth and much higher-latency; a remote host is worse in
//! both by orders of magnitude. One number forces a policy choice into the data, and callers
//! disagree - a tile GEMM is bandwidth-bound where an RPC path is latency-bound. So [`Cost`]
//! carries four fields and the caller names the [`Metric`] it cares about.
//!
//! **3. Some capabilities are not comparable by cost at all, and asking is a bug.** An
//! entropy source ranked by throughput is a regression: a fast source of unknown quality is
//! worse than a slow one that passed SP 800-90B health tests, which is the line
//! docs/TIME-IDENTITY.md already holds. A TPM has no meaningful latency ranking - it is a
//! trust root, and the only useful question is which measurement chain it roots. So
//! [`Graph::nearest`] **refuses** a metric that is meaningless for the class rather than
//! returning a plausible wrong answer.
//!
//! # Selection versus aggregation
//!
//! [`Graph::nearest`] picks one provider. [`Graph::providers`] returns *all* of them, and
//! that is not a convenience - it is the correct primitive for entropy. **Several RNGs feed
//! one DRBG**: SP 800-90B/C conditions multiple sources together so that one degraded or
//! compromised source cannot determine the output. A design that only offered "the best RNG"
//! would make the seed depend on a single source, which is the property the standard exists
//! to prevent. Crypto and matmul offload want `nearest`; entropy wants `providers`; the
//! graph offers both and neither is the default.
//!
//! # No solver
//!
//! [`Graph::cost`] answers for a **direct** edge and for a node to itself. It does not
//! compute a transitive closure, and `nearest` is a bounded scan. Barrelfish's system
//! knowledge base is the precedent and docs/GREENFIELD.md 2.10 records the judgement: adopt
//! the queryable model, refuse the constraint solver - unbounded search does not belong in
//! the one component whose discipline is bounded work and no allocation. Anything wanting
//! search runs in a cell over the same data.
//!
//! # Dependency-free on purpose
//!
//! Nothing here touches `arch`, ACPI, a device tree or a clock: this module is the *model*
//! and the *queries*, and the per-ISA discovery that fills it lives with the firmware
//! parsers that already exist. That is what lets the whole thing be model-checked on the
//! host in `verify/graph/`, the way `sched::entity` and `telemetry` already are - and a
//! graph is exactly the kind of index arithmetic where an off-by-one is invisible in a boot
//! test.

use crate::mm::kmeta::{Funded, Owner};

/// An index into the graph's node table.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct NodeId(pub u16);

impl NodeId {
    pub const NONE: NodeId = NodeId(u16::MAX);
}

/// What a node *is*. Deliberately short: everything specific is a [`Capability`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum NodeKind {
    Free = 0,
    /// A machine. One for a single host; more for a cluster, where the interesting edges
    /// are the expensive ones.
    Host = 1,
    /// A memory locality - a NUMA node, an HBM stack, a CXL region, a pmem region.
    MemoryNode = 2,
    /// A single execution unit at the level the scheduler places on.
    Cpu = 3,
    /// A cache, so "do these two CPUs share an LLC" is a membership question about a real
    /// node rather than a heuristic on ids.
    Cache = 4,
    /// A device you configure: a NIC, an NVMe controller, a display adapter.
    Device = 5,
    /// An executor you place work on. **Separate from `Device` on purpose**: a GPU is both,
    /// and conflating them is what makes accelerators need their own scheduler path instead
    /// of riding the one every entity uses.
    Engine = 6,
    /// A path between nodes with its own properties: a PCIe root port, a CXL link, a socket
    /// interconnect, a network.
    Link = 7,
    /// A DMA translation domain provider - VT-d, SMMUv3, the RISC-V IOMMU. A node because
    /// "which IOMMU must I program to let this device reach that memory" is a graph
    /// question.
    Iommu = 8,
    /// An interrupt controller - IOAPIC, GICv3, APLIC/IMSIC. A node because interrupt
    /// routing has locality: a vector delivered to a far core costs what a far access costs.
    IntCtrl = 9,
    /// A clock source, with accuracy and stability in its capability detail.
    Clock = 10,
    /// A power or thermal domain (docs/POWER.md): the unit that shares a frequency or a
    /// thermal budget, which is why two CPUs in one domain cannot be treated as independent.
    PowerDomain = 11,
    /// A trust root - a TPM, a secure element, a TEE.
    TrustRoot = 12,
    /// A storage namespace or zone set, so host-managed placement (ZNS, docs/GREENFIELD.md
    /// 2.8) has something to name.
    Storage = 13,
}

/// The ISA contract a node satisfies, so "can this binary run here at all" is answerable
/// before placement rather than as a `SIGILL` afterwards.
///
/// `baseline` is separate from `features` deliberately: a baseline is the contract a
/// *compiler* was given (`x86-64-v4`, `RVA23`), and the useful question is almost always
/// "does this node meet the baseline my binary was built for", not "does it have each of
/// thirty flags".
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct IsaSet {
    pub arch: Arch,
    pub baseline: u8,
    pub features: u64,
}

impl IsaSet {
    pub const NONE: IsaSet = IsaSet {
        arch: Arch::Unknown,
        baseline: 0,
        features: 0,
    };

    /// Whether this node can run code built for `want`. Same architecture, a baseline at
    /// least as high, and every requested feature present.
    pub fn satisfies(&self, want: &IsaSet) -> bool {
        self.arch == want.arch
            && self.baseline >= want.baseline
            && (self.features & want.features) == want.features
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Arch {
    Unknown = 0,
    X86_64 = 1,
    Aarch64 = 2,
    Riscv64 = 3,
}

/// What a node can *do*. The open-ended half of the model.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum CapClass {
    None = 0,
    /// Matrix multiply - a CPU SIMD tier, a GPU, an NPU, an AMX tile unit.
    Matmul = 1,
    /// Symmetric or public-key crypto. `detail` carries the algorithm set.
    Crypto = 2,
    Compress = 3,
    /// An entropy source. Ranked by evidence, never by rate - see [`Trust`].
    Entropy = 4,
    /// Measurement, sealing, attestation. Not comparable by cost at all.
    Attest = 5,
    /// A timestamp source; `detail` carries accuracy in nanoseconds.
    Timestamp = 6,
    /// A copy engine.
    Dma = 7,
    /// Video or image encode/decode.
    Codec = 8,
    /// Programmable packet processing on a NIC.
    PacketPipeline = 9,
    /// Remote DMA.
    Rdma = 10,
    /// Floating point / vector arithmetic, inline in a core.
    FloatSimd = 11,
    /// DMA address translation.
    DmaTranslate = 12,
    /// Memory encryption (SEV/TDX/CCA).
    MemEncrypt = 13,
    /// Host-managed placement on storage (ZNS/FDP).
    ZonedPlacement = 14,
    /// Compute co-located with storage.
    ComputeInStorage = 15,
}

/// How a capability is reached, and this split is structural rather than a label.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Reach {
    /// Available to code **executing on** this node: the FPU, SIMD, AES-NI, RDRAND. No
    /// queue, no DMA, and therefore no cost edge - it is an attribute of the execution
    /// context, not a destination.
    Inline,
    /// Reached by **submitting work across an edge**: a GPU, an NPU, a NIC's crypto engine.
    Offload {
        queue_depth: u16,
        doorbell_ns: u32,
        /// The request size below which **inline wins**, and the field usually missing.
        ///
        /// A NIC's crypto engine beats AES-NI only above some payload size; below it the
        /// doorbell and DMA cost more than the cipher. A graph that reported the capability
        /// without this would confidently make work slower, which is worse than not knowing
        /// - the same bandwidth-versus-latency mistake as a scalar distance, one level down.
        min_bytes: u32,
    },
}

/// Why a capability is preferred - and for two classes, *not* speed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Trust {
    /// Ranked by rate and cost. A codec, a DMA engine, a matmul unit.
    Performance,
    /// Ranked by **evidence**. A source that passed SP 800-90B health tests outranks a
    /// faster one that did not, which is the position docs/TIME-IDENTITY.md already takes
    /// for the per-cell DRBG.
    Entropy {
        health_tested: bool,
        /// Bits of min-entropy per byte, as assessed. Zero means "unassessed", which is
        /// worse than a low number because it is unknown rather than small.
        assessed_bits: u8,
    },
    /// Not comparable by cost. The only useful question is which measurement chain this
    /// roots.
    Attestation { chain: u32 },
}

/// One capability a node offers.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Capability {
    pub class: CapClass,
    pub reach: Reach,
    /// Units per second, or 0 for a capability that is not a throughput resource.
    pub rate: u32,
    pub trust: Trust,
    /// Class-specific: an algorithm mask, tile shapes, clock accuracy, a zone size.
    pub detail: u64,
}

/// What a caller requires of a capability, so Phase 1 can filter before Phase 2 ranks.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Request {
    pub class: CapClass,
    /// Bytes in the request, compared against an offload's `min_bytes`.
    pub bytes: u32,
    /// The ISA a candidate must satisfy, or [`IsaSet::NONE`] for "any".
    pub isa: IsaSet,
    /// When true, a candidate whose result would differ in the last bits is **refused**,
    /// not ranked down.
    ///
    /// Every tile kernel is such a caller, and that contract is what makes the
    /// FlashAttention proofs equalities rather than tolerances (docs/TILES.md). See
    /// docs/CPU-FEATURES.md 2.2 - `fma(a,b,c)` is not `a*b` then `+c`.
    pub bit_exact: bool,
    /// Required detail bits (an algorithm, a tile shape). All must be present.
    pub detail_mask: u64,
}

impl Request {
    pub const fn any(class: CapClass) -> Request {
        Request {
            class,
            bytes: 0,
            isa: IsaSet::NONE,
            bit_exact: false,
            detail_mask: 0,
        }
    }
}

/// How a candidate would satisfy a request. The four outcomes of
/// docs/CPU-FEATURES.md 2.1, and never a fifth.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// The node has the instruction or engine.
    Native,
    /// A different sequence computing the **same bits**.
    Translated,
    /// A portable sequence computing the same bits, much slower.
    Emulated,
    /// Mathematically the same operation, **different rounding**. Refused under a bit-exact
    /// contract rather than ranked down.
    Numeric,
    /// No honest answer exists here.
    Unavailable,
}

/// The four-component cost of reaching one node from another.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Cost {
    pub latency_ns: u32,
    pub bandwidth_mbs: u32,
    /// Topological hops - the one component every firmware source can supply, so a degraded
    /// graph is still usable.
    pub hops: u8,
    /// Relative energy per access (docs/POWER.md).
    pub energy: u8,
}

impl Cost {
    /// A node to itself.
    pub const LOCAL: Cost = Cost {
        latency_ns: 0,
        bandwidth_mbs: u32::MAX,
        hops: 0,
        energy: 0,
    };
    pub const UNKNOWN: Cost = Cost {
        latency_ns: u32::MAX,
        bandwidth_mbs: 0,
        hops: u8::MAX,
        energy: u8::MAX,
    };
}

/// Which component of [`Cost`] a caller is ranking by.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Metric {
    Latency,
    Bandwidth,
    Hops,
    Energy,
}

/// Why a query was refused. Each variant is a distinct fact, because "no answer" collapses
/// situations a caller must tell apart.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum QueryError {
    /// No node offers the class at all.
    NoProvider,
    /// Providers exist and every one was filtered out - by ISA, by `min_bytes`, by a
    /// bit-exact contract, or by required detail. Distinct from `NoProvider` because it is
    /// the answer that says "relax the request", not "add hardware".
    AllFiltered,
    /// The class is not comparable by this metric, and answering would be a wrong answer
    /// rather than a missing one. `nearest(Attest, Latency)` is the canonical case.
    MetricMeaningless,
    /// The graph holds no such node.
    NoNode,
}

/// Where the graph's contents came from, so no proof can claim a distance it does not have.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Source {
    /// Nothing was discovered: one node, no distances, everything equally near - which is
    /// exactly correct for a machine whose firmware describes nothing.
    None = 0,
    Acpi = 1,
    DeviceTree = 2,
    /// Architectural discovery only (CPUID, `ID_AA64*`).
    CpuidOnly = 3,
    /// Declared by a driver cell rather than read from firmware.
    Declared = 4,
}

/// One node.
#[derive(Copy, Clone, Debug)]
pub struct Node {
    pub kind: NodeKind,
    /// The memory locality this node belongs to, or [`NodeId::NONE`].
    pub node: NodeId,
    /// Set membership - a cache domain, an SMT set, a power domain. `NONE` when unset.
    pub set: NodeId,
    pub isa: IsaSet,
    /// Bus-device-function for a `Device`, a hardware id for a `Cpu`, a size for memory.
    pub hw_id: u64,
    /// Index of this node's first capability in the capability table, and how many.
    caps_at: u16,
    caps_len: u16,
}

impl Node {
    pub const EMPTY: Node = Node {
        kind: NodeKind::Free,
        node: NodeId::NONE,
        set: NodeId::NONE,
        isa: IsaSet::NONE,
        hw_id: 0,
        caps_at: 0,
        caps_len: 0,
    };
}

/// A directed edge. **Directed on purpose**: SLIT is not required to be symmetric, and a
/// link with different read and write bandwidth is ordinary.
///
/// # Why `used` exists, and it is not defensive
///
/// [`Funded`] grows into freshly-allocated frames, which arrive **zeroed** - that is the
/// module's stated contract on `T`. So an "empty" value must *be* the all-zero pattern, or
/// every slot of unfilled slack reads as real data. The first version of this struct marked
/// emptiness with `from: NodeId::NONE` (`u16::MAX`), and the consequence was immediate and
/// concrete: a two-node machine reported **512 edges**, because every zeroed slot read as a
/// valid edge from node 0 to node 0 - and `add_edge`'s free-slot search, looking for
/// `NodeId::NONE`, never found one and appended past the end on every call.
///
/// `Node` and `Capability` avoid this by construction because their empty discriminants are
/// zero (`NodeKind::Free = 0`, `CapClass::None = 0`), which is the same discipline
/// `sched::entity` follows with `State::Free`. An explicit flag is the version of that for a
/// struct whose fields have no spare zero.
#[derive(Copy, Clone, Debug)]
pub struct Edge {
    /// False in a freshly-zeroed slot, which is what makes slack distinguishable from data.
    pub used: bool,
    pub from: NodeId,
    pub to: NodeId,
    pub cost: Cost,
}

impl Edge {
    pub const EMPTY: Edge = Edge {
        used: false,
        from: NodeId::NONE,
        to: NodeId::NONE,
        cost: Cost::UNKNOWN,
    };
}

/// The graph. Storage is [`Funded`] so its size is bounded by the owner's frame budget
/// rather than by an array dimension (docs/SUBSTRATE.md pillar 1).
pub struct Graph {
    nodes: Funded<Node>,
    edges: Funded<Edge>,
    caps: Funded<Capability>,
    source: Source,
    /// Capabilities appended for a node that was not the most recent - which would break
    /// the contiguous `caps_at..caps_at+caps_len` range. Counted rather than silently
    /// mis-stored.
    misordered_caps: u32,
}

impl Graph {
    pub const fn new() -> Graph {
        Graph {
            nodes: Funded::new(),
            edges: Funded::new(),
            caps: Funded::new(),
            source: Source::None,
            misordered_caps: 0,
        }
    }

    pub fn init(&mut self, owner: Owner) {
        self.nodes.set_owner(owner);
        self.edges.set_owner(owner);
        self.caps.set_owner(owner);
    }

    /// What the contents were built from. Printed at boot and readable by a test, so a run
    /// cannot claim firmware distances it never read.
    pub fn source(&self) -> Source {
        self.source
    }

    pub fn set_source(&mut self, s: Source) {
        self.source = s;
    }

    pub fn node_count(&self) -> usize {
        (0..self.nodes.capacity())
            .filter_map(|i| self.nodes.get(i))
            .filter(|n| n.kind != NodeKind::Free)
            .count()
    }

    pub fn edge_count(&self) -> usize {
        (0..self.edges.capacity())
            .filter_map(|i| self.edges.get(i))
            .filter(|e| e.used)
            .count()
    }

    /// Capabilities appended out of order - see the field's doc. A test asserts zero.
    pub fn misordered_caps(&self) -> u32 {
        self.misordered_caps
    }

    pub fn get(&self, id: NodeId) -> Option<Node> {
        let n = self.nodes.get(id.0 as usize)?;
        if n.kind == NodeKind::Free {
            return None;
        }
        Some(n)
    }

    /// Add a node. Returns `None` when the owner's budget cannot fund the growth - a clean
    /// refusal, never a global "table full" (docs/MEMORY.md 7).
    pub fn add_node(&mut self, kind: NodeKind, hw_id: u64) -> Option<NodeId> {
        if kind == NodeKind::Free {
            return None;
        }
        let id = (0..self.nodes.capacity())
            .find(|&i| self.nodes.get(i).map(|n| n.kind) == Some(NodeKind::Free))
            .unwrap_or_else(|| self.nodes.capacity());
        let mut n = Node::EMPTY;
        n.kind = kind;
        n.hw_id = hw_id;
        n.caps_at = self.caps_high_water();
        if id >= self.nodes.capacity() {
            if !self.nodes.set_growing(id, n) {
                return None;
            }
        } else if !self.nodes.set(id, n) {
            return None;
        }
        Some(NodeId(id as u16))
    }

    fn caps_high_water(&self) -> u16 {
        (0..self.caps.capacity())
            .filter_map(|i| self.caps.get(i))
            .filter(|c| c.class != CapClass::None)
            .count() as u16
    }

    /// Place `id` in memory locality `node`.
    pub fn set_locality(&mut self, id: NodeId, node: NodeId) -> bool {
        let Some(mut n) = self.get(id) else {
            return false;
        };
        n.node = node;
        self.nodes.set(id.0 as usize, n)
    }

    /// Put `id` in set `set` - a cache domain, an SMT set, a power domain.
    pub fn set_member(&mut self, id: NodeId, set: NodeId) -> bool {
        let Some(mut n) = self.get(id) else {
            return false;
        };
        n.set = set;
        self.nodes.set(id.0 as usize, n)
    }

    pub fn set_isa(&mut self, id: NodeId, isa: IsaSet) -> bool {
        let Some(mut n) = self.get(id) else {
            return false;
        };
        n.isa = isa;
        self.nodes.set(id.0 as usize, n)
    }

    /// Attach a capability to `id`.
    ///
    /// Capabilities are stored contiguously per node, so they must be added while `id` is
    /// the most recently created node. Adding out of order is **counted and refused**
    /// rather than stored somewhere it will be read as another node's - a silent
    /// mis-attribution would make the graph confidently wrong, which is the failure mode
    /// this whole module exists to avoid.
    pub fn add_capability(&mut self, id: NodeId, cap: Capability) -> bool {
        if cap.class == CapClass::None {
            return false;
        }
        let Some(mut n) = self.get(id) else {
            return false;
        };
        let hw = self.caps_high_water();
        if n.caps_at + n.caps_len != hw {
            self.misordered_caps = self.misordered_caps.wrapping_add(1);
            return false;
        }
        if !self.caps.set_growing(hw as usize, cap) {
            return false;
        }
        n.caps_len += 1;
        self.nodes.set(id.0 as usize, n)
    }

    /// The capabilities `id` offers.
    pub fn capabilities(&self, id: NodeId) -> impl Iterator<Item = Capability> + '_ {
        let (at, len) = match self.get(id) {
            Some(n) => (n.caps_at as usize, n.caps_len as usize),
            None => (0, 0),
        };
        (at..at + len).filter_map(move |i| self.caps.get(i))
    }

    /// Record a directed cost. A second edge for the same pair **replaces** the first, so a
    /// firmware table read twice does not double.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId, cost: Cost) -> bool {
        if self.get(from).is_none() || self.get(to).is_none() {
            return false;
        }
        if let Some(i) = (0..self.edges.capacity()).find(|&i| {
            self.edges
                .get(i)
                .map(|e| e.used && e.from == from && e.to == to)
                .unwrap_or(false)
        }) {
            return self.edges.set(
                i,
                Edge {
                    used: true,
                    from,
                    to,
                    cost,
                },
            );
        }
        let free = (0..self.edges.capacity())
            .find(|&i| self.edges.get(i).map(|e| !e.used).unwrap_or(false))
            .unwrap_or_else(|| self.edges.capacity());
        self.edges.set_growing(
            free,
            Edge {
                used: true,
                from,
                to,
                cost,
            },
        )
    }

    /// Record a symmetric cost - two directed edges. Convenience for the common firmware
    /// case, kept explicit so an asymmetric link is not accidentally symmetrised.
    pub fn add_edge_both(&mut self, a: NodeId, b: NodeId, cost: Cost) -> bool {
        self.add_edge(a, b, cost) && self.add_edge(b, a, cost)
    }

    /// The cost of reaching `to` from `from`: `LOCAL` for a node to itself, a direct edge if
    /// one was recorded, else `None`.
    ///
    /// **Deliberately not transitive.** Chaining edges is a shortest-path search, which is
    /// the solver this module refuses to contain. A caller wanting a path asks a cell.
    pub fn cost(&self, from: NodeId, to: NodeId) -> Option<Cost> {
        if self.get(from).is_none() || self.get(to).is_none() {
            return None;
        }
        if from == to {
            return Some(Cost::LOCAL);
        }
        (0..self.edges.capacity())
            .filter_map(|i| self.edges.get(i))
            .find(|e| e.used && e.from == from && e.to == to)
            .map(|e| e.cost)
    }

    /// Everything in set `set`.
    pub fn members(&self, set: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        (0..self.nodes.capacity()).filter_map(move |i| {
            let n = self.nodes.get(i)?;
            if n.kind != NodeKind::Free && n.set == set {
                Some(NodeId(i as u16))
            } else {
                None
            }
        })
    }

    /// Every node in memory locality `node`.
    pub fn in_locality(&self, node: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        (0..self.nodes.capacity()).filter_map(move |i| {
            let n = self.nodes.get(i)?;
            if n.kind != NodeKind::Free && n.node == node {
                Some(NodeId(i as u16))
            } else {
                None
            }
        })
    }

    /// Find a node by kind and hardware id - "which node is this device".
    pub fn find(&self, kind: NodeKind, hw_id: u64) -> Option<NodeId> {
        (0..self.nodes.capacity()).find_map(|i| {
            let n = self.nodes.get(i)?;
            if n.kind == kind && n.hw_id == hw_id {
                Some(NodeId(i as u16))
            } else {
                None
            }
        })
    }

    /// **Phase 1**: how would `id` satisfy `req`?
    ///
    /// Filtering comes before ranking and never merges with it: a combined score can trade
    /// correctness for proximity, which is how work lands where it does not run or produces
    /// a different answer.
    pub fn resolve(&self, id: NodeId, req: &Request) -> Resolution {
        self.select(id, req)
            .map(|(_, r)| r)
            .unwrap_or(Resolution::Unavailable)
    }

    /// **The one place a candidate is filtered**, returning the capability that survived and
    /// how it resolves.
    ///
    /// It exists because the filter was written twice - once in `resolve` and once inside the
    /// cost computation - and a control that removed the offload-floor check from `resolve`
    /// **passed**, because the other copy still enforced it. Two places deciding one thing,
    /// with a test unable to tell: the defect docs/EXECUTION-MODEL.md 1 is about, and the
    /// third time it appeared in code written during this same piece of work (the others were
    /// `telemetry`'s duplicated admission checks and the entity fuzzer's own I5 oracle). One
    /// selector now, so a control on any clause reaches every caller.
    fn select(&self, id: NodeId, req: &Request) -> Option<(Capability, Resolution)> {
        let n = self.get(id)?;
        // ISA is a property of the node, not of a capability: a binary that cannot run here
        // cannot run here whatever this node offers.
        if req.isa != IsaSet::NONE && !n.isa.satisfies(&req.isa) {
            return None;
        }
        let mut best: Option<(Capability, Resolution)> = None;
        for c in self.capabilities(id) {
            if c.class != req.class {
                continue;
            }
            if req.detail_mask != 0 && (c.detail & req.detail_mask) != req.detail_mask {
                continue;
            }
            // An offload below its own threshold is not a candidate: using it would be
            // slower than not having it (docs/RESOURCE-GRAPH.md 2.6).
            if matches!(c.reach, Reach::Offload { min_bytes, .. } if req.bytes < min_bytes) {
                continue;
            }
            let r = resolution_of(&c);
            // A numerically-different path is refused under a bit-exact contract rather than
            // ranked down (docs/CPU-FEATURES.md 2.2 - `fma(a,b,c)` is not `a*b` then `+c`).
            if req.bit_exact && r == Resolution::Numeric {
                continue;
            }
            if best.map(|(_, br)| rank(r) > rank(br)).unwrap_or(true) {
                best = Some((c, r));
            }
        }
        best
    }

    /// **All** providers of `req`, in node order - the aggregation primitive.
    ///
    /// This is the correct call for entropy: **several RNGs feed one DRBG**, because
    /// SP 800-90B/C conditions multiple sources together so one degraded or compromised
    /// source cannot determine the output. A design offering only "the best RNG" would make
    /// the seed depend on a single source, which is the property the standard exists to
    /// prevent. `nearest` is for the offload cases; neither is the default.
    pub fn providers<'a>(&'a self, req: &'a Request) -> impl Iterator<Item = NodeId> + 'a {
        (0..self.nodes.capacity()).filter_map(move |i| {
            let id = NodeId(i as u16);
            self.get(id)?;
            self.select(id, req).map(|_| id)
        })
    }

    /// **Phase 2**: of the nodes that can satisfy `req`, the best reachable from `from` by
    /// `by`.
    ///
    /// Refuses `MetricMeaningless` for a class whose trust model is not a cost - see the
    /// module docs. Returns `AllFiltered` when providers exist but none survived Phase 1,
    /// which is the answer that says "relax the request" rather than "add hardware".
    pub fn nearest(
        &self,
        from: NodeId,
        req: &Request,
        by: Metric,
    ) -> Result<(NodeId, Resolution), QueryError> {
        if self.get(from).is_none() {
            return Err(QueryError::NoNode);
        }
        // **Refused by CLASS, not by the trust field.** The first version checked each
        // candidate's `trust` and refused when it found a non-`Performance` one - which made
        // rankability depend on a publisher labelling its data correctly. A driver cell that
        // published an entropy source with `Trust::Performance` would have made entropy
        // *rankable by throughput*, which is the exact regression this refusal exists to
        // prevent. The fuzzer found it on a random topology (seed 42405). The class decides.
        if !Self::is_cost_ranked(req.class) {
            return Err(QueryError::MetricMeaningless);
        }
        let mut any_provider = false;
        let mut best: Option<(NodeId, Resolution, u64)> = None;
        for i in 0..self.nodes.capacity() {
            let id = NodeId(i as u16);
            if self.get(id).is_none() {
                continue;
            }
            if self.capabilities(id).any(|c| c.class == req.class) {
                any_provider = true;
            }
            // Phase 1 **filters**; it does not dominate Phase 2. The first version ranked by
            // resolution first, which meant a bit-exact provider 10,000x slower beat a
            // numerically-different one even when the caller had *not* asked for
            // bit-exactness - preferring exactness the caller did not request over a real
            // speedup. Once a candidate survives the filter it is comparable; `bit_exact` is
            // what removes `Numeric` candidates, and it does so in `resolve`.
            let Some((cap, r)) = self.select(id, req) else {
                continue;
            };
            let Some(edge) = self.cost(from, id) else {
                continue;
            };
            let eff = effective(&cap, &edge, by);
            let better = match best {
                None => true,
                Some((_, _, b)) => match by {
                    // Bandwidth: larger is better. Everything else: smaller.
                    Metric::Bandwidth => eff > b,
                    _ => eff < b,
                },
            };
            if better {
                best = Some((id, r, eff));
            }
        }
        match best {
            Some((id, r, _)) => Ok((id, r)),
            None if any_provider => Err(QueryError::AllFiltered),
            None => Err(QueryError::NoProvider),
        }
    }

    // ---------------------------------------------------------------- per-class queries
    //
    // **Each capability class has its own handling of its own data**, and that is the point
    // of [`Trust`] rather than a decoration on it. A single generic query would have to pick
    // one semantics and would then be wrong for every class that does not share it:
    //
    // | class | the question that makes sense | wrong question |
    // |---|---|---|
    // | Matmul, Crypto, Compress, Codec, Dma | which is cheapest by *this* metric | - |
    // | Entropy | **all** of them, combined | which is fastest |
    // | Attest | which chain does it root | which is nearest |
    // | Timestamp | which is most accurate | which is fastest |
    // | ZonedPlacement | which zones are writable now | which is nearest |
    //
    // So [`Graph::nearest`] is **the Performance-class query** - it refuses any other class
    // by construction - and the accessors below are the typed ones. Adding a class means
    // deciding which of these it is, which is exactly the decision that should not be
    // implicit.

    /// Every entropy provider, with its assessed quality - the input to seeding a DRBG from
    /// several sources.
    ///
    /// Returned as a list rather than a choice, and the caller conditions them together. A
    /// source with `health_tested == false` is included and *labelled*, not silently
    /// dropped: SP 800-90C combining permits an unassessed source to contribute as long as
    /// it cannot be the only contributor, and dropping it here would remove diversity the
    /// standard wants.
    pub fn entropy_sources(&self) -> impl Iterator<Item = (NodeId, Capability)> + '_ {
        (0..self.nodes.capacity()).flat_map(move |i| {
            let id = NodeId(i as u16);
            self.capabilities(id)
                .filter(|c| c.class == CapClass::Entropy)
                .map(move |c| (id, c))
        })
    }

    /// Trust roots for measurement chain `chain`, or all of them when `chain` is 0.
    ///
    /// **Not ranked**, because a TPM's usefulness is which chain it roots, not how near it
    /// is - which ties it to docs/IDENTITY.md's `PrincipalId` and the attest-by-measurement
    /// engine story rather than to any cost metric.
    pub fn attest_roots(&self, chain: u32) -> impl Iterator<Item = (NodeId, u32)> + '_ {
        (0..self.nodes.capacity()).flat_map(move |i| {
            let id = NodeId(i as u16);
            self.capabilities(id).filter_map(move |c| match c.trust {
                Trust::Attestation { chain: got }
                    if c.class == CapClass::Attest && (chain == 0 || got == chain) =>
                {
                    Some((id, got))
                }
                _ => None,
            })
        })
    }

    /// The most accurate clock source - `detail` carries accuracy in nanoseconds, smaller
    /// being better.
    ///
    /// A clock is not chosen by latency: a fast reading of a drifting counter is worse than a
    /// slower reading of a disciplined one, the same shape as the entropy rule. This tree
    /// already takes that position where it matters - the NTP client's answer is a bounded
    /// *interval* rather than a point (docs/NETSTACK.md 20).
    pub fn best_clock(&self) -> Option<(NodeId, Capability)> {
        let mut best: Option<(NodeId, Capability)> = None;
        for i in 0..self.nodes.capacity() {
            let id = NodeId(i as u16);
            for c in self.capabilities(id) {
                if c.class != CapClass::Timestamp {
                    continue;
                }
                if best.map(|(_, b)| c.detail < b.detail).unwrap_or(true) {
                    best = Some((id, c));
                }
            }
        }
        best
    }

    /// Storage offering host-managed placement (ZNS/FDP, docs/GREENFIELD.md 2.8).
    ///
    /// A set rather than a choice: a log-structured writer places *across* zones
    /// deliberately, so "the nearest zone" is not the question.
    pub fn placement_targets(&self) -> impl Iterator<Item = (NodeId, Capability)> + '_ {
        (0..self.nodes.capacity()).flat_map(move |i| {
            let id = NodeId(i as u16);
            self.capabilities(id)
                .filter(|c| c.class == CapClass::ZonedPlacement)
                .map(move |c| (id, c))
        })
    }

    /// Whether [`Graph::nearest`] will rank `class`, so a caller can ask rather than discover
    /// it through a refusal.
    ///
    /// The four exceptions are the four classes whose data has its own handling: entropy is
    /// aggregated, attestation is a chain lookup, a clock is ranked by accuracy, and zoned
    /// placement is a set. Adding a capability class means deciding which of those it is.
    pub fn is_cost_ranked(class: CapClass) -> bool {
        !matches!(
            class,
            CapClass::Entropy | CapClass::Attest | CapClass::Timestamp | CapClass::ZonedPlacement
        )
    }
}

impl Default for Graph {
    fn default() -> Graph {
        Graph::new()
    }
}

/// Which resolution a capability offers. Kept as a free function so the mapping is in one
/// place rather than at each comparison.
fn resolution_of(c: &Capability) -> Resolution {
    // `detail` bit 63 marks a capability the publisher declared as numerically different
    // from the reference (an FMA contraction, a reassociated reduction). Everything else is
    // bit-exact by the publisher's contract.
    if c.detail & (1 << 63) != 0 {
        return Resolution::Numeric;
    }
    match c.reach {
        Reach::Inline => Resolution::Native,
        Reach::Offload { .. } => Resolution::Native,
    }
}

/// Preference order. `Numeric` ranks **below** `Emulated`: a slow exact answer is worth
/// more than a fast different one, which is the whole point of the bit-exact contract.
fn rank(r: Resolution) -> u8 {
    match r {
        Resolution::Native => 4,
        Resolution::Translated => 3,
        Resolution::Emulated => 2,
        Resolution::Numeric => 1,
        Resolution::Unavailable => 0,
    }
}

/// The cost of *using* an already-selected capability: the transport cost **and the work
/// rate together**.
///
/// Ranking by the edge alone made every local candidate win unconditionally, because
/// `Cost::LOCAL` reports `u32::MAX` bandwidth - so a CPU doing 1 GB/s beat a NIC engine doing
/// 100 GB/s across a 50 GB/s link. The edge is the cost of *reaching* a provider; `rate` is
/// how fast it then does the work, and a ranking that ignores the second cannot express the
/// inline-versus-offload crossover this model exists for. Found by the fuzzer's threshold
/// property before any of this ran.
///
/// Bandwidth composes as a **minimum** - a chain is as fast as its narrowest part. Latency
/// composes as a **sum** - transport plus doorbell. Hops and energy are properties of the
/// path only.
///
/// Takes the capability rather than re-deriving it, which is the whole point of
/// [`Graph::select`]: this function must not be a second copy of the filter.
fn effective(cap: &Capability, edge: &Cost, by: Metric) -> u64 {
    let doorbell = match cap.reach {
        Reach::Inline => 0u64,
        Reach::Offload { doorbell_ns, .. } => doorbell_ns as u64,
    };
    match by {
        Metric::Bandwidth => (edge.bandwidth_mbs as u64).min(cap.rate as u64),
        Metric::Latency => (edge.latency_ns as u64).saturating_add(doorbell),
        Metric::Hops => edge.hops as u64,
        Metric::Energy => edge.energy as u64,
    }
}

/// The kernel's graph. Filled at boot by [`super::graph_build::build`].
static mut GRAPH: Graph = Graph::new();

/// # Safety
/// Built once at boot before any secondary runs and read-only afterwards, so there is no
/// lock on the read path. A caller that mutates it after boot breaks that.
#[allow(clippy::mut_from_ref)]
pub unsafe fn graph() -> &'static mut Graph {
    // SAFETY: the caller's contract, above.
    unsafe { &mut *core::ptr::addr_of_mut!(GRAPH) }
}
