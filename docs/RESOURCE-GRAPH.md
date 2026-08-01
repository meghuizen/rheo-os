# The resource graph: one model of the machine, queried rather than hardcoded

**Status:** the model and its **discovery** are built; most **consumers** are not. Built and
proven on all three ISAs: the graph itself (nodes, cost vectors, the two-phase filter-then-rank
query, host-model-checked in `verify/graph/`), memory localities, node-to-node distances, HMAT
latency and bandwidth, the per-CPU core and cache topology (section 2.4a), and the `/sys`
rendering an unmodified topology-aware binary reads (section 6.3). Not built: the consumers in
section 6.4 - a steal that prefers its cache domain, memory placed by purpose,
`librheo::graph` - plus per-CPU feature divergence, core classes and device proximity. Every
section still names what would count as evidence, and says which of it exists.

The claim: this kernel knows *what* hardware exists and almost nothing about *how it
relates*. Placement decisions are therefore written as constants and heuristics, which is
tolerable on one socket and wrong on a real server, and unexpressible across hosts.

---

## 1. What exists, and the exact shape of the gap

`hw::Inventory` is real and populated on all three ISAs: CPU count and features, the typed
physical memory map, NUMA memory regions with CPU affinities, and PCIe enumeration
classifying each function into an engine kind. The **data** is there. What is missing is
**structure and cost**:

| Question a placement decision asks | Answerable today? |
|---|---|
| Which memory node is this frame on? | **Yes** - `frames::node_of` (docs/SUBSTRATE.md pillar 6) |
| Is this CPU on the same node as that memory? | **Yes** - SRAT CPU affinities |
| How much *further* is node 1 from node 0 than node 0 is from itself? | **Yes** - SLIT (x86-64) / `numa-distance-map-v1` (riscv64) |
| What bandwidth and latency does this initiator see to that target? | **Yes** on x86-64 - HMAT; 0 = unknown elsewhere, never derived from the distance |
| Which node is this NIC's DMA on? | **No.** A device's proximity domain (`_PXM`) is not read |
| Do these two CPUs share an LLC? Are they SMT siblings? | **Yes** - `graph::siblings(cpu, Cache \| Core)`, section 2.4a |
| Which engine is nearest to the memory holding my tiles? | **No** |
| Is that GPU local or on another host? | **Not expressible** |

The consequences are concrete, not theoretical:

- **Device DMA is placed blind.** A driver allocates its queues and bounce frames from the
  pool with no reference to the device's node, so on a two-socket machine half of them are
  remote by chance. The NVMe driver already partitions queues *per core*; it cannot
  partition them *per node* because nothing says which node the controller is on.
- **Interference decisions are impossible.** Caladan-style core reallocation
  (docs/GREENFIELD.md 2.2) needs to know which cores share an LLC and which are SMT
  siblings. Without that, "give this cell another core" cannot distinguish a core that adds
  throughput from one that steals cache from its own sibling.
- **The distance the NUMA work already needed is faked.** `alloc_on(node)` falls back to
  the pool at large when a node is dry and counts the fallback - correct - but "fall back to
  the *nearest* node" is not expressible, so a two-node fallback and a
  eight-node-across-two-sockets fallback are the same code.
- **P/E/LP core classes have nowhere to live.** docs/EXECUTION-MODEL.md 2.1 says core
  classes and accelerator engines are one taxonomy; that taxonomy needs a graph to be a
  taxonomy of.

---

## 2. The design

### 2.1 Typed nodes

```
  Host        (id, reachable-via)          - one for a single machine; more for a cluster
   |
   +- MemoryNode   (id, kind, size)        - kind: DDR | HBM | PMEM | CXL | Remote
   +- Cpu          (id, class, node)       - class: P | E | LP
   |    +- shares: LlcDomain, SmtSet       - membership, not a separate node kind
   +- Device       (bdf, kind, node)       - NIC | Nvme | Gpu | Npu | Accel | Other
   +- Engine       (id, kind, node)        - an executor: a CPU set, a GPU, an NPU
   +- Link         (kind, endpoints)       - PcieRoot | Cxl | Interconnect | Network
```

Two decisions worth defending. **`Engine` is separate from `Device`** because a GPU is both
- a device you configure and an executor you place work on - and conflating them is what
makes accelerators need their own scheduler path instead of riding the one every entity
uses. And **LLC/SMT membership is an attribute, not a node**, because it is a set the graph
is asked about, not a thing work is placed on.

### 2.2 Edges carry a cost **vector**, not a distance

A scalar distance cannot express the machine anyone actually has:

```rust
pub struct Cost {
    /// Access latency, nanoseconds. HMAT gives it; SLIT approximates it.
    pub latency_ns: u32,
    /// Achievable bandwidth, MB/s. The number HBM and CXL differ on.
    pub bandwidth_mbs: u32,
    /// Topological hops - the only field a device tree can always supply.
    pub hops: u8,
    /// Relative energy per access, if POWER.md's policy has anything to say.
    pub energy: u8,
}
```

HBM is *lower latency and much higher bandwidth* than DDR; CXL-attached memory is *similar
bandwidth and much higher latency*; a remote host is worse in both by orders of magnitude.
A single number forces a policy choice into the data, and different callers want different
answers - a tile GEMM is bandwidth-bound, an RPC path is latency-bound. So the graph
reports the vector and **the caller picks the field it cares about**.

`hops` exists because it is the one component every firmware source can supply, so a
degraded graph is still a usable graph.

### 2.3 Queried, not hardcoded - and no solver

```rust
pub fn cost(from: NodeId, to: NodeId) -> Option<Cost>;
pub fn nearest(from: NodeId, want: Kind, by: Metric) -> Option<NodeId>;
pub fn members(set: SetId) -> impl Iterator<Item = NodeId>;   // LLC domain, SMT set
pub fn node_of(dev: Bdf) -> Option<NodeId>;
```

Barrelfish's system knowledge base is the precedent and docs/GREENFIELD.md 2.10 records the
judgement: **adopt the queryable model, refuse the constraint solver.** A solver is
unbounded work in the one component whose discipline is bounded work and no allocation.
`nearest` is a bounded scan of a small graph; anything wanting search runs in a cell.

The graph is **funded metadata** (docs/SUBSTRATE.md pillar 1), built once at boot after
`hw::detect`, charged to the kernel, and immutable afterwards except for hot-plug - which
means no lock on the read path.

### 2.4 Where the data comes from

| Source | x86-64 | ARM64 | RISC-V |
|---|---|---|---|
| Memory nodes | ACPI SRAT (**have**) | none - no firmware table on a bare-ELF `virt` boot | device tree (**have**) |
| Node-to-node distance | ACPI **SLIT** | - | DT `numa-distance-map-v1` |
| Initiator/target bandwidth + latency | ACPI **HMAT** | - | - |
| Device proximity | ACPI `_PXM` | - | DT `numa-node-id` on the node |
| LLC domains, SMT sets | CPUID leaf `0x0B` + leaf 4 (**have**) | `MPIDR` affinity + MT bit (**have**) | DT `cpu-map` (**have**) |
| Core class (P/E/LP) | CPUID leaf 0x1A | DT `capacity-dmips-mhz` | DT `capacity-dmips-mhz` |
| PCIe topology | ECAM walk (**have**) | ECAM walk (**have**) | ECAM walk (**have**) |

ARM64's column is mostly empty and that is a **measured** fact, not an omission: QEMU hands
a bare-ELF `-kernel` boot on `virt` no device-tree pointer, checked rather than assumed
(`-dtb` does not reach it either), which is why the existing `numa` test skips there with a
reason. So the graph must be **useful when degraded**: with one node and no distances it
answers "everything is equally near", which is exactly correct for that machine, and
`graph_source()` reports what it was built from so no proof can claim distances it does not
have.

### 2.4a CPU topology: which CPUs share a core, which share a cache

Two questions the scheduler asks for two different reasons, and until this landed the answer
was nothing at all - `CpuInfo` carried a hardware id and a memory node, so every CPU looked
equidistant from every other:

- **"Whose run queue is cheap for me to steal from?"** A task whose working set is in a cache
  we share does not have to be fetched again. This is the input section 6.4b's steal order
  needs.
- **"Who am I contending with?"** Two threads of one core share execution resources, so
  spreading two compute-bound entities across cores beats packing them onto one.

`CpuInfo` gains `core_id` and `llc_id`, and the inventory gains a `TopoSource` saying where
they came from. **Both ids default to a sentinel, not to 0**: an undiscovered topology reading
`core_id = 0` everywhere would tell a scheduler that every CPU is a thread of one core, which
is worse than telling it nothing (docs/ENGINEERING.md 11).

**Two sources, in a fixed order.** A device tree's `cpu-map` first, because it is a
*statement* by firmware rather than a decoding of ids; failing that, the architectural rule
for taking a hardware id apart. Each ISA lands somewhere different, and the differences are
the interesting part:

- **x86-64** - CPUID leaf `0x0B` describes the hierarchy as shift widths: the SMT level's
  shift leaves the core id, the Core level's leaves the package id. Leaf 4 refines the cache
  boundary where it exists, which matters on parts whose last-level cache is *narrower* than a
  package (an AMD core complex, sub-NUMA clustering). Where leaf 4 says nothing the package is
  the fallback, and that direction is the safe one: it can merge two cache domains into one,
  costing a heuristic some locality, where splitting one would claim two CPUs do not share a
  cache when they do.
- **ARM64** - `MPIDR_EL1` is a stack of 8-bit affinity fields and its `MT` bit (24) says what
  the bottom one means: set, affinity 0 is a thread and affinity 1 the core; clear, affinity 0
  *is* the core. A read-only id register at EL1, which is what makes it usable here at all -
  this is the ISA that gets handed no device tree. The **cluster is taken as the cache domain
  and that is an inference**: ARM64 exposes no register saying who shares a cache
  (`CLIDR_EL1`/`CCSIDR_EL1` describe a cache's geometry, not its sharers), so the affinity
  hierarchy is the only architectural evidence, and a cluster is the level that shares an
  L2/L3 in Arm's own topology. Labelled `Architectural`, never `DeviceTree`, for exactly that
  reason.
- **RISC-V** - a hart id is opaque: the privileged spec says nothing about it naming a thread,
  a core or a cluster, and there is no sharing register. So `cpu_topology_bits` returns `None`
  there, written out rather than omitted because the absence *is* the finding - a caller that
  assumed a decomposition would group four independent harts into one core. The device tree's
  `cpu-map` is the whole source, read in its own walk because an entry names a CPU by
  **phandle** and a phandle is only resolvable once every `cpu` node has been seen (the
  specification does not order `cpu-map` after them, so depending on QEMU's ordering would be
  depending on a coincidence).

**In the graph it is nesting, not a second field.** A `Cache` node holds `Core` nodes, which
hold the `Cpu` nodes that are their threads, all through the one `set` field a node already
had - one field because that is the shape of the hardware, and a second membership field would
let two of them disagree. `graph::siblings(cpu, level)` is the query a consumer uses, and it
takes the level rather than existing twice: `Cache` answers the steal question, `Core` the
contention one. An undiscovered topology builds **no** `Cache` and no `Core` node, so
`siblings` answers empty and a scheduler falls back to whatever it does without the
information. Inventing one cache domain over every CPU would be the convenient lie: it looks
like a small machine and is indistinguishable from a discovered answer.

**The proof, and what it can and cannot reach.** `hwinfo` runs with
`-smp 4,sockets=1,cores=2,threads=2` - four CPUs, two SMT pairs, one cache domain - and those
numbers are the oracle. The base launch is a flat `-smp 4`, in which a correct discovery and a
broken one both say "four cores", so it could prove nothing. The cache claim is asserted on
all three ISAs; the SMT claim only on x86-64, because **QEMU cannot express threads to a guest
on the other two**, read out of its source rather than inferred from this kernel's output:
`arm_build_mp_affinity(idx, clustersz)` is `(idx / clustersz) << 8 | (idx % clustersz)`, purely
index-based with no MT bit and no thread field, and `hw/riscv/virt.c` emits
`cpu-map/cluster<socket>/core<hart>` with no `thread` nodes at all. Asserting two cores
everywhere would fail on two ISAs for a reason that has nothing to do with the code under
test, so those two assert what their platform genuinely describes - four independent cores -
and say why.

Five controls observed firing: the x86 package fallback removed (2 cache domains where the
launch declares 1 - this was a **real defect found this way**, since QEMU's TCG `-cpu max`
returns all zeros for every leaf-4 subleaf, measured with a debug print, and a first version
using leaf 4 alone reported a wrong grouping while labelling it `Architectural`); the
`cpu-map` walk removed (riscv64 reports no topology); the MPIDR rule removed (ARM64 reports
no topology); the CPU-to-core membership skipped (a CPU shares its cache with 0 CPUs); and, on
the host, the two graph levels collapsed into one (4 core siblings where there are 2). The
host driver also checks the three properties that make the nesting a **partition** -
reflexive, symmetric, and core-siblings a subset of cache-siblings - on shapes QEMU cannot be
asked for.

Still absent, and named rather than approximated: **core classes** (P/E/LP) and **per-CPU
feature divergence**, which are the other two rows of the table above and the prerequisite for
section 6.4d.

### 2.5 Capability sets, and why heterogeneity makes them first-class

A cost tells you where something is *best* run. It does not tell you where it *can* run,
and the moment a cluster mixes architectures or generations those are different questions.
So every executor node carries a capability set:

```rust
pub struct IsaSet {
    pub arch: Arch,          // X86_64 | Aarch64 | Riscv64
    pub baseline: u8,        // x86-64-v1..v4, ARMv8.0..9.x, RVA20/22/23
    pub features: FeatureMask,  // avx512f, vnni, amx, sve2, rvv, aes, ...
}

pub struct EngineCaps {
    pub kind: EngineKind,    // Gpu | Npu | Accel | Cpu
    pub isa_version: u32,    // an SM level, a GFX target, an NPU generation
    pub tile_shapes: ...,    // what the TileContract declares (docs/TILES.md)
    pub mem_bytes: u64,
}
```

Two things this makes expressible that nothing currently does:

- **A binary compiled for x86-64-v4 cannot run on a v3 node.** In a mixed-generation
  cluster that is not a performance question, it is a `SIGILL`. The graph must be able to
  answer it *before* placement, not after the fault.
- **A GPU kernel built for one ISA version will not load on another.** NVIDIA SM levels,
  AMD GFX targets and NPU generations are not interchangeable, and a "nearest GPU" query
  that ignores that returns a device the work cannot use.

`baseline` is separate from `features` deliberately: a baseline is a *contract a compiler
was given*, and the useful question is almost always "does this node meet the baseline my
binary was built for", not "does it have each of thirty flags".

### 2.6 Capabilities, not device kinds - and the taxonomy trap

The list of things worth placing work on does not stop at CPUs and GPUs. It includes
hardware RNGs, a TPM, TPUs and NPUs, integrated accelerators (APUs), the FPU/SIMD units
inside every core, and - the case that breaks a taxonomy outright - **the extra engines
sitting on a NIC**: inline IPsec/TLS crypto, compression, DMA engines, a precision-time
clock, RDMA, packet-processing pipelines, and on a DPU a set of embedded cores. Computational
storage puts compute on the SSD; a GPU carries video encoders and separate copy engines.

**So the graph must not enumerate device kinds.** A fixed `EngineKind` enum is a list of
what someone thought of, and every one of the resources above is a thing it did not think
of. `hw::Inventory` classifies PCI functions into engine kinds today, and that is exactly the
shape to stop extending. Instead, a node **offers capabilities**:

```rust
pub struct Capability {
    pub class: CapClass,     // Matmul | Crypto(alg) | Compress(alg) | Entropy | Attest |
                             // Timestamp | Dma | Codec | PacketPipeline | ...
    pub reach: Reach,        // Inline | Offload { queue_depth, doorbell_ns, min_bytes }
    pub rate: u32,           // units per second, or 0 for "not a throughput resource"
    pub trust: Trust,        // see below - NOT everything is ranked by speed
    pub detail: u64,         // class-specific: tile shapes, key sizes, clock accuracy
}
```

The precedent is already in the tree and working: docs/TILES.md defines an engine as
**anything that declares a TileContract**, and one tile program runs on whichever engines
exist. Section 2.6 is that idea generalised from matmul to every capability class.

#### Inline versus offload is a real structural split, not a label

- **Inline** means "available to code executing on this node": the FPU, SIMD, AES-NI, a
  CPU's RDRAND. No queue, no DMA, no edge - so the graph carries no cost edge for it, and
  the capability is a property of the *execution context* rather than a place to send work.
  This is why the FPU belongs in the list at all: it explains why some capabilities are
  attributes of a node and others are destinations.
- **Offload** means "reached by submitting work across an edge": a GPU, an NPU, a NIC's
  crypto engine, virtio-crypto. It has a queue depth, a doorbell latency, a DMA path that
  needs an IOMMU domain, and - the field that matters most and is usually missing -
  **`min_bytes`, the size below which inline wins.**

That threshold is the whole practical difference. A NIC's inline crypto engine beats AES-NI
only above some payload size; below it, the doorbell and DMA cost more than the cipher. A
graph that reports "this node can do AES-GCM" without the crossover will confidently make
work slower, which is worse than not knowing - it is the *bandwidth-versus-latency* mistake
of section 2.2 repeated one level down. So `min_bytes` is part of the capability, and the
Phase-1 filter drops an offload candidate whose threshold the request does not clear.

#### Not everything is ranked by speed

```rust
pub enum Trust {
    /// Ranked by rate. A codec, a DMA engine, a matmul unit.
    Performance,
    /// Ranked by *evidence*: an entropy source that passes SP 800-90B health tests
    /// outranks a faster one that does not.
    Entropy { health_tested: bool, source: EntropySource },
    /// Not comparable by cost at all: which measurement chain does this root?
    Attestation { chain: PrincipalId },
}
```

Two of the user's examples are precisely the reason this field exists:

- **A hardware RNG must not be ranked by throughput.** A fast source of unknown quality is
  worse than a slow one with evidence behind it, and this tree already holds that position:
  the per-cell DRBG seeds from RDSEED/RDRAND/RNDR **after SP 800-90B health tests**, is
  non-blocking, and falls back to a documented floor where no hwrng exists
  (docs/TIME-IDENTITY.md). Putting `Entropy` in the graph must not undo that by making a
  device RNG selectable because it is faster.
- **A TPM has no meaningful latency ranking.** It is not a throughput resource; it is a
  *trust root*. The only useful query is "which TPM roots the measurement chain this
  principal's attestation refers to", which ties it to docs/IDENTITY.md's `PrincipalId` and
  to the attest-by-measurement engine story rather than to any cost metric. A graph that
  offered `nearest(Attest, Metric::Latency)` would be answering a question nobody should
  ask.

#### Who publishes a capability - and why it is not the kernel

The kernel must not learn every vendor's offload registers; that is the taxonomy trap with
extra steps, and it would undo the driver-free property `svc::Bridge` exists to protect
(docs/ARCHITECTURE-DEBT.md 3.2 - the queue's opcode dispatch names no device driver).

So: **a driver cell publishes its device's capabilities into the graph**, declaring a
contract, exactly as an engine declares a `TileContract`. The kernel's own contribution is
what firmware and architectural discovery can tell it - PCIe class codes (0x10
encryption/decryption controller, 0x12 processing accelerator - both already recognised),
capability structures, ACPI and device-tree nodes - and everything vendor-specific arrives
from the cell that drives it. That also makes a **remote** capability expressible with no
new mechanism: a driver cell on another host publishes into the same graph with a network
edge's cost.

#### What is gateable here, checked against the built QEMU 11.0.3

| Capability | Provable in this container? |
|---|---|
| **Entropy, inline** (RDRAND/RDSEED/RNDR) | **Yes, already** - the DRBG seeding path with its health tests |
| **Entropy, offload** (`virtio-rng`) | **Yes** - QEMU models `virtio-rng-pci` |
| **Crypto offload** (`virtio-crypto`) | **Yes** - QEMU models `virtio-crypto-pci`, a genuine queue-based offload engine, which makes the `min_bytes` crossover measurable in *icount* rather than only on hardware |
| **Attestation / TPM** | **Yes, with work** - QEMU supports TPM; it is absent from *our* build only because this repository's configure passes `--disable-tpm`, and `swtpm` is not installed. A build flag and a package, not a QEMU limitation - stated precisely so nobody records it as impossible |
| **Matmul, inline** (AVX-512/VNNI) | **Yes, already** - host-proven bit-exact (docs/CPU-FEATURES.md 2.4a) |
| **TPU / NPU / APU** | **No model.** PCI class 0x12 is recognised and registered; nothing exists to drive |
| **NIC offload engines** | **No model.** Vendor-specific; `virtio-crypto` is the generic stand-in for the *shape*, and a real one is hardware-gated |

`virtio-crypto` is the interesting entry: it gives the offload path a real device with a real
queue, so the inline-versus-offload crossover can be established as a *path-length*
measurement here and re-measured on hardware later - rather than being asserted as a
constant, which is how such thresholds usually rot.

---

## 3. Placement is two phases, and the order matters

This is the part heterogeneity forces, and it is where the graph meets
docs/CPU-FEATURES.md 2:

```
   want: an int8 GEMM, 64x64x64, on a tile of 4 MiB
      |
   PHASE 1 - FILTER by capability.  Which nodes can run this AT ALL?
      |         each candidate resolves to one of:
      |           Native      - it has the instruction
      |           Translated  - a different sequence, SAME BITS (cost penalty applies)
      |           Emulated    - portable scalar, same bits, large penalty
      |           Unavailable - drop the candidate
      v
   PHASE 2 - RANK the survivors by cost, on the metric the CALLER named
      |         (bandwidth for a GEMM, latency for an RPC, energy for background work),
      |         with the resolution penalty folded into the cost
      v
   place
```

Filter first, then rank - never one combined score. A combined score can trade correctness
for proximity, which is how you get work placed on a node where it produces a *different
answer* or does not run at all. And the resolution outcome is not binary because
CPU-FEATURES.md 2.2 already establishes that a translation may be **bit-exact** or merely
**numerically similar**: a caller under a bit-exact contract - which every tile kernel is,
and which is what makes the FlashAttention proofs equalities - must have `Numeric`
candidates filtered *out*, not ranked down. So the filter takes the caller's contract as an
input.

The payoff is that "run this where it fits best" becomes one query over one graph whether
the candidates are two sockets, a CPU and a GPU, or two hosts of different generations.

---

## 4. What a driver does with it

This is the point, and it connects docs/DRIVERS.md D2 directly. A driver cell asks:

1. **Where is my device?** `node_of(bdf)` - so queues, descriptors and bounce frames are
   allocated with `alloc_on(that node)` instead of from the pool at large.
2. **Which cores should serve it?** `members(llc_of(node))` - so a per-core queue pair is
   created for the cores that can reach the device cheaply, and MSI-X vectors are routed to
   those cores rather than to whichever index happens to be next.
3. **Which memory should the client's buffers be in?** So a zero-copy grant handed to the
   NIC is not on the far node, which is the case where zero-copy stops being a win.

None of that needs a new kernel object or verb. It is a query over data the kernel already
collects, plus placement calls that already exist (`frames::alloc_on`, per-queue MSI-X
routing, `claim_vcore`).

## 4. The cluster and remote-resource generalisation

An HPC cluster is the same graph with more `Host` nodes and edges whose `latency_ns` is
three or four orders of magnitude larger. That is not a metaphor - it is the reason to build
a cost *vector* now rather than a distance:

- `nearest(from, Kind::Engine, Metric::Bandwidth)` answers "which GPU should run this" with
  one implementation, whether the answer is on this socket, the next socket, or another
  host. The *caller* does not branch on locality; the cost does.
- A remote resource needs a transport and an identity, both of which exist in design:
  docs/CLUSTER.md for the topology, docs/IDENTITY.md for the `PrincipalId`, and the N3b/N5a
  stack for the wire. Arcan's **A12** is the same idea for display specifically - the same
  client semantics over a network rather than a tunnel - and it belongs here as an edge with
  a cost, not as a second protocol.
- **Deliberately not designed here:** cache coherence across hosts, remote memory
  *transparently* mapped, and migration of a running entity between hosts. The first is a
  hardware property, the second is a lie that leads to unpredictable latency, and the third
  is not even done between cores yet (docs/SMP.md 10.0).

## 5. The gate - and it is provable here

Unusually for a topology feature, this does **not** need hardware, and that moves it up the
list. QEMU accepts both of the launches the proof needs, checked against the built
QEMU 11.0.3:

```
  -numa dist,src=0,dst=1,val=20            # SLIT: a real, asserted distance
  -machine q35,hmat=on -numa hmat-lb,...   # HMAT: bandwidth and latency
```

So the proof is the shape the existing `numa` test already uses - **assert against an oracle
the launch names, never against the code's own tables**:

1. Launch two nodes with a *stated* distance of 20 and read it back through
   `graph::cost(0, 1).hops` / `.latency_ns`, asserted equal to what the launch declared,
   and node-to-self asserted at the SLIT-defined local value of 10.
2. Launch with HMAT bandwidth and latency and assert the same for those fields.
3. Attach a device to node 1 and assert `node_of(bdf) == 1`, then assert that a driver
   allocating with the graph's answer lands its frames on node 1 - which
   `frames::node_of` can already check independently.
4. **The degraded case, asserted rather than assumed:** the same kernel with one node and no
   SLIT must report `graph_source()` honestly and answer every `cost` query as equal, and
   the single-node behaviour must be **unchanged** - the rule the NUMA work already follows,
   so that "topology landed" never quietly alters a machine that has none.

A control exists for each: an inverted distance table, a device's `_PXM` ignored, and a
driver allocating from the pool at large should each fail a named assertion.

## 6. The consumers: kernel, libraries, and POSIX translation

A machine model that only the kernel can see is half a model. Three consumers, and the
third is the one that makes existing software work.

### 6.1 The kernel

- `frames::alloc_on(node)` exists; `nearest(node, MemoryNode, Latency)` is what makes its
  *fallback* a choice rather than "the pool at large".
- The entity's `node` and `core_class` fields already exist in the hot line
  (docs/EXECUTION-MODEL.md 4.1) and are currently set by a round-robin. Phase 1/Phase 2
  above is what sets them from evidence.
- Driver queue, bounce-frame and MSI-X placement (section 4).
- The `sched::dispatch` seam is unchanged: the graph informs *where*, the ready queue still
  decides *when*. Keeping those apart is why the queue has produced no defects.

### 6.2 The libraries

`librheo` gets a `graph` module - read-only queries over what the kernel published, no new
verb, the `SYS_ENGINE_INFO` / `SYS_VCORE_INFO` shape. It replaces per-library probing:
`tile::simd` currently probes CPUID itself, `compute` asks `Engine::info`, and `mem`
takes a node hint - three components each discovering a slice of one model. With the graph
they *ask*, which is also what lets a tile program pick a lowering for a **remote** engine
it cannot execute a CPUID instruction on.

### 6.3 POSIX translation - and this is the biggest practical unlock

The POSIX/Linux surface for topology is **`/sys` plus a few syscalls**, and the de facto
consumer is **hwloc** - which OpenMP, MPI, and essentially every HPC runtime and batch
scheduler sits on. Synthesize that surface from the graph and unmodified HPC software gets a
correct machine view with no port:

| Surface | Fed by |
|---|---|
| `/sys/devices/system/cpu/online`, `present`, `possible` | the graph's CPU set - **done** |
| `/sys/devices/system/cpu/cpuN/topology/{core_id,physical_package_id,thread_siblings_list}` | SMT sets and packages - **done** |
| `/sys/devices/system/cpu/cpuN/cache/indexN/{level,size,shared_cpu_list}` | LLC domains |
| `/sys/devices/system/node/nodeN/{cpulist,meminfo,distance}` | **`distance` is SLIT** - the file libnuma and hwloc read - **done** |
| `/sys/devices/system/node/nodeN/hmem_attrs/…` | HMAT bandwidth and latency |
| `/sys/bus/pci/devices/*/numa_node` | a device's proximity domain |
| `/proc/cpuinfo` flags | the per-node `IsaSet` |
| `getcpu`, `sched_getaffinity`, `sysconf(_SC_NPROCESSORS_ONLN)` | the graph + the entity's owner |
| `mbind`, `set_mempolicy`, `move_pages` | `alloc_on` and the migration path |

**The rule, which this tree has already paid to learn:** every one of those must be
*generated from the graph at open*, never seeded as a static file. `/proc/self/maps` is the
precedent - it is rendered from the cell's real VMA list precisely because "a static `maps`
would be a fabricated memory layout, and a runtime reading it to locate its own code would
be misled rather than refused" (docs/LINUX-COMPAT.md). Topology is the same: a program that
reads a fabricated distance matrix places its data wrongly and reports a benchmark.

**And there is already a live instance of exactly that defect.** `xtask` seeds
`/sys/devices/system/cpu/online` = `0-0` with the justification "one CPU schedules cells;
SMP bring-up runs a second core for bounded work but nothing is dispatched to it". That was
true when written and **is now false**: `linuxsmp` runs four Linux cells across four cores,
`linuxbunsmp` / `linuxnodesmp` / `linuxclaudesmp` run the real runtimes on a secondary, and
`place_cells` dispatches to whichever core is free. So the value is a constant that lies -
the `st_ino = 1` scar restated ("a field left constant is a field that lies",
docs/ENGINEERING.md 11) - and the consequence is not cosmetic: **libuv sizes its thread
pool from the CPU count**, so Node and Bun under-parallelise on a 4-core boot because the
kernel told them there is one core. Fixing it by editing the fixture to `0-3` would be the
same defect with a different constant; it has to come from `smp::online_count()`, which is
the narrow first step toward this whole document.

**Both of those are now built** (docs/LINUX-COMPAT.md, docs/ARCHITECTURE-DEBT.md 7.2), and
they are the first two rows of the table above:

- `online`/`present`/`possible` render from `smp::online_count()`, and the seeded file is
  **removed** from the disk image rather than corrected, so no constant can answer first.
- `cpuN/topology/{core_id,physical_package_id,thread_siblings_list,core_siblings_list}`
  renders from the discovered per-CPU topology (section 2.4a) - the same fields the graph's
  `Cache` and `Core` nodes are built from, so sysfs and the graph cannot disagree, because
  there is one source and two renderings of it.

Two details worth keeping, both of which are the "never seeded" rule doing work:

- **`core_siblings_list` is Linux's *package* and this kernel discovers a *cache domain*.**
  They coincide on every machine without a sub-package cache split; where they differ the
  cache domain is the narrower answer, so a reader that treats it as a package under-shares
  rather than over-shares. Stated at the render site rather than papered over.
- **An unknown topology makes the open fail, not answer.** A defaulted `core_id = 0` would
  tell a program every CPU is one core and it would pack its whole worker set onto what it
  thinks is a single core - strictly worse than the file being absent, which every one of
  these readers already handles by falling back to a flat CPU count.

`cache/indexN/` is still absent and that is deliberate: this kernel discovers who *shares* a
cache, not how big it is (CPUID leaf 4 returns zeros under QEMU's TCG - measured), and a
fabricated `size` is a number a reader would compute a blocking factor from.

**The `node/` half is built too**: `online`, `nodeN/cpulist`, `nodeN/distance` and
`nodeN/meminfo`, from the same localities and distances the graph's `MemoryNode` edges carry.
`distance` is the one that matters - it is how libnuma and hwloc learn a node is *further*
rather than merely different, which is the entire reason SLIT and `numa-distance-map-v1` were
parsed. Two more of the rule's consequences:

- **`MemFree` is absent, not zero.** This kernel tracks free frames per node as an allocator
  *search range*, not as a count, and a fabricated `MemFree` is a number a runtime sizes a heap
  from. A missing field is a signal every reader already handles; a wrong one is not.
- **`hmem_attrs/` is still absent** even though HMAT is parsed, because that directory states
  per-initiator attributes and this kernel reads the SLLBI for memory-side latency and
  bandwidth only. Rendering it would be broader than what was read.

One thing the launch taught, worth keeping: **the same `-numa` line produces three different
machines**, and each ISA's rendering is that machine rather than the line. x86-64 reports two
nodes in one CPU package. riscv64 reports two nodes *and* two cache domains, because QEMU's
`virt` builds one `cpu-map` cluster per NUMA socket rather than from `-smp sockets=`. ARM64
reports one node holding every CPU, because a bare-ELF `virt` boot is handed no firmware table
at all - and that degraded answer is **asserted rather than skipped**, since a graph that is
only correct on the well-described machine is not correct.

### 6.3a A rejection: the cache-domain steal has no victim to choose yet

With `siblings(cpu, Cache)` built, "steal within a cache domain first" looks like the obvious
next slice. It was examined and **refused**, because the cost it would optimise does not exist
in either of this tree's two steal paths:

- **`smp::steal` moves only cells that have not *started***. That is the whole reason it is
  safe (docs/SMP.md 10.0: migrating a *running* entity was attempted twice and reverted
  twice). An unstarted cell has never executed an instruction, so **nothing of it is in the
  victim's cache** - stealing it across a cache domain costs nothing to move, and a preference
  would be optimising a transfer that is not happening. What *does* matter for an unstarted
  cell is which memory node its pages are on, and that is the node-affine claim already built
  (docs/SUBSTRATE.md pillar 6).
- **The strand injector is one shared queue**, not per-vcore deques, so there is no *victim*
  to prefer between: a thief takes the head of the single `TicketLock<VecDeque<_>>`
  (docs/CONCURRENCY.md). Per-vcore deques would create the choice, and that is the change to
  make first - together with a way for a cell to *learn* its cache domain, since the runtime
  is userspace and has no register to read it from.

So the real precondition is **E3/E4**: per-entity run state with migration, at which point a
warm victim exists and the preference has something to be right about. Written down rather
than built, because a preference over unstarted cells would pass a test, count zero crossings,
and mean nothing - the shape docs/ENGINEERING.md 7 calls a stub that reports success.

## 6.4 The graph as backbone: every consumer, and what each one asks

The graph earns its place only if it is *the* answer to "where", the way the queue ABI is
the answer to "how work moves" and the capability model is the answer to "who may". Listed
per consumer, with the query each makes and what is missing before it can make it.

| Consumer | The question it asks the graph | Blocked on |
|---|---|---|
| **Driver framework** (DRIVERS.md D2) | `node_of(bdf)`, `siblings(cpu, Cache)` - place queues, bounce frames and MSI-X vectors near the device | discovery: `_PXM` (the cache half is **done**) |
| **Kernel scheduler** | `cost`, `siblings` - which core to place an entity on, which node its pages want | E2-E4 (discovery is **done**) |
| **Work stealing** | `siblings(victim, Cache)` - **steal within a cache domain first** | **E3/E4, and not the query** - see the rejection below |
| **Introspection** (`cpuinfo`, `lshw`, `hwinfo`) | render the graph | nothing for CPUs; `_PXM` for devices |
| **POSIX translation** | `/sys/devices/system/{cpu,node}`, `getcpu`, `sched_getaffinity` (6.3) | discovery + the synthesis |
| **.NET / Java / Go task schedulers** | nothing new - they read the POSIX surface | 6.3 |
| **Translation and optimisation layer** (CPU-FEATURES.md 2) | `IsaSet` of the *target* node - so a JIT emits for the machine it will run on, not for a baseline | nothing; the `IsaSet` exists |
| **Tile lowering** (TILES.md) | `nearest(Matmul, Bandwidth)` with a bit-exact contract | discovery of engines |
| **Memory placement** | `nearest(from, MemoryNode, metric)` per allocation *purpose* - see below | discovery of HMAT |
| **Cluster / remote** | the same queries with `Host` nodes and expensive edges | the transport (N3b/N5a) |

Two of those deserve their own treatment because they are where the model earns its keep
rather than merely applying.

### 6.4a Which memory for what - the decision the cost vector exists for

A single "nearest memory" answer is wrong for every workload that has more than one kind of
data. With a cost *vector* and a purpose, the answer follows from the data rather than from
a heuristic:

| Purpose | Access shape | Wants | Why |
|---|---|---|---|
| Inference **weights** | read-only, huge, streamed once per layer | highest **bandwidth** near the engine; HBM if it fits, else DDR on the engine's node | bandwidth-bound and read-only, so it is also the ideal candidate for one sealed grant **shared read-only across cells** - several inference cells on one host should not each hold a copy |
| **KV cache** | grows per token, re-read every step | **bandwidth and latency**, near the engine, and **tiered as context grows** | the hot window belongs in HBM; older pages spill to DDR then CXL. The tile framework already carries paged KV (docs/TILES.md 13), so the pages exist to place |
| **Activations** | short-lived, hot, reused | lowest **latency**, an arena reused across steps | lifetime is a step, so capacity does not matter and locality does |
| **Cold / spill** | rarely touched | **capacity**: CXL, or pmem | paying HBM prices for cold data is what tiering exists to stop |
| **A parked entity's stack and FP save area** | untouched until it resumes | the node of the CPU that will **resume** it | see below |

The last row is the interesting one and it is a real consequence of the entity model. When
an entity parks, its kernel stack and FP save area are untouched until something wakes it -
so **placement should follow the wake, not the sleep**. If a woken entity will be resumed on
another node (because its owner changed, or a stealer took it), its saved state is a small,
cold, movable object that should move with it. The entity already carries `owner` and `node`
in its hot line (docs/EXECUTION-MODEL.md 4.1), so the query is expressible today; what is
missing is the discovery that makes "another node" mean anything.

### 6.4b Work stealing must respect the cache domain

A steal moves a working set. Stealing from a victim on the same LLC domain costs almost
nothing; stealing across a socket moves every line the thief then touches. So the steal
protocol - which already exists for unstarted cells (docs/SMP.md 10.0) and for the strand
injector (docs/CONCURRENCY.md) - should prefer, in order: **this LLC domain, then this
memory node, then anywhere**, and count the crossings.

That is a *policy over the graph*, not a new mechanism, and it composes with the rule
already recorded: when locality and work conservation conflict, **work conservation wins and
the crossing is counted** (section 6.2's edge-case table). A thief that idles rather than
cross a socket is a worse failure than one that crosses and says so.

### 6.4d A CPU with no FPU: place it, or trap and move it

A machine whose cores do not all have the same floating-point hardware is ordinary - RISC-V
cores built without `F`/`D`, ARM cores without a vector width their siblings have, an
asymmetric part whose efficiency cores are narrower. The graph already carries
`CapClass::FloatSimd` with `Reach::Inline`, so the question is what the scheduler and the trap
path do with it. **Two mechanisms, and they are complements rather than alternatives.**

**1. Place it - the cheap path, and it is already expressible.** Hard-float work names
`FloatSimd` (and, for a specific width, the `detail` bits) in its `Request`, and Phase 1
filters out any CPU that does not offer it *before* Phase 2 ranks the rest. Nothing new is
needed: this is the ISA filter of section 3 applied to a capability instead of a baseline. It
is also the right default, because it costs nothing at run time.

**2. Trap and move it - for the work that was placed before anyone knew.** A cell does not
declare its FP use, and a JIT decides at run time, so placement cannot always be right. On a
core without FP the first FP instruction traps: RISC-V with `sstatus.FS == Off` takes an
illegal instruction, ARM64 with `CPACR_EL1.FPEN` traps to EL1, x86-64 raises `#UD` or `#NM`.
Today `on_user_trap` maps all of those to `FaultCause::Illegal` and they become a SIGILL for a
Linux cell or a terminal fault for a native one.

The new path asks the graph first, and it composes with a shape this kernel already has -
**the resumable fault**. Demand paging returns the frame unchanged so the faulting instruction
re-executes (`linux::fill_fault`); an FP trap does the same thing after moving the entity:

```
   FP trap on a core with no FPU
      |
      +-- graph: does another CPU offer FloatSimd?
      |     |
      |     +-- yes -> re-place the entity there, return the frame UNCHANGED.
      |     |          The instruction re-executes, natively, on a core that can run it.
      |     |
      |     +-- no  -> emulate the instruction in software, IEEE-exact, and REPORT it
      |                (a program silently running 100x slower looks like broken hardware)
      |
      +-- neither possible -> SIGILL / terminal, exactly as today
```

Three outcomes, in the vocabulary docs/CPU-FEATURES.md 2.1 already uses - `Native` by
migration, `Emulated` in software, `Unavailable` - and never a fourth.

**Why migrating on the *first* FP instruction is unusually cheap**, and this is the property
that makes the mechanism attractive rather than merely possible: there is **no FP state to
move**. The trap happened on the first FP instruction the entity ever executed, so its FP
register file is the ABI-default image `fp_area_init` wrote and nothing is lost by discarding
it. The migration is therefore the address-space-free case - the frame and the ownership
claim change, and the vector file does not, which is exactly what
`user::switch_native_vcore` already does within a cell. A migration triggered *later* (by
preemption, say) does have to carry the saved area, which is the "placement follows the wake"
case of 6.4a.

**The emulation branch inherits the FMA trap, and must not be allowed to hide it.** A software
emulation of `fmadd` that computes `a*b` then `+c` rounds twice where the hardware rounds once
(docs/CPU-FEATURES.md 2.2). So the emulation is `Native`-equivalent only if it is IEEE-exact -
single-rounded, correct in the subnormal and NaN cases - and if it is not, it is `Numeric` and
must be **refused** under a bit-exact contract rather than substituted. A tile kernel would
rather fail than return a slightly different answer; that is the whole reason its proofs are
equalities.

**Prerequisites, both already in the register.** This needs per-CPU feature discovery to know
*which* cores differ (§7.2 - the machine-wide claim in `graph_build` is the placeholder, and it
says so at the site), and it needs **E4**, per-entity resources, to migrate an entity at all
(§7.1). Neither is a reason to defer the design: the trap sites exist, the graph query exists,
and the resumable-fault shape exists.

**And it is gateable here, with a synthetic asymmetry that is honest about being synthetic.**
QEMU's `virt` gives homogeneous cores, so the hardware case cannot be reproduced - but the
*mechanism* can, because FP is enabled per core in software: leave `sstatus.FS` off on one
secondary, run hard-float work there, and assert it traps, migrates, completes natively, and
that the graph was consulted. That is the same construction `netwait` uses when it raises the
interrupt-controller line directly because QEMU's UART loopback does not - the device asymmetry
is synthetic and the kernel path exercised is the real one. What it would **not** prove is that
real heterogeneous silicon reports what this expects, which stays a lab claim.

### 6.4c What makes it a backbone rather than another table

The graph is the fifth thing in this design that everything else composes onto, beside the
four that already are: the **queue ABI** (how work moves), the **cell and its capabilities**
(who may), the **strong identity** (`PrincipalId`, on whose behalf), and the **telemetry
ring plus flow context** (what happened). The test of whether it belongs in that list is
whether it needs a kernel object of its own - and it does not. It is derived state over
`hw::Inventory`, published by driver cells for what firmware cannot describe, queried by
everyone, and owned by nobody.

## 7. Honesty

- **Nothing here is built.** `hw::Inventory` has the data; the graph, the costs and the
  queries do not exist.
- **QEMU models topology, not its costs.** SLIT and HMAT values are *declared* at launch and
  reported faithfully, so this proves **parsing, structure and query** - it does not and
  cannot prove that placing frames near a device is *faster*, because TCG models no memory
  system. Wall-clock benefit stays a hardware-lab claim (docs/TOOLING.md 4), and the two
  must not be conflated.
- **ARM64 will skip most of it with a reason**, because no firmware describes memory to a
  bare-ELF boot there. That is the same honest shape as PMEM and the RISC-V IOMMU.
- **No new kernel object.** The graph is mechanism under the existing inventory, and every
  placement call it feeds already exists.
