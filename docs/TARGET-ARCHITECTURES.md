# Lattice OS - Target Architectures

**Status:** Draft v0.1
**Companion to:** ARCHITECTURE.md

The device and ISA matrix is deliberately narrow. Lattice's bet only pays on
fleet servers; every architecture below is chosen for that target, and the
"non-targets" section is as binding as the targets.

This is a **greenfield, modern-first project** (ARCHITECTURE.md 1.4): the
baselines below are set high on purpose, modern acceleration features are
exploited when present rather than designed around, and older hardware /
embedded parts are future stages, not present compromises.

---

## 1. Support tiers and the per-profile floor

Two things vary together: the **ISA tier** (below) and the **hardware floor**,
which is **per profile** (PROFILES.md). The server profile applies the strict
floor in section 3 (IOMMU, measured-boot root, hardware entropy). Edge,
embedded, low-power, and lab profiles relax specific requirements with the
degradations stated honestly (as the Raspberry Pi lab does, CLUSTER.md 9). The
security *model* never relaxes; only which hardware assists are present does.

| Tier | Meaning | Targets |
|---|---|---|
| **Tier 1** | CI-gated on every commit (QEMU) + hardware lab. Release blockers. | x86-64-v3+, ARM64 (Armv8.4+ server) |
| **Tier 2** | CI-gated on QEMU; hardware best-effort. | RISC-V RVA23 server profile |
| **Experimental** | Builds, tracked, no promises. | CHERI (Morello / CHERI-RISC-V) |
| **Later-phase profiles** | Architected-in now (PROFILES.md), delivered later. | Embedded (MMU-class) / real-time small-footprint; low-power/remote; desktop; wider ARM/RISC-V edge SoCs |
| **Permanent boundaries** | Will not be targeted. | 32-bit anything, sub-MMU microcontrollers (no MMU = no capability model), pre-v3 x86-64, phone/mobile handsets |

The distinction that changed from earlier drafts: desktop and embedded are no
longer "non-targets" - they are **later-phase profiles** the one foundation is
architected for (PROFILES.md 3). The permanent boundaries are the genuinely
incompatible cases: no MMU (the capability/cell model needs one), 32-bit, and
mobile handsets (out of scope as a product).

Rationale for three ISAs from day one: the `Arch` trait layer (see section 4)
only stays honest if at least two very different memory models (x86 TSO vs
ARM weak ordering) are exercised continuously. RISC-V is the forward bet -
capability-adjacent hardware research (CHERI) and open accelerator
integration keep landing there first.

---

## 2. CPU ISAs

### 2.1 x86-64 (Tier 1)

- **Baseline:** x86-64-v3 (AVX2, BMI2, FMA). The kernel image assumes v3;
  no runtime fallback below it.
- **Dispatched at runtime (measured, not assumed):** AVX-512 (VAES, GFNI,
  VPCLMULQDQ for the crypto/checksum paths), AMX (tile registers - a direct
  tile-IR lowering target), AVX10 as it lands in server parts.
- **SIMD/vector acceleration is foundational, exploited when present.**
  Wide-vector and matrix units are not an afterthought bolted onto scalar
  code; the performance-critical paths (crypto/AEAD, checksums, hashing,
  memory zeroing/copy, the tile-IR CPU backend, Arrow/Parquet decode) are
  written to a vector abstraction and lowered to the widest available ISA.
  AVX-512 is used as a first-class acceleration path where a CPU exposes it,
  falling back to AVX2 on v3 parts - the *choice* is runtime-dispatched on the
  measured feature set (the crypto-dispatch rule in section 4: the CPU is the
  benchmark every offload must beat, and among CPU paths the widest one that
  actually wins is chosen). This is the greenfield stance applied to the ISA:
  design for the capable machine, degrade explicitly, never design down.
- **Kernel-relevant features:** PCID (address-space-tagged TLBs - required
  for cheap cell switches), x2APIC, TSC-deadline timers (the tickless
  one-shot mechanism), invariant TSC (engine-clock correlation), CET
  shadow stacks (enabled for kernel and blessed system cells), MPK/PKS
  (intra-cell hardening for personalities), RDT/CAT+MBA (the memory-bandwidth
  partitioning behind interference control in shared pools).
- **Explicitly not used:** TSX (dead), SGX (dead in server parts; TDX covers
  the confidential-compute path later), legacy BIOS boot.
- **Reference platforms:** current-generation Intel Xeon (Granite Rapids
  class) and AMD EPYC (Zen 5 class, including MI300A APU systems for the
  unified-memory degenerate case).

### 2.2 ARM64 / AArch64 (Tier 1)

- **Baseline:** Armv8.4 server class; Armv9 features dispatched.
- **Kernel-relevant features:** SVE/SVE2 (vector-length-agnostic lowering in
  the tile IR's CPU backend), **MTE** (memory tagging - cheap enough to leave
  on in production for hardened-allocator cells; first-class in the Arch
  trait), PAuth + BTI (kernel and system cells compiled with both), ASID-
  tagged TLBs, GICv3/v4 (interrupt routing to vcores; v4 direct virtual
  interrupt injection for the VM dev platform), SMMUv3 (the IOMMU floor -
  stage-2 translation for every device queue), MPAM (the RDT equivalent for
  bandwidth partitioning), generic timer (one-shot tickless), FEAT_LSE
  atomics (required - LL/SC-only parts are out).
- **Memory ordering:** the weak model is the design's honesty check - all
  synchronization is written against explicit acquire/release, never assumed
  TSO.
- **Reference platforms:** AWS Graviton (4/5 class), Ampere, NVIDIA Grace
  (including Grace-Hopper NVLink-C2C coherent systems - the "coherent fabric"
  memory-kind case).

### 2.3 RISC-V (Tier 2)

- **Baseline:** RVA23 server profile - this is the line where RISC-V becomes
  a serious server target: hypervisor extension, Vector 1.0, Sstc (timer),
  Svpbmt (page-based memory types), Sscofpmf (perf counters).
- **Required platform pieces beyond the profile:** the RISC-V IOMMU spec
  implemented in silicon, AIA (advanced interrupt architecture) for MSI-style
  routing to vcores.
- **Posture:** QEMU-first; hardware as credible RVA23 server silicon ships.
  The Arch trait implementation is kept current so the ISA is a recompile,
  not a port.

### 2.4 CHERI (Experimental)

- Morello and CHERI-RISC-V tracked as the one hardware trend aligned exactly
  with the design's spine: software capabilities compiling down to hardware-
  enforced unforgeable pointers.
- Commitment level: the Arch trait and capability-core data layouts avoid
  decisions that would foreclose CHERI (no pointer-integer punning in the
  ABI, capability handles kept abstract). No CHERI-specific code paths yet.

---

## 3. Hardware floor (per-host requirements)

A machine must have all of the following to boot Lattice in a supported
configuration. These are requirements, not recommendations, because the
security and isolation story depends on each.

1. **IOMMU** (VT-d / AMD-Vi / SMMUv3 / RISC-V IOMMU) with per-queue,
   per-device isolation. Every DMA in the system is IOMMU-mediated; a host
   without one cannot enforce cell isolation against devices. Non-negotiable.
2. **Measured boot root of trust:** TPM 2.0 or DICE. Host identity,
   capability tokens, and the entropy attestation chain all root here.
3. **UEFI boot** (x86/ARM) or the RISC-V boot flow with an equivalent
   measurement chain. No legacy BIOS path exists.
4. **Hardware entropy source** (RDSEED / RNDR / platform TRNG) or a
   provisioned sealed seed - otherwise the host fails attestation by design.
5. **Invariant / synchronized timestamp counters** across cores.
6. **64-bit only, >= 39-bit physical addressing**, huge-page support (2MB
   mandatory, 1GB exploited where present).

Strongly preferred (degraded-but-supported without):

- **PTP hardware timestamping** on at least one NIC (else e degrades to NTP
  bounds and lease windows widen accordingly).
- **RDMA-capable fabric** for multi-host (else the mTLS/QUIC transport
  carries east-west with honest latency costs).
- **Power-loss-protected NVMe write cache or PMEM zone** (else `durable`
  completions pay full flush latency).

---

## 4. The Arch trait layer

All ISA differences live behind one trait per concern; per-ISA modules
implement them. Nothing outside these modules may conditionally compile on
architecture.

| Trait area | Covers |
|---|---|
| Page tables | Formats, levels, huge-page sizes, ASID/PCID tagging, TLB shootdown mechanics |
| IOMMU | Map/unmap, per-queue domains, ATS/PRI capability discovery, fault reporting |
| Interrupts | Vector allocation, MSI-X routing to vcores, doorbell delivery, user-level interrupt support |
| Timers | One-shot deadline timers, engine-clock correlation, invariant counter reads |
| Atomics/ordering | Acquire/release mappings, fence selection (the code above is memory-model-honest; the trait picks instructions) |
| Context switch | Register save/restore, extended-state (AVX/SVE/AMX tile) lazy handling, shadow-stack switch |
| Security features | MTE tag management, PAuth key handling, CET/BTI enablement, MPK/PKS domains |
| Bandwidth partitioning | RDT/CAT/MBA (x86) and MPAM (ARM) behind one QoS interface |
| Power management | DVFS (frequency/voltage scaling), idle/sleep states, per-engine power gating, energy counters - behind one interface; central to the low-power/remote profiles (PROFILES.md 4), a no-op on wall-powered servers |
| Crypto dispatch | Measured selection among VAES/AVX-512, ARM CE, vector crypto - with the CPU as the benchmark every offload must beat |

Boot, exception vectors, and the context-switch inner loop are the only
assembly, a few files per ISA.

---

## 5. Engines (accelerators and devices)

Per the QCE doctrine: the kernel's per-engine surface is queues, IOMMU
mappings, reset, partitioning, attestation, and metering. Everything
vendor-specific runs in contained userspace driver cells. Every engine is
benchmarked at attach; measured numbers, not datasheets, enter the topology
graph.

### 5.1 GPUs

| Vendor / family | Support posture |
|---|---|
| NVIDIA Ampere through Blackwell | Primary target. MIG spatial partitioning, TMA/tensor-core lowering in the tile IR, contained ptxas in the compilation service, contained CUDA-library cells for peak paths. GPUDirect peer-to-peer paths (NIC/NVMe to HBM) are the reference zero-copy pipeline. |
| AMD CDNA (MI300 class) | Tier-1 ambition: MFMA tile lowering via ROCm/LLVM (open stack fits the contained-driver model well). MI300A APU is the coherent unified-memory reference platform. |
| Intel Xe / Gaudi class | Tracked; Vulkan-compute floor first, native lowering as demand justifies. |
| Anything else | Vulkan compute as the portable floor. |

### 5.2 NPUs / TPU-class

- Command-buffer DSP-style NPUs (Qualcomm Cloud AI class, embedded NPUs):
  whole-device or spatial-partition grants only (no-preemption contract),
  SRAM exposed as a small explicit memory kind, firmware attested, DMA
  in/out expressed as graph nodes.
- Systolic TPU-class devices with private interconnects: the interconnect
  registers in the topology graph as fabric links; collectives are graph
  nodes.
- Honest expectation: the uniform engine contract covers ~80%; per-family
  quirks live in driver cells forever.

### 5.3 DPUs / SmartNICs

- NVIDIA BlueField-3+ and AMD Pensando class as first targets.
- Roles: pre-steering offload (tier-a WASM programs compiled to pipeline
  tables or DPU cores), inline TLS/QUIC crypto per queue, storage-side
  Parquet pushdown, and eventually full transport termination on DPU cores
  running the same `no_std` Rust code as the host.
- CID-based QUIC steering in NIC flow tables is a hard requirement for the
  edge tier.

### 5.4 FPGAs

- Bitstreams load as attested engines (measured and signed like code, because
  they are code). Partial-reconfiguration regions map to spatial partitions.
- Posture: supported as engines; no synthesis toolchain ownership - that
  stays a vendor-cell concern.

### 5.5 Storage and fabric

- **NVMe** (1.4+) with SGL support; multiple namespaces; PLP write cache
  detection at attach. ZNS and FDP exploited by the native object store where
  present.
- **NICs:** 100G+ with flow steering, RSS, header/payload split (the
  network-to-GPU zero-copy path), hardware timestamping; RDMA (RoCEv2 /
  InfiniBand) for fabric east-west.
- **CXL:** 2.0 memory expansion as a typed memory kind (latency class ~2-3x
  local DRAM, stated, never hidden); 3.x pooling/fabric tracked - it changes
  capacity economics, not the typed-placement model.
- **Memory technologies:** DDR5, HBM3/3e (device-resident kinds), PMEM-class
  log zones where available.

---

## 6. Virtualization and confidential compute

- **Bare metal is the primary deployment.**
- **Run-as-guest (KVM/cloud instances) is the primary development platform**
  and a supported production mode with stated degradations: virtual IOMMU
  quality varies, e is worse, engine passthrough required for accelerator
  work, entropy fed via virtio-rng plus local jitter and mandatory reseed.
- **Lattice as a host for VMs** is out of scope initially; cells are the
  isolation unit. Revisit if a hard multi-tenant boundary below the cell is
  demanded.
- **Confidential compute** (TDX / SEV-SNP / ARM CCA): tracked as a future
  trust-domain tier - the attestation chain is designed so a confidential
  host slots in as a stronger host-identity class without model changes.

---

## 7. QEMU and CI matrix

- Every commit: QEMU x86-64-v3, QEMU ARM64 (max + SVE/MTE), QEMU RVA23 -
  boot, capability-core proof-adjacent test suite, loom/fuzz corpora, P1-P5
  microbenchmarks (trend-tracked; absolute numbers gate only on hardware).
- Hardware lab (from M1): one Xeon or EPYC node, one Graviton/Ampere-class
  node, one NVIDIA GPU node (adding AMD at M5), one RDMA pair, PTP-capable
  switch. P6-P12 gate here.
- Cross-compilation is a Cargo `--target` invocation by design; any porting
  step that requires more than the Arch trait implementation is treated as an
  architecture bug.

---

## 8. Boundaries and the per-profile floor

Genuinely binding, permanent:

- 32-bit ISAs, x86-64 below v3, and sub-MMU parts (no MMU means no capability
  or cell model - PROFILES.md 6).
- Windows-style binary driver compatibility and Linux kernel module
  compatibility in-kernel: never. Drivers are contained cells against the
  engine ABI (Linux drivers can be run *in a driver VM* for the long tail -
  PRODUCTION.md 3.3 - but never loaded into the kernel).
- Phone/mobile handsets as a product: baseband, mobile power stacks, and the
  app ecosystem are out of scope. The primitives do not foreclose it; nothing
  is aimed there.

Per-profile, not binding (this replaces the earlier blanket exclusions):

- **The IOMMU / measured-boot / hardware-entropy floor is the *server*
  profile's floor.** Edge, embedded, low-power, and lab profiles relax
  specific requirements with honest degradations (the Pi lab runs exactly this
  way - reduced-trust attestation, degraded device isolation, wider clock
  bound - CLUSTER.md 9). What never relaxes is the security *model*; what
  varies is which hardware assists back it.
- **Desktop and embedded are later-phase profiles, not non-targets**
  (PROFILES.md 3). Desktop stays foundation-capable via the Vulkan/compositor
  path (GRAPHICS.md) with a full desktop *product* being a long, later effort;
  MMU-class embedded is a real later profile with its own footprint and RT
  work.
- **Power-management depth**, previously deprioritized, is a first-class
  concern for the low-power/remote profiles (section 4 Arch trait,
  PROFILES.md 4) - not absent, just profile-scoped.
