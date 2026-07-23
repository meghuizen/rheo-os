# Deployment Profiles - One Foundation, Many Targets

**Status:** Draft v0.1. Defines how one foundation serves a wide spectrum of
workloads and form factors. Relates to ARCHITECTURE.md (the primitives),
TARGET-ARCHITECTURES.md (the per-profile hardware floor), and the per-subsystem
docs. This document supersedes the earlier "servers only, desktop/embedded
later" framing: those are now **profiles**, architected-in from the start and
delivered in phases.

Position: **one kernel and one set of primitives, built and configured into
per-target profiles.** The same foundation runs a fleet server, a firewall
appliance, a database box, an edge node, and (in later phases) an embedded
device or a desktop - the way one Linux codebase spans a home router and a
supercomputer. This is credible, not marketing, because the core abstractions
are form-factor-independent; it is honest, not overreach, because delivery is
phased and some targets are far harder than others.

## 1. Why one foundation can span this range

The primitives were chosen to not care about form factor:

- **A microkernel control plane** has a small trusted base that scales *down*
  to constrained devices and *up* to servers - the same reason seL4 runs on
  both microcontrollers and server hardware.
- **`no_std` Rust** already targets everything from microcontroller-class
  parts to datacenter CPUs; the kernel has no libc or OS-runtime assumption to
  drop.
- **Capabilities, cells, queues** are pure abstractions with no inherent size
  - a cell is a cell whether it holds a 200 KB sensor loop or a 200 GB model.
- **The engine + typed-memory model** already spans tiny NPU SRAM to HBM+CXL
  (ACCELERATORS.md, MEMORY.md); a sensor, a NIC, and a datacenter GPU are all
  engines.
- **Personalities at the edge** (POSIX, OCI/Docker, the Kubernetes API) supply
  software compatibility per profile without changing the native model
  (doctrine 10).
- **Partition-tolerant, local-first cluster design** (leases, tiny consensus,
  works-within-partition) is exactly what intermittent-connectivity edge
  deployments need (ARCHITECTURE.md 4.8).

A **profile** is therefore a build/configuration choice: which subsystems and
personalities are compiled in, which hardware floor applies
(TARGET-ARCHITECTURES.md), and which policies default on. The `xtask` build
already carries this idea (the "Pi profile" in DEVELOPMENT.md/CLUSTER.md); this
document names the full set.

## 2. What every profile shares (the invariant core)

No matter the profile: the capability core, cells, queues, typed memory,
the scheduler, leases, the event stream, and attestation. Security and
isolation never weaken by profile - a smaller device has fewer subsystems, not
a softer security model.

## 3. The profiles

### 3.1 Strong fits (the architecture was, in effect, designed for these)

| Profile | Why it fits | Profile-specific work |
|---|---|---|
| **Server / fleet** | The anchor target; everything above is built for it | none - this is baseline |
| **AI inference** | Foundational: engines, paged KV, tile IR, shared weights (AI-ARCHITECTURE.md) | model/serving services |
| **VM host OS** | vCPU-as-engine, SR-IOV, EPT/NPT, confidential compute (VIRTUALIZATION.md) | the VMM product (later milestone) |
| **Container / Docker-foundation OS** | Cells *are* containers; OCI import; runs as a guest or bare metal (CONTAINERS-KUBERNETES.md) | OCI converter, registry client |
| **Database appliance OS** | The "database is most important" contract: reservations, durability classes, group commit, typed memory, no OOM roulette (SCHEDULING.md 5, IO.md 2, MEMORY.md 7) | DB-tuned reservation policy, log-stream config |
| **Data warehouse OS** | Arrow in memory + Parquet at rest are *native*; storage-node pushdown; DMA graphs; columnar zero-copy (DATA-FORMATS.md 4, FILESYSTEMS.md) | query-engine integration |
| **Internet exchange** | Cell-owned NIC queues, line-rate steering, DPU offload, per-cell buffer isolation (NETWORKING.md) | route/peering control cells |
| **Firewall appliance** | WASM dataplane at three tiers, capability-gated, per-cell isolation, drop-cost inversion (NETWORKING.md 4-5) | policy/management plane |
| **Cloudflare-type edge** | Edge gateway cells, QUIC-native, WASM-workers-as-cells, DDoS drop pipeline, content via object store (NETWORKING.md, FILESYSTEMS.md) | CDN/cache logic, WASM worker runtime |

These profiles are among the project's most compelling, because the modern
primitives (queue data plane, WASM dataplane, DPU offload, columnar zero-copy,
durability-by-contract) are things these workloads fight the general-purpose
OS to achieve today. Networking appliances (IX, firewall, edge) and data
appliances (database, warehouse) are plausible *early wins*, not afterthoughts.

### 3.2 Harder profiles (foundation-capable, product-distant or new-work)

**Desktop.** The kernel can run a desktop: the Vulkan mapping, compositor
cells, capability-scoped HID, and the POSIX personality are the foundations
(GRAPHICS.md, POSIX-PERSONALITY.md). What is *far* off is the desktop
*experience* - a window-manager/app ecosystem, consumer GPU/peripheral driver
breadth, and deep power management. Honest stance: the foundation is
architected not to foreclose desktop, a basic compositor + Vulkan + POSIX apps
is reachable, but a competitive desktop *product* is a large, later ecosystem
effort, not a foundation deliverable.

**Embedded (MMU-class).** Modern embedded with an MMU - Cortex-A/R-class SoCs,
the Raspberry-Pi-and-up range - is a real target: the microkernel is small,
`no_std` already fits, and the RT reservation model (SCHEDULING.md 4) suits
embedded real-time. Profile-specific work: tiny-footprint builds (strip unused
subsystems), deterministic memory footprints, hard-RT-only configurations, and
board bring-up. Delivered in a later phase, architected-for now.

**IoT.** MMU-class IoT gateways and richer sensors fit as small embedded
profiles with the edge/offline features below. Constrained-but-MMU devices run
a minimal cell set; connectivity and power features (section 4) matter most
here.

**Remote / low-connectivity / low-energy** (the "remote Africa, flaky internet,
low power supply" case). This is a distinct profile combining several strengths
plus genuinely new work - covered in section 4 because it is the most
interesting of the new targets.

## 4. The remote / low-connectivity / low-power profile

This profile is where the design's partition tolerance and modern event-driven
primitives pay off, plus real new subsystem work.

**What already fits:**

- **Offline-first by construction.** The cluster design already keeps serving
  within a partition until leases expire, with a tiny consensus surface and a
  peer-to-peer capability data plane (ARCHITECTURE.md 4.8). A remote node
  disconnected from the wider fleet keeps operating locally and autonomously -
  exactly what flaky connectivity demands.
- **Degraded-transport tolerance.** Location-transparent queues run over the
  mTLS/QUIC fallback when there is no good fabric (NETWORKING.md 3); QUIC's
  connection migration suits links that come and go.
- **Clock honesty under bad sync.** Wide error bound e simply widens lease
  windows and makes the node more conservative, rather than corrupting
  (TIME-IDENTITY.md 1) - the right failure mode for poor time sync.
- **Event-driven, tickless design is inherently power-friendly.** No periodic
  HZ tick, no busy-poll by default, work only on events (SCHEDULING.md 1) -
  the modern primitives happen to align with low-energy operation, where a
  legacy tick-based OS wakes the CPU constantly.

**New work this profile requires (honestly, subsystems we deprioritized for
servers):**

- **Power management as a real subsystem.** DVFS (frequency/voltage scaling),
  deep idle states, race-to-idle scheduling, and per-engine power gating,
  exposed through the Arch trait like any hardware feature. Energy becomes a
  schedulable, metered resource - a natural extension of the reservation model
  ("budget of joules," not just cycles). This did not matter for a fleet
  server on wall power; it is central here. Full design in POWER.md.
- **Solar/battery awareness.** Policies that shed elastic load, defer
  non-urgent graph jobs, and scale down when on battery or low solar input -
  expressed as pressure events (MEMORY.md 7 pattern, generalized to energy).
- **Store-and-forward / opportunistic sync.** When a link returns, batched
  reconciliation of local state upstream; CRDT-friendly or last-writer-wins-
  with-HLC merge for state that diverged during disconnection (TIME-IDENTITY.md
  2). The append-log and content-addressed object classes (FILESYSTEMS.md) are
  good substrates for this.

This profile is a strong argument *for* the whole design: an OS that is
partition-tolerant, event-driven, and capability-secure by construction is a
better fit for austere, intermittent, low-power environments than a
server-tuned Linux - the same primitives, a different profile and policy set.

## 5. Phasing (honest order of delivery)

Capability is designed in for all profiles now; delivery is sequenced by where
the work and value land first. Each profile ships only when its validation
table in VALIDATION.md is green against its best-incumbent baseline:

1. **Server, AI inference, container/OCI, database, data-warehouse** - ride the
   core build order directly (BUILD-ORDER.md).
2. **Networking appliances** (IX, firewall, edge/Cloudflare-type) - follow the
   networking milestones; plausible early flagship wins.
3. **VM host OS** - after the VMM/confidential-compute work (VIRTUALIZATION.md).
4. **Remote / low-power / IoT / embedded (MMU-class)** - after the power-
   management and offline-sync subsystems (section 4) are built; the core
   already tolerates partitions.
5. **Desktop** - foundation kept open; a real product is the longest arc.

## 6. Honest boundaries

- **Sub-MMU microcontrollers are out.** The capability and cell model assumes
  an MMU (and an IOMMU for device isolation). The smallest Cortex-M-class,
  no-MMU parts are a different OS's job; "embedded" here means MMU-class
  embedded upward. This is a real, permanent-ish boundary, not a phase.
- **Desktop and IoT bring long driver/ecosystem tails** (consumer peripherals,
  sensors) - handled by the contained driver-cell model and the Linux-driver-
  in-a-VM bridge (PRODUCTION.md 3.3), but breadth is earned, as with all
  hardware support.
- **Phone/mobile handsets are not a target** - baseband, mobile power stacks,
  and the app ecosystem are out of scope; the primitives do not foreclose it,
  but nothing is aimed there.
- **Still greenfield, still modern-first** (ARCHITECTURE.md 1.4): these
  profiles target *modern* hardware in each class, not the resurrection of old
  or tiny legacy parts. Breadth of *form factor* expanded; the no-legacy-
  hardware-baggage stance did not.
- **Every profile holds the same security bar** (section 2). A smaller or
  remoter device has fewer subsystems, never weaker isolation.
