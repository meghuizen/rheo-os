# Cloud - Running In the Cloud, and as a Cloud Provider's Host OS

**Status:** Draft v0.1. Relates to VIRTUALIZATION.md (guest mode, VMM role),
PRODUCTION.md (quality bar, hardware breadth), NETWORKING.md, SECURITY-
IDENTITY.md (multi-tenancy), CLUSTER.md (fleet operation), BOOT.md
(attestation).

**We are not building a cloud.** This project builds a *host operating system*.
The relevance to cloud is two-fold, and neither is operating a hosting
business:

- **Run in the cloud** - Lattice as a guest on AWS, Azure, GCP, and
  KVM/OpenStack, to production standard. This is reachable *early* because a
  cloud instance's hardware is a small, standardized set of paravirtual
  devices, and it is also the natural first place to battle-test real
  workloads (ARCHITECTURE.md 8.5).
- **Be the host OS a cloud provider could build on** - the substrate a
  hyperscaler or infrastructure operator runs on their bare-metal fleet,
  *underneath* their own virtualization, control, and tenant-facing services.
  The pitch is not that we host tenants; it is that this OS's modern primitives
  - the queue-based data plane, capability isolation, the engine model for
  accelerators and DPUs, confidential-compute integration - are what a
  provider's platform team would want in a host OS designed for exactly the
  hardware and workloads a modern cloud runs. We provide the host OS and its
  primitives; the provider builds the cloud on top.

## 1. Why "run in the cloud" reaches production fast

Bare-metal breadth is the long pole (PRODUCTION.md 2). Cloud is the opposite:
every major cloud exposes a **small, well-documented, standardized device
surface** to guests. Support those devices well and you have production-
complete hardware coverage for that entire cloud - no long tail. This is why
the virtualized/cloud path hits production quality far sooner than bare metal,
and why it is a priority target, not an afterthought.

## 2. The guest device surface (what to support, per cloud)

All reached through the same primitives - a paravirtual device is an engine
whose backend lives in the hypervisor; the driver is a contained cell
(ACCELERATORS.md 1, VIRTUALIZATION.md 5).

| Cloud | Network | Storage | Notes |
|---|---|---|---|
| **AWS** | ENA (Elastic Network Adapter); EFA for RDMA-class HPC/ML | NVMe (EBS + instance store) | Nitro exposes clean NVMe + ENA; strong fit |
| **Azure** | NetVSC (Hyper-V/VMBus) + Mellanox accelerated networking (SR-IOV) | StorVSC + NVMe | Hyper-V VMBus transport needed |
| **GCP** | gVNIC (and virtio-net legacy) | NVMe (Persistent Disk / Local SSD) | gVNIC is the modern path |
| **KVM / OpenStack / on-prem** | virtio-net, vhost, vDPA | virtio-blk / virtio-scsi, NVMe | virtio is the baseline everywhere |
| **All** | virtio-rng (entropy), a PV clock, PV console | | see sections 3-4 |

Production requirement: the mainstream of these (ENA, gVNIC, NetVSC, virtio
family, cloud NVMe) is supported to a usable standard - SR-IOV accelerated
paths included where the instance offers them - before "runs in cloud X" is
claimed. virtio is not a toy here; it is the genuine production datapath for
virtualized and many cloud environments.

## 3. Cloud identity, metadata, and attestation

- **Instance metadata (IMDS):** a metadata client cell reads the cloud's
  metadata service (AWS IMDSv2, Azure IMDS, GCP metadata server) for instance
  identity, placement, and network config. Treated as an external, partly-
  trusted input (validated, never blindly executed).
- **Cloud attestation feeds host identity:** AWS Nitro attestation, Azure
  attestation, GCP Confidential VM / Shielded VM measurements extend the boot
  chain (BOOT.md 1) - the cloud's own root of trust becomes evidence in the
  host identity, with the trust tier marked accordingly (a normal cloud VM is
  a weaker tier than a bare-metal TPM root; a Confidential VM is a stronger
  one - section 6, SECURITY-IDENTITY.md 8).
- **Entropy:** virtio-rng plus local jitter, mandatory reseed on any snapshot
  restore, entropy class marked "hypervisor-fed" in attestation
  (TIME-IDENTITY.md 4). Snapshot/clone RNG reuse is structurally prevented.
- **Time:** cloud PV clocks and NTP/chrony-class sync give a wider error bound
  e than PTP-equipped bare metal; leases widen accordingly and honestly
  (TIME-IDENTITY.md 1). Some clouds now offer high-accuracy time (e.g. PTP-
  backed) - consumed when present to tighten e.

## 4. Cloud bring-up and lifecycle

- **First-boot configuration:** a cloud-init-equivalent - the reconciler reads
  IMDS + a provided desired-state manifest and converges the node (no
  imperative boot scripts; it is the same desired-state model as the fleet,
  CLUSTER.md 3).
- **Autoscaling:** nodes join and leave the trust domain via the standard
  attest-and-join / lease-expiry flow (CLUSTER.md 3); scaling the fleet is the
  cloud's autoscaler creating/destroying instances that self-join.
- **Images:** the content-addressed system image (BOOT.md 2) is published as a
  cloud machine image (AMI / Azure image / GCE image) per supported cloud;
  A/B rollback still applies within the instance.
- **Block and object storage:** cloud NVMe volumes back the native object
  store or Ceph OSDs (CLUSTER.md 7); cloud object stores (S3/Blob/GCS) are
  reached by a gateway cell speaking their API at the edge (DATA-FORMATS.md 6)
  for import/export and tiering.

## 5. Networking in cloud

- Cloud SDN gives each instance a virtual NIC; Lattice owns its guest NIC
  queues exactly as bare metal (NETWORKING.md 1), with the cloud's SR-IOV
  accelerated networking used when the instance type offers it.
- **RDMA-class fabrics** (AWS EFA, Azure InfiniBand HPC SKUs) are used for
  east-west shared-compute where available; otherwise the mTLS/QUIC transport
  fallback carries it (NETWORKING.md 3, CLUSTER.md 6).
- The DDoS pre-steering and edge machinery (NETWORKING.md 5) runs in front of
  internet-facing gateway cells; at the cloud's own edge it composes with the
  provider's DDoS protection rather than replacing it.

## 6. Why a cloud provider would want this as their host OS

The repositioning: a provider does not want *us* to run a cloud - they want a
host OS that makes *their* cloud better. What such a platform team values in a
host OS, and what Lattice provides as reusable primitives (they build the
tenant-facing product on top):

- **Isolation primitives built for multi-tenancy.** Capability multi-tenancy
  (sub-trust-domains) is cryptographic, not label-based (SECURITY-IDENTITY.md,
  CONTAINERS-KUBERNETES.md 4) - a stronger substrate for tenant separation
  than namespaces-on-a-shared-kernel.
- **Confidential-compute integration** for the hardware-enforced boundary a
  provider needs for hostile multi-tenancy: guest workloads in TDX / SEV-SNP /
  CCA VMs whose memory the host cannot read, attestation required
  (VIRTUALIZATION.md 7). This is exactly the boundary the VMM role exists to
  give a provider.
- **A queue-based, DPU-native data plane.** The design already pushes the data
  plane off the host CPU onto SmartNICs/DPUs (NETWORKING.md 7) - the direction
  every hyperscaler's own host architecture has moved (Nitro-style offload).
  Lattice is built for that model natively rather than retrofitting it.
- **The engine model for accelerator fleets.** GPUs/NPUs as attested,
  partitionable, metered engines (ACCELERATORS.md) is what an AI-cloud
  operator needs to slice and account for expensive silicon.
- **Metering and quotas as primitives.** Every capability carries budget and
  usage counters (CLUSTER.md 4), so the *provider's* billing and fairness
  logic reads real per-tenant numbers - the host OS supplies the meter, the
  provider supplies the business logic.
- **Fleet-native operation:** attestation, desired-state, A/B rollback, and
  observability built in (CLUSTER.md, PRODUCTION.md 5) - the operational
  substrate a provider builds their control plane against.

In short: Lattice aims to be an attractive *host OS and set of primitives* for
whoever operates infrastructure at scale - a cloud provider being the most
demanding such operator. The tenant portal, billing system, region/zone
management, and marketplace are *their* product, not ours.

## 7. Honest costs and scope

- **We are building a host OS, not a cloud.** Operating a hosting service
  (tenant portals, billing, regions, SLAs to end customers) is explicitly out
  of scope - that is a provider's product built on top. What we owe is an
  excellent host OS with the primitives above, to the PRODUCTION.md bar.
- **Running in cloud is the near-term production target** and battle-testing
  ground; it is reachable because the guest device surface is bounded
  (section 1). A guest sees weaker isolation and timing than bare metal
  (virtual IOMMU quality varies, e is wider) - stated in the host's
  attestation tier, and acceptable for most workloads (VIRTUALIZATION.md 9).
- **Adoption as a provider's host OS is a longer arc** and depends on the VMM /
  confidential-compute maturity that VIRTUALIZATION.md flags as a later
  milestone, plus bare-metal hardware breadth (PRODUCTION.md 3). The
  *primitives* a provider would build on (capabilities, metering, sub-trust-
  domains, DPU offload, the engine model) are production from the start; pro
  adoption is earned by the foundation being genuinely better for their fleet,
  not by us shipping a hosting product.
- Per-cloud driver and attestation integration is real, ongoing work - each
  cloud is a distinct target with its own devices and trust root, supported
  one at a time to the PRODUCTION.md bar rather than all at once thinly.
