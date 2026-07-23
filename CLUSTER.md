# Running a Cluster - Orchestration, Shared Compute, Storage, and a Pi Lab

**Status:** Draft v0.1. Operational companion. Ties together CONTAINERS-
KUBERNETES.md (the absorbed model), ARCHITECTURE.md 4.8 (cluster
fundamentals), SCHEDULING.md (NUMA/placement), FILESYSTEMS.md (storage tiers),
NETWORKING.md (transports), TIME-IDENTITY.md (clocks/leases), BOOT.md (join),
and BUILD-ORDER.md steps 15-16 (when this gets built).

Short version: a cluster is **one trust domain of attested hosts sharing a
typed desired-state store**. There is no single-system-image illusion.
Orchestration is the Kubernetes model absorbed into the OS; `kubectl` and Helm
work through a compatibility edge. Shared compute is gang-scheduled graph jobs
over accelerator partitions. Storage is CephFS as a first-class client or the
native object store. And you can build a real (if reduced-trust) lab out of
Raspberry Pis - section 9 is the concrete parts list and bring-up.

## 1. What a cluster is here

- A **trust domain** rooted in one certificate authority / attestation root.
- **Hosts** that measured-boot, attest, and receive an identity (BOOT.md 6,
  SECURITY-IDENTITY.md 1).
- A **desired-state store** (typed, capability-scoped, watch-as-completion-
  queue) that is the cluster's source of truth (CONTAINERS-KUBERNETES.md 3).
- **Leases** that make failure an event, not a hang (ARCHITECTURE.md 4.8).
- A **tiny consensus surface**: only membership and placement need consensus;
  the data plane is peer-to-peer capabilities that keep working within a
  network partition until leases expire.

A single host is the same design with a trust domain of one - no "cluster
edition," no mode switch (doctrine 9).

The continuum extends below the host, too: within a single machine the kernel
is per-core (multikernel-style), and core-to-core communication uses the same
capability + queue + flow-ID mechanism as node-to-node, differing only in
transport (IPI + shared memory instead of RDMA or QUIC) and latency. A graph
edge across cores and a graph edge across hosts are the same construct at
different scales. See SCHEDULING.md 1a for the intra-machine multicore model.

## 2. Node roles

Roles are just which cells a host is placed to run; any host can hold several.

| Role | Runs | How many |
|---|---|---|
| **Control** | membership consensus (Raft-class) + the state-store replicas | 3 or 5 (odd, for quorum) |
| **Worker** | tenant workloads, controllers, inference cells | most of the fleet |
| **Storage** | native object-store engines, or Ceph OSD/MDS cells | 3+ for replication/EC |
| **Edge** | internet-facing gateway cells + the DDoS pre-steering stage | as ingress needs |

Control nodes should be dedicated in anything beyond a lab so consensus never
competes with tenant load - the system-pool carve-out (SCHEDULING.md 2)
protects the control plane on a shared node, but physical separation is
cleaner for the quorum members.

## 3. How a node joins

1. Measured boot produces an attestation report (BOOT.md 1).
2. The node presents it to the trust domain's registration service.
3. Policy checks firmware allow-list, entropy class, platform features
   (IOMMU present, and so on).
4. A host identity is issued (SPIFFE-shaped); short-lived credentials begin
   rotating.
5. The host enters membership consensus; desired state starts flowing; its
   reconciler (PID 1) converges the node.

Re-imaging a node is manifest-download + reboot (BOOT.md 7) - there is no
per-node mutable config to drift, which is what makes a fleet reproducible.

## 4. How orchestration works in the cluster

- **Desired state in, actual state out.** You write typed objects to the state
  store; controllers (ordinary cells with narrow watch/write grants) reconcile
  toward them. The reconciler on each host is the node agent - there is no
  separate kubelet protocol (CONTAINERS-KUBERNETES.md 2).
- **Placement engine** consumes the full topology graph - hosts, engines,
  memory kinds, network distance, fabric islands - and does coarse placement;
  each host's kernel does local dispatch (SCHEDULING.md 3). Placement respects
  gang (all-or-nothing) constraints for jobs that need many nodes at once.
- **Leases drive lifecycle:** a cell-group lease expiring *is* eviction; a host
  lease expiring *is* node failure; controllers observe expiry events and
  reconcile. Under partition, each side keeps serving with the capabilities it
  holds until leases lapse; only membership/placement pauses pending quorum.
- **Metering is built into capabilities**, so autoscalers and billing read
  real numbers, and scale signals (queue depth, reservation misses, pressure
  events) arrive on the event stream with flow context (OBSERVABILITY.md 6).

## 5. Full Kubernetes support (operationally)

The compatibility edge speaks the real Kubernetes API, so existing muscle
memory and tooling work from day one.

- **`kubectl`, Helm, and GitOps (Argo/Flux)** talk to the edge; it translates
  API objects into native ones (the mapping table in CONTAINERS-KUBERNETES.md
  5): Pod to cell group, Service to identity-named queue endpoint, Secret to
  minted capability, NetworkPolicy to grant policy, Node to host+attestation.
- **CRDs and operators keep working** - an operator is a controller cell; the
  difference is it can no longer exceed its grants (no more "every operator is
  cluster-admin").
- **Observability tooling works**: the OTel exporter cell speaks OTLP to
  Grafana/Tempo/Jaeger; Prometheus-style scraping is served from the metering
  data (OBSERVABILITY.md).
- **What genuinely does not translate**, stated honestly: `hostPath`,
  privileged pods, `hostNetwork`, node-SSH-as-admin, and tooling that assumes
  pod IPs or a Linux `/proc` on nodes (some node exporters, CNI-era
  operators). The edge documents each with its nearest native equivalent; a
  workload leaning on those needs adaptation.

Net experience: `kubectl apply -f deployment.yaml` works and behaves as
expected; the cluster underneath is cells, capabilities, and queues, not
containerd + CNI + a mesh.

## 6. Shared compute

The point of the cluster for AI/data workloads: pool accelerators and memory
across nodes and schedule them as one.

- **Accelerator partitions are engine capabilities.** A GPU MIG slice or an
  SM/NPU partition is granted to a cell exactly like memory (ACCELERATORS.md
  3). The placement engine hands partitions across the fleet, so "give this
  job 8 GPU slices spread over 4 hosts" is a placement decision, not manual
  wiring.
- **Graph jobs are the shared-compute primitive** (CONTAINERS-KUBERNETES.md
  4): a training step spanning hosts is one dependency graph - compute nodes,
  RDMA transfer nodes, and collective operations - gang-scheduled as a unit.
  Kubernetes has no such concept; frameworks fake it above the API.
- **Transport is location-transparent** (doctrine 9): the same typed queue
  pair is shared memory on one host, RDMA between RDMA-capable hosts, or
  mTLS/QUIC on commodity networks - chosen at connect (NETWORKING.md). East-
  west data moves NIC-to-NIC without either kernel touching bytes where the
  fabric allows.
- **Fair sharing is contracts, not priorities:** each tenant's slice of shared
  compute is a reservation with a budget, admission-checked; overrun is
  throttled, not trusted (SCHEDULING.md 4). A greedy job cannot starve the
  pool.

## 7. CephFS and shared storage

Three ways to give the cluster shared storage, pick per need:

- **CephFS as a first-class client (Tier 2, FILESYSTEMS.md 2).** A CephFS
  client cell holds grants to reach the Ceph OSD/MDS queue endpoints; RDMA
  transports map directly; Ceph's cephx auth is replaced by native workload
  identity. Use this to mount an existing Ceph cluster or a shared POSIX
  namespace during migration. HDFS and NFS attach the same way.
- **Run Ceph inside the cluster.** The OSD, MON, and MDS daemons run as
  storage-role cells (initially via the POSIX personality, since Ceph is
  Linux software), serving both Lattice clients and any external consumers.
  A lab or a migration bridge often does this.
- **The native object store (Tier 3).** Typed, versioned, content-addressed
  objects with erasure coding / replication as object-class policy across
  hosts and fabric islands, consuming the cluster's HLC clocks, leases, and
  membership directly (FILESYSTEMS.md 3). This is the destination; Ceph
  carries real data while it matures.

In all three, access is grants on object sets, and the bulk data path is DMA
graphs (NVMe to NIC to remote HBM peer-to-peer where hardware allows) rather
than copy-through-CPU.

## 8. NUMA and topology

- **Intra-node NUMA** (multi-socket servers): a cell is born with a home NUMA
  domain; its cores and memory come from that domain; work-stealing is
  topology-bounded (SCHEDULING.md 6). Threads follow memory, not the reverse.
- **Inter-node is "distributed NUMA."** The design treats remote host memory
  as its own **memory kind** with a microsecond latency class and no coherence
  (ARCHITECTURE.md 4.8) - the same typed-placement machinery that handles
  HBM-vs-DDR handles local-vs-remote. Placement reads one topology graph that
  spans both scales: NUMA distances *and* network distances.
- **CXL** memory pools appear as another kind (~2-3x local DRAM latency,
  stated). Capacity changes; the typed-placement answer does not.
- Practical consequence: the placement engine co-locates a job's compute with
  the memory and accelerators it will touch, at both the socket level and the
  rack/island level, from the same graph - and never pretends any of these
  distances are zero.

## 9. The Raspberry Pi lab

A Pi cluster is an excellent *learning and integration* lab and an honest
**reduced-trust, relaxed-floor tier** - not a supported production config. Be
clear-eyed about what it does and does not validate (section 10).

### 9.1 Why it is "reduced-trust"

A Pi violates parts of the hardware floor (TARGET-ARCHITECTURES.md 3):

| Floor requirement | On a Pi | Lab stance |
|---|---|---|
| IOMMU per-device isolation | Pi 4: none usable. Pi 5: IOMMU support is being upstreamed but young. | Pi 5 only; accept degraded device isolation, dev-only |
| Measured-boot hardware root | No built-in TPM; boot is firmware-first and partly proprietary | Add an SPI TPM module; treat attestation as best-effort, not a true root of trust |
| Hardware entropy | Present (Broadcom HW RNG) | Satisfied |
| ARM baseline v8.4 server | Pi 5 is Cortex-A76 (v8.2), GIC-400-class interrupts | Relax the baseline for the lab build |
| PTP hardware timestamping | Not on the Pi NIC | Software time sync; wider clock error bound e, wider lease windows (TIME-IDENTITY.md 1) |
| RDMA fabric | None | East-west uses the mTLS/QUIC transport fallback (NETWORKING.md) |

So the Pi lab runs a **relaxed target profile** and its hosts attest into a
lab trust domain that policy marks as reduced-trust - exactly the honesty the
"guest mode" and "degraded engine" patterns use elsewhere.

### 9.2 Parts list (a 4-node starter)

- **4x Raspberry Pi 5** (8GB or 16GB), Cortex-A76 quad-core. Pi 5 specifically,
  for PCIe and the emerging IOMMU.
- **4x NVMe HAT + small NVMe SSD** per node (PCIe on Pi 5) - real storage for
  the object store / Ceph OSDs, far better than SD cards.
- **4x SPI TPM module** (Infineon SLB9670 "LetsTrust"-class) - gives a
  measured-boot root, best-effort.
- **A managed gigabit switch** (2.5GbE if you want more headroom via the
  PCIe/USB path) - managed so you can play with VLANs and see steering.
- **PoE+ HAT or a multi-port USB-C PSU**, plus a stacked case with cooling
  (the A76 throttles hot under sustained load).
- **Optional: 1-2x Raspberry Pi AI HAT+ (Hailo-8/8L NPU, PCIe)** on a couple
  of nodes - a real, cheap accelerator to validate the engine/driver-cell
  model and a contained NPU driver cell (ACCELERATORS.md 5.2).

### 9.3 Bring-up

1. **Build the relaxed ARM64 image** (`cargo xtask` with the Pi profile:
   v8.2 baseline, GIC-400 interrupt path, Pi 5 IOMMU where available, HW RNG
   driver, SPI-TPM driver). DEVELOPMENT.md covers the compile/boot mechanics;
   the Pi adds a board-specific boot stub and device-tree handling.
2. **Flash and boot each node**, TPM module attached; the boot chain measures
   what it can into the TPM (best-effort root).
3. **Form the trust domain:** bring up the registration service on one node;
   each node attests and receives a (reduced-trust) identity (section 3).
4. **Place control roles on 3 nodes** (Raft quorum + state-store replicas);
   the 4th (and any more) are workers/storage.
5. **Desired state flows**; reconcilers converge; the cluster is live.

### 9.4 Running things on it

- **Kubernetes experience:** point `kubectl` at the compat edge and
  `kubectl apply` a Deployment - it lands as a cell group, scheduled by the
  placement engine across the Pis (section 5).
- **Shared storage:** run Ceph OSDs across the NVMe drives (as storage-role
  cells via the POSIX personality) and mount CephFS from client cells, or run
  the native object store with replication across the four nodes (section 7).
- **Shared compute:** if you fitted AI HAT+ NPUs, run a small inference graph
  job spanning two nodes - contained Hailo driver cells, gang-placed, traced
  end to end with one flow ID (OBSERVABILITY.md, AI-ARCHITECTURE.md).
- **Failure drills:** pull a node's power or network cable and watch leases
  expire, membership reconverge, and the workload reschedule - the most
  valuable thing the lab teaches, and cheap to do repeatedly.

### 9.5 What the Pi lab teaches - and what it does not

**Teaches well:** the whole control plane (identity, attestation flow,
state store, reconcilers, leases, membership consensus), Kubernetes-edge
compatibility, orchestration and placement across nodes, failure/partition
behavior, CephFS/object-store integration, the NPU engine + driver-cell model,
and end-to-end tracing.

**Does not validate:** the security guarantees (no real IOMMU/TPM root - it is
reduced-trust by construction), the performance claims (P1-P12 gate only on
the server hardware lab, ARCHITECTURE.md 8.4), intra-node NUMA (Pis are
single-domain - though inter-node "distributed NUMA" *is* exercised, section
8), RDMA and PTP-grade timing (absent - the mTLS/QUIC and software-time
fallbacks run, with honestly wider lease windows), and IX-scale DDoS
(the drop pipeline runs but at hobbyist packet rates).

## 10. Honest costs and scope

- The Pi lab's degradations are real and stacked - accept it as a *functional*
  and *distributed-systems* lab, not a security or performance one.
- Running Ceph via the POSIX personality inherits that personality's ~80%
  arbitrary-software fidelity (POSIX-PERSONALITY.md 5); most of Ceph works,
  edges may not, and that is itself useful signal.
- Software time sync on the Pi means wider e, so leases are slower to fence and
  the cluster is more conservative under partition than a PTP-equipped server
  fleet would be - a faithful demonstration of the clock-quality-as-safety-
  input design (TIME-IDENTITY.md 1), just tuned looser.
- For anything claiming production readiness, move to the server lab profile
  in TARGET-ARCHITECTURES.md 7; the Pi cluster is where you learn the system
  and shake out orchestration bugs cheaply first.
