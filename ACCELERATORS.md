# Accelerators - GPU, NPU, TPU, FPGA, DPU

**Status:** Draft v0.1. Expands ARCHITECTURE.md object 4 (engine) and 4.11;
see TARGET-ARCHITECTURES.md section 5 for the hardware matrix.

Position: every accelerator is an **engine** - a first-class kernel object,
scheduled alongside CPU vcores, sharing one address-space abstraction and one
security model. But the kernel's per-engine surface is deliberately tiny, and
all vendor logic is contained in userspace driver cells. This is the direct
lesson of Linux disabling its QCE crypto driver as "harmful" after a decade
of in-kernel vendor rot.

## 1. The engine contract (what the kernel provides)

Exactly this, for every accelerator, no more:

- **Queues + doorbells** - submit work, receive completions (the same ABI as
  all I/O).
- **IOMMU mappings** - every engine DMA is mediated and grant-checked.
- **Timeline semaphores** - cross-engine sync objects so CPU/GPU/NPU/DMA
  chains execute as one dependency graph without CPU wakeups between stages.
- **Reset** - provable return to a known state.
- **Spatial partitioning** - MIG-style slices handed out as engine grants,
  sized like memory allocations, where hardware supports it.
- **Attestation** - firmware measured and signature-checked at attach.
- **Metering** - per-grant budget and usage counters.

Everything else - command encoding, shader/kernel compilers, firmware blobs,
vendor libraries - lives in a **driver cell** with grants only to its own
device. The kernel does not trust the blob; it **contains** it. A proprietary
NPU runtime physically cannot touch the network, other memory, or other
devices.

### 1.1 Execution contract — including probabilistic engines

Each engine declares an execution contract alongside its preemption contract.
This is where the 30-year horizon (quantum, neuromorphic, analog) is handled
without any quantum-specific kernel machinery — the existing engine
abstraction already spans it:

```rust
pub enum ExecutionContract {
    Deterministic,                     // CPU, GPU, NPU, DMA — exact result
    Probabilistic {                    // quantum, neuromorphic, analog
        result_carries_confidence: bool,  // completion includes error/confidence
        requires_classical_control: bool, // e.g. quantum: classical setup + readout
        repeatable: bool,                  // same input → same output? (usually no)
    },
}
```

A probabilistic engine's completion entry carries an error rate or confidence
value in its status field (`NodeStatus` has room, OPEN-QUESTIONS.md §2). A
**quantum co-processor** is an engine with a probabilistic, non-preemptible,
classical-control-required contract; a quantum node in a graph is:

```
[classical control node] → [quantum node: N shots, probabilistic] → [classical readout node]
```

a plain DAG with a probabilistic node in the middle whose output edge carries
a confidence value. A **neuromorphic / analog** engine is one whose output is
a probability distribution, consumed by a downstream node that thresholds or
samples it. No new kernel objects, no quantum-specific scheduler — one enum
and the existing graph model. This is designing *for* the future (keeping the
abstraction open) rather than speculating *about* it (building machinery for
hardware that does not yet exist). See REFLECTION-NEXUS.md §4.

## 2. The three QCE-derived rules

1. **Offload proves itself at attach.** Measured throughput/latency per op
   class enters the topology graph at attach time (BOOT.md 5). An engine that
   loses to the CPU for an op simply never receives that op. QCE shipped
   slower-than-CPU for a decade; here it would have been benchmarked into
   irrelevance immediately.
2. **Exclusive attested ownership, or a degraded trust class.** An engine
   yields full attested control (firmware measured, reset verified, IOMMU-
   isolated) or is marked *shared-with-firmware* and gets no secrets and no
   multi-tenant grants. No pretending the OS owns a device it races with a
   secure world for.
3. **Vendor logic never enters the kernel.** The per-engine kernel surface is
   vendor-free; blobs rot in contained cells where their rot is bounded.

## 3. Scheduling accelerators: space, not time

Accelerators preempt poorly (saving the state of 100k GPU threads is
enormous; many NPUs cannot preempt at all), so:

- **Spatial partitioning is first choice** - partitions as capabilities.
- **Command-buffer-granularity arbitration** - the kernel admits work into
  hardware queues and enforces budgets/priority there; it never pretends to
  schedule warps.
- **Declared preemption contract** - engines state their preemption
  capability; the scheduler only co-locates work consistent with it.
- **Budget-kill, not polite preemption** - a cell submitting unbounded
  kernels onto a shared partition is killed by budget, because polite
  preemption is physically impossible on the hardware.
- **Persistent/megakernels** carry cooperative yield points at tile-loop
  back-edges (AI-ARCHITECTURE.md), the strand yield-point idea moved onto the
  device, so they stay compatible with the budget contract.

## 4. Per-family notes

### GPUs

- NVIDIA (Ampere->Blackwell) primary: MIG partitioning, tensor-core/TMA
  lowering in the tile IR, contained ptxas and CUDA-library cells for peak
  paths, GPUDirect peer-to-peer as the reference zero-copy pipeline.
- AMD CDNA (MI300): MFMA lowering via ROCm/LLVM - the open stack fits the
  contained-driver model well; MI300A is the coherent unified-memory
  reference (one memory kind, migration nodes become no-ops).
- Intel Xe/Gaudi: Vulkan-compute floor first, native lowering as demand
  justifies.
- Portable floor for anything: Vulkan compute (GRAPHICS.md).

### NPUs

- Command-buffer DSP-style: whole-device or spatial-partition grants only
  (no-preemption contract), on-chip SRAM exposed as a small explicit memory
  kind (double-buffering DMA is a graph pattern), firmware attested.
- The tile IR fits NPUs *better* than CUDA's thread model does: explicit tile
  copies are literally their DMA descriptors; MMA is literally the systolic
  array (AI-ARCHITECTURE.md 4). Honest coverage: ~80% uniform, ~20% per-
  family quirks in driver cells forever.

### TPU-class systolic

- Private interconnects (ICI-like) register in the topology graph as fabric
  links; collectives are graph nodes scheduled as a unit with the fabric's
  real bandwidth costs exposed.

### FPGAs

- Bitstreams load as **attested engines** - measured and signed like code,
  because they are code. Partial-reconfiguration regions map to spatial
  partitions. Synthesis toolchains stay a vendor-cell concern; the OS owns
  loading, isolation, and metering, not HDL.

### DPUs / SmartNICs

- Both an engine class and an offload target: pre-steering, inline crypto,
  storage pushdown, and full transport termination on DPU cores running the
  same `no_std` Rust as the host (NETWORKING.md 7). Attested like any engine.

## 5. Memory and accelerators

- Device memory (HBM, NPU SRAM, device-BAR) are **typed memory kinds** with
  their real contracts (ARCHITECTURE.md 4.1). `Buffer<Hbm>` is a distinct
  type; an engine that cannot reach host DRAM does not implement the transfer
  for it - a compile error, not a runtime surprise.
- Transfers are scheduled DMA graph nodes; peer-to-peer (NVMe/NIC -> HBM)
  where hardware allows, no CPU bounce buffer.
- Unified memory is honored where the *hardware* is genuinely coherent (APU,
  NVLink-C2C): the typed model degenerates gracefully to one kind, one
  coherence domain. Over PCIe it is never faked.

## 6. What the kernel refuses

No accelerator code in kernel space, ever. No vendor compiler in the trusted
base (ptxas and friends run in the contained compilation-service cell). No
"AI-driven scheduler" - learned placement policies may *advise* as ordinary
controller cells, but the scheduler's contracts stay deterministic and
auditable, and any advice passes the same admission math as a human request.
