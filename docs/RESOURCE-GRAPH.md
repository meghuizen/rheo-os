# The resource graph: one model of the machine, queried rather than hardcoded

**Status:** designed, not built. Every section names what would count as evidence, and one
of them is provable in this container today.

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
| How much *further* is node 1 from node 0 than node 0 is from itself? | **No.** SLIT is not parsed; distance is binary |
| What bandwidth and latency does this initiator see to that target? | **No.** HMAT is not parsed |
| Which node is this NIC's DMA on? | **No.** A device's proximity domain (`_PXM`) is not read |
| Do these two CPUs share an LLC? Are they SMT siblings? | **No.** Not modelled at all |
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
| LLC domains, SMT sets | CPUID leaf 4 / 0x1F | `MPIDR` affinity levels + DT `cpu-map` | DT `cpu-map` |
| Core class (P/E/LP) | CPUID leaf 0x1A | DT `capacity-dmips-mhz` | DT `capacity-dmips-mhz` |
| PCIe topology | ECAM walk (**have**) | ECAM walk (**have**) | ECAM walk (**have**) |

ARM64's column is mostly empty and that is a **measured** fact, not an omission: QEMU hands
a bare-ELF `-kernel` boot on `virt` no device-tree pointer, checked rather than assumed
(`-dtb` does not reach it either), which is why the existing `numa` test skips there with a
reason. So the graph must be **useful when degraded**: with one node and no distances it
answers "everything is equally near", which is exactly correct for that machine, and
`graph_source()` reports what it was built from so no proof can claim distances it does not
have.

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
| `/sys/devices/system/cpu/online`, `present`, `possible` | the graph's CPU set |
| `/sys/devices/system/cpu/cpuN/topology/{core_id,physical_package_id,thread_siblings_list}` | SMT sets and packages |
| `/sys/devices/system/cpu/cpuN/cache/indexN/{level,size,shared_cpu_list}` | LLC domains |
| `/sys/devices/system/node/nodeN/{cpulist,meminfo,distance}` | **`distance` is SLIT** - the file libnuma and hwloc read |
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
