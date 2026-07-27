# Lattice OS - Architecture

**Status:** Draft v0.1
**Codename:** "Lattice" is provisional. Rename freely.
**Scope:** A from-scratch operating system whose **one foundation is built and
configured into a spectrum of deployment profiles** - servers, AI inference,
VM host, container/OCI foundation, database and data-warehouse appliances,
internet exchange, firewall and Cloudflare-type edge appliances, and (in later
phases) embedded/IoT, low-power/remote, and desktop. The core primitives are
form-factor-independent; profiles are compile/configuration choices over one
kernel. Server, data, and networking profiles come first; embedded, low-power,
and desktop are architected-in from the start and delivered in later phases.
See PROFILES.md. Phone/mobile handsets are not a target.

> **Quality bar: production-grade foundation, not an MVP.** The foundation -
> the kernel primitives and the layers up to a usable, multi-tenant system -
> is engineered to a production standard from the start: the robustness,
> hardware breadth (within each profile's class), reliability, security
> hardening, and operability you would expect of a Linux-class platform
> running real production workloads, and deployable both *inside* public
> clouds (as a guest) and as a host OS a provider could run their fleet on.
> The verification and milestone plan in section 8 is a discipline for
> de-risking the genuinely novel parts - it is **not** license to ship stubs.
> See PRODUCTION.md for the quality bar and hardware-breadth strategy, CLOUD.md
> for cloud deployment and the host-OS positioning, and PROFILES.md for the
> full set of deployment profiles.

> **Project stance: greenfield, modern-first.** Lattice is a clean-slate
> design for *current* server hardware. It sets a modern hardware floor and
> exploits newer capabilities (wide SIMD such as AVX-512/SVE2, matrix/tile
> units such as AMX, accelerators, DPUs, CXL, RDMA) via measured runtime
> dispatch, never a lowest-common-denominator baseline. Older platforms and
> embedded/edge form factors are deliberate *later* additions, out of scope
> now. Full statement in section 1.4.

---

## 0. Glossary (read this first)

Plain-English definitions of the terms this document uses everywhere.

| Term | Meaning |
|---|---|
| **Cell** | The unit of isolation. One address space + one set of capabilities + its queues. Think "container", but native. |
| **Capability** | An unforgeable handle that grants access to exactly one thing (a buffer, a queue, a device). You can only touch what you were handed. |
| **Queue pair** | A submission ring + completion ring in shared memory. The only way to ask the kernel (or another cell) to do anything. |
| **Engine** | Anything that executes work: a CPU core, a GPU partition, an NPU, a DMA engine, a NIC, a storage device. |
| **Grant** | A capability to memory: a typed region (DDR, HBM, etc.) with explicit commit rules. |
| **Strand** | A user-level thread. Costs ~200 bytes. The kernel never sees it. |
| **Vcore** | A CPU core granted to a cell. The kernel schedules vcores; the cell's runtime schedules strands on them. |
| **Lease** | A grant that expires unless renewed, with a fencing token. The one primitive for locks and failure handling. |
| **Seal** | Making a filled buffer immutable (kernel flips page permissions). After sealing, many readers can trust it without copies. |
| **Personality** | A compatibility layer (POSIX, Kubernetes API) that translates legacy interfaces onto native primitives at the edge. |
| **HLC** | Hybrid logical clock: a timestamp that stays close to real time but never violates cause-and-effect ordering. |
| **Attestation** | Cryptographic proof of what software/firmware is actually running (measured boot, signed firmware). |

---

## 1. Why - the problem statement

### 1.1 What changed

Hardware and workloads moved; general-purpose kernels did not.

- **Compute is heterogeneous.** GPUs, NPUs, TPUs, FPGAs, and DPUs do most of the
  work in modern fleets. Linux treats them as character devices poked through
  vendor ioctl blobs, outside the scheduler, outside the security model.
- **Memory is not uniform.** HBM, DDR, CXL, PMEM, and remote memory have wildly
  different bandwidth, latency, and coherence. Pretending memory is one flat pool
  was already a lie on NUMA; it is now unaffordable.
- **Data moves device-to-device.** RDMA, GPUDirect, NVMe peer-to-peer: the fast
  path never touches the CPU. The kernel's job shrank to setup and policing, but
  Linux still architecturally sits in the data path.
- **Identity replaced perimeter.** Inside a cluster, "which IP" means nothing;
  "which attested workload" means everything. SPIFFE, mTLS, and service meshes
  are an industry built to compensate for the OS having no identity concept.
- **Orchestration is the real init.** Machines are fleet nodes reconciled toward
  desired state, not timesharing terminals. Kubernetes is a second operating
  system stacked on the first.
- **AI inference is becoming a default workload**, and its core techniques
  (paged KV caches, continuous batching, memory tiering) are operating-system
  ideas reinvented in userspace because the kernel could not help.

### 1.2 Why not evolve Linux / BSD / Hurd

- **Linux** keeps absorbing the right ideas (io_uring, VFIO, eBPF, HMM) but as
  retrofits constrained by 30+ years of ABI: synchronous syscalls, UID/DAC
  security, a global shared network stack, an OOM killer, priorities instead of
  contracts. Its driver ecosystem is the real moat - and irrelevant for a
  narrow fleet target where the device list is short.
- **BSD** is the same fundamental model with cleaner execution. Capsicum is a
  real capability system, but bolted onto POSIX file descriptors.
- **Hurd** had the right instinct (multiserver microkernel) and fatal execution:
  slow synchronous IPC and the goal of faithfully re-implementing POSIX as the
  native model. Lattice makes POSIX a translation layer, never the target.
- **Closest living relatives:** Fuchsia/Zircon (capabilities, userspace drivers,
  no global namespace) and Barrelfish (treat one machine as a distributed
  system). Lattice combines both instincts and aims them at fleets.

### 1.3 The honest bet (falsifiable)

As accelerators and DPUs take over the data plane, the kernel's remaining job
is **identity, placement, and queue setup**. A kernel designed for only that
job beats one carrying a 1970s timesharing contract - *for fleet workloads*.
On general-purpose machines Lattice loses to Linux and should not compete.

A cautionary tale that shaped the driver rules: Linux is disabling its
Qualcomm Crypto Engine (QCE) driver in 7.3 as "harmful" - slower than the CPU,
a decade of bugs, and races with firmware it never exclusively owned. That is
what happens when vendor accelerator logic lives in kernel space and offload
is never benchmarked against the CPU it claims to beat.

### 1.4 Project stance and scope

This is a **greenfield project designed for modern systems.** It does not
carry legacy weight, and it optimizes for the hardware and workloads of today
and the near future, not the installed base of the past.

- **Modern-first, deliberately.** The design assumes recent server silicon and
  its capabilities - wide SIMD and vector units (AVX-512, AVX10, SVE2, RVV),
  matrix/tile instructions (AMX), IOMMUs, measured boot, hardware crypto,
  PTP-capable NICs, RDMA fabrics, and accelerators. These are treated as
  **foundational acceleration options exploited when present** (runtime-
  dispatched and always benchmarked, never assumed) - see
  TARGET-ARCHITECTURES.md 2. We do not design down to the weakest common
  denominator.
- **Backwards compatibility is an edge, and a later concern.** The
  compatibility personalities that exist (POSIX/SSH, the Kubernetes API,
  HTTP/JSON, TCP - doctrine 10) are translation layers at the boundary so
  existing software and tooling keep working. They are not the native model,
  and broadening legacy support (more OSes, older hardware, wider POSIX
  fidelity) is explicitly a **future** direction, not a present goal. Today's
  focus is getting the modern-native core right.
- **Broad form-factor reach via profiles, delivered in phases.** The one
  foundation is architected to be built into many deployment profiles -
  servers, data and networking appliances, VM host, edge, and (later)
  embedded/IoT, low-power/remote, and desktop (PROFILES.md). This is designed
  in from the start: the microkernel is small, the Arch trait bounds hardware
  differences, `no_std` Rust already spans microcontroller-class to server
  parts, and capabilities/cells/queues are form-factor-independent. What is
  *phased* is delivery, not architectural capability - server/data/networking
  profiles first; embedded (MMU-class), low-power/remote, and desktop follow,
  each bringing its own new work (power management, offline sync, driver
  breadth). The one real boundary: sub-MMU microcontrollers are out, because
  the capability model needs an MMU (PROFILES.md 6).

The one-line version: **build the modern-native core first; it is architected
to be configured into many profiles, delivered in phases - never at the cost
of the core, and never below its security bar.**

---

## 2. Doctrines

These ten rules generated every design decision. When in doubt, re-derive from
here.

1. **No ambient authority.** A capability or nothing. No root, no global
   namespaces (PID, mount, port), no default reachability.
2. **The kernel is a control plane.** It arbitrates and enforces. It never
   touches payload bytes, parses data formats, or runs vendor logic.
3. **Typed, never uniform.** Memory kinds, engines, clocks, IDs, and buffers
   carry their real hardware contracts. Differences are load-bearing.
4. **Explicit over transparent.** Placement, migration, durability, batching,
   and copies are declared contracts the scheduler can exploit - never hidden
   heuristics. No transparent page migration, no invisible page cache, no
   silent copies.
5. **Admission over priority.** Guarantees (CPU, memory, bandwidth, residency)
   are accepted by math or rejected loudly. "Importance" is a resource
   contract, never a priority number.
6. **Contain, don't trust.** Vendor blobs, legacy stacks, probes, and tenants
   run with exactly their grants. Offload must prove itself by measurement at
   attach time (the QCE test).
7. **Failure is an event.** Leases expire, peers die, budgets exhaust - all
   delivered on the same completion queues as success. No OOM killer, no
   silent hangs.
8. **Ownership moves; bytes don't.** Descriptors, seals, and grants replace
   copies. Remaining copies are visible nodes in a dependency graph.
9. **Local and distributed are one mechanism at two scales.** Queues,
   capabilities, leases, and identity work identically over shared memory and
   over the network fabric. A one-host cluster is just a trust domain of one.
10. **Legacy is a personality at the edge.** POSIX, the Kubernetes API,
    HTTP/JSON, and TCP are translated at boundaries. New software targets the
    native model.

---

## 3. The kernel object model

The complete set. Ten objects, roughly three dozen operations. The governance
rule in section 6 keeps this list closed.

1. **Cell** - address space + capability set + queues, plus an immutable
   **principal** the kernel derives at creation (image measurement + parent)
   and reports but never decides from. Cell *groups* add co-placement and a
   shared lease (the pod replacement), nothing more.
2. **Capability** - unforgeable, typed, delegatable, epoch-revocable,
   budget-metered. Becomes a signed cryptographic token when it crosses hosts.
   It is simultaneously the security model, the audit log, and the metering
   system.
3. **Queue pair** - submission/completion rings + doorbell. The entire syscall
   surface. Blocking does not exist below the library level.
4. **Engine** - any executor. Attested firmware, *measured* (not claimed)
   throughput recorded at attach, a declared preemption contract, spatial
   partitioning (MIG-style) where hardware allows.
5. **Memory grant** - typed kind (DDR / HBM / CXL / PMEM / device-BAR /
   remote), explicit commit policy, hard or elastic, sealable to immutable.
6. **Dependency graph** - work nodes across engines, timeline semaphores,
   DMA/transfer nodes, conditional edges (for speculative decoding, MoE
   routing), and yield/budget contracts on unbounded nodes.
7. **Reservation** - admission-checked guarantees: CPU (budget, period,
   deadline - EDF schedulability math), memory floors, queue depth, I/O and
   memory bandwidth, model residency.
8. **Lease** - expiring grant with fencing token. Locks between cells, failure
   detection across hosts, and slow-consumer handling are all this one object.
9. **Clock and entropy objects** - monotonic clock, wall clock as a bounded
   interval [T-e, T+e], engine clocks with known offsets; a per-host root DRBG
   feeding per-cell DRBGs, reseeded on every restore, entropy sources included
   in the attestation chain.
10. **Event stream** - typed, schema'd events with 16-byte flow context that
    the kernel propagates through every queue entry, graph edge, and transport
    frame. Observability the system cannot fail to produce.

**The verb set:** create/destroy cell; mint/delegate/revoke capability;
establish queue pair; grant/commit/decommit/seal memory; submit graph;
reserve/release; arm timer/doorbell; checkpoint/restore (DRBG state and sealed
shared objects excluded from images by rule); attest; plus delivery of
pressure, revocation, lease-expiry, and completion events.

Cell scheduling on one CPU is part of the **create/destroy cell** verb, not a
verb of its own: the native cooperative hand-offs (`SYS_SWITCH`, a directed
`cur^1` switch; `SYS_YIELD`, its round-robin generalisation added for service
fan-out, docs/NETSTACK.md 17) are pure mechanism over object 1 with policy
outside - who runs next is a fixed round-robin, and a hand-off transfers no
authority (the cells share one capability bundle). They pass section 6 on the same
grounds as the address-space switch they wrap: a cell cannot switch its own page
tables, and the CPU is shared hardware. Preemptive multi-core scheduling (SMP,
task #27) is still ahead of the design here.

**Identity is deliberately not an object.** A cell's principal fails §6 test 2 -
it arbitrates no shared hardware - so it is a field on object 1, and reporting
it is the existing **`attest`** verb rather than a new one. POSIX users, groups
and `rwx` are then a userspace projection: the credential is per-cell
synthesized state in the personality, and mode bits are checked by the file
server, which is outside the kernel by doctrine 5. The kernel makes no access
decision from an identity; `grant_check` is untouched. Full model in
docs/IDENTITY.md.

Everything else in this document is **composition** of these by cells.

---

## 4. How - the subsystems

Condensed design per subsystem. Each is a landing zone above the kernel, not
part of it.

### 4.1 Memory

- Unified *addressing*, explicit *placement*. One virtual namespace; every
  allocation names a memory kind with declared bandwidth/latency/coherence/DMA
  properties. `Buffer<Hbm>` and `Buffer<Ddr>` are different types.
- Migration is a scheduled DMA node in a graph, never a page-fault side
  effect. Fault-driven migration exists only as a labeled opt-in slow path.
- Topology (engines, memories, links, NUMA, fabric islands) is exposed as a
  graph, not hidden.
- Huge pages (2MB) are the default commit quantum; no khugepaged-style churn.
- Stacks: reserve big virtual, commit lazily, guard pages always. Kernel
  stacks are per-vcore, not per-thread.
- Heaps: per-vcore arenas over grants (mimalloc-style remote-free queues),
  typed per memory kind, pre-zeroed pages from a background engine job.
- Reclaim: elastic grants + pressure events with deadlines. Cooperative first,
  bounded forced decommit second, no global OOM victim selection ever.

### 4.2 Scheduling

- **Two-level:** kernel places vcores/engines onto cells (slow, coarse);
  runtimes dispatch strands (fast, fine). Scheduler activations, viable now
  because no blocking syscall exists to reconcile.
- **Pools:** dedicated tickless cores for latency-critical cells (no timer,
  no preemption); a shared EDF/fair pool for everything else; a boot-carved
  system pool the control plane provably keeps under any overload.
- **Real time:** reservations (budget, period, deadline) with admission
  control and throttled overrun. Priority inheritance is mandatory on any
  mutex a reservation-holder touches.
- **GPU/NPU:** schedule space, not time. Spatial partitions as engine
  capabilities; command-buffer-level arbitration; budget-kill instead of
  impossible preemption; declared preemption contracts gate co-location.
- **NUMA:** threads follow memory. Cells are born with a home domain;
  work-stealing is topology-bounded.
- **SMT:** siblings only within one cell; disabled outright in latency pools.

### 4.3 Threads

- Strands: ~200-byte user-level threads, tens-of-ns switches, stackless
  (async state machines) or stackful (lazy-commit stacks), spawned in the
  hundred-thousands.
- Common issues answered structurally: guard pages (overflow), exact vcore
  counts (oversubscription), compiler yield points + a kernel preemption
  doorbell (hogging), sequence-counted park/unpark (lost wakeups), wake-one
  default + directed handoff (thundering herd), vcore-local storage (TLS
  bloat), wait-for graph detection in debug (deadlock), lease timeouts
  (cross-cell deadlock).

### 4.4 IPC, streams, and zero copy

- Connect = capability exchange yielding a typed queue pair (IDL-declared
  protocol: request/response, stream, one-way).
- Small payloads inline in the entry (copy wins below ~1-4KB, measured per
  platform); large payloads travel as capability references to buffers.
- Streams = descriptor rings over per-stream arenas. Scatter-gather lists are
  the universal chunk currency; re-framing between producer/consumer grain is
  descriptor arithmetic; genuine re-layouts are one explicit DMA gather node.
- **Seal** turns filled buffers immutable before fan-out: validate once, N
  readers, devices see read-only IOMMU mappings, TOCTOU dead by construction.
- Backpressure: bounded depths, byte-denominated credits, reader leases for
  slow consumers.

### 4.5 Time, order, identity, randomness

- Wall time is an interval with error bound e; PTP-first sync by an
  authenticated, capability-gated daemon; leases self-fence as e grows.
- HLC timestamps on every cross-host message provide causal order; wall time
  never decides ordering or uniqueness.
- UUIDv7 for cluster-visible object IDs (index locality); typed alternative
  (v4) where creation-time leakage matters; 64-bit local handles promoted at
  host boundaries.
- Per-cell DRBGs off an attested root; restore always reseeds; hosts without
  provable entropy fail attestation.

### 4.6 Storage

- Tier 1: legacy filesystems (ext4/ZFS/btrfs) as userspace server cells for
  interchange. Tier 2: CephFS/HDFS/NFS as first-class clients. Tier 3: the
  native object store.
- Native store: typed, versioned, content-addressable objects; namespaces are
  indexes (per-tenant views), not a global tree; access is grants on object
  sets; I/O is queues and DMA graphs end to end.
- Durability is a typed completion (`volatile` / `ordered` / `durable`), with
  completion windows enabling automatic group commit across cells. The
  fsync-latency floor is hardware: PLP write caches or a PMEM log kind.
- Every index-shaped structure is differential (delta layer + merged base,
  LSM economics) with declarable per-object-class index policy.
- Arrow in memory, Parquet at rest, both native; Parquet
  projection/filter pushdown executes at the storage node or DPU.

### 4.7 Networking

- The kernel owns queue plumbing, flow steering, and grant checks. No kernel
  TCP/IP.
- Cells own NIC RX/TX queues directly (IOMMU-isolated DPDK-as-native).
  Transports are libraries: one blessed Rust TCP/QUIC library as a system
  cell; third-party stacks allowed, contained, unsupported.
- QUIC is first class (CID-based steering, 0-RTT with anti-replay, streams
  terminate as native queues, inline NIC crypto offload per queue). TCP is
  the compat protocol. TLS 1.3 keys are capabilities programmed per queue.
- The eBPF role: verified WASM dataplane programs at three tiers - NIC/DPU
  pipeline, stateless host pre-steering cores, in-cell hooks - all
  capability-gated and cycle-budgeted.
- DDoS: drop-cost inversion. Steering misses die in NIC hardware; a
  system-pool pre-steering stage does SYN cookies / QUIC Retry / sketch-based
  rate limits in bounded memory; per-cell arenas mean floods exhaust only the
  target's buffers. East-west traffic is capability-gated RDMA - internal
  services have no IP to flood.

### 4.8 Cluster

- Host identity is hardware-rooted (TPM/DICE measured boot); hosts attest
  their cells; identities are SPIFFE-shaped.
- Capabilities cross hosts as signed, scoped, expiring tokens (deterministic
  CBOR encoding); revocation is epoch-based and honest about being eventual.
- Queue transports: shared memory, RDMA, or mTLS/QUIC - chosen at connect,
  invisible above.
- Remote memory is a memory kind, never a transparent tier. No distributed
  shared memory illusions.
- Consensus surface is tiny: membership and placement only. Data plane is
  peer-to-peer capabilities that survive partitions until leases expire.
- No process migration; movement is explicit checkpoint/restore, priced
  honestly.

### 4.9 Orchestration (Kubernetes, absorbed)

- Deleted because native: kubelet, container runtimes, CNI, kube-proxy,
  service meshes, secrets plumbing, NetworkPolicy, device plugins.
- Kept and promoted: the declarative resource model. A typed, capability-
  scoped state store (watch = completion queue) replaces etcd + API server;
  controllers are ordinary cells with narrow grants; tenants are
  sub-trust-domains; the pod becomes the cell group; the scheduler is
  topology- and gang-aware; graph jobs are a first-class workload type.
- A compatibility edge speaks the real Kubernetes API outward (kubectl, Helm,
  GitOps keep working), translating Pods/Services/Policies into native
  objects.

### 4.10 Observability

- Flow context (W3C-traceparent-shaped, 16 bytes) rides every queue entry,
  graph edge, and transport frame; the trace follows the DMA across engines
  and hosts.
- Typed event streams (scheduling decisions, grant denials, pressure, lease
  expiry) in bounded per-cell rings; collectors are cells with read grants -
  observability visibility is capability-scoped, tenant-safe.
- Dynamic probes are verified WASM (the eBPF role) attached under audited
  debug grants; interactive debugging is the same grant escalated.
- OpenTelemetry is the export mapping at the edge (an exporter cell speaking
  OTLP), not the kernel mechanism. Metrics fall out of capability metering.

### 4.11 AI inference layer

- Engines already cover GPU/NPU/TPU; vendor runtimes are contained driver
  cells (the QCE doctrine); offload is benchmarked at attach.
- Models are content-addressed, immutable, sealed objects (flat safetensors-
  style layout): dedup, hash-as-integrity, attestable provenance, loaded by
  one DMA graph at line rate, shared read-only across cells (one HBM copy per
  device), residency governed by reservations.
- KV cache is paged memory done honestly: block-granular grants, GPU-MMU
  block tables, copy-on-write prefix sharing, content-addressed prefix cache.
  Continuous batching is the completion-window contract. Speculative decoding
  and MoE are conditional graph edges.
- The compilation service lowers a graph IR through a **tile IR** (typed
  tiles, async tile copies, shape-negotiated MMA, compiler-owned layouts) to
  PTX/SASS (via contained ptxas), ROCm, NPU command streams, and AMX -
  meeting CUDA where it is going (cuTile/Triton direction). Autotune results
  and compiled artifacts are content-addressed cluster-wide objects.
- Vulkan: compute lowering floor for unblessed GPUs; graphics maps almost 1:1
  (timeline semaphores are the native sync object). HID devices are event
  queues granted to a session/compositor cell.

### 4.12 Compatibility personalities

- POSIX: gVisor-style syscall translation per cell; fork becomes clone-within-
  capability-bundle; a per-identity synthesized filesystem view (Plan 9's
  namespaces, enforceable this time). SSH-to-bash works; ~99% interactive
  fidelity, ~80% arbitrary-script fidelity, honestly stated.
- Linux binaries: the Linux personality (docs/LINUX-COMPAT.md) implements
  the Linux syscall ABI over cells and grants. Currently kernel-hosted like
  `svc.rs` (a documented bridge, not a doctrine change): it adds no kernel
  object - PIDs/fds/signals/**processes** are per-cell synthesized state - so
  section 5's exclusions hold; the kernel proper gains only mechanisms that
  pass the section 6 test (thread-pointer switch state, unmap/protect,
  multi-context cells per 4.3, U-mode FP/SIMD state, and - for L6 processes -
  a page-table user-leaf walk behind eager-copy/reclaim `fork` and a
  generalized cross-cell run loop reusing the native address-space switch).
  fork/execve/wait4/cross-cell pipes run unmodified static-glibc programs and
  a shell driving the upstream uutils/coreutils (the P11 gate, measured
  12/12 = 100% on all three ISAs).
- Edges: Kubernetes API, gRPC/HTTP/JSON gateways, Arrow Flight, YAML
  manifests (tightened parser) - all translate at the boundary.

---

## 5. The negative constitution

Permanently outside the kernel: filesystems, network stacks, device drivers
beyond queue/IOMMU/reset plumbing, fork, signals, periodic ticks, an OOM
killer, priorities, global PID/mount/port namespaces, in-kernel crypto
offload logic, inference, data-format parsers, vendor code, printf logging as
a system citizen, transparent paging or migration of any kind.

Each deletion is what makes a landing zone above possible.

---

## 6. Governance: the kernel admission rule

A proposed kernel addition must prove all three:

1. It needs **unforgeable enforcement** (cannot be a library).
2. It **arbitrates shared hardware** (cannot be a cell).
3. It is **mechanism with policy fully outside** (cannot be a config knob).

Track record within the design itself: only seal, pressure events, doorbell
preemption, conditional graph edges, and checkpoint/restore were forced in by
this test - and each was reused by three or more unrelated subsystems. New
workloads should land as compositions; the day one cannot, re-examine the
doctrine list before the object list.

---

## 7. Language and tooling (summary)

- **Rust** for kernel and privileged components: ownership *is* the
  capability model (non-Clone moves = unforgeable transfer), generics encode
  typed memory, typestate encodes queue disciplines, `no_std` + async fit a
  control-plane kernel. Unsafe concentrated in small audited crates (~2-5%).
- Assembly for boot/context-switch/vectors; an `Arch` trait bounds all ISA
  differences (no #ifdef confetti).
- The innermost capability core (grant check, derivation, revocation - a few
  thousand lines) targets machine-checked proofs (Verus; Kani as fallback).
- Frozen C-ABI kernel surface generated from a system IDL (FIDL-inspired:
  Cap'n-Proto-style arena layout, protobuf-style evolution, handle-typed
  fields) into Rust/C/Go bindings.
- Userspace is polyglot; WASM components are a first-class cell type for
  controllers, policy, probes, and dataplane programs.
- Tooling kept boring: Cargo workspaces, LLVM targets, QEMU-first CI on all
  ISAs, Miri, loom for queue/lock permutations, structure-aware fuzzing on
  every descriptor parser and the capability token codec, perf-regression
  gates on the three numbers a control-plane kernel lives on (grant check,
  queue round trip, context switch).

---

## 8. Verification - how we find out if this works

Ordered from cheapest to most expensive. Every layer has kill criteria,
because a design that cannot fail a test is theater.

Two things this plan is **not**: it is not a plan to ship an MVP, and the kill
gates are not permission to lower quality. They de-risk the parts that are
genuinely novel (the capability-core proofs, the distributed protocols) before
large effort is committed. Every layer that *does* ship is built to the
production bar in PRODUCTION.md - full error handling, hardware breadth for its
class, security hardening, and the reliability targets - not to demo quality.
Passing a gate means a layer is production-ready, not merely demonstrated.

### 8.1 Layer 0 - paper checks (before code)

- **Schedulability math review:** the EDF admission model, PI interaction,
  and budget-throttle semantics checked against the real-time literature by
  someone who does RT analysis for a living.
- **Distributed protocol models:** TLA+ (or equivalent) specs for (a) epoch
  revocation, (b) lease/fencing under partition, (c) state-store watch
  ordering under HLC. Model-check the failure interleavings before
  implementing. Kill criterion: a liveness or safety hole with no bounded
  repair means redesigning the primitive, not patching the spec.

### 8.2 Layer 1 - formal proofs (the trusted core)

Scope: the capability core only (~3-5k lines): mint, delegate, derive-subset,
revoke-by-epoch, grant check. Properties to machine-check (Verus):

1. **Unforgeability:** no sequence of operations yields a capability not
   derived from an existing one.
2. **Monotonic attenuation:** delegation never widens rights.
3. **Revocation soundness:** after epoch E is invalidated, no capability from
   E passes a grant check.
4. **Isolation lemma:** two cells with disjoint capability sets cannot affect
   each other's memory or queues through any kernel path.

Property 4 has a **precondition that is not itself about capabilities**, and an
audit found the implementation missing it: the kernel services a trap in
S-mode/EL1/ring 0 *with the calling cell's root active*, and every cell root maps
all of kernel RAM supervisor-RWX through the linear map. So an address a cell puts
in a syscall argument reaches kernel memory unless the kernel bounds it, and a
resource a cell names by address is freed out from under another cell unless the
kernel checks ownership. Neither is visible to a proof about mint/delegate/revoke:
the capability core can be perfectly sound while an out-parameter write, an
unbounded allocation length, or an unowned `munmap` walks straight through it.

The isolation lemma therefore reads, in full: **for every kernel entry point, an
address or length or handle a cell supplies is bounded, budgeted or
ownership-checked before use** - and only then does "disjoint capability sets"
imply non-interference. `docs/ENGINEERING.md` 12 is the corresponding engineering
rule, the `security` test kernel is the runtime evidence from an unprivileged
cell, and the three findings behind it are recorded there. When the Verus work
starts, this precondition is part of what layer 1 must state, not an assumption
underneath it.

Benchmark: seL4 proved this class of property is achievable. Kill criterion:
if the proof effort exceeds ~2 person-years without closing, shrink the core
further or adopt seL4's kernel as the bottom layer instead of writing one.

### 8.3 Layer 2 - implementation correctness

- **loom** permutation testing on every lock-free structure (rings, Chase-Lev
  deques, epoch reclamation).
- **Miri** + MTE/ASAN-class runs on all unsafe crates, gated in CI.
- **Structure-aware fuzzing**, continuously: submission-entry parsers, the
  cryptographic capability codec (the single most attacked surface), IDL
  decoders, the WASM verifier. Any panic or invariant break in the kernel
  from fuzz input is a release blocker.
- **Fault injection:** kill hosts, expire leases, corrupt clocks (grow e),
  partition the fabric - assert the invariants from the TLA+ models hold in
  the implementation (Jepsen-style harness for the state store).

### 8.4 Layer 3 - performance falsification

Each hypothesis has a target and a kill threshold, measured against a
**tuned** Linux control (io_uring + isolcpus + DPDK/XDP + jemalloc - beating
stock Linux proves nothing).

| # | Hypothesis | Target | Kill threshold |
|---|---|---|---|
| P1 | Grant check (hot path) | < 50 ns p99 | > 150 ns |
| P2 | Null queue op round trip | < 1 us single; < 100 ns amortized batched | > 3 us / > 300 ns |
| P3 | Same-cell context switch | < 200 ns | > 500 ns |
| P4 | Strand spawn / switch | < 100 ns / < 50 ns | 3x worse |
| P5 | Cross-cell small message | < 500 ns same host | > 1.5 us |
| P6 | Seal 2MB (8-vcore shootdown) | < 5 us | > 20 us |
| P7 | Model load NVMe-to-HBM | >= 80% of raw device line rate | < 60% |
| P8 | Group commit throughput | within 10% of a hand-tuned DB's own group commit | > 25% worse |
| P9 | DDoS: line-rate garbage at one tenant | < 5% p99 degradation for other tenants; drop cost < ~20 cycles/pkt at pre-steering | > 15% degradation |
| P10 | Tile IR GEMM/attention | >= 85% of cuBLAS/FlashAttention on 2 GPU generations | < 70% |
| P11 | POSIX personality | SSH interactive parity; >= 80% of a defined coreutils/tooling suite passes | < 60% suite pass |
| P12 | Tickless jitter (dedicated core) | max scheduling-induced jitter < 5 us over 24h | > 50 us events |

Kill semantics: a red cell means the *mechanism* is re-examined (or the claim
is withdrawn from the architecture), not that the benchmark is re-tuned until
green.

P1-P12 validate the foundation's core mechanisms. The full per-profile
validation - database, warehouse, container, VM host, IX, firewall, edge,
embedded, remote/low-power, each against its best incumbent baseline
(P13-P46) - is in VALIDATION.md, together with the coverage matrix showing
every profile is a composition of the same ten kernel objects.

### 8.5 Layer 4 - battle-testing with real production workloads

The highest-fidelity test, and the one the production quality bar exists to
unlock: once the foundation is genuinely production-grade (PRODUCTION.md), the
best validation is to **throw real production workloads at it** - not synthetic
proxies. The relationship is mutually reinforcing: you cannot mirror
production traffic onto a stub, so quality is the entry ticket; and no formal
proof, microbenchmark, or Jepsen run reproduces what a real workload under real
traffic exposes over time, so real-workload testing is what hardens the whole
system to and beyond that bar. Proofs and Jepsen de-risk the novel *core*
early; this layer validates the *whole system* under conditions nothing
synthetic reproduces.

Techniques, roughly in order of confidence they buy:

- **Real workload suites, not microbenchmarks.** Run actual production-
  representative software to completion under load: a real database (Postgres,
  ScyllaDB) under TPC-style workloads; the vLLM inference port under
  production traffic shapes; real Helm charts and operators through the
  Kubernetes edge; a real distributed training job; mixed-R/W storage against
  Ceph and the native object store. Portability (POSIX personality, K8s edge)
  is what makes this possible - the same workload runs on Linux and Lattice.
- **Differential testing at the workload level.** Run the identical workload on
  a tuned-Linux control and on Lattice; diff results, behavior, and tail
  latency. Divergence is a bug in one of them - workload-level differential
  testing catches what syscall-level conformance misses.
- **Shadow / mirrored traffic.** Mirror real production traffic from an
  existing fleet onto a Lattice fleet and compare correctness and latency with
  zero user impact - the standard, safe way to battle-test infrastructure.
- **Chaos under real load.** Beyond protocol-level Jepsen: kill nodes, inject
  latency, partition, and exhaust resources *while real workloads run*, to
  prove graceful degradation happens in practice, not just in the model
  (PRODUCTION.md 4).
- **Soak / longevity.** Run production workloads for weeks to surface the
  slow-burn failures microbenchmarks and short CI never see: memory leaks,
  arena fragmentation, clock drift, lease churn, handle exhaustion, autotune-
  cache growth. A short benchmark cannot find a leak measured in days.
- **Progressive rollout as validation.** Because the system is fleet-native
  (desired state, A/B rollback, canary), the *rollout mechanism is the
  battle-test mechanism*: canary Lattice nodes into a real fleet, take a small
  percentage of real traffic, expand as error budgets hold (PRODUCTION.md 5).
- **Dogfooding.** Run the project's own infrastructure - CI, build farm, the
  state store, observability backends - on Lattice, so the team feels every
  rough edge before anyone else does.

Gating: this layer attaches to the later milestones - infrastructure workloads
from M2-M4, AI-serving workloads at M5 - and is not a one-time check but the
continuous validation regime the foundation lives under from that point on.
Kill/redesign criterion: a class of real workload that cannot be made to run
correctly and within its latency budget after honest engineering is a signal
about the design, not just the port.

### 8.6 Milestone gates

- **M0 - Capability core + queues** on QEMU (x86-64, ARM64). Gate: proofs
  from 8.2 items 1-3 closed; P1/P2 green.
- **M1 - Cells, memory grants, two-level scheduling** on real hardware.
  Gate: P3-P6, P12 green; loom/fuzz suites clean.
- **M2 - Single-host I/O:** NVMe + NIC queues to cells, native object store
  alpha, blessed transport library. Gate: P7, P8; iperf/fio parity with
  tuned-Linux control within 10%; first real single-host workload (a database)
  battle-tested (8.5).
- **M3 - Cluster fundamentals:** attestation, crypto capabilities, leases,
  state store. Gate: Jepsen-style suite clean under partitions; revocation
  epoch model verified in implementation.
- **M4 - Orchestration + observability:** reconcilers, K8s edge (kubectl
  works), flow tracing end to end. Gate: deploy-and-trace a 3-service demo
  across 3 hosts with zero manual instrumentation; real Helm workloads and
  shadow traffic under chaos + soak (8.5).
- **M5 - Inference layer:** model store, shared weights, paged KV, tile IR on
  one GPU family. Gate: P9, P10; serve a 7B-class model at throughput within
  15% of vLLM-on-tuned-Linux on identical hardware, under production traffic
  shapes (8.5).
- **M6 - POSIX personality + SSH.** Gate: P11.

Each gate is also a decision point: continue, descope, or stop. The two
research-grade risks (distributed revocation, lease/failure semantics) must
prove out by M3 or the cluster story reverts to single-host + explicit
federation.

### 8.7 Risk register (ranked)

1. **Distributed revocation at scale** - research-grade. Mitigation: short
   TTLs, epoch coarsening, honesty about eventual semantics; M3 gate.
2. **Ecosystem gravity** - drivers, tools, habits. Mitigation: narrow device
   matrix (see TARGET-ARCHITECTURES.md), personalities at every edge,
   kubectl/OTLP/POSIX compatibility from day one.
3. **Transport library maintenance** - owning TCP/QUIC forever, meeting
   pathological middleboxes Linux met decades ago. Mitigation: one blessed
   library, aggressive interop test corpus, QUIC-first posture.
4. **Compilation service vs CUDA's kernel moat.** Mitigation: contained
   vendor cells for peak paths; portable investment concentrated on the ~20
   kernels that are ~95% of inference FLOPs; the autotune cache.
5. **Scheduler-activation complexity** repeating its 1990s failure.
   Mitigation: it is only viable because no blocking syscalls exist; P3-P5
   and runtime-introspection tooling gate it.
6. **Proof effort overrun** on the capability core. Mitigation: seL4 fallback
   path defined in 8.2.
7. **Scale of effort.** A production-grade, cloud-capable OS foundation is a
   multi-year, well-resourced systems program, not a spike. The milestone
   gates front-load the research-grade risks (capability proofs, distributed
   protocols) so they are settled early; everything after is disciplined
   production engineering to the bar in PRODUCTION.md. The plan is structured
   to *de-risk* aggressively, not to justify shipping something thin.

---

## 9. Open questions

- State store consistency: HLC causal+ (chosen default) vs TrueTime-style
  commit-wait - revisit if external consistency becomes a hard requirement.
- Deadline inheritance across dependency graphs (RT task waiting on a slow
  service) - *resolved:* budget propagation along the flow ID, adapting
  seL4's scheduling-context donation to async graph edges (SCHEDULING.md 10).
  A serviced node runs on the originating reservation's budget, not the
  service's, so urgency is inherited and graph-level priority inversion is
  removed. Lease TTL remains the failure mechanism.
- Sketch sizing/rotation policy for adversarial DDoS conditions.
- How far the uniform engine contract stretches across NPU families before
  per-family quirks dominate (estimate: 80/20).
- CHERI: if Morello/CHERI-RISC-V matures, software capabilities compile down
  to hardware ones. The Arch trait keeps the door open; not a dependency.

---

*Companion document: TARGET-ARCHITECTURES.md - supported ISAs, platforms,
accelerators, and the hardware floor.*
