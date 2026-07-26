# Tiles - One Tile Program, Every Engine

**Status:** Building. Expands AI-ARCHITECTURE.md 4 (the tile IR),
GPU-HARDWARE.md 11 (the tile/memory collaboration contract), and
ACCELERATORS.md 1/3 (the engine contract and its scheduling rules).
Implementation: `librheo/src/tile/` (the framework), graph-node ops 4-5
(the kernel slice), the `librheotile` + `librheotilebattle` test kernels,
`bench-core` `p5_*`, and `comparison/tiles/` (the host battle test).

Position: tile-centric compute - TileLang/cuTile/Triton on GPUs, SME tile
registers and AMX on CPUs, systolic arrays on NPUs/TPUs, partial
reconfiguration regions on FPGAs - is the industry's convergence point,
and on this OS it is a **library discipline over existing kernel
objects**, not a kernel subsystem. A tile is shape x dtype x memory
space; an engine is anything that declares a TileContract; one
TileProgram runs on whichever engines exist. The kernel gains exactly two
graph-node op codes and nothing else: tiles ride the engine (object 4),
the memory grant (object 5), and the dependency graph (object 6), passing
ARCHITECTURE.md 6 with zero new objects, zero new syscalls, and zero new
queue opcodes.

---

## 1. The tile model

A **tile** is a rectangular view into a typed buffer: shape (rows x
cols), dtype (section 2), stride, and the memory space it lives in. A
**TileBuf<D>** is a dtype-tagged buffer over a memory grant - the same
capability that lets a cell map memory is the one its kernels tile over
(GPU-HARDWARE.md 11). Views are bounds-checked at creation; an
out-of-range tile is a `None`, not a fault.

The memory-space vocabulary is the typed-memory doctrine recursed inside
the chip, and it is what unifies the engine kinds:

| Engine        | Register class      | Staging class    | Bulk class          |
|---------------|---------------------|------------------|---------------------|
| GPU           | MMA fragments/regs  | shared memory    | HBM (`Buffer<Hbm>`) |
| CPU (SME/AMX) | ZA tiles / AMX regs | L1/L2 (implicit) | DDR                 |
| NPU           | PE registers        | on-chip SRAM     | DDR/HBM             |
| TPU-class     | systolic cells      | vector memory    | HBM                 |
| FPGA          | fabric registers    | BRAM/URAM        | HBM/DDR             |

One program names spaces abstractly (`Host`, `Scratch`, `Device`); the
per-engine lowering (section 4) decides what each means on real silicon.
On the CPU executor `Host` is the grant itself and `Scratch` is an Arena
sub-allocation whose locality benefit comes from the cache hierarchy -
which QEMU does not model, so that benefit is only demonstrable on real
hardware (section 10, `comparison/tiles`).

## 2. Dtypes and quantization

Dtype rides the buffer type (GPU-HARDWARE.md 11): `TileBuf<I8>` and
`TileBuf<F32>` are different types, and a GEMM whose inputs disagree with
its accumulator is a **compile error at program build**, not a garbage
result at runtime. The dtype set:

- **Native** (representable and executable on the CPU executor today):
  `I8`, `U8`, `I32`, `F32`. Integer paths are exact everywhere; `F32`
  executes as soft-float in a cell (the bare targets are soft-float -
  honest and slow, stated).
- **Declared** (types now, executable when an engine lowers them):
  `F16`, `Bf16`, `F8E4M3`, `I4Block32`. These have no MMA implementation
  trait, so `gemm` over them does not compile - the type system saying
  "no engine here can run this" at build time.

The workhorse is **int8 x int8 -> i32** - the production quantized-
inference GEMM, integer-exact on every ISA, and the only dtype pair the
kernel slice accepts (section 6). Quantize/dequantize (per-block
symmetric f32 <-> i8 with allocation-visible scale planes) and integer
requantize (shift + clamp i32 -> i8) are **program ops**, scheduled and
counted like any tile op - never silent format coercion.

## 3. The TileContract

What an engine declares at attach, extending the engine object's
attest-by-measurement contract (GPU-HARDWARE.md 9):

```rust
pub struct TileContract {
    pub kind: EngineKind,            // Cpu | Gpu | Npu | Other
    pub vendor: u16,                 // PCI vendor for device engines
    pub measured: bool,              // attach benchmark ran (never a claim)
    pub mma: &'static [(Dtype, TileShape)], // native MMA shapes per dtype
    pub spaces: &'static [SpaceDesc],       // memory spaces + sizes
    pub copy_engines: u8,            // async bulk-copy paths (TMA-class)
    pub preemption: Preemption,      // Instruction | OpBoundary
}
```

`contract_for(engine_index)` builds this in librheo from
`SYS_ENGINE_INFO` plus a per-kind table: the **CPU** engine is measured
(the attach benchmark) and executes; **GPUs** are enumerated with their
real vendor IDs and an honest zero measured cost (recognised, not yet
driven for compute - GPU-HARDWARE.md 12); **NPU/TPU** engines arrive
through the same PCIe classification (`EngineKind::Accelerator`, PCI
class 0x12, already recognised by the hardware discovery) with
declared-by-kind defaults; an **FPGA**'s contract rides its bitstream
identity - a bitstream is an attested engine (ACCELERATORS.md 4), so a
partial-reconfiguration region declares its tile shapes the way a fixed
device does. No contract field is ever fabricated: `measured == false`
means exactly that placement has no measured basis yet.

## 4. One program, per-engine lowering

A `TileProgram` is built once and lowered per engine:

- **CpuExecutor** - the library-call lowering (the TIME-IDENTITY.md 4
  "library call, not syscall" pattern applied to compute): the program
  runs in the cell, strand-parallel over disjoint output row-bands, with
  scalar inner kernels today and SIMD (AVX2/SVE/SME/AMX) when U-mode
  vector-state save/restore exists - until then the on-OS build stays
  scalar and the SIMD inner kernel lives in the host comparison behind a
  feature flag with a differential fuzz proof (the rheo-json precedent).
- **EngineExecutor** - lowers the SAME program to dependency-graph nodes
  (section 6) and submits over the queue (`OP_GRAPH_SUBMIT`). This is the
  device-portable artifact: engine 0 (the CPU engine) executes it today;
  a GPU/NPU/TPU/FPGA engine executes it when its contained driver cell
  exists (GPU-HARDWARE.md 5), and until then the executor returns
  `EngineUnavailable` - never a faked run.

Portable-by-default, vendor-contained-by-exception (AI-ARCHITECTURE.md
5): the named anti-goal is OpenCL's failure mode, a portable layer just
bad enough that everyone routes around it. The portable tile program is
the default; a vendor's hand-tuned kernel runs *contained* for peak
paths, and both are priced by the same measured contract.

## 5. Pipelines and yielding

Tile programs pipeline: copy stages and compute stages of independent
tiles interleave on the strand executor. Honest scope: on the single-CPU
cooperative runtime this is **interleaving, not overlap** - true overlap
of copy and compute arrives with SMP (task #27) and with device copy
engines.

The yield rule: the CPU executor awaits `yield_now()` at every tile-loop
back-edge - the persistent-kernel rule (ACCELERATORS.md 3: megakernels
carry cooperative yield points at tile-loop back-edges) applied to
strands. This keeps a compute-bound tile program from starving its
cell's other strands, and it keeps the reactor's idle ladder fed: a
dependency chain of tile stages that never touches the queue still makes
progress every round because yielded strands re-queue as runnable (the
`librheotilebattle` pipeline-depth fence proves exactly this against the
runtime's no-progress guard).

## 6. Tiles on the dependency graph (object 6)

The kernel slice - the "buffer-carrying node kind" LIBRHEO.md Phase C
documented as the next step - is two node op codes in the existing
`OP_GRAPH_SUBMIT` payload:

- **op 4, BufReduce**: `node.a` = the cell VA of a
  `BufReduceDesc { va, elems, dtype }`; the engine returns the wrapping
  u64 sum (sign-extending signed dtypes).
- **op 5, TileGemm**: `node.a` = the cell VA of a
  `TileGemmDesc { a_va, b_va, c_va, m, n, k, strides, dtypes }`; the
  engine zeroes C, runs the int8->i32 GEMM whole, and returns the FNV-1a
  hash of C.

Design decisions, stated:

- **Descriptor-VA indirection** (one `read_unaligned` of a `#[repr(C)]`
  struct from cell memory, the same trust model as `nodes_va` itself)
  rather than packing shapes into the node's `a`/`b` fields: it costs
  one node per op instead of three and extends to future ops with zero
  ABI churn. `GraphNode` stays 32 bytes; `MAX_NODES` stays 32 - a node
  is a tile *op*, not a tile; the loop lives inside the node.
- **Results stay u64**: the buffer carries the real output; the node
  result is a deterministic **receipt** (sum or FNV checksum) that the
  test kernels assert equals the library executor's receipt for the
  identical program.
- **Validation caps, not trust**: `1 <= m,n,k <= 256`, strides >= the
  matching dims, dtypes exactly (I8, I32); reduce `elems <= 1 << 20`,
  dtype in the native set. Any violation completes with `STATUS_DENIED`
  - never a kernel fault. Worst node = 16.7M MACs, worst graph = 32
  nodes: the documented bound on the synchronous doorbell drain.
- **No float in the kernel**: the kernel path is integer-only (the
  aarch64 kernel target is soft-float; float tile ops are CpuExecutor
  library calls in the cell).

Admission clearance (ARCHITECTURE.md 6): this adds **no kernel object,
no syscall, and no queue opcode** - two op codes inside an existing
node format, executed by the existing CPU engine, over VAs backed by
existing grants. Mechanism; every policy (tiling choice, placement,
scheduling) stays in the library.

## 7. The deterministic tile simulator (co-design)

`TileSim::simulate(program, contract) -> SimReport` walks a TileProgram
against a TileContract and **counts**: MMA tiles, MAC ops, element ops,
bytes moved per space class, tile round trips, yield points. Counts
only, never timing - fabricated clocks are exactly what EMULATION.md
rejects. Determinism is asserted (same program, same report, every run).

The co-design loop this enables (the tile-level simulation idea applied
to what this repo can honestly validate):

1. **Choose** a tiling by simulated traffic (bytes staged per space).
2. **Verify the work leg under QEMU icount**: tilings of the same GEMM
   have the same MAC count, so their instruction path lengths must be
   ~flat (`bench-core p5_gemm64_block{8,16,32}`).
3. **Verify the traffic leg on real caches**: the simulator's
   bytes-staged ordering across tilings must rank them in the same order
   host wall-clock does (`comparison/tiles`, SIM-vs-HOST table). A
   ranking divergence is **reported, never hidden** - the model is
   falsifiable, and the report is itself the deliverable.

## 8. The autotune cache key

Tuning results are keyed by
`(program hash, engine kind + vendor + identity, shape class)` - the
shape class buckets dims to powers of two so one tuning serves a family.
The key is computed now (`autotune_key`); the content-addressed,
cluster-wide autotune cache it indexes is the future system service
(AI-ARCHITECTURE.md 4) and is stated as unimplemented. A firmware or
measured-identity change invalidates the key by construction - stale
performance claims cannot survive an engine change.

## 9. What the kernel refuses

No tile compiler in the kernel. No vendor lowering in the kernel. No
float ops in the kernel engine. No unbounded node work (the section 6
caps). No fabricated device execution - an engine without an executor
returns `EngineUnavailable`, and a contract without a measurement says
`measured: false`.

## 10. The battle tier - production-shaped validation

The framework is battle-tested with **production-shaped** workloads on
the hardware this repo actually has: QEMU on three ISAs, and the x86-64
Linux host. The banner first: **no GPU numbers exist; QEMU proves paths
and correctness; the host proves cache behavior; production shapes are
the workloads' geometry, not a claim of serving a real model.**

Workloads (`librheotilebattle` in QEMU, `comparison/tiles` on the host):

- **7B-class layer GEMMs, int8**: the true shapes - 4096x4096x4096
  (QKV/O projections), 4096x11008x4096 (MLP), grouped-query attention
  (32 Q / 8 KV heads, head_dim 128) - run **full size on the host**. In
  QEMU they run **scaled, with the ratio printed in the output**
  (geometry preserved: square projections, the ~2.7:1 MLP aspect, the
  4:1 GQA head grouping); TCG cannot execute 68 GMAC in a 120 s test
  budget, and pretending otherwise is the dishonesty this repo forbids.
- **An attention block as one TileProgram**: QKV GEMMs -> score GEMM ->
  integer requantize (shift+clamp, the softmax slot - a true exp softmax
  is an F32 library map, stated) -> weighted-V GEMM, with an exact
  frozen checksum per stage.
- **A paged-KV pattern** (AI-ARCHITECTURE.md 3): fixed KV block tiles in
  one grant, two sequences whose block tables share a prefix; the
  shared-prefix output rows must be bit-identical - block-granular tiles
  and prefix sharing proven at library level (kernel block/remap/share
  stays future work).
- **The columnar scan expressed as tiles** (section 11's v1 conversion):
  the librheodata-shaped SUM/COUNT/MAX predicate scan as a TileProgram
  reduce, exact aggregate asserted.

Stress: a 100-iteration soak of the attention pipeline over reused
grants/arenas (identical checksum every iteration, frame counts checked
for leaks); boundary shapes ((13,17,5) under block 16, 1xN and Nx1,
stride > width, tail quantization blocks) all against the naive
reference; and the **pipeline-depth fence** - a 64-deep dependent tile
chain that never touches the queue must complete, the explicit
regression fence for the runtime's idle-ladder guard.

Where each leg runs and what it proves:

| Leg | Environment | Proves |
|---|---|---|
| `librheotilebattle` | QEMU x86-64 / ARM64 / RISC-V | correctness: exact checksums on all ISAs |
| `bench-core p5_*` | QEMU `-icount` | deterministic instruction path lengths per tile op and per tiling |
| `comparison/tiles` | x86-64 Linux host | real-cache tiled-vs-naive at true shapes; SIMD inner kernel; wall-clock per comparison rules |
| differential fuzz | host | tiled == naive and SIMD == scalar over >= 10k randomized shapes/strides |

## 11. Where the OS itself is tiles - the audit

Two directions: existing code paths that *are* tile operations (and what
v1 does about each), and kernel mechanisms that *augment* tiles.

**Paths that are tile ops today:**

| Subsystem | Tile op | Benefit | v1 action |
|---|---|---|---|
| librheodata columnar scan (Phase B) | REDUCE | the strongest existing tile-shaped workload; proves the framework expresses real OS workloads | **convert as proof**: expressed as a TileProgram stage in the battle tier (librheodata itself untouched) |
| Page zeroing (MEMORY.md 5) | FILL | zeroing as a scheduled engine job at memory-controller speed | document-only: becomes an engine fill node when a copy engine exists |
| Migration/eviction between kinds (MEMORY.md 2, GPU-HARDWARE.md 6) | COPY | already doctrine ("migration is a scheduled DMA graph node") | document-only: the tile copy op is its program-level name |
| virtio-gpu present memcpy + Phase E compositor composite | COPY/BLEND | present/composite become schedulable, meterable tile programs | document-only mapping |
| rheo-json SSE2 string scan | byte-tile scan | same inner-kernel discipline (scalar reference + SIMD + differential fuzz) | document-only; the discipline is already shared |
| rheo-net sharded framing | packet tiles | per-shard framing is tile-striped buffer work | document-only mapping |
| ChaCha20 DRBG block generation | tile-shaped state | block function over fixed-shape state | document-only |
| ext4/blockfs sector I/O | tile-granular transfer | sector runs are strided tile copies | document-only |

No speculative refactors: one conversion is proven in the battle tier;
everything else is a documented mapping that future work can pick up
with the framework already in place.

**Mechanisms that augment tiles:**

- **Grant commit quantum and Arena alignment** (MEMORY.md 4): the 2 MiB
  commit quantum and 1 GiB HBM pages are tile-working-set geometry; the
  allocator's alignment guarantees are what make tile strides honest.
- **The NUMA hint** (`reserve_on`'s node argument - recorded, not acted
  on, single-node in QEMU): the tile scheduler's placement input when
  multi-node hardware arrives.
- **The engine attach benchmark** (GPU-HARDWARE.md 9): its op classes
  *are* the tile op set - GEMM tile per dtype, copy per direction, fill
  - so placement prices exactly what tile programs emit.
- **Sealed-grant zero-copy sharing** (`SYS_GRANT_SHARE`): tile-buffer
  handoff between cells - the compositor/present precedent generalizes
  to producer/consumer tile pipelines across cells.
- **The deterministic-simulation seam** (EMULATION.md 5): the same
  clean queue-transport swap that simulates cells is the substrate
  TileSim extends toward whole-pipeline co-design.

## 12. Honest status

- **Runs today**: the CPU tile path - strand-parallel library executor
  and the kernel graph ops - on all three ISAs, integer-exact, with
  deterministic icount path lengths. F32 runs soft-float in cells.
- **QEMU has no cache hierarchy**, so tiling's locality win is invisible
  there by construction; it is demonstrated on the host
  (`comparison/tiles`) with real caches, and only there.
- **In-cell SIMD is blocked** on U-mode vector-state save/restore (no
  ISA has it yet); the SIMD inner kernel exists host-side behind a
  feature flag with a differential fuzz proof, per the rheo-json
  precedent.
- **Single vcore**: pipelining is cooperative interleaving until SMP
  (task #27).
- **Device engines are enumerated, not executing**: GPUs (with real
  vendor IDs), NPU/TPU-class accelerators (PCI class 0x12), and FPGAs
  ride the same contract and the same graph lowering; execution awaits
  their contained driver cells (GPU-HARDWARE.md 5). Nothing here
  pretends otherwise.
