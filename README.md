# rheo-os

**Status:** Draft v0.1. Design codename "Lattice OS" (provisional).

A greenfield operating system for modern systems: a `no_std` Rust kernel
with capability-based security, developed emulation-first in QEMU on three
ISAs (x86-64, ARM64, RISC-V 64). It targets today's server hardware and its
capabilities (wide SIMD, IOMMU, measured boot, RDMA, accelerators) and
exploits them when present rather than designing to the weakest denominator.
Backwards compatibility exists only as translation edges (POSIX, the
Kubernetes API). See `docs/ARCHITECTURE.md` 1.4.

## Getting started

### 1. Install the prerequisites

- **Rust** via [rustup](https://rustup.rs/). The exact toolchain (a pinned
  nightly, plus the three bare-metal targets and components) is declared in
  `rust-toolchain.toml` - rustup installs all of it automatically the first
  time you run a `cargo` command in this repository.
- **QEMU 8.x+** system emulators. On Debian/Ubuntu:

  ```sh
  sudo apt install qemu-system-x86 qemu-system-arm qemu-system-misc
  ```

  On macOS: `brew install qemu`.
- Optional, for debugging: `gdb-multiarch` (or LLDB).

### 2. Build the kernel

```sh
cargo xtask build --arch all          # x86_64 + aarch64 + riscv64
cargo xtask build --arch riscv64      # just one ISA
```

`cargo xtask` is a small Rust helper (in `xtask/`) that wraps the awkward
parts: the `build-std` flags, linker script selection, and QEMU invocations.
Plain `cargo build` only builds that helper - the kernel always needs a
bare-metal `--target`, which xtask supplies.

### 3. Run it in QEMU

```sh
cargo xtask run --arch riscv64
```

You get the serial console on your terminal, multiplexed with the QEMU
monitor (Ctrl-A C toggles between them, Ctrl-A X quits). Today the kernel
prints one boot line and exits - that is BUILD-ORDER.md step 1.

### 4. Run the boot tests

```sh
cargo xtask test --arch all
```

Each ISA is booted headless with a 60-second timeout. The kernel reports
pass/fail through a QEMU exit device, so the result is the process exit
code - the same check CI runs on every push. Serial output is saved to
`target/qemu-<arch>.log`.

### 5. Debug

Add `-s -S` style debugging per `docs/DEVELOPMENT.md` 7: run QEMU with a GDB
stub, attach `gdb-multiarch`, break on `kernel_main`. The same document
covers the QEMU monitor, tracing flags, and turning a panic address back
into a source line.

## Repository layout

```
docs/         the design documents - the spec everything must stay consistent with
kernel/       the no_std kernel crate
  src/        ISA-independent kernel code
  src/arch/   per-ISA Rust modules (x86_64, aarch64, riscv64)
  arch/       per-ISA assembly (boot.S entry stubs)
  link/       linker scripts per ISA
xtask/        build/run/test orchestration (cargo xtask ...)
idl/          system IDL + codegen                    (future)
runtime/      strand runtime library                  (future)
services/     system service cells                    (future)
tests/        in-QEMU test kernels                    (future)
targets/      custom target JSON, if ever needed
```

## Documentation map

Read `docs/ARCHITECTURE.md` first. It contains the why, the doctrines, the
kernel object model, and the verification plan. Every other document expands
one subsystem and must stay consistent with the doctrines and the kernel
admission rule defined there. `docs/BUILD-ORDER.md` is the implementation
roadmap; `docs/DEVELOPMENT.md` is the practical how-to.

| Document | Covers |
|---|---|
| `docs/ARCHITECTURE.md` | Why, doctrines, kernel object model, all subsystems condensed, verification plan, risks |
| `docs/PROFILES.md` | One core, many targets: per-profile fit map, needs, sequencing (servers, edge, embedded, IoT, remote, desktop) |
| `docs/VALIDATION.md` | Per-profile validation: coverage matrix, workloads, targets and kill thresholds vs best-incumbent baselines (P13-P46) |
| `docs/SIMULATION.md` | Concrete kernel-event simulation of all 12 deployment scenarios (fleet server through remote Africa to desktop) |
| `docs/REFLECTION-NEXUS.md` | Scrutiny of an alternative design; simulated the divergences; three refinements adopted, three rejected with reasons |
| `docs/OPEN-QUESTIONS.md` | Five design refinements resolved: graph granularity, error/cancellation, strand-vcore mapping, typed memory in graphs, distributed debuggability |
| `docs/KERNEL-RUST.md` | Rust implementation guide: kernel targets, capability types, ring buffers, Verus proofs, code size, C#/cross-language interop |
| `docs/SHELL.md` | lsh: Lattice-native shell, latte-term emulator, typed pipelines, graph blocks, TUI, tab completion, scripting |
| `docs/LOGGING.md` | Fast logging: per-strand ring buffers, lazy formatting, zero-cost disabled paths, template interning, console.writeline compat |
| `docs/REALTIME.md` | Real-time and time-certain workloads: sleep precision, jitter sources, PeriodicTask API, hardware timers, SMI/NMI limits |
| `docs/TARGET-ARCHITECTURES.md` | ISAs, per-profile hardware floor, Arch trait, engine matrix, support tiers |
| `docs/PRODUCTION.md` | Production quality bar, hardware-breadth/driver strategy, reliability, operability |
| `docs/CLOUD.md` | Running as a guest in AWS/Azure/GCP, and being an attractive host OS for cloud providers to build on |
| `docs/BOOT.md` | Measured boot chain, attestation, system image model, host bring-up |
| `docs/SECURITY-IDENTITY.md` | SPIFFE-style identity, capabilities, revocation, secrets, audit |
| `docs/MEMORY.md` | Typed memory, stack/heap, per-vcore arenas, huge pages, reclaim, safety |
| `docs/POWER.md` | Energy as a typed/metered/reservable resource, DVFS, idle, battery/solar, brownout |
| `docs/SCHEDULING.md` | Tickless cores, pools, two-level scheduling, real-time reservations, NUMA, SMT, interference |
| `docs/CONCURRENCY.md` | Strands, light threading, the common bug classes, locking, memory model |
| `docs/TIME-IDENTITY.md` | Interval clocks, PTP/NTS, HLC ordering, UUIDv7, per-cell DRBGs |
| `docs/DATA-FORMATS.md` | The three format tiers, Arrow/Parquet/Avro/CBOR, IPC and edge protocols, text formats |
| `docs/CONTAINERS-KUBERNETES.md` | Cells as containers, absorbed Kubernetes, state plane, compat edge |
| `docs/CLUSTER.md` | Running a fleet: node roles, join, orchestration, kubectl support, shared compute, CephFS, NUMA, Raspberry Pi lab |
| `docs/VIRTUALIZATION.md` | Cell vs VM, container/VM hardware acceleration, SR-IOV, confidential compute |
| `docs/IO.md` | Queue ABI, completion contracts, streams, zero copy, DMA graphs |
| `docs/FILESYSTEMS.md` | Three storage tiers, native object store, POSIX view synthesis |
| `docs/NETWORKING.md` | Cell-owned NICs, QUIC/TLS, WASM dataplane, DDoS pipeline |
| `docs/ACCELERATORS.md` | Engine contract, GPU/NPU/TPU/FPGA/DPU, driver cell containment |
| `docs/AI-ARCHITECTURE.md` | Kernel vs service vs library split, model objects, KV paging, tile IR |
| `docs/GRAPHICS.md` | Vulkan mapping, compositor cells, HID, display scope |
| `docs/DISPLAY.md` | Frame buffers, vsync events, double/triple buffering, VRR, frame pacing, input-to-photon latency |
| `docs/OBSERVABILITY.md` | Flow context, event streams, span trees, WASM probes, OTel export |
| `docs/POSIX-PERSONALITY.md` | The translation layer, SSH/bash, Linux binary support, known gaps |
| `docs/EMULATION.md` | QEMU tiers, guest mode, virtual engines, deterministic simulation, cross-ISA |
| `docs/TOOLING.md` | Build, CI, debugging, probes, image pipeline, CLI |
| `docs/DEVELOPMENT.md` | Compile, boot models, QEMU invocations, GDB/monitor debugging, disassembly |
| `docs/BUILD-ORDER.md` | Dependency-ordered subsystem sequence, verify gates, roadmap |

Editing rule: a change that adds a kernel object or verb must pass the
admission rule in `docs/ARCHITECTURE.md` section 6 and be reflected there
first.

## Continuous integration

Every push and pull request runs `.github/workflows/ci.yml`: rustfmt and
clippy over all targets, then a headless QEMU boot test per ISA with the
serial log uploaded as an artifact. QEMU runs gate correctness; absolute
performance numbers only gate on the hardware lab (from milestone M1, see
`docs/TOOLING.md` 4).
