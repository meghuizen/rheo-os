# Production Readiness - Quality Bar and Hardware Breadth

**Status:** Draft v0.1. Sets the engineering standard for the whole project.
Relates to ARCHITECTURE.md 8 (verification), TARGET-ARCHITECTURES.md (the
hardware matrix), ACCELERATORS.md (the engine/driver model), CLOUD.md (cloud
deployment), and BUILD-ORDER.md (sequence).

Position: the **foundation is production-grade, not an MVP.** It is built to
the robustness, hardware breadth (within the server/cloud class), reliability,
security hardening, and operability of a Linux-class platform carrying real
production workloads - and it is deployable both inside public clouds (as a
guest) and as a host OS an infrastructure operator could run their fleet on.
Sequencing subsystems (BUILD-ORDER.md) is about *order*,
never about *quality*: each layer that ships is finished to this bar, not
stubbed for a demo.

## 1. What "production-grade" means here, concretely

Every shipped layer must meet all of these, not a subset:

- **Full error handling.** Every failure path is handled and observable - no
  `unwrap()` in the kernel outside proven-infallible spots, no silent
  swallowing, no "impossible" branches left unhandled. Errors are events
  (doctrine 7).
- **Hardware breadth for its class.** Not virtio-only. Real enterprise NICs,
  NVMe, GPUs, and server platforms are supported to a usable standard before a
  layer is called done (section 3).
- **Security hardening on by default.** W^X, stack protection (CET/BTI/PAuth),
  ASLR-equivalent for cells, MTE where present, spectre-class mitigations
  where they matter, fuzzed parsers - not opt-in flags (SECURITY-IDENTITY.md,
  TARGET-ARCHITECTURES.md 4).
- **Reliability targets** (section 4): defined SLOs, graceful degradation, no
  single points of failure in the control plane, tested crash/recovery.
- **Operability** (section 5): upgrade/rollback, observability, capacity
  management, debuggability in production - built in, not bolted on.
- **Performance to the gates** (ARCHITECTURE.md 8.4), sustained, not just peak
  in a microbenchmark.
- **Test and proof coverage** proportional to blast radius: proofs for the
  capability core, Jepsen for the state store, fuzzing for every parser, loom
  for every lock-free structure (TOOLING.md 5).

The org engineering rules apply throughout: simplest code that solves the
problem, surgical changes, verify before claiming done. Production-grade is not
gold-plating; it is *finished*, not *speculative*.

## 2. What is production from day one vs earned over time

Being honest about the one thing repeated throughout this design - Linux's
real moat is its driver and hardware-support ecosystem, a multi-year effort:

- **Production-grade from the start:** the core primitives (capability core,
  scheduler, memory, queues, cells), the cluster control plane, and the
  **virtualized/cloud hardware path** - because in a VM or a cloud instance
  the "hardware" is a small, standardized set of paravirtual devices
  (virtio / ENA / gVNIC / NetVSC), which is a *bounded, fully achievable*
  target. This is the key insight: **cloud and virtualized deployment reaches
  production hardware completeness far sooner than bare metal**, because the
  device surface is small and standard (CLOUD.md).
- **Earned over time to Linux-class breadth:** the long tail of bare-metal
  enterprise devices. The foundation is *architected* to make this achievable
  (stable engine ABI, contained driver cells - section 3) rather than
  achieving the whole tail on day one. We do not pretend a greenfield OS
  matches Linux's device breadth immediately; we make the path to it real and
  keep each supported device production-quality.

## 3. Hardware-support strategy (Linux-level, within the class)

The goal: production-quality support for the hardware a server/cloud fleet
actually runs, with a driver model that scales to breadth without ever putting
vendor code in the kernel (the QCE doctrine, ACCELERATORS.md).

### 3.1 The driver-cell model at production scale

- Drivers are **contained cells** holding only their device's queue and IOMMU
  grants. The kernel surface is queues, IOMMU mappings, reset, partitioning,
  attestation, and metering - vendor-free and stable.
- A crashing or buggy driver is bounded to its cell and restartable; a driver
  fault is an event, not a kernel panic. This is *more* robust than Linux's
  in-kernel drivers, and is what makes broad third-party hardware support safe
  to accept.
- The **engine ABI is versioned and stable** so vendors (or the community) can
  write drivers against a fixed target - the precondition for breadth.

### 3.2 The production hardware matrix (bare metal)

Targeted to production standard, in priority order:

- **NICs:** the enterprise mainstream first - Mellanox/NVIDIA ConnectX
  (RDMA/RoCE), Intel (E810 class), Broadcom - including SR-IOV and inline
  crypto offload; then DPUs (BlueField, Pensando). These carry the fleet's
  east-west and edge traffic (NETWORKING.md).
- **Storage:** enterprise NVMe (1.4+, SGL, multiple namespaces, PLP), ZNS/FDP
  where present; SAS/SATA via an HBA driver cell for interchange.
- **GPUs/accelerators:** NVIDIA (Ampere->Blackwell) and AMD CDNA to production
  serving/training standard, via contained vendor driver cells for peak paths
  plus the portable tile-IR route (AI-ARCHITECTURE.md, ACCELERATORS.md); NPUs
  and FPGAs as engines.
- **Platform:** current Intel Xeon / AMD EPYC and ARM64 server SoCs (Graviton,
  Ampere, Grace) - full platform bring-up (IOMMU, RDT/MPAM, PCIe, ACPI/DT,
  RAS/error reporting, thermal/power telemetry), not just boot.

### 3.3 Leverage strategies (to reach breadth without a decade of writing)

Options the design keeps open, each honestly caveated:

- **A Linux-driver compatibility path** for the long tail: run an unmodified
  Linux driver inside a dedicated **driver VM** (VIRTUALIZATION.md) with the
  device passed through by IOMMU, bridging it to the engine ABI. Buys
  enormous breadth at the cost of a Linux kernel in that VM - used for
  peripheral/legacy devices, never for the hot path, and clearly a
  compatibility bridge, not the native model.
- **Community/vendor drivers** against the stable engine ABI once it is
  published.
- **Reuse of hardware-facing logic** (register maps, init sequences) from
  permissively-licensed sources where clean-room porting is impractical.

The bare-metal long tail is where breadth is *earned*; the driver-cell model
and these leverage paths are what make earning it tractable.

## 4. Reliability

- **Defined SLOs** per service tier (control-plane availability, placement
  latency, data durability) with error budgets tracked from the metering data.
- **No control-plane single point of failure:** consensus is quorum-based;
  the state store is replicated; any single host loss is a lease-expiry event
  the cluster reconciles around (CLUSTER.md, ARCHITECTURE.md 4.8).
- **Graceful degradation:** under overload tenants degrade by contract; the
  system pool protects the control plane (SCHEDULING.md 2); memory pressure is
  cooperative before forced (MEMORY.md 7); clock degradation self-fences
  rather than corrupts (TIME-IDENTITY.md 1).
- **Tested crash/recovery:** power-loss, kill-a-node, partition, and
  clock-skew drills are part of CI (the Jepsen-style suite, ARCHITECTURE.md
  8.3), not manual.
- **RAS:** hardware error reporting (ECC events, PCIe AER, MCE) is surfaced as
  events; a failing device is drained and its driver cell isolated.

## 5. Operability

- **Upgrade and rollback:** atomic A/B system images with automatic rollback
  (BOOT.md 2); rolling fleet upgrades driven by desired state; workloads move
  by checkpoint/restore, priced honestly (CONTAINERS-KUBERNETES.md 4).
- **Observability built in:** flow-ID tracing, typed events, metering as
  metrics, OTLP export to standard backends (OBSERVABILITY.md) - so a
  production incident is a query, not archaeology.
- **Production debuggability:** capability-gated, audited debug and probe
  access on live cells (WASM probes, TOOLING.md 6) - powerful without a global
  back door.
- **Capacity and quota:** per-tenant budgets, reservations, and metering make
  capacity planning and multi-tenant fairness first-class (CLUSTER.md 6).
- **Compatibility surface for existing ops tooling:** kubectl/Helm/GitOps and
  OTel/Prometheus work through the edges (CLUSTER.md 5), so teams operate it
  with familiar tools.

## 6. The security bar for multi-tenant host workloads

A host OS that an infrastructure operator (a cloud provider being the most
demanding) runs untrusted tenant workloads on demands the strong end of the
threat model (SECURITY-IDENTITY.md 8): capability isolation plus, where a
hardware-enforced boundary is required, confidential-compute VMs
(VIRTUALIZATION.md 7). The multi-tenancy primitives are cryptographic
(sub-trust-domains), not label-based, and the audit trail is the grant chain.
These are the *primitives a provider builds their multi-tenant platform on* -
supplying them is a precondition for being an attractive host OS at scale
(CLOUD.md 6). We provide the isolation, metering, and attestation substrate;
the operator builds the hosting product.

## 7. Quality and battle-testing reinforce each other

The production bar is not only the goal - it is the precondition for the
strongest validation available. Once the foundation is genuinely production-
grade, real production workloads can be thrown at it: real databases, real
inference serving, real Kubernetes workloads, real training jobs, mirrored
production traffic, chaos, and multi-week soak (ARCHITECTURE.md 8.5). This is
circular in the productive sense:

- **Quality is the entry ticket.** You cannot mirror production traffic onto a
  stub, run a real database on a half-finished storage layer, or soak-test a
  system that leaks by design. Only a production-grade foundation *earns the
  right* to be tested this way.
- **Battle-testing is what secures the quality.** No proof, microbenchmark, or
  Jepsen run reproduces what a real workload under real traffic exposes over
  days: the fragmentation, the drift, the tail-latency pathologies, the
  interaction bugs. Real workloads are how the foundation is hardened to - and
  kept at - the bar.

So the two are one loop, not two phases: build to production quality, throw
production use cases at it, let what breaks drive the next round of hardening.
The portability edges (POSIX personality, the Kubernetes API, standard OTel)
exist partly for this reason - they let real, unmodified production software
run on the foundation as its toughest test, and let the same workload be diffed
against Linux. This is the difference between an OS that has been *demonstrated*
and one that has been *run in anger*.

## 8. What this does not claim

- Not Linux's *full* device breadth on day one - that tail is earned, with the
  path made real (section 2, 3.3).
- Not a general-purpose desktop/mobile OS (TARGET-ARCHITECTURES.md 8).
- Not "done" at any milestone gate in the MVP sense - gates certify a layer is
  production-ready for its scope, and scope widens deliberately over the
  program, always at this quality bar.
