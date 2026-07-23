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

Steps 0-1 of BUILD-ORDER.md are done, plus early slices of steps 2, 4, 5
and 6 built for concept validation: exception vectors + cycle counters +
context switch per ISA; the capability core (mint / derive-subset /
delegate / revoke-by-epoch / grant-check, runtime-tested for the four
ARCHITECTURE.md 8.2 proof properties); the queue-pair ABI (SqEntry/CqEntry
rings + doorbell) with per-entry grant checks and flow-context
propagation; cells as capability-table protection contexts (no hardware
address spaces yet - user-mode cells need steps 3/5); and a benchmark +
seL4 comparison harness (comparison/RESULTS.md). Timers, memory
management, and the Verus proofs are still open.

## Commands

Everything routes through the xtask runner (`xtask/src/main.rs`):

```
cargo xtask build --arch <x86_64|aarch64|riscv64|all>   # cross-compile all kernels
cargo xtask run   --arch riscv64 [--bin bench-core]     # boot in QEMU, serial on terminal
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
  src/        ISA-independent: capability core, queue ABI, cells, console
  src/arch/   per-ISA Rust modules (one dir per ISA)
  arch/       per-ISA assembly (boot.S, vectors.S/trap.S, context_switch.S)
  link/       linker scripts per ISA
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline, bench-core
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
