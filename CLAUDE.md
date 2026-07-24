# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

rheo-os (design codename "Lattice OS") is a greenfield operating system for
modern server hardware: a `no_std` Rust kernel, capability-based security,
built emulation-first in QEMU on three ISAs (x86-64, ARM64, RISC-V 64).

The design lives in `docs/` and is the source of truth. Read
`docs/ARCHITECTURE.md` first; `docs/BUILD-ORDER.md` says what gets built in
what order; `docs/DEVELOPMENT.md` covers the day-to-day mechanics.

## Current state

BUILD-ORDER.md steps 0-5 are done, plus slices of 6-10, and a native
shell: exception vectors + cycle counters + context switch per ISA; a
bitmap frame allocator and per-ISA paging (Sv39 / AArch64 4 KiB granule /
x86-64 4-level) with the MMU on; the capability core (runtime-tested for
the four ARCHITECTURE.md 8.2 proof properties); the queue-pair ABI with
per-entry grant checks and flow-context propagation; **cells in real user
mode behind hardware address spaces** (RISC-V U-mode, ARM64 EL0, x86-64
ring 3), isolation MMU-enforced, with a cross-cell directed switch.

The full single-host **kernel object model** is implemented: memory grants
(typed, commit/decommit/seal), a monotonic clock + interval wall clock +
per-cell DRBG entropy, typed event streams with flow context, EDF-admitted
reservations, leases with fencing tokens + epoch revocation, and a
dependency graph executed on a compute engine (attest-by-measurement). On
top of these, **lsh** runs as a U-mode shell cell over a PTY (serial line
discipline bridged by the kernel), with builtins that query the real
objects (`uptime`, `rand`, `meminfo`, `caps`, `ps`, `event`, `graph`,
`reserve`, `lease`) and the machine inventory (`cpuinfo`, `lspci`, `numa`);
a pipeline is a dependency graph submitted to the kernel (docs/SHELL.md).
Run it: `cargo xtask run --bin lsh --arch <isa>`.

A **hardware-discovery** layer (`kernel/src/hw/`) builds one portable
machine `Inventory` at boot: firmware source (ACPI on x86-64 via the PVH
RSDP, a flattened device tree on RISC-V, a fixed QEMU-virt profile on
ARM64), CPU count and instruction-set features (CPUID / `ID_AA64*` / the
device-tree ISA string), the typed physical memory map (DDR / reserved /
ACPI / pmem), NUMA topology (SRAT memory + CPU affinities, memory regions
split at node boundaries), and PCIe enumeration through the ECAM/config
space, classifying each function into an engine kind - GPU, NIC, NVMe, or a
processing accelerator (NPU/TPU, PCI base class 0x12). The `hwinfo` test
kernel asserts the basics on all three ISAs; `cargo xtask run --bin hwinfo`
prints the full inventory.

Deferred (documented): cross-host/cluster, PTP/NTS time sync, attested
firmware + real GPU/NPU engines, elastic-grant pressure events, the Verus
proofs, and the hardware-lab performance numbers. **SMP secondary-core
bring-up** is scoped separately: CPU *detection* and topology are done (4
cores on x86-64/RISC-V, per-node affinity), but starting the other cores
running kernel code needs per-CPU state + locking (the kernel is currently
single-CPU `static mut`) and is blocked portably here - ARM64 PSCI CPU_ON
traps from EL1 (no EL3/EL2 in this QEMU config) and x86 APs need a 16-bit
real-mode trampoline.

The `.user` linker window holds U-mode code (`.user.text`), shared
read-only constants (`.user.rodata`), and per-cell data (`.user.bss`) in
one 2 MiB span; per-cell page tables differ only in the per-cell data
mappings, which is what makes cross-cell isolation MMU-enforced. U-mode
code (the programs in `kernel/src/user_progs.rs`, including the shell) must
be free of out-of-line calls into kernel `.text` (no panics, no bounds
checks, no `core::fmt`, no autovectorized constant pools) since kernel
`.text`/`.rodata` are not mapped in a cell - which is why the test kernels
build `--release` and aarch64 uses the soft-float target.

## Commands

Everything routes through the xtask runner (`xtask/src/main.rs`):

```
cargo xtask build --arch <x86_64|aarch64|riscv64|all>   # cross-compile all kernels
cargo xtask run   --arch riscv64 [--bin lsh]            # boot in QEMU, serial on terminal
cargo xtask test  --arch all                            # boot every test kernel, pass/fail
cargo xtask bench --arch all                            # icount path lengths (always release)
cargo fmt --all                                         # format (CI-gated)
cargo clippy -p xtask -- -D warnings                    # lint host code (CI-gated)
```

Kernel clippy needs the build-std flags:

```
cargo clippy -p kernel --target x86_64-unknown-none \
  -Zbuild-std=core,alloc,compiler_builtins \
  -Zbuild-std-features=compiler-builtins-mem -- -D warnings
```

Plain `cargo build` builds only xtask (the kernel needs a bare-metal
`--target`; `default-members` excludes it on purpose). The toolchain is a
pinned nightly from `rust-toolchain.toml` - rustup installs it automatically.
QEMU 8.x system emulators must be installed to run or test.

## Layout

```
docs/         the design documents (the spec - keep code consistent with it)
kernel/       the no_std kernel library + boot demo bin
  src/        ISA-independent: capability core, queue ABI, cells, mm
              (frames + grants), time (clock/entropy), event streams,
              sched (reservations), lease, engine, graph, pty, svc
              (shell/resource syscalls), hw (ACPI/FDT/PCIe discovery +
              the machine Inventory), user run loop, U-mode programs
              (user_progs.rs incl. the lsh shell), abi
  src/arch/   per-ISA Rust modules incl. paging.rs (one dir per ISA)
  arch/       per-ISA assembly (boot, vectors/traps, context switch, user)
  link/       linker scripts per ISA (incl. the .user text/rodata/data window)
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline,
              isolation-hw, resources, shell-smoke, hwinfo, bench-core, and
              the interactive lsh bin (+ harness.rs for user-mode cells)
comparison/   seL4 comparison: methodology, sel4bench script, RESULTS.md
xtask/        build/run/test/bench orchestration (cargo xtask ...)
idl/          system IDL + codegen        (future, step 6)
runtime/      strand runtime library      (future, step 7)
services/     system service cells        (future, phase 5)
targets/      custom target JSON          (only if built-in targets fail)
```

## Rules

- **Docs first.** A change that adds a kernel object or verb must pass the
  admission rule in `docs/ARCHITECTURE.md` section 6 and be reflected there
  before it lands in code.
- **Portability.** Only `kernel/src/arch/` and `kernel/arch/` may differ per
  ISA. A change that needs per-ISA code anywhere else is an architecture bug
  (docs/TARGET-ARCHITECTURES.md 4). Every change must build and boot on all
  three ISAs - run `cargo xtask test --arch all` before pushing.
- **Assembly is exceptional.** Only boot, exception vectors, and the
  context-switch inner loop may be assembly (docs/TOOLING.md 1).
- **Unsafe is concentrated.** Keep `unsafe` in small, audited, documented
  blocks behind safe wrappers; no `unsafe` outside arch/MMIO/DMA code
  without a written justification.
- **No new dependencies casually.** xtask has zero dependencies and the
  kernel has none; keep it that way unless a doc names the crate.
- **CI must stay green** on all three ISAs (.github/workflows/ci.yml). A
  panic in the kernel exits QEMU with a failure code, so CI catches it.

## How benchmarks stay honest

`cargo xtask bench` boots tests/src/bench_core.rs under QEMU
`-icount shift=0` - results are deterministic instruction path lengths,
never wall-clock claims (QEMU has no caches/TLB; hardware gates P1-P12 at
the lab, docs/TOOLING.md 4). The kernel self-calibrates the
tick:instruction ratio each run. Comparisons against seL4 must use the
same QEMU + icount setup on both sides: comparison/README.md.

## How the boot test works

`cargo xtask test` boots every test kernel headless in QEMU with a 120s
timeout and reads the QEMU process exit code (docs/DEVELOPMENT.md 6):
x86-64 uses isa-debug-exit (success exits 33), ARM64 uses semihosting
SYS_EXIT (exits 0), RISC-V uses the sifive_test device (exits 0). Serial
output lands in `target/qemu-<arch>-<bin>.log`.
