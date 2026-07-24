# Virtualization and Hardware Acceleration

**Status:** Draft v0.1. New subsystem. Relates to EMULATION.md (guest mode,
cross-ISA), ACCELERATORS.md (engines), SECURITY-IDENTITY.md (trust tiers),
MEMORY.md (page tables), BOOT.md (attestation).

Position: the **container is the default and the cell is the container** -
near-zero-overhead isolation with no VM in the path. Full virtual machines are
a *heavier isolation tier* for the cases cells cannot cover: running other
operating systems, a hardware-enforced tenant boundary below the cell, and
confidential compute. The important claim of this document is that the
**acceleration primitives a VMM needs already exist in the foundation** -
because they are the same primitives cells use (IOMMU, nested paging, SR-IOV,
posted interrupts) - so a VM-hosting role is a described extension that reuses
them, not a second architecture.

## 1. Cells vs VMs - when each

| Need | Use |
|---|---|
| Isolate workloads, multi-tenant, run native or Linux-binary software | **Cells** (default) - CONTAINERS-KUBERNETES.md |
| Run a different OS (a Windows appliance, an unmodified Linux kernel, a legacy VM image) | **VM** |
| Hardware-enforced boundary stronger than the cell (regulatory, hostile multi-tenant) | **VM**, ideally confidential (section 7) |
| Cross-ISA occasional binary | Emulation inside a POSIX cell (EMULATION.md 4) |

Cells win almost always because a VM carries a guest kernel, a second
scheduler, and a second memory manager - all of which Lattice's cell already
provides once. VMs are the exception tier, priced honestly.

## 2. Container acceleration - the cell model is the acceleration

"Accelerating containers" on Linux means shaving the cost of namespaces,
cgroups, seccomp, and overlay filesystems stacked on a process. Here there is
nothing to shave because none of that is stacked - the cell **is** the
primitive. What makes cells fast:

- **Cheap context switches** via address-space tagging (PCID on x86, ASID on
  ARM): switching between cells does not flush the TLB; switching between
  strands of one cell touches no page tables at all (MEMORY.md, CONCURRENCY.md).
- **No per-container overlay-FS cost:** images are content-addressed sealed
  objects mapped read-only and shared across cells - one physical copy, no
  per-container copy-up (CONTAINERS-KUBERNETES.md 1, FILESYSTEMS.md).
- **No syscall-interception tax for native cells:** native software talks
  queues directly; only the POSIX personality pays translation cost, and it
  batches syscalls through the queue ABI and provides a vDSO-equivalent for
  hot read-only calls (clock, getpid) to avoid round trips (POSIX-
  PERSONALITY.md).
- **Hardware isolation aids** for intra-cell hardening, exposed via the Arch
  trait: MTE tagging (ARM), MPK/PKS protection keys (x86) for personalities
  that host multiple legacy processes in one cell.

Net: the cell start/stop and per-operation costs are microseconds and
nanoseconds, not the tens-of-milliseconds container cold-start of a full OCI +
CNI + overlay stack.

## 3. VM acceleration - the vCPU as an engine

When a VM is needed, it slots into the existing object model rather than
introducing a new one:

- A **vCPU is an engine** (ACCELERATORS.md 1): the VMM cell submits guest
  execution as work, the kernel schedules it onto a physical core with the
  same reservation and pool machinery as any engine (SCHEDULING.md). A VM's
  "importance" is a resource contract, identical to a database's.
- **Guest physical memory is a memory kind** - a grant with a guest-physical
  address space, placed and metered like any typed memory (MEMORY.md).
- The VMM itself is an ordinary cell holding: vCPU engine grants, a guest-
  memory grant, and device-backing capabilities. It cannot exceed those - a
  compromised guest is bounded by the VMM cell's grants exactly like any
  workload.

## 4. Hardware acceleration used

Everything a modern VMM relies on, mapped to where it lives here:

| Hardware feature | Role | Where it lands |
|---|---|---|
| VT-x / AMD-V / RISC-V H-ext | CPU virtualization (guest/host mode) | vCPU engine entry/exit behind the Arch trait |
| **EPT / NPT** (nested page tables) | Guest-physical -> host-physical translation in hardware | The same two-level translation the page-table Arch trait already programs; guest memory grant supplies the second level (MEMORY.md) |
| VPID / ASID tagging | Avoid TLB flush on VM entry/exit | Same tagging used for cheap cell switches (section 2) |
| **Posted interrupts / APICv / AVIC** | Deliver interrupts to a guest without a VM exit | The kernel's interrupt-routing Arch trait; also backs user-level interrupt delivery for the strand doorbell (CONCURRENCY.md 4) |
| VMCS / VMCB | Guest state block | Managed by the vCPU engine implementation |

The point of the table: nested paging, interrupt posting, and address-space
tagging are **not VM-specific additions** - they are already in the foundation
for cells and strands, so the VMM reuses them.

## 5. I/O virtualization

- **SR-IOV** is the natural fit: a virtual function (VF) of a NIC, NVMe
  device, or GPU **is an engine grant** handed to a VMM cell or straight to a
  guest - the same way a cell owns a NIC queue (NETWORKING.md 1). The IOMMU
  isolates the VF; no software data-path.
- **VFIO-style passthrough** is already how engines work here - every device
  DMA is IOMMU-mediated and grant-checked (doctrine 1), so passing a whole
  device to a guest is just an engine grant with no host driver in the path.
- **virtio / vhost / vDPA** for para-virtual devices: implemented as a
  device-backing cell serving the guest over the queue ABI. A vDPA device
  (hardware datapath, virtio control plane) maps cleanly - hardware queues to
  the guest, control through the backing cell.
- Para-virtual (non-passthrough) devices are **degraded-trust engines** from
  the guest's view, marked as such - the same honesty the design applies to
  any shared-with-firmware engine (ACCELERATORS.md 2).

## 6. GPU/accelerator virtualization

- **SR-IOV GPUs and MIG** map directly onto the spatial-partitioning model
  already used for accelerators (ACCELERATORS.md 3): a partition is an engine
  capability, whether it goes to a cell or a guest.
- **Mediated devices (mdev)** - a host driver multiplexing one device across
  guests - run as a contained driver cell doing the multiplexing, never in
  the kernel (the QCE doctrine).
- Para-virtual GPU (API-forwarding) is possible via a backing cell but is the
  slow, degraded-trust path; passthrough or SR-IOV is preferred.

## 7. Confidential compute

- TDX (Intel), SEV-SNP (AMD), and ARM CCA are treated as **stronger host-
  identity / trust tiers**, not a separate mechanism. A confidential guest (or
  a confidential cell, where the hardware allows) gets memory the host cannot
  read and an attestation report that extends the boot chain
  (BOOT.md 1, SECURITY-IDENTITY.md 8).
- This is the answer to the "hardware boundary below the cell" need and to the
  physical-attacker case the base threat model excludes: run the sensitive
  workload in a confidential VM whose attestation the cluster policy requires.
- Integration point: the attestation chain is designed so a confidential host
  slots in as a stronger class without model changes - the trust tier is a
  property on the host identity, consumed by placement policy.

## 8. Live migration - why checkpoint/restore instead

- Lattice does **not** do transparent live migration of running VMs or cells
  (doctrine 4: no transparent movement). Movement is explicit **checkpoint/
  restore**, priced honestly (CONTAINERS-KUBERNETES.md 4).
- For VMs this means a coordinated pause, state capture (vCPU + guest memory +
  device state), and restore elsewhere - a scheduled, observable operation,
  not a background page-chase that silently degrades the guest.
- **RNG safety is mandatory on restore:** a restored VM or cell reseeds its
  DRBG, kernel-enforced, and DRBG state is excluded from the checkpoint image -
  because resumed-snapshot RNG reuse is a real key-duplication bug class
  (TIME-IDENTITY.md 4).

## 9. Scope and honest costs

- **Cells are the product; the VM-hosting role is a described extension.** The
  foundation supports it (sections 3-7 reuse existing primitives), but a
  production-grade VMM is a later milestone, not part of the initial build
  (this refines EMULATION.md 3, which flagged VM-hosting as out of scope -
  the *acceleration primitives* are in scope from the start; the *VMM
  product* is not).
- Guest performance depends on hardware: without EPT/NPT and posted
  interrupts a VM is slow; without device SR-IOV or passthrough, I/O goes
  through a backing cell with real cost. The grant layer exposes what each
  platform actually offers (typed-hardware doctrine).
- Nested virtualization (a guest itself running guests) is supported only
  where the hardware supports nested EPT/VPID; it is a niche and marked
  degraded.
- A VM defeats many of the design's structural advantages inside the guest
  (the guest has its own scheduler, its own opaque memory, no flow-ID
  tracing across the boundary). That is the price of running foreign OSes and
  is exactly why cells, not VMs, are the default.
