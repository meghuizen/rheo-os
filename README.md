# Lattice OS - Documentation Map

**Status:** Draft v0.1. Codename provisional.

**Stance:** This is a greenfield project for modern systems. It targets today's
server hardware and its capabilities (wide SIMD/AVX-512/SVE2, tile/AMX
instructions, IOMMU, measured boot, RDMA, accelerators) and exploits them when
present rather than designing to the weakest denominator. Backwards
compatibility exists only as translation edges (POSIX, the Kubernetes API) and
broadening it is a future concern; embedded/real-time-small-footprint support
is a planned later stage. See ARCHITECTURE.md 1.4.

Read `ARCHITECTURE.md` first. It contains the why, the doctrines, the kernel
object model, and the verification plan. Every other document expands one
subsystem and must stay consistent with the doctrines and the kernel
admission rule defined there.

| Document | Covers |
|---|---|
| `ARCHITECTURE.md` | Why, doctrines, kernel object model, all subsystems condensed, verification plan, risks |
| `PROFILES.md` | One core, many targets: per-profile fit map, needs, sequencing (servers, edge, embedded, IoT, remote, desktop) |
| `VALIDATION.md` | Per-profile validation: coverage matrix, workloads, targets and kill thresholds vs best-incumbent baselines (P13-P46) |
| `SIMULATION.md` | Concrete kernel-event simulation of all 12 deployment scenarios (fleet server through remote Africa to desktop) |
| `REFLECTION-NEXUS.md` | Scrutiny of an alternative design; simulated the divergences; three refinements adopted, three rejected with reasons |
| `OPEN-QUESTIONS.md` | Five design refinements resolved: graph granularity, error/cancellation, strand-vcore mapping, typed memory in graphs, distributed debuggability |
| `KERNEL-RUST.md` | Rust implementation guide: kernel targets, capability types, ring buffers, Verus proofs, code size, C#/cross-language interop |
| `SHELL.md` | lsh: Lattice-native shell, latte-term emulator, typed pipelines, graph blocks, TUI, tab completion, scripting |
| `LOGGING.md` | Fast logging: per-strand ring buffers, lazy formatting, zero-cost disabled paths, template interning, console.writeline compat |
| `REALTIME.md` | Real-time and time-certain workloads: sleep precision, jitter sources, PeriodicTask API, hardware timers, SMI/NMI limits |
| `TARGET-ARCHITECTURES.md` | ISAs, per-profile hardware floor, Arch trait, engine matrix, support tiers |
| `PRODUCTION.md` | Production quality bar, hardware-breadth/driver strategy, reliability, operability |
| `CLOUD.md` | Running as a guest in AWS/Azure/GCP, and being an attractive host OS for cloud providers to build on (not operating a cloud) |
| `BOOT.md` | Measured boot chain, attestation, system image model, host bring-up |
| `SECURITY-IDENTITY.md` | SPIFFE-style identity, capabilities, revocation, secrets, audit |
| `MEMORY.md` | Typed memory, stack/heap, per-vcore arenas, huge pages, reclaim, safety |
| `POWER.md` | Energy as a typed/metered/reservable resource, DVFS, idle, battery/solar, brownout |
| `SCHEDULING.md` | Tickless cores, pools, two-level scheduling, real-time reservations, NUMA, SMT, interference |
| `CONCURRENCY.md` | Strands, light threading, the common bug classes, locking, memory model |
| `TIME-IDENTITY.md` | Interval clocks, PTP/NTS, HLC ordering, UUIDv7, per-cell DRBGs |
| `DATA-FORMATS.md` | The three format tiers, Arrow/Parquet/Avro/CBOR, IPC and edge protocols, text formats |
| `CONTAINERS-KUBERNETES.md` | Cells as containers, absorbed Kubernetes, state plane, compat edge |
| `CLUSTER.md` | Running a fleet: node roles, join, orchestration, kubectl support, shared compute, CephFS, NUMA, Raspberry Pi lab |
| `VIRTUALIZATION.md` | Cell vs VM, container/VM hardware acceleration, SR-IOV, confidential compute |
| `IO.md` | Queue ABI, completion contracts, streams, zero copy, DMA graphs |
| `FILESYSTEMS.md` | Three storage tiers, native object store, POSIX view synthesis |
| `NETWORKING.md` | Cell-owned NICs, QUIC/TLS, WASM dataplane, DDoS pipeline |
| `ACCELERATORS.md` | Engine contract, GPU/NPU/TPU/FPGA/DPU, driver cell containment |
| `AI-ARCHITECTURE.md` | Kernel vs service vs library split, model objects, KV paging, tile IR |
| `GRAPHICS.md` | Vulkan mapping, compositor cells, HID, display scope |
| `DISPLAY.md` | Frame buffers, vsync events, double/triple buffering, VRR, frame pacing, input-to-photon latency |
| `OBSERVABILITY.md` | Flow context, event streams, span trees, WASM probes, OTel export |
| `POSIX-PERSONALITY.md` | The translation layer, SSH/bash, Linux binary support, known gaps |
| `EMULATION.md` | QEMU tiers, guest mode, virtual engines, deterministic simulation, cross-ISA |
| `TOOLING.md` | Build, CI, debugging, probes, image pipeline, CLI |
| `DEVELOPMENT.md` | Compile, boot models, QEMU invocations, GDB/monitor debugging, disassembly |
| `BUILD-ORDER.md` | Dependency-ordered subsystem sequence, verify gates, roadmap |

Editing rule: a change that adds a kernel object or verb must pass the
admission rule in `ARCHITECTURE.md` section 6 and be reflected there first.
# rheo-os
