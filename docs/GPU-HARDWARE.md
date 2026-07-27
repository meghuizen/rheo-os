# Real GPU Hardware - PCIe, IOMMU, VRAM, and the Contained Driver Cell

**Status:** Building. Section 12 **stage 1 is done** (the `gpuhw` test, all
three ISAs) and **stage 2's IOMMU is done on both x86-64 (VT-d) and ARM64
(SMMUv3)** (the `iommu` test: a virtio-blk DMA is mediated by an IOMMU
domain and an out-of-grant DMA faults; RISC-V skip-with-reason, no QEMU
model): PCIe bridge recursion with kernel-programmed bus numbers, BAR
sizing + opt-in assignment, the capability walk (MSI/MSI-X/PCIe/FLR),
vendor recognition across the major GPU vendors (`kernel/src/hw/gpu.rs` -
AMD proven against QEMU's real `ati-vga` device model; NVIDIA/Intel
recognised by ID, skip-with-reason), and GPU engine registration behind
`SYS_ENGINE_INFO` enumeration. The rest is design. Expands ARCHITECTURE.md
objects 4 (engine) and 5 (memory grant) and the section 5 allowance
"queue/IOMMU/reset plumbing"; makes ACCELERATORS.md 1-2 concrete for
physical PCIe GPUs. Relates to GRAPHICS.md, DISPLAY.md 12, MEMORY.md 2,
AI-ARCHITECTURE.md 3-4, BOOT.md, and BUILD-ORDER.md steps 12 and 18.

Position: a real GPU is not a new kind of thing. It is an **engine**
(object 4) plus **typed memory grants** (object 5) plus the negative
constitution's one driver allowance - queue, IOMMU, and reset plumbing in
the kernel, everything vendor-shaped contained in a userspace driver cell.
This document adds **zero kernel objects and zero verbs**; like SMP.md and
MEMORY.md 2.1 it states its ARCHITECTURE.md 6 clearance explicitly, and it
designs only the plumbing and the containment. Everything policy-shaped -
command encoding, compilers, firmware blobs, allocator strategy - stays
outside the kernel, where ACCELERATORS.md already put it.

---

## 1. Scope and admission clearance

"Real GPU support" here means the path from a PCIe function with class code
0x03 to a vendor driver blob running contained: enumeration, register access,
DMA containment, device-local memory, firmware trust, reset, sync, and the
memory collaboration that makes tile-level compute and inference serving
work. The target is discrete PCIe GPUs (the ACCELERATORS.md 4 families:
NVIDIA, AMD CDNA, Intel Xe), approached emulation-first with QEMU stepping
stones (section 12) and hardware-lab gates for what QEMU cannot model.

The admission test (ARCHITECTURE.md 6), applied:

1. **Unforgeable enforcement:** the IOMMU mapping between a grant and a
   device's DMA is exactly the thing a library cannot provide - a cell that
   could program its own IOMMU entries could read any memory in the machine.
2. **Arbitrates shared hardware:** one GPU, many cells; partitions, queues,
   and VRAM ranges are contended resources handed out as capabilities.
3. **Mechanism with policy outside:** the kernel maps, meters, resets, and
   refuses; which commands to send, how to compile, how to sub-allocate VRAM,
   which model to load - all cell policy.

And the zero-new-surface claim, item by item: the engine object exists
(`kernel/src/engine.rs`, object 4); `MemKind::Hbm` and `MemKind::DeviceBar`
exist in the grant type (`kernel/src/mm/grant.rs`, object 5); the queue ABI
already reserves a per-entry `engine_id` routing field
(`kernel/src/queue/mod.rs`); dependency graphs (object 6) already carry the
cross-engine execution model. What this document designs is the **backing**:
making those existing types true for physical silicon, the same move
MEMORY.md 2.1 made for `MemKind::Pmem`.

---

## 2. What exists today (honest inventory)

Every later section builds against this table, so the document cannot
over-claim. "Claimed" is what the design docs assert; "built" is what is in
the tree.

| Claimed (docs) | Built (tree) |
|---|---|
| Engines attested + benchmarked at attach (ACCELERATORS.md 1) | The CPU `Engine` attach = a 4096-iteration integer-Add micro-benchmark (`engine.rs`); GPU engines register with an honest zero measured cost (no execution path yet) |
| Declared preemption contract | `Preemption::{Instruction, OpBoundary}` declared; a registered GPU engine declares `OpBoundary` (the accelerator contract) |
| Graphs execute across engines (object 6) | `graph.rs` runs up to 32 nodes sequentially on one `&Engine`; `SqEntry.engine_id` is in the ABI but read nowhere |
| GPUs enumerated as engines | **Stage 1, built:** `hw/pci.rs` recurses bridges (programming secondary bus numbers where firmware left them zero), sizes BARs by the mask probe, walks capabilities (MSI/MSI-X/PCIe/FLR), and offers opt-in BAR assignment (`assign_pci_bars`); `hw/gpu.rs` classifies every display-class function by vendor (NVIDIA, AMD, Intel, virtio, Bochs, Cirrus, VMware, Red Hat/QXL) AND silicon family (NVIDIA Pascal/Turing/Ampere/Ada/Hopper/Blackwell, AMD GCN/RDNA/CDNA, Intel Xe) into the machine inventory, each with a per-vendor driver front-end (`vendor_driver`) declaring its lowering path (ACCELERATORS.md 4); each registers in the engine table. **Every GPU QEMU models is driven**: AMD/Bochs/Cirrus/VMware/QXL by a framebuffer-aperture MMIO write+read-back and virtio-gpu by its 2D command driver (six vendors on x86-64, four on arm/riscv where VMware+QXL are x86-only). Proven by the `gpuhw` test on all three ISAs |
| Every engine DMA mediated and grant-checked (ACCELERATORS.md 1, doctrine 1) | **Stage 2 built on x86-64 + ARM64:** `hw/iommu.rs` (VT-d) and `hw/smmuv3.rs` (SMMUv3 stage-1) each bring up an identity domain and the `iommu` test proves a virtio-blk DMA is mediated - succeeds when granted, faults when revoked. The existing virtio drivers still hand raw `virt_to_phys` (identity IOVA==PA); RISC-V has no QEMU IOMMU model |
| Device memory as typed kinds (MEMORY.md 2) | `MemKind::{Hbm, Cxl}` silently DDR-backed; `MemKind::DeviceBar` refused by a literal `kind == 4` check at the syscall boundary (`kernel/src/user.rs`) |
| Engine introspection | `SYS_ENGINE_INFO(out_va, index)` enumerates the engine table - the CPU, then every recognised GPU with its PCI vendor ID - and returns the count |
| A real GPU device round-trip | virtio-gpu 2D driver (`hw/virtio_gpu.rs`, DISPLAY.md 12): kernel-resident, test-installed, synchronous busy-poll, `OP_GPU_PRESENT` is a CPU memcpy into a fixed 128x128 framebuffer |

Two facts in that table were assets from the start: the classification
already landed on `Gpu`, and the queue ABI already carries `engine_id` -
the wire format needs nothing new for multi-engine routing. What stage 1
did NOT change is just as load-bearing: no IOMMU, no VRAM backing, no
vendor command submission - a recognised NVIDIA/AMD/Intel GPU is
enumerated, sized, and registered, honestly not driven (sections 4-6 are
the design for driving one).

---

## 3. PCIe for real devices

The virtio drivers deliberately avoid BARs via the `VIRTIO_PCI_CAP_PCI_CFG`
config-space tunnel (DISPLAY.md 12). A real GPU ends that option - its
registers, doorbells, and VRAM aperture live behind BARs, and its
completions arrive by MSI-X. Who programs BARs turns out to be per-boot
(measured, stage 1): on x86-64 q35 the `-kernel` loader path runs SeaBIOS,
which does PCI init before the kernel boots; the bare arm/riscv `virt`
boots run nobody, and every BAR reads zero. The kernel therefore grows
real PCIe plumbing that works in both worlds, all of it mechanism:

- **Bridge recursion.** Enumeration walks root ports and switches (today:
  bus 0 only). Each discovered function gets a `PciFunction` record; the
  bus/dev/fn triple is the **requester ID** that keys IOMMU domains
  (section 4), which is why enumeration and containment are one design.
- **BAR sizing and assignment by the kernel.** With no firmware to inherit
  from, the kernel sizes each BAR (write-ones, read-back mask) and assigns
  addresses from the host bridge's MMIO windows with a bump allocator -
  32-bit non-prefetchable and 64-bit prefetchable ranges tracked separately.
  This is boot-time, single-pass, no hotplug (deferred, section 14).
- **Capability walks.** The standard list (MSI-X, PCIe capability for FLR)
  and the extended config space (AER, SR-IOV, ACS, resizable BAR). The walk
  records offsets into `PciFunction`; interpreting vendor capabilities is
  driver-cell business.
- **MSI-X routing to vcores.** The MSI-X table (itself in a BAR) is
  programmed by the kernel only: a vector's message address/data target a
  specific CPU's interrupt controller. A device interrupt becomes a typed
  completion on a queue - the Phase D/F interrupt work (UART RX, timer)
  already proved the park-until-interrupt shape per ISA; MSI-X generalizes
  the source. Section 10 builds semaphore wake on exactly this.
- **FLR as the reset substrate.** Function-level reset is the mechanical
  bottom of the engine reset verb (section 8): provable return to a known
  state without touching the rest of the machine.
- **AER as a typed event source.** Correctable/uncorrectable PCIe errors
  become events on the driver cell's queue, not log lines (doctrine:
  failure is an event).

```rust
/// One enumerated PCIe function. The requester ID (seg:bus:dev.fn) keys
/// the IOMMU domain; the capability offsets are recorded, not interpreted.
pub struct PciFunction {
    pub rid:        RequesterId,        // seg:bus:dev.fn
    pub ids:        (u16, u16),         // vendor, device
    pub class:      (u8, u8, u8),       // class, subclass, prog-if
    pub bars:       [Option<BarAssignment>; 6],
    pub msix:       Option<MsixInfo>,   // table BAR + offset, vector count
    pub flr:        bool,               // PCIe cap advertises FLR
    pub sriov:      Option<SriovInfo>,  // total VFs, VF stride (section 9)
}

/// A sized and kernel-assigned BAR. `DeviceBar` grants (section 5) are
/// windows into exactly these ranges - a grant can never name MMIO space
/// that enumeration did not assign.
pub struct BarAssignment {
    pub index:        u8,
    pub base:         u64,              // physical, kernel-assigned
    pub size:         u64,              // power of two, from the mask probe
    pub prefetchable: bool,             // the VRAM-aperture case
    pub is_64bit:     bool,
}

/// An MSI-X vector bound to a vcore. The device writes the message; the
/// kernel turns it into a typed completion on the owning cell's queue.
pub struct MsixVector {
    pub entry:        u16,
    pub target_vcore: u16,
}
```

What stays out of the kernel: config-space policy (power states, link speed
negotiation beyond what boot needs), vendor capability interpretation, and
hotplug. TARGET-ARCHITECTURES.md 4 already lists vector allocation and
MSI-X routing as Arch-trait concerns; the per-ISA delivery path (x2APIC /
GICv3 ITS / IMSIC) lands there, everything above it is portable.

---

## 4. IOMMU - the containment mechanism (device-neutral)

This section is the load-bearing one, and it is deliberately
**device-neutral**: NIC and NVMe containment cite it unchanged. It is the
repo's first concrete IOMMU design; when step 12 fully lands it lifts into
its own IOMMU.md with this section becoming a pointer (promotion clause at
the end).

**Built (stage 2): two IOMMU backends, both proven by the `iommu` test**
with a real device (virtio-blk, negotiating `VIRTIO_F_ACCESS_PLATFORM` so
its DMA is subject to the IOMMU). Both prove BUILD-ORDER step 12's
done-when: a block read **succeeds** through an identity domain (DMA
mediated, not blocked), then after the domain is **revoked** (mapped to
nothing) the same read **faults** and the driver reads the fault back.

- **x86-64 VT-d** (`kernel/src/hw/iommu.rs`): the `intel-iommu` register
  base is discovered from the ACPI DMAR table; the driver builds a root
  table, a shared context table, and a second-level page-table domain
  (identity, 2 MiB superpages), enables **queued invalidation** (QEMU's
  caching-mode IOMMU only tears down device shadow mappings via QI, not the
  register-based path), and sets translation-enable. The fault is read from
  the fault-recording register.
- **ARM64 SMMUv3** (`kernel/src/hw/smmuv3.rs`): the register base is the
  fixed QEMU `virt` address; the driver builds a linear **stream table**,
  a **Context Descriptor**, and ARM LPAE **stage-1** page tables (QEMU
  models stage-1 only - a stage-2 STE is rejected `C_BAD_STE`), drives a
  **command queue** for STE/TLB invalidation with a `CMD_SYNC`, and enables
  translation. Revoking marks the STEs invalid; the fault (`C_BAD_STE`) is
  read from the **event queue**.

RISC-V has no QEMU IOMMU model in 8.2, so it surfaces no register base and
skips-with-reason. The device-neutral design below is the full target
these two backends realize.

The claim to make true: ACCELERATORS.md 1 says "every engine DMA is
mediated and grant-checked," and BOOT.md 1 makes a missing or disabled
IOMMU a hard boot refusal. Today neither is backed by a line of code -
every driver hands raw physical addresses to its device.

**Domain lifecycle.** An IOMMU domain is created when an engine attaches
and destroyed when it detaches or is reset. The domain is keyed by the
device's requester ID (section 3). A domain starts **empty**: a freshly
attached device can DMA to nothing. There is no identity-mapped grace
period and no default-allow window - the device is offline until its first
mapping.

**The grant-to-mapping path.** The only way memory enters a domain is
through an existing, committed memory grant held by the owning cell:

```
cell holds Grant (committed, DMA right)
        -> kernel installs IOMMU mapping in the engine's domain
        -> device may DMA exactly that range, nothing else
```

This makes BUILD-ORDER step 12's done-when ("a device can only DMA into
buffers the owning cell granted") a direct consequence of the existing
grant check rather than a new permission system. Revocation composes the
same way: epoch-revoking the grant tears down the mapping before the
revocation completes - a device is never a way to use memory after free.

**DMA addresses are a distinct type.** The design's enforcement shows up in
the kernel's own code as a newtype:

```rust
/// An address meaningful to a device through its IOMMU domain. Produced
/// only by installing a mapping; never fabricated from a VA or PA.
pub struct DmaAddr(u64);

impl IommuDomain {
    /// The only constructor of DmaAddr: map a committed grant range into
    /// this domain. Fails if the grant lacks the DMA right or the range
    /// is not committed.
    pub fn map(&mut self, grant: &Grant, range: Range<usize>)
        -> Result<DmaAddr, IommuError>;
}
```

```rust
// This is a compile error - the descriptor field is typed DmaAddr, and
// virt_to_phys returns a bare physical address:
ring.desc[i].addr = arch::virt_to_phys(buf_va);
// error: expected `DmaAddr`, found `u64`
// The virtio drivers' raw-phys DMA (section 2) is exactly the hole this
// type closes; they migrate to `DmaAddr` when step 12 lands.
```

**Faults are typed events.** A device DMA outside its domain is not a
kernel panic and not a log line: the IOMMU fault (requester ID, faulting
address, read/write) becomes a typed event delivered to the owning cell's
queue, and the offending transaction is aborted. The engine's trust
standing degrades (section 8). A fault storm is budget-kill territory, the
same contract as compute.

**ATS/PRI stay off by default.** Address Translation Services let a device
cache translations and (with PRI) fault-and-retry; they also let a
malicious device probe. They are enabled per trust class only - an
exclusively-owned, attested device may negotiate them; a
shared-with-firmware device (section 7) never does. Paged-KV-style GPU MMU
cooperation (section 11) does not require PRI: block tables are programmed
ahead of use by the driver cell.

**Per-ISA formats, behind the Arch trait.** The portable layer speaks
domains, mappings, and faults; the per-ISA layer programs:

- **x86-64:** VT-d (and AMD-Vi later). QEMU q35 already boots with
  `-device intel-iommu,intremap=on` and split irqchip (DEVELOPMENT.md 5) -
  the emulated hardware for stage 2 of section 12 is already on the launch
  line.
- **ARM64:** SMMUv3. Likewise already on the launch line
  (`iommu=smmuv3`).
- **RISC-V:** the ratified RISC-V IOMMU spec - **skip-with-reason today**:
  QEMU 8.2's `virt` machine has no RISC-V IOMMU device model, so this ISA
  proves the portable layer against a null backend and gates the real one
  on emulator support, exactly the MEMORY.md 2.1 nvdimm pattern.

**Promotion clause.** When step 12 is implemented, this section becomes
IOMMU.md (device-neutral, citing NIC/NVMe/GPU as consumers) and
GPU-HARDWARE.md 4 becomes a pointer to it. Pre-authorized here so the split
is not re-litigated later.

---

## 5. The driver cell - containing the vendor blob

ACCELERATORS.md 1-2 asserts the vendor blob is contained, not trusted.
This section is the concrete interface: what the cell can touch, what the
kernel keeps, and why the blob's worst day is bounded. (DRIVERS.md 4
generalizes this interface to every device class - block, NIC, HID, LKL
cells hosting Linux driver code - so it is written once, here.)

**BAR windows are capabilities.** A `MemKind::DeviceBar` grant is a window
into a `BarAssignment` range (section 3), MMIO-mapped into the driver cell
uncached. This is what lifts the current blanket refusal (`kind == 4`,
section 2): device-BAR grants become real, but only over ranges enumeration
assigned, only to the cell holding the engine capability, and grant-checked
at map time like every other mapping.

```rust
/// The capability view of a BAR sub-range. Minted against an enumerated
/// BarAssignment; there is no way to name MMIO space outside it.
pub struct BarWindow {
    pub bar:    u8,
    pub offset: u64,
    pub len:    u64,
    pub write:  bool,       // register windows are rw; some ROMs are ro
}
```

**Doorbell pages are the grant-checked sub-case.** A doorbell write is a
work submission, so doorbell pages are minted as separate single-page
`BarWindow` grants. Handing a cell a doorbell is handing it the right to
submit to that queue and nothing else - the queue-pair model applied to
hardware rings.

**GPU page tables are programmed by the driver cell, not the kernel.**
This sounds like a containment hole and is the opposite, by a two-level
argument: the GPU's own MMU translates GPU-virtual to GPU-physical, but
every resulting bus access still resolves **through the IOMMU domain**
(section 4). The blob can corrupt its own device's address space; it cannot
name a single byte of host memory the owning cell's grants did not map.
The kernel neither parses nor mirrors vendor page-table formats - it would
be vendor logic in the kernel, doctrine violated with extra steps.

**No command-stream validation in the kernel.** The kernel does not parse
vendor command buffers. Containment, not validation, is the security
boundary: a hostile command stream can wedge or misprogram the device, and
the answer is the reset ladder (section 8) plus the IOMMU (the device
cannot be aimed at anyone else's memory). Linux's experience validating
command streams (a decade of GPU command-parser CVEs) is the cautionary
tale, and ACCELERATORS.md 6 already refuses it.

**Registration echoes the existing service pattern.** A driver cell
registers with the kernel the way the POSIX personality's `FileOps` does
(`kernel/src/svc.rs`): a small function-pointer vtable, kernel-resident
today, a documented bridge to a fully message-driven service later.

**Built in stage 1: the per-vendor front-end.** `hw/gpu.rs::vendor_driver`
resolves every recognised vendor to a concrete `VendorDriver` - its
lowering strategy (NVIDIA PTX/SASS + tensor-core tile IR, AMD MFMA via
ROCm/LLVM, Intel Vulkan-compute floor, virtio's 2D control queue), which
in-tree driver can drive it (virtio-gpu only, today), and an honest
per-environment status. The silicon family (`classify_arch` -> `GpuArch`)
picks that strategy per generation, not just per vendor ID. This is the
kernel-side half of the interface above; it names what each vendor needs
without executing any vendor command stream (which stays in the cell).

```rust
/// What a driver cell registers. The kernel calls these; everything else
/// the cell does happens over its own queues and BAR windows.
pub struct GpuDriverService {
    /// Probe the device behind these BAR windows; report VRAM size,
    /// engine count, firmware version (feeds sections 6, 7, 9).
    pub probe:  fn(&[BarWindow]) -> ProbeReport,
    /// Vendor-specific quiesce before FLR (section 8). Best effort;
    /// the kernel FLRs regardless when the budget expires.
    pub quiesce: fn() -> bool,
}
```

**Wedged blob recovery** is: kill the cell (budget or fault), FLR the
device (section 8), tear down the IOMMU domain, re-attach, re-attest,
restart the cell. Nothing the blob did survives into the next life except
what it wrote into its own persistent objects.

---

## 6. VRAM - device-local memory as a real typed kind

Key decision first: **no new MemKind.** Device-local VRAM - HBM on
datacenter parts, GDDR on the rest - is `MemKind::Hbm` (device-local,
high-bandwidth, not CPU-coherent), and the CPU-visible aperture into it is
`MemKind::DeviceBar` (section 5). Resizable BAR is precisely the case
where the aperture covers all of VRAM and the two views coincide; on
devices without it, most VRAM is reachable only by the device and by DMA,
which the typed model expresses naturally: a `Buffer<Hbm>` with no CPU
mapping is legal and common.

The MEMORY.md 2.1 PMEM pattern, transposed:

- **Discovery.** The kernel cannot size VRAM on most devices; the driver
  cell's `probe` (section 5) reports it, and the kernel records the span in
  the machine `Inventory` as it records an nvdimm's. Trust note: the size
  comes from the blob, so it is capacity metadata, never a security input -
  the IOMMU does not care how big the blob claims its memory is.
- **Allocation ownership is split like MEMORY.md 1.** The kernel grants
  VRAM **ranges** (coarse, power-of-two blocks out of the reported span) to
  cells as `Buffer<Hbm>` grants and meters them; the **driver cell owns the
  sub-allocator** within its ranges (buddy/slab/rings - vendor-shaped
  policy). An in-kernel VRAM allocator is refused (section 14).
- **Commit is delegated backing.** `Grant::commit` for `MemKind::Hbm`
  cannot pull from the DDR frame pool; it resolves against the granted VRAM
  range. Where no real device exists (QEMU), `Hbm` stays honestly
  DDR-emulated and labeled, as MEMORY.md 2 already states - the emulation
  is of placement, never of bandwidth.
- **The zeroing rule holds, by DMA.** MEMORY.md 5 is absolute: no page
  crosses a cell boundary un-zeroed. VRAM pages moving between cells are
  zeroed by a scheduled DMA/compute node on the device itself (memset at
  memory-controller speed), enqueued by the kernel as a graph node the
  receiving cell's first use depends on - not a trusted-blob promise.
- **Eviction is a scheduled DMA node.** VRAM oversubscription is handled
  the way MEMORY.md 2 handles all migration: an explicit
  `Hbm -> Ddr` transfer node in a dependency graph, initiated by policy
  (the residency service, AI-ARCHITECTURE.md 2), never a transparent fault.

---

## 7. Firmware - measurement, blobs, and the trust class

The honest core: on GSP/PSP-mediated GPUs (every modern NVIDIA and AMD
part), the OS loads firmware through a vendor mailbox and **cannot read
back the running image**. "Measured at attach" (ACCELERATORS.md 1)
therefore means, precisely:

1. The **blob-as-loaded** is hashed - the bytes the OS handed the mailbox.
2. Where the silicon supports it (SPDM attestation on recent datacenter
   parts, confidential-compute modes), the **device's own report** is
   requested and verified against vendor roots.
3. The result assigns the trust class of ACCELERATORS.md 2: **exclusive
   attested ownership** where both hold, otherwise
   **shared-with-firmware** - and for most consumer GPUs,
   shared-with-firmware is the honest **default**, not the exception. Such
   an engine gets no secrets and no multi-tenant grants; it still computes.

```rust
/// The evidence recorded at attach; feeds the trust class and the
/// attach-benchmark cache key (section 9).
pub struct FirmwareEvidence {
    pub blob_hash:     [u8; 32],           // blob-as-loaded
    pub device_report: Option<SpdmReport>, // silicon attestation, if any
    pub trust_class:   TrustClass,         // Exclusive | SharedWithFirmware
}
```

**Blob lifecycle.** Firmware images are content-addressed, immutable
objects in the object store (FILESYSTEMS.md 3), like model weights
(AI-ARCHITECTURE.md 2). Admission is an **allow-list of hashes** held as
policy in a controller cell - the kernel checks a hash against evidence it
is handed; it does not carry a vendor table. The blob hash enters the
attestation evidence (a regulated deployment can prove which firmware ran)
and keys the attach-benchmark cache, so a firmware update invalidates
stale performance claims automatically.

---

## 8. Reset, faults, and recovery

Doctrine 7: failure is an event. The engine contract promises "provable
return to a known state"; for a real GPU that is a ladder, each rung with
a defined blast radius:

| Rung | Mechanism | Blast radius |
|---|---|---|
| Queue teardown | stop one hardware ring | that queue's in-flight work completes-with-error |
| Context reset | vendor per-context reset via the driver cell | one cell's GPU contexts; others undisturbed |
| Device reset | **FLR** (section 3), kernel-initiated | everything on the device |

Effects on outstanding state, uniformly: in-flight graph nodes complete
with an error status through the existing completion path (the
OPEN-QUESTIONS.md 2 error model); timeline semaphores the dead work would
have signaled are **poisoned** - signaled to a sentinel that propagates
error rather than deadlock; memory grants **survive** (memory is not the
thing that failed) but IOMMU mappings into the dead context are torn down
and must be re-established by new grants after re-attach.

**TDR is budget-kill.** A hung submission is detected by its budget
expiring, and the response is the ladder above - never "polite preemption"
the hardware cannot do (ACCELERATORS.md 3). The driver cell's `quiesce`
(section 5) gets a bounded chance to do it gently; FLR does not wait for
it.

**GPU MMU faults** (the device's own translation, distinct from IOMMU
faults) are reported by the driver cell as typed events to the offending
cell, carrying the faulting device address. **ECC/RAS** errors surface as
events on the driver cell's queue; an uncorrectable error demotes the
engine's trust standing until a re-attach re-attests it, and pages the
error names are retired from the VRAM ranges the kernel grants
(section 6).

```rust
/// Typed engine-fault event, delivered on a queue like every other event
/// (object 10). `addr` is device-side for GPU MMU faults, DmaAddr-space
/// for IOMMU faults.
pub struct EngineFault {
    pub engine:  EngineId,
    pub kind:    FaultKind,   // IommuFault | GpuMmuFault | Ecc | Hang
    pub addr:    Option<u64>,
    pub fatal:   bool,        // did this trigger the reset ladder?
}
```

---

## 9. Attach - benchmark, partitions, metering

**The benchmark, made real.** "Offload proves itself at attach"
(ACCELERATORS.md 2 rule 1) currently means a 4096-iteration integer Add on
the CPU engine (section 2). For a GPU the measured op classes are:

- GEMM tile throughput (the tile IR's MMA shape, section 11), per dtype -
  fp16/bf16/fp8/int8 paths measured separately, because quantized
  throughput is exactly what placement decisions need;
- H2D / D2H / D2D memcpy bandwidth (the DMA-node cost model);
- kernel-launch latency; semaphore signal-to-CPU-wake round-trip
  (section 10).

Results enter the topology graph (BOOT.md 5). Boot-time cost is bounded by
**caching keyed on `(vendor:device:revision, firmware blob hash)`** - a
cache hit skips the benchmark entirely, and a firmware change invalidates
it (section 7). The cache is a content-addressed object, cluster-shared,
the same shape as the autotune cache (AI-ARCHITECTURE.md 4).

**Partitions as engines.** MIG-style spatial slices and SR-IOV VFs are
created by the **driver cell** (vendor mailbox - vendor logic), but each
resulting partition is enumerated by the **kernel** as an engine in its
own right: its own engine capability, its own metering - and, decisively
for SR-IOV, **its own requester ID**, hence its own IOMMU domain
(section 4). Partitioning composes with containment for free; that is why
partitions-as-capabilities (ACCELERATORS.md 3) needs no new machinery.

```rust
/// The attach-time record; cached by (device identity, firmware hash).
pub struct AttachReport {
    pub engine:      EngineId,
    pub evidence:    FirmwareEvidence,          // section 7
    pub measured:    &'static [(OpClass, Throughput)],
    pub preemption:  Preemption,                // OpBoundary, honestly
    pub partitions:  u8,                        // 0 = unpartitioned
}
```

Metering stays per-grant counters (engine time, bytes moved, VRAM held),
read through the existing introspection path; `SYS_ENGINE_INFO` grows from
its hardcoded single CPU answer to enumerating attached engines - reporting
existing state, no new verb.

---

## 10. Timeline semaphores and peer-to-peer DMA

**Semaphores on real silicon.** The kernel's cross-engine sync object
(ACCELERATORS.md 1, GRAPHICS.md 1) maps onto what GPUs actually implement:
a **monotonic 64-bit value at a memory location the device writes** -
NVIDIA semaphore surfaces and AMD user-fence words are both exactly this.
The location lives in a grant mapped into the device's IOMMU domain
(section 4), so signaling is an ordinary contained DMA write. The three
waiter cases:

- device waits on device: the hardware polls/waits on the value natively -
  no kernel involvement per wait;
- CPU waits on device: the driver cell arms a device interrupt at a
  threshold; the MSI-X vector (section 3) delivers a typed completion and
  the waiting strand wakes - **never a CPU poll**;
- device waits on CPU: the CPU writes the value; the device observes it
  through the same mapped location.

**Peer-to-peer DMA (GPUDirect-class).** The mechanism is deliberately
nothing new: mapping one device's BAR range into **another** device's
IOMMU domain - an ordinary section-4 mapping whose backing grant happens
to be `MemKind::DeviceBar` instead of DDR. NVMe -> HBM model loading and
NIC -> HBM ingest (AI-ARCHITECTURE.md 2, NETWORKING.md) are this one
operation. What makes P2P honest is topology:

- discovery walks the PCIe tree (section 3) for a common switch and reads
  **ACS** - platforms that force P2P through the root complex (most
  consumer boards) get it at reduced bandwidth, and the topology graph
  records which;
- where P2P is unavailable or refused, the fallback is an explicit,
  labeled **bounce-through-DDR DMA node** in the graph - visible in the
  graph and in metering, never a silent downgrade (the ACCELERATORS.md 5
  "no CPU bounce buffer" claim holds where hardware allows, and degrades
  loudly where it does not).

---

## 11. Tile programming, inference, and quantization - the memory collaboration

The tile IR and the inference algorithms are designed in
AI-ARCHITECTURE.md 3-4; they are first-class *consumers* of this document.
This section is the collaboration contract: what the GPU plumbing and the
memory system must provide so tiles, paged KV, and quantized weights are
real on silicon - and it is where "the memory system" of MEMORY.md and the
device model of this document meet.

**Tile memory spaces are the typed-memory doctrine inside the chip.** A
tile carries shape, dtype, and memory space (register / shared / HBM -
AI-ARCHITECTURE.md 4). The outer space is literally this document's
`Buffer<Hbm>` (section 6): a tile IR kernel's HBM operands are ranges of
grants, so the same capability that lets a cell map a buffer is the one
that lets its kernels tile over it. Async tile copies lower to the
device's bulk-copy engines (TMA-class); at the OS level a cross-buffer or
cross-kind copy is a DMA graph node, so the cost model the scheduler uses
(section 9's measured H2D/D2H/D2D classes) prices exactly the operations
the tile IR emits. Kernel resource shapes (shared-mem per block, registers
per thread) ride the submission ABI so partition sizing (section 9) works
from facts.

**Paged KV is grants + the GPU MMU, split exactly along this document's
seams.** AI-ARCHITECTURE.md 3 makes the KV cache a paged memory object:
block-granular HBM allocations, block tables the GPU MMU walks,
copy-on-write prefix sharing. Concretely:

- KV **blocks** are small fixed-size `Buffer<Hbm>` grant ranges (the
  MEMORY.md commit-quantum discipline, sized to the attention block);
- the **block table** is GPU-MMU state, programmed by the driver cell
  (section 5) - safe under the two-level argument, since every block-table
  entry still resolves through the IOMMU domain;
- **copy-on-write and prefix sharing** are grant sharing plus
  content-addressed block hashes (the `share_ro_into` precedent at CPU
  level, applied to device mappings);
- eviction of cold blocks is the section-6 eviction node, driven by the
  serving cell's policy.

The kernel's whole contribution is block/remap/share of typed grants and
the IOMMU floor under the GPU MMU - no attention logic, no batching, no
serving policy anywhere near the kernel (ARCHITECTURE.md 5).

**Quantization is a property of the type, and the memory system respects
it.** Quantized inference (int8, fp8, int4 block-quant) is not a compute
detail bolted on top - it changes what the memory system stores and moves:

- **dtype rides the buffer type.** A quantized weight buffer is
  `Buffer<Hbm, F8E4M3>`-shaped: the element format is part of the typed
  buffer, the same formats the tile IR's MMA shapes negotiate
  (AI-ARCHITECTURE.md 4). A kernel compiled for fp16 operands cannot be
  handed an int4 buffer - a type error at graph build, not a garbage
  inference at runtime.
- **Block-quant layout is allocation-visible.** int4-block-quant stores
  scales (and zero points) per block; the layout (data + scale planes,
  block granularity) is part of the sealed model object's format, so
  grant sizing and tile copies operate on the true byte layout - the
  allocator never "helpfully" rounds a scale plane away.
- **Quantized weights are sealed objects, hash and all.** The
  content-address covers the quantized bytes, so "which quantization of
  which model served this request" is attestable for free
  (AI-ARCHITECTURE.md 2, SECURITY-IDENTITY.md); dedup across cells works
  per quantization.
- **Quant/dequant conversions are graph nodes** - explicit, scheduled,
  metered transforms (often fused into tile kernels by the compiler; the
  fused form is still declared in the graph's dtype edges). A KV cache
  quantized to fp8 halves block bytes; the block table does not care, and
  the attach benchmark's per-dtype GEMM classes (section 9) tell placement
  what the quantized path is actually worth on this silicon.

Honesty: the tile layer now exists as the librheo framework + two
graph-node ops executable on the CPU engine (TILES.md - the tile model,
contracts, executors, and battle tier live there); there is still no tile
*compiler* in-tree, and no device engine executes tiles. The contract
matters because sections 3-10 are designed against it: per-dtype
benchmark classes, block-granular grants, driver-cell page tables, and
P2P loading exist in this document *because* this is the workload they
must carry (BUILD-ORDER step 18's gate is serving a 7B model within 15%
of vLLM).

---

## 12. Bring-up path - QEMU first, hardware lab second

The repo's culture is emulation-first with named test kernels and honest
per-ISA skips; real-GPU work follows it in stages, each stage a test
kernel that CI runs:

- **Stage 0 (exists):** the virtio-gpu 2D command round-trip
  (`librheogpu`, DISPLAY.md 12) - proves the transport and the queue
  opcode seam.
- **Stage 1 - PCIe (done):** bridge recursion (the kernel programs
  secondary bus numbers where firmware left them zero), BAR sizing by the
  mask probe, the capability walk (MSI/MSI-X/PCIe/FLR), opt-in BAR
  assignment from the per-ISA host-bridge window (`assign_pci_bars` -
  invisible to every boot that does not call it), vendor recognition
  (`hw/gpu.rs`), and engine registration. The `gpuhw` test proves it on
  **all three ISAs** with the same three GPU functions: a real AMD/ATI
  vendor device (QEMU's `ati-vga`, 0x1002 - a 16 MiB framebuffer aperture
  and a 16 KiB register window, sized by the probe), a Bochs display, and
  a virtio-gpu placed *behind* a `pcie-root-port` - reachable only through
  the kernel's own bridge programming. On arm/riscv the kernel assigns 6
  BARs and reads them back; on x86 SeaBIOS got there first and the
  read-back proves agreement. The stage closes with the tree's first
  **real vendor-GPU MMIO**: the AMD device's 16 MiB framebuffer aperture
  is mapped through a per-ISA device window (`arch::mmio_map_window` - a
  second fixed 2 MiB-page window beside the pmem one on x86-64, where a
  BAR sits above the top-2 GiB linear map; the missing 1..2 GiB gigapage
  installed in the kernel root on RISC-V, whose PCIe window falls in the
  gap between the boot map's device gigapage and RAM; plain
  `phys_to_virt` on ARM64), written through, and read back - the full
  enumeration -> BAR -> mapping -> decode -> device-memory path on real
  AMD-vendor silicon emulation. Two more increments ride the same seam:
  **attach measurement** (`hw::gpu_attach_measure`, opt-in) streams 64 KiB
  through each GPU's aperture and records ticks/KiB - the section 9
  "offload proves itself" rule applied to the only path exercisable
  without a vendor driver cell, honestly a *transport* measurement, not
  compute - reported live by `SYS_ENGINE_INFO` (the engine table IS the
  inventory); and a **Bochs dispi register handshake** (the 16-bit ID
  register at MMIO-BAR + 0x500 answers 0xB0C5), so every GPU device model
  QEMU has is genuinely driven at some level: virtio-gpu by the Phase H
  2D driver, AMD through its framebuffer aperture, and the Bochs display
  by a **real 2D modeset** (`hw/gpu.rs::bochs_modeset` over the DISPI/VBE
  register interface - program 640x480x32 + LFB, render into the linear
  framebuffer, read pixels back, on all three ISAs). MSI-X *routing* (vectors to vcores) is not in stage 1;
  only capability presence is recorded.
- **Stage 2 - IOMMU (x86-64 + ARM64 done):** the `iommu` test kernel proves
  a device DMA **inside** a granted (identity) domain succeeds and one
  **outside** it (after revoke) faults - BUILD-ORDER step 12's done-when -
  on both `-device intel-iommu` (VT-d, fault from the fault-recording
  register) and `-machine virt,iommu=smmuv3` (SMMUv3 stage-1, fault from the
  event queue). RISC-V skips-with-reason (no QEMU IOMMU model - section 4).
- **Stage 3 - the driver cell:** virtio-gpu **re-homed** from the kernel
  into a driver cell holding BAR-window grants (virtio-pci's BARs, no more
  config-space tunnel) and queue-memory grants, with the kernel keeping
  only doorbell, IOMMU mapping, and reset. First full proof of section 5
  - and it needs no vendor hardware.
- **Stage 4 - the lab:** real silicon for everything QEMU cannot model.

**The kernel-residency tension, named.** The Phase H virtio-gpu driver
(`kernel/src/hw/virtio_gpu.rs`) is kernel-resident, which sits in tension
with ARCHITECTURE.md 5 ("device drivers beyond queue/IOMMU/reset plumbing"
are permanently outside the kernel). DISPLAY.md 12 does not flag this;
this document does. It is defensible only as a bring-up seam - installed
by a test kernel, never part of a system image - and stage 3 retires it:
same driver, contained. That migration is also the cheapest end-to-end
proof this design has.

**Per-capability provability:**

| Capability | QEMU-provable (model) | Lab-gated | Per-ISA notes |
|---|---|---|---|
| PCIe walk, BAR assign, vendor recognition | **proven** (`gpuhw`: ati-vga + bochs + virtio behind a root port) | timing only | all three ISAs |
| IOMMU domains, out-of-grant fault | **proven x86-64 + ARM64** (`iommu` test: VT-d + QI on `intel-iommu`, SMMUv3 stage-1 on `smmuv3`; virtio-blk DMA faults on revoke) | ATS/PRI | **riscv skip-with-reason: no QEMU IOMMU model** |
| Driver-cell BAR containment | yes (stage 3, virtio-gpu re-homed) | vendor blob | all three ISAs |
| VRAM (`Hbm`) real backing | no VRAM device model - honest DDR stand-in, labeled | yes | - |
| Firmware measurement / SPDM | no | yes | - |
| MIG / SR-IOV partitions | partially (QEMU SR-IOV emulation is narrow) | yes | - |
| Timeline semaphore MSI-X wake | approximated (testdev-class interrupt) | vendor mapping | - |
| P2P DMA | no (no ACS/switch fidelity) | yes | - |
| Tile IR / quantized GEMM paths | no (nothing to execute them) | yes (step 18 gate) | - |

QEMU proves protocol and containment correctness; it never proves
bandwidth, latency, or TDR timing (EMULATION.md 2, TOOLING.md 4 - the
P-gates live at the lab).

---

## 13. Build-order placement

BUILD-ORDER.md step 18 (accelerators + AI layer) depends on step 12
(IOMMU), which is unbuilt. This document owns that dependency explicitly:

- **Section 4 is step 12's design.** Stage 2 of section 12 is its
  done-when, verbatim.
- **Sections 3, 5, 6 are the GPU-specific prefix of step 18** - the
  plumbing that must exist before a vendor driver cell has anything to
  hold; section 11 is the contract step 18's gate (a 7B model within 15%
  of vLLM) is measured against.
- **Section 12's stages are the sub-ordering** inside steps 12 and 18.

Explicitly deferred to their owning documents: display/scanout on real
GPUs - modes, vsync delivery, the swapchain (DISPLAY.md 1-11); the
compilation service, tile IR internals, and the autotune cache
(AI-ARCHITECTURE.md 4); NPU/TPU/FPGA specifics (ACCELERATORS.md 4);
GPU/accelerator virtualization beyond SR-IOV enumeration
(VIRTUALIZATION.md 6).

---

## 14. Honest costs, what the kernel refuses, what this does not claim

**Not claimed:**

- No vendor GPU runs today. Nothing in sections 3-11 is implemented beyond
  section 2's inventory; this is a design document, and the tree's honest
  state is one CPU engine and a test-installed virtio-gpu.
- Firmware "measurement" on consumer GPUs is measurement of the
  blob-as-loaded, not the running image. The default trust class for a
  GeForce-class part is shared-with-firmware, and no amount of attestation
  vocabulary makes it a confidential-compute device.
- QEMU results prove protocol and containment, never performance. DMA
  bandwidth, P2P throughput, semaphore wake latency, and TDR timing gate
  on the hardware lab.
- The RISC-V IOMMU path is designed, not provable, until QEMU (or real
  silicon) provides the device.

**Honest costs:**

- BAR assignment and bridge recursion add real boot-time PCIe code to the
  kernel - the price of PVH's firmware-free boot, paid once, in the
  allowance ("queue/IOMMU/reset plumbing") the negative constitution
  grants.
- IOMMU mappings are not free: map/unmap on grant commit/revoke adds a
  per-operation cost and an IOTLB-invalidation cost the DDR-only path
  never paid. Metering must show it.
- The two-level page-table argument (section 5) accepts that a blob can
  wedge **its own** device arbitrarily; recovery is reset, not prevention.
  A workload sharing a partition with a hostile neighbor inherits that
  neighbor's reset blast radius (section 8's table is the honest map).

**What the kernel refuses** (extending ACCELERATORS.md 6):

- No vendor command-stream parsing or validation in the kernel, ever.
- No in-kernel VRAM allocator - the kernel grants ranges and meters; the
  driver cell sub-allocates.
- No shader/kernel compiler in the trusted base - the tile IR toolchain
  and vendor compilers run in contained cells (AI-ARCHITECTURE.md 1).
- No transparent VRAM oversubscription - eviction is an explicit,
  scheduled, visible DMA node or it does not happen.
- No identity-mapped IOMMU "compatibility mode" - a domain is empty until
  grants fill it, on every device, always.
