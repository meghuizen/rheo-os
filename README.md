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
monitor (Ctrl-A C toggles between them, Ctrl-A X quits). Pass
`--bin bench-core` (or another test kernel) to boot that instead of the
boot demo.

### 4. Run the boot tests

```sh
cargo xtask test --arch all
```

Every test kernel is booted headless with a timeout: the boot demo, the
capability-invariants suite, the queue-pipeline scenario, `isolation-hw`
(real user-mode cells whose isolation is enforced by the MMU faulting),
the resource-object suite, `shell-smoke`, `hwinfo` (hardware discovery:
firmware source, CPU features, typed memory map, NUMA topology, PCIe
devices), `rng` (the ChaCha20 DRBG), `runtime` (the strand async runtime:
heap/`alloc`, executor, async channel, type-level capability rights, native
async over the real queue-pair ABI), `posix` (the filesystem + POSIX
stack: a read-write ramfs and a read-only ext4 image behind a VFS, the
POSIX fd surface, and a `std::fs` facade), and `blockfs` (a virtio-blk
driver reading a live ext4 disk through the `BlockDevice` seam; skips on
x86-64, which has no virtio-mmio). Each reports pass/fail through a
QEMU exit device, so the result
is the process exit code - the same check CI runs on every push. Serial
output is saved to `target/qemu-<arch>-<bin>.log`.

### 5. Run the benchmarks and the seL4 comparison

```sh
cargo xtask bench --arch all
```

Boots the benchmark kernel under deterministic instruction counting and
prints "the three numbers" (grant check, queue round trip, context
switch) as instruction path lengths, plus the RNG path (`rng_*`).
`comparison/` holds the methodology, the script that builds and runs seL4's
own benchmark suite in the same QEMU for a fair baseline, and the measured
results (`comparison/RESULTS.md`).

For the cryptographic RNG there is a separate real-hardware comparison
against Linux's own `getrandom`/`getentropy`/`/dev/urandom`:

```sh
sh comparison/rng/run.sh
```

Same ChaCha20 primitive Linux uses; the win is the per-cell library-call
model (no syscall on the hot path). On the reference host rheo-os is ~4.8x
faster on key/nonce-sized draws and ~1.3x on bulk (`comparison/rng/README.md`).

And the strand (light-thread) model against Linux/Go/Python:

```sh
sh comparison/threads/run.sh
```

Strands spawn+tear down in ~85 ns and switch in ~12 ns - ~1,200-1,600x
faster than OS threads (`std::thread`, Python `threading`), ~150x vs Python
`asyncio`, ~8-17x vs Go goroutines (`comparison/threads/README.md`).

### 6. Run the shell (lsh)

```sh
cargo xtask run --bin lsh --arch riscv64
```

Boots the kernel and drops you at the `lsh>` prompt on the serial console.
lsh is a real user-mode cell talking to the kernel over a PTY; its builtins
query genuine kernel objects. Try `help`, `uptime`, `rand` (bytes from the
cell's own cryptographic ChaCha20 DRBG via `SYS_RANDOM`), `meminfo`,
`caps`, `ps`, `event 8`, `graph 6` (a pipeline is a
dependency graph
submitted to the kernel), `reserve 3 10`, `lease`, and the hardware
builtins `cpuinfo`, `lspci`, `numa` (from the boot discovery pass), then
`exit`. The headless `shell-smoke` test drives the same shell with a
scripted session for CI.

To see the full machine inventory on its own, boot the discovery kernel:

```sh
cargo xtask run --bin hwinfo --arch riscv64
```

### 7. Debug

Add `-s -S` style debugging per `docs/DEVELOPMENT.md` 7: run QEMU with a GDB
stub, attach `gdb-multiarch`, break on `kernel_main`. The same document
covers the QEMU monitor, tracing flags, and turning a panic address back
into a source line.

## Repository layout

```
docs/         the design documents - the spec everything must stay consistent with
kernel/       the no_std kernel library + boot demo
  src/        ISA-independent code: capability core, queue ABI, cells
  src/arch/   per-ISA Rust modules (x86_64, aarch64, riscv64)
  arch/       per-ISA assembly (boot, vectors, context switch)
  link/       linker scripts per ISA
tests/        in-QEMU test kernels (invariants, pipeline, benchmarks)
comparison/   seL4 comparison harness and measured results
xtask/        build/run/test/bench orchestration (cargo xtask ...)
idl/          system IDL + codegen                    (future)
runtime/      strand runtime library                  (future)
services/     system service cells                    (future)
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
