# AI Architecture

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.11; depends on
ACCELERATORS.md (engines), FILESYSTEMS.md (objects), IO.md (DMA graphs).
The hardware/memory collaboration contract for tiles, paged KV, and
quantized buffers on physical GPUs is GPU-HARDWARE.md 11.

Position: AI inference is a **foundational service layer**, treated like
graphics or storage - but the kernel gains almost nothing new for it, because
the engine abstraction, typed memory, dependency graphs, and sealed objects
already are the inference substrate. The striking pattern: modern inference
techniques are operating-system ideas (paging, batching, memory tiering)
reinvented in userspace because Linux could not help. This design gives them
their honest home one layer down, shared and attested.

## 1. The split: kernel vs service vs library

| Layer | Owns | Examples |
|---|---|---|
| **Kernel** | engines, typed memory (incl. HBM/SRAM kinds), dependency-graph execution, sealed buffers, attestation, metering | nothing AI-specific - it is the same control plane |
| **System services** | model registry, model loader, residency cache, compilation service + autotune cache, one or more inference servers | shipped with the OS, like a filesystem is |
| **Driver cells** | vendor GPU/NPU runtimes and compilers, contained | CUDA libraries, ROCm, ptxas, NPU firmware runtimes |
| **Workload cells** | the actual math kernels and serving logic | ports of vLLM, llama.cpp, whisper.cpp against the engine ABI |

The kernel refuses, permanently: inference in kernel space, vendor compilers
in the trusted base, and "AI-driven kernel scheduling" mysticism
(ACCELERATORS.md 6).

## 2. Models as objects

- Models are **content-addressed, immutable, sealed objects** with a flat
  safetensors-style layout - pointer-cast loadable, no parse
  (FILESYSTEMS.md 3). Consequences:
  - Dedup across tenants for free; the hash *is* integrity; provenance is
    attestable ("this cell serves exactly weights sha256:...").
  - **Loading is one DMA graph:** NVMe -> HBM (or NVMe -> NIC -> remote HBM)
    peer-to-peer, no CPU bounce, one flow ID, line-rate, observable
    (ARCHITECTURE.md P7 gates >= 80% of device line rate).
  - **Weights are shared, not copied:** N inference cells map the same sealed
    read-only HBM buffer - one physical copy per device. LoRA adapters layer
    as small per-cell deltas over the shared base.
- **Unloading is refcounts + reservation policy.** A residency reservation
  pins a latency-critical model in HBM (the database-contract pattern), so it
  never suffers cold-start roulette; everything else is LRU. Checkpoint/
  restore of an inference cell excludes weights (they reattach by hash), so
  inference cells are nearly stateless and cheap to move.
- **Tiering is typed placement**, not app hacks: llama.cpp-style layer
  offloading becomes per-tensor-group placement policy across HBM/DDR/CXL,
  with migration-as-scheduled-DMA handling promotion.

## 3. Algorithms that unify with OS primitives

- **PagedAttention is virtual memory.** vLLM's KV blocks + block tables +
  copy-on-write prefix sharing is paging rebuilt in a Python process. Native
  form: the **KV cache is a kernel-visible paged memory object** - block-
  granular HBM grants, block tables the GPU MMU walks, copy-on-write across
  requests, and prefix caching as **content-addressed KV blocks** (identical
  prompt prefix -> identical block hash -> shared block, even across cells).
  Kernel provides block/remap/share; the server keeps eviction policy.
- **Continuous batching is the completion-window contract** (IO.md 2):
  requests declare latency windows, the system forms per-iteration batches
  across heterogeneous deadlines. Interactive tokens get tight windows, batch
  jobs loose ones, one mechanism.
- **Speculative decoding, MoE routing, early exit are conditional dependency-
  graph edges** - one graph feature serves all three, instead of each
  framework hand-rolling CPU-side control loops that stall the device.
- **Streaming inference** (whisper.cpp-style) composes from parts: audio
  frames arrive on a device queue with real-time reservations
  (ARCHITECTURE.md 4.2), feeding an inference graph - speech-to-text with
  latency contracts, not hope.

## 4. The compilation stack and the tile IR

Unified dispatch happens at the IR, not the binary:

```
Graph IR      operators, dependencies, conditional edges
   |
Tile IR       typed tiles (register/shared/HBM), async tile copies,
   |          shape-negotiated MMA, compiler-owned layouts, pipelines
Per-engine    PTX/SASS (via contained ptxas), ROCm/MFMA, NPU command
lowering      streams, CPU AVX-512/AMX/SVE
   |
Autotune +    content-addressed, cluster-wide shared objects
artifact cache
```

- **Tiles are the industry's convergence point:** NVIDIA MMA fragments + TMA,
  AMD MFMA, Intel AMX tile registers, NPU systolic arrays are the same idea.
  "Directly CUDA-effective" means the IR lowers 1:1 onto async tile copies +
  tile MMA + staged pipelines - meeting CUDA where it is going (cuTile/Triton
  direction), not emulating the 2008 scalar-thread model.
- The tile IR is the typed-memory doctrine recursed *inside the chip*: tiles
  carry shape, dtype (fp16/bf16/fp8/int4-block-quant - the same typed buffer
  formats as the model store), and memory space; layouts (swizzles, bank-
  conflict avoidance) are compiler-owned algebra (CuTe-style), not folklore.
- **The autotune cache as a system service** is the OS-level win CUDA-land
  lacks: tuning results keyed by (kernel IR hash, engine model + firmware,
  shape class) are content-addressed objects valid cluster-wide. First run
  anywhere tunes; everyone on identical silicon gets a lookup. Fleet-scale
  amortization of the most wasteful ritual in GPU computing.
- **Kernel resource shapes** (shared mem/block, registers/thread, occupancy)
  ride the submission ABI, so the engine scheduler sizes partitions and
  co-location from facts (ACCELERATORS.md 3).

## 5. Honest costs

- The compilation service is the giant risk: CUDA's moat is a decade of
  hand-tuned kernels, so a graph-IR + Vulkan/tile route starts slower on
  NVIDIA than native CUDA. Mitigation: contained vendor-blob cells run
  vendor-compiled kernels for peak paths (native performance without kernel
  trust), and portable investment concentrates on the ~20 kernels that are
  ~95% of inference FLOPs - attention and GEMM families - where parity is a
  finite target (ARCHITECTURE.md P10 gates >= 85% on two GPU generations).
- Paged KV needs the GPU MMU to cooperate at useful granularities: modern
  GPUs yes, most NPUs no (they fall back to server-managed blocks).
- The uniform engine contract is ~80% real, ~20% per-family quirks forever.
- OpenCL's failure mode - a portable layer just bad enough that everyone
  routes around it - is the thing to design against: portable-by-default,
  vendor-contained-by-exception.

## 6. Where inference meets the rest of the system

- Serving is a cell (or cell group) with reservations; it is scheduled,
  metered, traced, and secured like everything else.
- Multi-host training is a **graph job** (CONTAINERS-KUBERNETES.md 4):
  compute nodes, RDMA transfer nodes, and collectives as one gang-scheduled
  dependency graph across hosts - a concept Kubernetes lacks and frameworks
  fake above the API.
- Model provenance plugs into attestation (SECURITY-IDENTITY.md): a
  regulated deployment can prove exactly which weights served a request.
