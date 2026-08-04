# rheo-os

Design codename "Lattice OS" (provisional).

A greenfield operating system for modern systems: a `no_std` Rust kernel
with capability-based security, developed emulation-first in QEMU on three
ISAs (x86-64, ARM64, RISC-V 64). It targets today's server hardware and its
capabilities (wide SIMD, IOMMU, measured boot, RDMA, accelerators) and
exploits them when present rather than designing to the weakest denominator.
Backwards compatibility exists only as translation edges (POSIX, the
Kubernetes API). See `docs/ARCHITECTURE.md` 1.4.

## Where it is

`docs/BUILD-ORDER.md` steps 0-5 are complete, plus slices of 6-12. **71 test
kernels boot green on all three ISAs** (213 runs), which is the gate every
claim below rests on.

What runs today:

- **Real user-mode cells** behind hardware address spaces on all three ISAs,
  with isolation enforced by the MMU faulting, over a capability core and a
  queue-pair ABI.
- **Unmodified Linux binaries**, through a kernel-resident personality that
  adds no kernel object: static and dynamic glibc C, unpatched Rust `std`, the
  real upstream uutils/coreutils, and - streamed off a live ext4 disk with
  JIT enabled and under preemption - **Node.js, Bun and the Claude Code
  binary**.
- **Four cores**, with cells placed by claim rather than assignment, work
  stealing, per-core preemption, and one cell running on two cores at once
  (vcores).
- A **native userspace** (`librheo`) with an async strand runtime, typed
  memory grants, a tile framework including FlashAttention 2/3, and a
  from-scratch network stack (`net`).

`docs/ARCHITECTURE-DEBT.md` is the consolidated register of what is *named but
not built*; each row carries its gate. Read it before planning. Claims in
`docs/` are written to be checkable - if something is reasoned rather than
proven, it says so.

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
- **Cross toolchains**, to build the Linux-personality test fixtures. These are
  real glibc binaries compiled from source per ISA, so the boot tests need a
  cross compiler *and its C library*:

  ```sh
  sudo apt install gcc-aarch64-linux-gnu gcc-riscv64-linux-gnu \
                   libc6-dev-arm64-cross libc6-dev-riscv64-cross \
                   g++-aarch64-linux-gnu g++-riscv64-linux-gnu \
                   libstdc++6-arm64-cross libstdc++6-riscv64-cross \
                   libgcc-s1-arm64-cross libgcc-s1-riscv64-cross
  ```

  Name `libc6-dev-*-cross` explicitly. It supplies `crt1.o`, `crti.o`,
  `crtn.o` and `libc.a`, and it is only a *Recommends* of the cross gcc - so a
  minimal install (or `--no-install-recommends`) leaves you with a compiler
  that cannot link, failing as `cannot find crt1.o`. The C++ set is for one
  four-library fixture; without it that phase skips and says so.
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

For kernel work the inner loop is `check`, not `build`:

```sh
cargo xtask check --arch all
```

`build` cross-compiles every userspace program, cell and glibc fixture before
it reaches a line of kernel code - none of which a `kernel/src/` change can
affect. `check` type-checks the kernel package only, twice: **with and without
the `smp` feature**, because that feature is a separate compilation of the same
library. It cannot catch a link error or a missing fixture, so it does not
replace `build`, but it turns a compile error around in seconds.

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

This boots **all 71 test kernels** headless, each with a timeout, and each
reports pass/fail through a QEMU exit device - so the result is the process
exit code, which is the same check CI runs. Serial output is saved to
`target/qemu-<arch>-<bin>.log`.

To iterate, name the ones you care about:

```sh
cargo xtask test --arch riscv64 --bin observe,smp,numa
```

The authoritative list is `TEST_KERNELS` in `xtask/src/main.rs`; the layout
section of `CLAUDE.md` describes what each one proves. Roughly, they cover the
capability invariants and the syscall-surface hardening, hardware discovery and
the device drivers (virtio-blk/net/gpu/rng/input, NVMe, TPM, IOMMU), the POSIX
and filesystem stack, the strand runtime, the `librheo` native userspace and
its tile framework, the network stack, the scheduler and SMP, the observability
plane, and the Linux personality up to Node.js, Bun and Claude Code.

A kernel that cannot run on a given ISA **skips with a printed reason** rather
than failing quietly - a missing QEMU device model, no cross toolchain for that
ISA, no `node` on the host. So a green run does not by itself mean every phase
executed; the log says which were skipped and why.

Six kernels need host binaries that are not in the repository and are not
downloaded for you: `linuxnode`, `linuxbun`, `linuxclaude` and their
secondary-core twins. Without `node`, `bun` and the Claude Code CLI installed,
those six skip and the other 65 still gate.

### 4b. Model-check the state machines on the host

```sh
cargo xtask verify
```

Seconds, no QEMU. Six drivers `#[path]`-include shipped kernel source verbatim
and drive it on the host: the execution-entity table, the per-CPU record rings
and event ring, the dependency graph, heterogeneous-core placement, and the
frame-allocator bitmap search. This is for the cases a boot cannot reach - a
counter wrapping at 2^32, a bitmap boundary, 20,000 random operation sequences
over four CPUs - not a replacement for the boot tests. See `verify/README.md`,
which also records, per driver, which controls were observed to fire and which
could not.

### 5. Run the benchmarks and the seL4 comparison

```sh
cargo xtask bench --arch all
```

Boots the benchmark kernel under deterministic instruction counting and
prints "the three numbers" (grant check, queue round trip, context
switch) as instruction path lengths, plus the RNG, observability, funded-table,
frame-allocator, strand and tile paths. These are **instruction counts, never
wall-clock**: QEMU models no caches and no TLB, so a number here tracks a trend
and nothing else. Two costs it structurally cannot show are named where they
matter - an atomic read-modify-write counts as one instruction, and cache-line
handoffs between cores are invisible.

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

Two more xtask verbs help when the question is "where did it all go":

```sh
cargo xtask sizes --arch x86_64 --bin smp    # largest static allocations, biggest last
cargo xtask trace --arch x86_64 --bin smp    # window a boot's structured trace
```

`trace --ledger` balances the trace per owner, which is how a leaked funded
frame gets attributed to whatever charged it.

## Repository layout

```
docs/         the design documents - the spec everything must stay consistent with
abi/          rheo-abi: the on-wire user/kernel ABI, defined once (no deps, no lang
              items), so divergence is a compile error rather than a wrong number
kernel/       the no_std kernel library + boot demo
  src/        ISA-independent code: capability core, queue ABI, cells, mm,
              scheduler, drivers, the Linux personality, observability
  src/arch/   per-ISA Rust modules (x86_64, aarch64, riscv64)
  arch/       per-ISA assembly (boot, vectors, context switch)
  link/       linker scripts per ISA
tests/        the 71 in-QEMU test kernels, plus the glibc fixtures they run
verify/       host model-checking of the integer-only kernel state machines
comparison/   seL4, tuned-Linux, RNG, thread and tile comparison harnesses
xtask/        build/check/run/test/bench/verify orchestration (cargo xtask ...)

runtime/      the strand runtime: heap, async executor, channels, capability
              rights at the type level
librheo/      the native userspace foundation library (the role a libc plays,
              rebuilt for this kernel) + the cells that prove it
net/          rheo-net: the network stack as portable userspace
posix/        the VFS, POSIX fd surface and std::fs-shaped facade (zero deps)
ext4fs/       the read-only ext4 driver, adapting the ext4plus crate to posix
libc/         rheo-libc: a relibc-style C/POSIX translation layer
userland/     native U-mode programs loaded from an ELF
json/         rheo-json: a dependency-free zero-copy JSON parser
targets/      the rheo-os custom target specs and the std port
tutorials/    long-form written material (not part of the build)
idl/          system IDL + codegen                    (future)
services/     system service cells                    (future)
```

## Documentation map

Read `docs/ARCHITECTURE.md` first. It contains the why, the doctrines, the
kernel object model, and the verification plan. Every other document expands
one subsystem and must stay consistent with the doctrines and the kernel
admission rule defined there. `docs/BUILD-ORDER.md` is the implementation
roadmap; `docs/DEVELOPMENT.md` is the practical how-to.

### Start here

These six carry the most weight for anyone changing code:

| Document | Covers |
|---|---|
| `docs/ARCHITECTURE.md` | Why, doctrines, kernel object model, the admission rule for a new object or verb |
| `docs/ENGINEERING.md` | How a change lands: observe-never-infer, deadlines not iteration counts, rejections as deliverables, saying exactly what is true, and a register of recorded hazards - every rule cited with the defect that forced it |
| `docs/ARCHITECTURE-DEBT.md` | The consolidated "named but not built" register: one row per gap, each with its gate and what blocks it |
| `docs/EXECUTION-MODEL.md` | What a thread, process, task, cell, vcore and CPU each are here, with the invariants - read before touching scheduling or the trap path |
| `docs/GREENFIELD.md` | The design rationale: the lineage taken, ten unlanded research ideas judged one at a time, and a refusals table with reasons |

### By subsystem

| Document | Covers |
|---|---|
| `docs/PROFILES.md` | One core, many targets: per-profile fit map, needs, sequencing (servers, edge, embedded, IoT, remote, desktop) |
| `docs/VALIDATION.md` | Per-profile validation: coverage matrix, workloads, targets and kill thresholds vs best-incumbent baselines (P13-P46) |
| `docs/SIMULATION.md` | Concrete kernel-event simulation of all 12 deployment scenarios (fleet server through remote Africa to desktop) |
| `docs/REFLECTION-NEXUS.md` | Scrutiny of an alternative design; simulated the divergences; three refinements adopted, three rejected with reasons |
| `docs/OPEN-QUESTIONS.md` | Five design refinements resolved: graph granularity, error/cancellation, strand-vcore mapping, typed memory in graphs, distributed debuggability |
| `docs/KERNEL-RUST.md` | Rust implementation guide: kernel targets, capability types, ring buffers, Verus proofs, code size, C#/cross-language interop |
| `docs/SHELL.md` | lsh: Lattice-native shell, latte-term emulator, typed pipelines, graph blocks, TUI, tab completion, scripting |
| `docs/LOGGING.md` | Fast logging: per-strand ring buffers, lazy formatting, zero-cost disabled paths, template interning, console.writeline compat |
| `docs/REALTIME.md` | Real-time and time-certain workloads: sleep precision, jitter sources, PeriodicTask API, hardware timers, SMI/NMI limits |
| `docs/SUBSTRATE.md` | Substrate 2: re-founding the cell substrate - funded metadata, real address spaces, vcores, NUMA-typed memory, the staged migration and its proofs |
| `docs/SMP.md` | Per-CPU state, locking, secondary-core bring-up, and the multi-core scheduling phases with their audits |
| `docs/RESOURCE-GRAPH.md` | One typed graph of CPUs, memory, devices and links with cost vectors, queried rather than hardcoded |
| `docs/CPU-FEATURES.md` | Event delivery, and what to do when the hardware says no |
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
| `docs/JSON.md` | rheo-json: the dependency-free zero-copy parser and its benchmarks |
| `docs/DATA-FORMATS.md` | The three format tiers, Arrow/Parquet/Avro/CBOR, IPC and edge protocols, text formats |
| `docs/CONTAINERS-KUBERNETES.md` | Cells as containers, absorbed Kubernetes, state plane, compat edge |
| `docs/CLUSTER.md` | Running a fleet: node roles, join, orchestration, kubectl support, shared compute, CephFS, NUMA, Raspberry Pi lab |
| `docs/VIRTUALIZATION.md` | Cell vs VM, container/VM hardware acceleration, SR-IOV, confidential compute |
| `docs/IO.md` | Queue ABI, completion contracts, streams, zero copy, DMA graphs |
| `docs/FILESYSTEMS.md` | Three storage tiers, native object store, POSIX view synthesis |
| `docs/NETWORKING.md` | Cell-owned NICs, QUIC/TLS, WASM dataplane, DDoS pipeline |
| `docs/NETSTACK.md` | rheo-net: the greenfield network stack, phases N1-N7 |
| `docs/ACCELERATORS.md` | Engine contract, GPU/NPU/TPU/FPGA/DPU, driver cell containment |
| `docs/GPU-HARDWARE.md` | Real PCIe GPUs: enumeration/BARs, IOMMU containment, VRAM backing, firmware trust, driver cells, tile/inference memory contract |
| `docs/TILES.md` | One tile program, every engine: the unified tile framework (dtypes, contracts, executors, TileSim, battle tier) |
| `docs/AI-ARCHITECTURE.md` | Kernel vs service vs library split, model objects, KV paging, tile IR |
| `docs/GRAPHICS.md` | Vulkan mapping, compositor cells, HID, display scope |
| `docs/DISPLAY.md` | Frame buffers, vsync events, double/triple buffering, VRR, frame pacing, input-to-photon latency |
| `docs/OBSERVABILITY.md` | Flow context, event streams, span trees, WASM probes, OTel export |
| `docs/LINUX-COMPAT.md` | The Linux personality: milestones L0-L8, the syscall honesty table, and the real Node/Bun/Claude Code runs |
| `docs/USERLAND.md` | Building and running native apps: the ELF loader, rheo-libc, the std port, coreutils |
| `docs/LIBRHEO.md` | The native userspace foundation library, phases A-J |
| `docs/DRIVERS.md` | Reusing the Linux driver ecosystem; driver cells |
| `docs/POSIX-PERSONALITY.md` | The translation layer, SSH/bash, Linux binary support, known gaps |
| `docs/EMULATION.md` | QEMU tiers, guest mode, virtual engines, deterministic simulation, cross-ISA |
| `docs/TOOLING.md` | Build, CI, debugging, probes, image pipeline, CLI |
| `docs/DEVELOPMENT.md` | Compile, boot models, QEMU invocations, GDB/monitor debugging, disassembly |

Editing rule: a change that adds a kernel object or verb must pass the
admission rule in `docs/ARCHITECTURE.md` section 6 and be reflected there
first.

## Continuous integration

`.github/workflows/ci.yml` runs on pushes to `main`, on every pull request,
and on demand (`workflow_dispatch`). Two jobs:

- **Format and lint** - rustfmt, clippy on the host helper, `cargo xtask
  verify`, then clippy over the kernel and all test kernels for each of the
  three bare-metal targets **in both feature postures**. Both, because `smp` is
  a separate compilation: five test kernels are gated behind that feature, so a
  single-posture run does not build them and would not lint them.
- **Boot test**, one job per ISA - the full 71-kernel suite, then the icount
  benchmarks, with the serial logs uploaded as an artifact.

QEMU runs gate correctness. Absolute performance numbers only gate on the
hardware lab (from milestone M1, see `docs/TOOLING.md` 4); what CI checks is
instruction path length, which is deterministic under icount.

If you are reproducing CI locally, install the cross toolchains from step 1 -
including `libc6-dev-*-cross`. That package being a *Recommends* is what kept
this pipeline red for several runs.
