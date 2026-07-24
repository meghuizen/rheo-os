# Build Order - Which Subsystems, In What Sequence

**Status:** Draft v0.1. The implementation roadmap. Expands the milestone
gates in ARCHITECTURE.md 8.6 into a finer dependency-ordered sequence. Each
step lists what it depends on, what it unlocks, and how you know it works.
Pairs with DEVELOPMENT.md (the mechanics) and the per-subsystem docs.

Guiding rule: **build the thing everything else stands on, verify it, then
build the next thing that only depends on verified layers.** The two
components everything trusts - the capability core and the attestation chain -
get built and proven early, because retrofitting trust is impossible
(SECURITY-IDENTITY.md 9).

## Phase 0 - Foundation you cannot skip

**Step 0. Toolchain, xtask, QEMU harness, CI.**
- Depends on: nothing.
- Unlocks: every other step.
- Done when: `cargo xtask run` builds and boots an empty kernel on all three
  ISAs in QEMU, and CI is green (DEVELOPMENT.md 10).

**Step 1. Boot + serial output.**
- Depends on: step 0.
- Build: per-ISA `boot.S` entry, stack setup, BSS clear, jump to Rust
  `kernel_main`; wire the UART for `println!`; wire the QEMU exit device.
- Unlocks: all debugging. You cannot debug what cannot talk.
- Done when: the kernel prints a line and exits clean via the test device on
  x86-64, ARM64, RISC-V.

**Step 2. Arch trait skeleton.**
- Depends on: step 1.
- Build: exception/trap vectors, one-shot timer, per-core bring-up (SMP later),
  and the trait seams for page tables, IOMMU, interrupts, atomics
  (TARGET-ARCHITECTURES.md 4). Most implementations are stubs at first.
- Unlocks: memory management and everything requiring traps.
- Done when: a deliberate trap (breakpoint, page fault) is caught and reported
  to serial, not a triple fault; `-d int` shows the expected vector.

## Phase 1 - The trusted core (verify before proceeding)

**Step 3. Physical frame allocator + virtual memory.**
- Depends on: step 2 (page tables).
- Build: frame allocator, the page-table walker/mapper behind the Arch trait,
  a minimal typed **memory grant** (MEMORY.md). Huge-page support from the
  start (MEMORY.md 4).
- Unlocks: address spaces, therefore cells.
- Done when: map/unmap works, `info tlb` in the monitor matches expectations,
  and ASID/PCID tagging switches address spaces without a full flush.

**Step 4. The capability core.** *(The single most important step.)*
- Depends on: step 3.
- Build: mint, delegate, derive-subset, revoke-by-epoch, grant-check - a few
  thousand lines, kept small on purpose (SECURITY-IDENTITY.md 2).
- Verify: the Verus proofs from ARCHITECTURE.md 8.2 (unforgeability, monotonic
  attenuation, revocation soundness, isolation lemma). This is a gate, not a
  nice-to-have. If proofs overrun, invoke the seL4 fallback (8.2).
- Unlocks: all isolation and all security. Nothing above is sound without it.
- Done when: proofs close for items 1-3 and the grant-check microbenchmark
  (P1: < 50 ns p99) trends correctly in QEMU.

## Phase 2 - Making the kernel usable

**Step 5. Cells + minimal scheduler.**
- Depends on: steps 3, 4.
- Build: the **cell** object (address space + capability set), and a
  single-vcore round-trip scheduler good enough to run one cell.
- Unlocks: something to schedule and something to protect.
- Done when: two cells run with disjoint capability sets and provably cannot
  touch each other's memory (the isolation lemma, now tested in the
  implementation, not just proven).

**Step 6. Queue pairs + doorbell - the syscall surface.**
- Depends on: step 5.
- Build: submission/completion rings, the doorbell, the fixed-layout Tier-1
  ABI entries (IO.md 1, DATA-FORMATS.md 1), generated from the IDL.
- Unlocks: every interaction between a cell and the kernel, and later between
  cells. This is *the* interface - after this, new functionality is mostly
  new opcodes and new services, not new kernel mechanisms.
- Done when: a cell submits work and receives completions; the null round-trip
  benchmark (P2) trends correctly; the entry parsers are under fuzzing.

**Step 7. Two-level scheduler + strands.**
- Depends on: steps 5, 6.
- Build: vcore granting + revocation events, the userspace strand runtime
  (CONCURRENCY.md), scheduler-activation notifications over queues. Pools and
  reservations come next (step 9).
- Unlocks: real concurrency, light threads, the async model end to end.
- Done when: strand spawn/switch (P4) and same-cell context switch (P3) trend
  correctly; a runaway strand is preempted by the doorbell (CONCURRENCY.md 4).
- **Status (in progress):** the `runtime/` crate implements the core strand
  model - a `Future`-based executor whose strands park on a token and are
  woken by the queue-pair completion carrying it (async on the real queue-pair
  ABI), an async channel, `spawn`/`JoinHandle`/`yield_now`, an async `Mutex`
  (park-based) + a fair `TicketLock`, a heap allocator so `alloc` works in a
  cell, and capability rights at the type level. Proven kernel-context on all
  three ISAs (`runtime` test kernel). P4 is measured: `bench` reports
  `p4_strand_spawn_teardown` (~450 insns) and `p4_strand_switch` (~150 insns),
  and comparison/threads/ validates strands as light threads on the host
  (~1,200-1,600x faster spawn than OS threads, ~8-17x vs goroutines). Still
  ahead: multiple vcores + granting, the preemption doorbell (needs the
  timer/IRQ path), scheduler-activation notifications, stackful strands,
  priority-inheritance locks, and running the runtime inside a U-mode cell (a
  `.user` heap grant + `mem*` shims).

## Phase 3 - Time, memory, and eyes

**Step 8. Full memory grants + reclaim.**
- Depends on: steps 3, 6.
- Build: elastic grants, pressure events with deadlines, per-vcore arenas,
  scheduled zeroing (MEMORY.md 4-7). Retire the minimal grant from step 3.
- Done when: a cell under pressure returns memory cooperatively, and forced
  decommit makes only the offending cell fault (no OOM killer, MEMORY.md 7).

**Step 9. Clocks, timers, entropy + reservations.**
- Depends on: steps 2, 7.
- Build: monotonic/wall/engine clock objects with error bound e, one-shot
  tickless timers, the root DRBG + per-cell DRBGs (TIME-IDENTITY.md), and the
  EDF **reservation** admission model (SCHEDULING.md 4). Pools finalized here
  (latency/shared/system).
- Done when: a hard reservation holds its deadline on a dedicated tickless
  core under load (P12 jitter target); DRBG health tests pass; restore reseeds.

**Step 10. Event stream + flow context.** *(Do this early, not late.)*
- Depends on: step 6.
- Build: typed events into per-cell/per-vcore rings, 16-byte flow context on
  every queue entry, kernel propagation through graphs (OBSERVABILITY.md 2-4).
- Why here: retrofitting tracing after the system is large is exactly the
  Linux pain this design avoids; the propagation must be in the ABI from the
  moment the ABI carries real traffic.
- Done when: a request's flow ID is visible across cells and completions in a
  captured event log.

## Phase 4 - Talking to hardware

**Step 11. First engine: a virtio device.**
- Depends on: steps 6, 8.
- Build: the **engine** object and its attach/benchmark flow (ACCELERATORS.md
  1, BOOT.md 5), proven against a simple `virtio-blk` or `virtio-net` in
  QEMU. Start with virtio because it is simple and available before any real
  driver - and note it is *not* a throwaway: virtio (plus ENA/gVNIC/NetVSC) is
  the genuine production datapath for cloud and virtualized deployment
  (CLOUD.md 2), so this step is built to the production bar, not stubbed.
- Unlocks: I/O, the whole engine model validated cheaply, and the cloud-guest
  path (PRODUCTION.md 2 - cloud reaches hardware completeness early).
- Done when: a cell owns a virtio queue and does real I/O through the queue
  ABI, benchmarked at attach, with full error/reset handling.

**Step 12. IOMMU integration.**
- Depends on: step 11.
- Build: per-queue IOMMU domains behind the Arch trait, so every device DMA is
  mediated and grant-checked (doctrine 1). QEMU `intel-iommu` / `smmuv3` /
  RISC-V IOMMU model this.
- Done when: a device can only DMA into buffers the owning cell granted; an
  out-of-grant DMA faults.

**Step 13. Storage: minimal object store.**
- Depends on: steps 11, 12.
- Build: the append-log object class and content-addressed immutable objects
  first - the state store, model registry, and image pipeline all need exactly
  these two (FILESYSTEMS.md 6). Durability classes and group commit (IO.md 2).
- Done when: content-addressed put/get with hash-as-integrity, and a durable
  append-log survives a simulated crash.

**Step 14. Networking + transport library.**
- Depends on: steps 11, 12.
- Build: cell-owned NIC queues, the blessed Rust transport library
  (QUIC-first), inline-crypto seam (NETWORKING.md). WASM dataplane and the
  DDoS pre-steering stage follow once the basics work.
- Done when: two cells exchange data over a real transport; throughput trends
  toward the tuned-Linux control (M2 gate).

## Phase 5 - Distribution and the control plane

**Step 15. Cluster fundamentals.**
- Depends on: steps 4, 9, 13, 14.
- Build: host attestation + the boot chain (BOOT.md), cryptographic
  capabilities with epoch revocation (canonical CBOR, DATA-FORMATS.md 2),
  leases with fencing, membership consensus, and the typed **state store**
  with queue-based watch (CONTAINERS-KUBERNETES.md 3).
- Verify: the TLA+ models (ARCHITECTURE.md 8.1) plus a Jepsen-style suite
  under partitions; the deterministic simulation harness (EMULATION.md 5)
  drives this cheaply and reproducibly.
- Gate: this is where the two research-grade risks (distributed revocation,
  lease/failure semantics) must prove out, or the cluster story reverts to
  single-host + explicit federation (M3, ARCHITECTURE.md 8.7).

**Step 16. Orchestration + observability export.**
- Depends on: step 15.
- Build: the host reconciler as PID 1, controllers as cells, cell groups,
  gang-aware placement, graph jobs (CONTAINERS-KUBERNETES.md), the Kubernetes
  compat edge (kubectl works), and the OTel exporter cell (OBSERVABILITY.md 6).
- Done when: a 3-service demo deploys across 3 hosts and is traceable end to
  end with zero manual instrumentation (M4 gate).

## Phase 6 - Personalities and the payload workloads

**Step 17. POSIX personality + SSH.**
- Depends on: steps 7, 13, 14.
- Build: the syscall translation layer, PTYs, per-session filesystem view,
  identity-based login (POSIX-PERSONALITY.md).
- Done when: SSH in, get bash, navigate the object store; P11 fidelity gate.

**Step 18. Accelerators + AI layer.**
- Depends on: steps 11, 12, 13.
- Build: real GPU/NPU engines as contained driver cells, model objects +
  shared weights + paged KV, the compilation service and tile IR on one GPU
  family (AI-ARCHITECTURE.md, ACCELERATORS.md).
- Done when: serve a 7B-class model within 15% of vLLM-on-tuned-Linux on
  identical hardware; tile-IR GEMM/attention hits P10 (M5 gate).

## Phase 7 - Later tiers (in-doctrine, deferred)

**Step 19. Graphics** (Vulkan + compositor cell, GRAPHICS.md) - only when a
form factor demands it.

**Step 20. Virtualization** (the VMM product over the acceleration primitives
already present from Phase 4, VIRTUALIZATION.md 9) and **cross-ISA emulation**
(EMULATION.md 4).

## The dependency picture in one glance

```
0 toolchain
1 boot/serial
2 arch skeleton
3 frame alloc + VM ─────────────┐
4 capability core [VERIFY] ──────┤
5 cells + sched                  │
6 queues/doorbell (the ABI) ─────┼── after this, most work is
7 two-level sched + strands      │    services + opcodes, not
8 memory grants + reclaim        │    new kernel mechanisms
9 clocks + entropy + reservations│
10 events + flow context (early!)┘
11 first engine (virtio)
12 IOMMU
13 object store (log + CAS first)
14 networking + transport
15 cluster [VERIFY: TLA+/Jepsen] ── research-risk gate (M3)
16 orchestration + OTel edge
17 POSIX + SSH
18 accelerators + AI + tile IR
19 graphics        20 virtualization/emulation  (deferred)
```

Two hard gates dominate the schedule: **step 4** (the capability core must be
proven before anything trusts it) and **step 15** (the distributed protocols
must survive partition testing before multi-host is promised). Everything
between them is engineering; those two are where the design lives or dies.
