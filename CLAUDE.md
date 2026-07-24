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
**cryptographic per-cell entropy** (a ChaCha20 DRBG with fast key erasure,
seeded from the hardware RNG - RDSEED/RDRAND on x86-64, RNDR on ARM64 -
after SP 800-90B health tests, non-blocking, falling back to a documented
floor where no hwrng exists; the design's "library call over the cell's own
DRBG state, not a syscall" path is proven at the primitive level in the host
comparison, while the lsh `rand` builtin currently draws via `SYS_RANDOM` -
linking the full DRBG into a U-mode cell awaits the runtime's `.user` heap +
`mem*` shims, per docs/TIME-IDENTITY.md 4), typed event
streams with flow context, EDF-admitted reservations, leases with fencing
tokens + epoch revocation, and a dependency graph executed on a compute
engine (attest-by-measurement). On
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

The **strand runtime** (`runtime/`, BUILD-ORDER.md step 7,
docs/CONCURRENCY.md) is the userspace library that brings native async and
`alloc` on the OS's own terms - not a POSIX threading port. It has a
free-list **heap allocator** (`GlobalAlloc`, host-fuzzed) so `alloc`
collections (Box/Vec/String/BTreeMap) work in a cell (the kernel itself is
allocation-free); an async **executor** where a *strand* is a `Future` that
"blocks" structurally by parking on a token and is woken by the queue-pair
completion carrying that token in `user_data` - one drained completion ring,
N strands resumed, no blocking syscall; an async **channel** (park on empty,
wake on send); `spawn`/typed `JoinHandle` (structured concurrency),
`yield_now`, and locking adapted to the model - an async **`Mutex`** that
parks a strand on contention (never loses the vcore) and a fair `TicketLock`
for the future multi-vcore case; and **capability rights at the type level**
(KERNEL-RUST.md 2: `Rights<MASK>` + `SubsetOf`, so widening a capability's
rights is a compile error). The `runtime` test kernel exercises all of it -
including strands doing I/O over the real queue-pair ABI - on all three ISAs.
Full Rust (generics, traits, async/await) runs natively; the runtime is
proven kernel-context here, and the same library links into a U-mode cell
(that last integration - a `.user` heap grant + `mem*` shims - is future
work, the shell shows the U-mode constraints).

**Strands are validated as light threads** (comparison/threads/): the exact
executor, measured on the host against the named runtimes, spawns+tears down
a task in ~85 ns and switches in ~12 ns - roughly **1,200-1,600x faster than
OS threads** (Rust `std::thread`, Python `threading`), ~150x faster than
Python `asyncio`, and ~8-17x faster than Go goroutines (strands are
stackless, so there is no per-task stack to allocate). In-QEMU icount path
lengths (`bench` `p4_*`): ~450 instructions to spawn+tear down, ~150 to
switch, consistent across ISAs. (.NET 10 is not installed here, so it is left
unmeasured with an architectural note rather than a fabricated number.)

A **POSIX + filesystem stack** (`posix/`, docs/FILESYSTEMS.md,
POSIX-PERSONALITY.md) sits on a **VFS** translation layer (a `FileSystem`
trait): a read-write **ramfs** (the working store), a read-only **ext4**
driver that parses a real ext4 image (superblock, block-group descriptors,
inodes, the extent tree, linear dirs - host-validated against a `mkfs.ext4`
image), a mount table + path resolution (the per-session `/`), the **POSIX
fd surface** (`open/read/write/close/lseek/stat/getdents/mkdir/unlink` with
errno), and a **`std::fs`-shaped facade** (`File`, `OpenOptions`,
`read`/`write`/`read_to_string`, `read_dir`, `metadata`) so standard-library
file code runs natively. The `posix` test kernel exercises ramfs rw, ext4 ro
(incl. a multi-block file), the errno surface, and the std facade on all
three ISAs.

A **live-disk block stack** closes the loop from storage transport to
filesystem: a **`BlockDevice` trait** (`kernel/src/hw/block.rs`, 512-byte
sectors, transport-agnostic) and a **virtio-blk driver**
(`kernel/src/hw/virtio_blk.rs`) - reset/feature negotiation, a split
virtqueue, and the block request protocol - over **two transports**:
virtio-mmio on arm/riscv `virt`, and **virtio-pci on x86-64 q35**. The x86
path drives the device *entirely through PCI configuration space* using the
`VIRTIO_PCI_CAP_PCI_CFG` capability (virtio spec 4.1.4.8), so no BAR needs
to be assigned or mapped - which matters because PVH boot has no firmware to
program BARs and the kernel only identity-maps the low 1 GiB (the q35 PCI
window sits above it); DMA still reaches the identity-mapped virtqueue since
PA=VA. The `blockfs` test kernel discovers the device, reads a real ext4
image off the *live disk* (attached by QEMU with `-drive`), mounts it, and
reads files through `std::fs` - on **all three ISAs**. At the `BlockDevice`
seam existing Rust FS drivers (redoxfs, fatfs, a read/write ext4 crate) can
be dropped in rather than hand-written - gated by the no-deps rule (a doc
must name any crate).

A **native-userland** path is being built out so real Rust/C/C++ apps
(eventually the uutils/coreutils) run as cells, recompiled for a rheo-os
target over a relibc-style Rust libc (docs/USERLAND.md, milestones M1-M5).
Done so far: an **ELF loader** (`kernel/src/elf.rs` + `load.rs`) with a
general per-cell address space (`arch::paging_map_frame` maps an arbitrary
user VA to an allocated frame, creating tables on demand) - a separately-
compiled program (the `userland` crate) is loaded into a cell and run, no
longer baked into the kernel image (the `elfrun` test); and a
**multi-argument POSIX syscall surface** - the ABI is now
`decode_syscall -> (nr, [u64;6])`, with kernel-native `mmap`-anon +
`exit_group`, and fd-based `open/close/read/write/lseek` forwarded to a
**personality handler** (`svc::FileOps`, function pointers a service
registers) backed by the `posix/` VFS, keeping the kernel filesystem-free
(the `posixrun` test runs a native program that reads a file over the VFS);
and a **Rust libc** (`libc/`, package `rheo-libc`) - a relibc-style
translation layer with `crt0` (`_start`), a heap over `SYS_MMAP` wired as the
global allocator (so Rust `alloc` works) plus C `malloc`/`free`, fd-based I/O
wrappers, and `println!` (the `libcrun` test runs `libcdemo`, which builds a
`Vec`, round-trips C `malloc`, formats output, and reads a file via the VFS).
A **custom Rust target + std port** is in progress (M4): real `std` compiles
and links for a `rheo-os` target (`targets/rheo_os-*.json`, `os = "rheo"`) via
a repo-held, idempotent rust-src patch (`cargo xtask std-patch`,
`targets/patch-std.py` + `targets/std-rheo/`) - std routes rheo to the
single-threaded portable fallbacks (SMP is deferred) with a real hole-list
allocator over `SYS_MMAP`; a *running* std program still needs a crt0 `_start`
and the rheo `stdio`/`process`/`fs` sys arms. Then coreutils (M5). Also built
alongside as an M4-prep workload: **rheo-json** (`json/`), a dependency-free
zero-copy JSON parser that runs on the OS and is benchmarked against simdjson
(docs/JSON.md).

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
              (frames + grants), time (clock), rng (ChaCha20 DRBG +
              hwrng seeding), event streams,
              sched (reservations), lease, engine, graph, pty, svc
              (shell/resource/POSIX-file syscalls), hw (ACPI/FDT/PCIe
              discovery + the machine Inventory; block BlockDevice trait +
              virtio_blk driver), elf + load (ELF loader for native
              programs), user run loop, U-mode programs
              (user_progs.rs incl. the lsh shell), abi
  src/arch/   per-ISA Rust modules incl. paging.rs (one dir per ISA)
  arch/       per-ISA assembly (boot, vectors/traps, context switch, user)
  link/       linker scripts per ISA (incl. the .user text/rodata/data window)
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline,
              isolation-hw, resources, shell-smoke, hwinfo, rng, runtime,
              posix, blockfs (live virtio-blk disk), elfrun (load a native
              ELF), posixrun (native program over the POSIX syscalls),
              libcrun (a program linked against rheo-libc), jsonrun (a
              program parsing JSON with rheo-json on-OS), bench-core, and
              the interactive lsh bin (+ harness.rs); fixtures/ holds the
              ext4 test image (+ gen-ext4.sh)
comparison/   seL4 comparison: methodology, sel4bench script, RESULTS.md
xtask/        build/run/test/bench orchestration (cargo xtask ...)
idl/          system IDL + codegen        (future, step 6)
runtime/      strand runtime: heap (alloc), async executor + channel,
              type-level capability rights (BUILD-ORDER step 7)
userland/     native U-mode programs built for a bare target and loaded
              from an ELF (docs/USERLAND.md): hello, iodemo
libc/         rheo-libc: the Rust libc translation layer (crt0, heap +
              allocator, malloc, fd I/O, println) + the libcdemo/jsondemo
              programs
json/         rheo-json: a dependency-free, zero-copy JSON parser (scalar +
              SSE2 string-scan), no_std, host-tested + benchmarked
              (docs/JSON.md, comparison/json/)
services/     system service cells        (future, phase 5)
targets/      rheo-os custom target specs + the std port: rheo_os-*.json,
              patch-std.py (rust-src std patch), std-rheo/ (rheo sys sources
              + a std proof program). docs/USERLAND.md M4
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
