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
x86-64 4-level) with the MMU on, the kernel run in the **higher half** on all
three ISAs (docs/MEMORY.md) so the whole low half is free for user programs -
a stock Linux `ET_EXEC` (0x400000) loads unmodified; the capability core
(runtime-tested for
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
program BARs. Since the kernel moved to the high half (docs/MEMORY.md) PA no
longer equals VA, so the driver hands the device **physical** addresses for
the virtqueue via `virt_to_phys` (the queue lives in the kernel's own RAM,
reached through its linear map). The `blockfs` test kernel discovers the device, reads a real ext4
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
A **custom Rust target + std port** works (M4): real `std` compiles, links,
and **runs on the OS on all three ISAs** - a std program using
`String`/`Vec`/`format!`/`println!` returning an `ExitCode` (the `stdrun`
test). The `rheo-os` targets (`targets/rheo_os-*.json`, `os = "rheo"`,
soft-float) build std via a repo-held, idempotent rust-src patch
(`cargo xtask std-patch`, `targets/patch-std.py` + `targets/std-rheo/`): std
routes rheo to the single-threaded portable fallbacks (SMP deferred) with real
rheo arms for the heap (a hole-list allocator over `SYS_MMAP`), non-blocking
`stdio` (fds over the M2 syscalls), and `process::exit` (`SYS_EXIT_GROUP`); a
crt0 (`rheo-rt`) provides `_start`. Float-heavy programs await U-mode FP/SIMD
enablement. Also built
alongside as an M4-prep workload: **rheo-json** (`json/`), a dependency-free
zero-copy JSON parser that runs on the OS and is benchmarked against simdjson
(docs/JSON.md).

**Coreutils run on the OS** (M5, docs/USERLAND.md): standard command-line
tools as a U-mode cell. Three OS capabilities land first - **argv/env** (the
kernel builds the System V initial process stack in `setup_stack`; the crt0
reads `argc`/`argv` so `std::env::args` works; env is an in-process table), and
a **`std::fs` read/write path over the VFS** (a rheo `std` `fs` arm translating
`File`/`metadata`/`read_dir` onto the file syscalls, now including `stat`/
`fstat`/`getdents`, forwarded to the POSIX personality). On top of them
**rheo-coreutils** (`targets/std-rheo/coreutils/`) is a busybox-style multicall
program built against real `std` - `true`/`false`/`echo`/`cat`/`wc`/`head`/
`ls`/`seq`/`basename`/`dirname`/`nl`/`pwd`/`env`, dispatched by `argv`. The
`coreutils` test loads it with a real `argv`, runs one utility per invocation
over a ramfs, and asserts each utility's exit code and exact stdout on all
three ISAs. These are faithful from-scratch ports, not the upstream uutils
crate (whose clap/uucore tree needs `std` surface rheo lacks - `IsTerminal`,
locale, terminal width - so it is deferred; docs/USERLAND.md M5).

A **Linux personality** is complete (docs/LINUX-COMPAT.md, milestones
L0-L7 all done) so *unmodified* Linux binaries - unpatched Rust std
(`*-unknown-linux-gnu`), glibc-linked C (static and dynamic), stock tools - run
as cells. It is
kernel-resident like `svc.rs` (a documented bridge to a future personality
cell) and adds no kernel object: PIDs/fds/signals are per-cell synthesized
state. L0 is done: a per-cell `Personality { Native, Linux }` tag branches
dispatch *before* the syscall number is decoded (the ABIs collide),
per-ISA Linux syscall-number tables live in `arch::linux_abi` (x86-64
legacy table; asm-generic table shared by arm64/riscv64), unknown numbers
log `linux: ENOSYS nr=<n>` and return -ENOSYS. L1 added the ELF auxv,
ET_DYN loading, the U-mode thread pointer (fs_base / tpidr_el0 / tp), and
U-mode FP/SIMD enablement. **L2 is done**: the core syscall set (read/write/
readv/writev, openat/close/lseek, fstat/newfstatat, getdents64, brk,
anonymous mmap/munmap/mprotect, poll/ppoll, uname, clock_gettime, getrandom,
prlimit64, ioctl(TIOCGWINSZ), dup/fcntl, and the identity/recorded/ENOSYS
calls - honesty table in docs/LINUX-COMPAT.md), a per-cell fd table
(`kernel/src/linux/fd.rs`: console, VFS files, /dev/{null,zero,urandom}),
real memory over the cell's own address space (`AddressSpace::unmap`/
`protect` + per-ISA `paging_unmap_frame`/`paging_protect` + `frames::free`;
`kernel/src/linux/mem.rs`), and per-ISA `ticks_to_ns`. The `linuxrun` test
now also runs, on **all three ISAs** with exact stdout + exit asserted, two
**unpatched static-glibc** binaries built from source: a Rust `std` hello and
a C hello, each loaded at glibc's **stock ET_EXEC base, no relink**
(docs/LINUX-COMPAT.md L2). glibc (not musl) is the supported libc. **L3 is
done**: the **unmodified upstream uutils/coreutils** multicall binary (crates.io
`coreutils` 0.0.29, pinned, static-glibc, built from source by xtask - no binary
in git) runs as a `Personality::Linux` cell on **all three ISAs**; the
`linuxtools` test asserts exact stdout + exit for eleven utilities (true, false,
echo, cat, seq, head, wc, basename, dirname, ls, pwd). L3 added per-cell cwd
(getcwd/chdir), statx, mremap, a single-process pipe2 (splice stays ENOSYS -
uu_cat falls back), a per-fd getdents64 cursor, `/proc/self/auxv`, and the
`openat` dirfd-as-`int` fix. **L4 is done**: **threads as multi-context cells**
(the CONCURRENCY.md vcore model made real) - a Linux cell holds up to 8
execution contexts (a `TrapFrame` + FP save area each), scheduled
**cooperatively, round-robin, at syscall boundaries** on the single CPU
(`kernel/src/linux/thread.rs`), sharing one address space and kernel stack with
eager FP/SIMD save-restore and per-context TLS. Added `clone` (pthread flag set,
arch-specific arg order via `CLONE_BACKWARDS`), `futex` (WAIT/WAKE + _BITSET),
per-context `gettid`, thread `exit` vs `exit_group`, CHILD_CLEARTID clear+wake,
`set_tid_address`, real `sched_yield`, and `sched_getaffinity`/`prctl`(name);
memory gained demand-commit (PROT_NONE `mmap` reserves without frames,
`mprotect` commits) so glibc's per-thread 64 MiB arenas don't exhaust the pool.
PIDs/TIDs/futex waiter lists stay per-cell synthesized state (no kernel object).
An **unpatched multi-threaded Rust `std`** binary (4 `std::thread`s + `mpsc` +
`Mutex` + `Arc<AtomicUsize>` + join) runs on **all three ISAs** with exact
stdout/exit asserted (the `linuxthreads` test), and `sort` (rayon-threaded) is
re-enabled in `linuxtools`. Cooperative-only (a spinning thread starves siblings
until timer preemption, task #27) and futex wake is FIFO (priority inheritance is
a documented TODO) - both disclosed in docs/LINUX-COMPAT.md + CONCURRENCY.md.
**L5 is done**: **synthesized POSIX signals, no kernel object** - dispositions a
per-cell table, masks/pending/altstack per-context (`kernel/src/linux/signal.rs`).
Delivery is a **saved-`TrapFrame` rewrite** in trap context: a Linux `rt_sigframe`
(siginfo + ucontext with the interrupted GPRs/PC/SP + saved mask) is built on the
user stack, the frame's PC set to the handler and its return to the restorer -
glibc's `SA_RESTORER` on x86-64, an injected 2-instruction `rt_sigreturn`
trampoline page (`arch::SIGTRAMP_VA`) on ARM64/RISC-V; `rt_sigreturn` restores.
Real `rt_sigaction`/`rt_sigprocmask`/`sigaltstack`/`rt_sigreturn` (the L2 stubs)
plus `kill`/`tgkill`/`tkill`/`rt_sigqueueinfo` (self-targeting). **Synchronous
faults become signals**: `on_user_trap` maps the per-ISA fault cause (vector /
ESR EC / scause -> `arch::FaultCause`) to SIGSEGV/SIGBUS/SIGILL/SIGFPE and, for a
Linux cell with an installed unblocked handler, delivers by frame rewrite; else
terminates 128+signo. A **native** cell fault stays terminal (`Outcome::Faulted`)
- delivery is behind the Linux branch only; the x86-64 ring-3 fault path
(`vectors.S`) now captures the full `TrapFrame` and resumes via `sysret`
(ARM64/RISC-V already carried the frame). The `linuxsig` test asserts, on **all
three ISAs**, three static-glibc C fixtures: `sig_raise` (async
`raise(SIGUSR1)`->handler->resume, exit 0), `sig_segv` (null write ->
SIGSEGV handler `_exit(0)`, not a killed cell), `sig_dfl` (`raise(SIGABRT)`, no
handler -> terminate 134). Scope: self / fault delivery (a signal to a non-running
sibling context is recorded pending, not force-delivered); FP state is not
saved across a handler - both documented.
**L6 is done**: **processes - fork / execve / wait4 / cross-cell pipes** (no new
kernel object; all per-cell synthesized state, `kernel/src/linux/proc.rs` +
`pipe.rs`). `fork` is clone-cell-within-capability-bundle: a new `user` cell in
the parent's bundle with the parent's committed pages **eager-copied** (COW
deferred) via a per-ISA page-table user-leaf walk (`arch::paging_for_each_user_leaf`
behind `AddressSpace::fork_from`/`free_user_frames`), the `LinuxState`/fds/cwd/
signal dispositions deep-copied, child pid synthesized. `execve` **streams** a new
ELF from the VFS into a fresh address space (`load::exec_elf_from_vfs`: only the
header + phdrs are buffered; segments read page-by-page into destination frames,
the kernel holds no whole-image buffer). `wait4` blocks the parent cooperatively
until a child exits, reaping WIFEXITED/WIFSIGNALED status and freeing the child.
`pipe2` is a **global cross-cell** ring buffer (`kernel/src/linux/pipe.rs`) whose
two ends live in different cells after fork; reads/writes block cooperatively with
cross-cell wake, EOF on all-writers-closed, SIGPIPE on all-readers-closed. The run
loop is **generalized**: a cell that blocks or exits hands the CPU to the next
runnable cell via the same address-space switch the native `SYS_SWITCH` uses -
the native path is byte-for-byte unchanged, `crate::linux::proc` drives the
cross-cell switch behind the `Personality::Linux` branch. `MAX_CELLS` 8 -> 16. The
`linuxproc` test proves it on **all three ISAs**: (A) a direct static-glibc
multi-process fixture (`procdemo`: pipe2+fork+dup2+execve of `/bin/cecho` from the
VFS+wait4, exact transcript+exit), and (B) the **P11 gate** - `rsh`, a minimal
from-scratch static-glibc shell (dash cross-build was out of budget), forks+execs
the unpatched upstream uutils/coreutils multicall over pipelines and `&&`/`||`,
passing a 12-command coreutils suite **12/12 = 100%** (gate >= 80%; POSIX-
PERSONALITY.md 5).
**L7 is done** (the final milestone): **dynamic linking - an *unmodified,
dynamically-linked* glibc binary runs**. `load::load_elf_linux` parses `PT_INTERP`
and, when present, loads BOTH the main program (ET_DYN, 4 GiB bias) and the ELF
interpreter `ld-linux-*.so` (ET_DYN, 64 GiB bias `LINUX_INTERP_BASE`, streamed
from the VFS), starting execution in ld.so with the auxv carrying `AT_BASE` =
interp bias, `AT_PHDR`/`AT_ENTRY` = the main program's. The kernel does **no
relocation processing** - ld.so self-relocates then maps + relocates the program
and `libc.so.6` (initial-exec TLS + IRELATIVE/ifunc relocs run to completion on
all three ISAs). `mmap` gained **file-backed MAP_PRIVATE + MAP_FIXED**
(`kernel/src/linux/mem.rs`): ld.so reserves a library's span then MAP_FIXED-
overlays each segment (text/data from the file, an anonymous **zeroed** overlay
for the bss - the anon path frees+replaces the reservation's file frames, or
libc's stdio/malloc bss locks keep file garbage and self-deadlock), plus
`pread64` and x86-64 legacy `access`. The real per-ISA glibc (`ld-linux` +
`libc.so.6`) is **copied from the cross toolchain at build time** into a
gitignored dir (never committed) and seeded into a ramfs `/lib`; a missing
runtime lib makes that ISA skip-with-reason (static coverage stays). The
`linuxdyn` test runs a stock **dynamically-linked glibc C hello** (gcc default
PIE, unmodified) on **all three ISAs**, exact stdout + exit asserted. This closes
"unmodified Linux binaries run" for the common dynamic case; the whole
**L0-L7 Linux personality is complete** - unpatched static and dynamic glibc C,
unpatched Rust `std`, and the real upstream uutils/coreutils all run as cells,
kernel-resident like `svc.rs` and adding no kernel object (`execve` of a *dynamic*
binary and a dynamic Rust/uutils-0.9.x fixture are the documented next steps).

**librheo** (`librheo/`, docs/LIBRHEO.md) is the greenfield **native userspace
foundation library** - the role a libc plays, rebuilt for this kernel:
async-first, capability-native, built ON `runtime/` (not a POSIX threading
port, and distinct from `libc/`'s C/POSIX compat role). **Phase A** ships the
spine: a `no_std`+`alloc` crate (a loaded ELF cell for the bare targets) with
`mem` (a grow-on-demand global allocator over `runtime::Heap` + `SYS_MMAP`,
which added `Heap::add_region`), `rng` (a ChaCha20 fast-key-erasure DRBG as a
**library call**, seeded once over `SYS_RANDOM` - realizing TIME-IDENTITY.md 4
in a cell), `cap` (capability-typed handles over `runtime::rights` + a
startup `CapSet`), and `rt` (the strand executor + a **userland reactor**:
submit -> `SYS_DOORBELL` -> drain CQ -> `complete(user_data)`, so a strand
parked on a token wakes on its completion). To make a loaded cell actually own
a queue pair, the **queue ABI was redesigned to a stable on-wire layout**
(`kernel/src/queue/mod.rs`): a `repr(C)` `QueueHeader` (version, depth, the four
ring indices at fixed offsets, sq/cq offsets) followed by the SQ/CQ arrays, with
`QueuePair` now an overlay over a single region (`init`/`attach`) rather than
head/tail atomics inside the Rust struct - unified, so bench-core/queue-pipeline/
runtime/isolation-hw stay green. The loader gained `map_queue` (maps the ring at
**16 GiB** = `USER_QUEUE_VA` into a loaded cell, mints a `QueuePair` cap, records
`(qp_va, cap_id)`), `submit` carries args + `reap` returns the full `CqEntry` +
`SYS_DOORBELL` returns the processed count, and **`SYS_QUEUE_INFO` (31)** reports
`(qp_va, cap_id)` to the cell. The `librhearun` test loads `librheo-demo` into a
cell with a real mapped queue pair and asserts it exits `0x42` - reached only if
heap+rng+cap and an **async queue round-trip** (8 strands each `submit_and_await`
an `OP_ECHO` and verify the echo) all pass, on **all three ISAs**.
**Phase B** makes librheo the **terabytes / analytical-DB / warehouse**
substrate: **typed memory grants exposed to a cell** (`SYS_GRANT`/`COMMIT`/
`DECOMMIT`/`SEAL`/`MMAP_FILE`/`MUNMAP` - object 5 as mechanism: reserve typed
address space + mint a MemoryGrant cap, demand-commit frames without a fault
handler, seal immutable, mmap a VFS file, and a real unmap that fixes the anon-
`SYS_MMAP` frame leak; only DDR is real, HBM/CXL/PMEM emulated-as-DDR, device-BAR
refused, NUMA single-node - all honest/documented, per-cell grant tables as fixed
statics, every commit/decommit/seal grant-checked), and **real async I/O opcodes
over the queue** (`OP_OPEN`/`READ`/`WRITE`/`CLOSE`/`FSTAT` bridged to
`kernel_process` via `svc::FileOps`, completing through the CQ with the strand
token; per-opcode rights replacing hardcoded-WRITE; distinct CapError statuses;
IO.md contract flags with the inline-vs-by-reference threshold - small writes
inline, larger I/O by-reference and **zero-copy** straight into the cell's mapped
pages). librheo gained `mem` (`Grant`/`Arena`/`Mapping` + NUMA hint), `io` (async
`File`/`read_at`/`write_at`/batched/`Contract`/`Stream`, zero-copy `read_into` a
grant), and a thin `store` (`Dataset`). The `librheodata` test is the **mini-
DuckDB proof**: a columnar dataset (65536 rows x 2 u32 cols, generated by xtask,
never committed) on a **live virtio-blk disk** is served to a librheo cell, which
mmaps it zero-copy, fans a `SUM`/`COUNT`/`MAX`-under-predicate columnar scan
across 8 strands, and asserts the exact aggregate (`SUM=1073741824`), on **all
three ISAs**.

**Phase C** makes librheo the **parallel / accelerated compute + QoS** substrate:
**userspace-built dependency graphs submitted to the compute engine** (a new
`OP_GRAPH_SUBMIT` queue opcode - a cell writes an `abi::GraphNode` list into its
own buffer, the kernel validates the edges topologically, runs it on the CPU
engine, and writes per-node results back, completing through the CQ with the
strand token - objects 4/6 as mechanism), **engine introspection**
(`SYS_ENGINE_INFO` surfaces the throughput measured at attach - attest-by-
measurement), and **admission-checked reservations** (`SYS_RESERVE_ADMIT`/`QUERY`/
`RELEASE` - the per-cell EDF schedulability math in `sched.rs` admits CPU
budget/period/deadline + an advisory memory floor, mints a Reservation capability,
and refuses an over-committed set cleanly; object 7). librheo gained `compute`
(`map_reduce`/`parallel_for`/`scan` strand workers, `Engine::info`, and a
`GraphBuilder` that builds + `submit().await`s a graph) and `sched` (`Reservation`
RAII handle with typed rejections, plus a `lattice-rt`-shaped `Priority`/
`PeriodicTask`/`TimingReport`). The `librheocompute` test proves it on **all three
ISAs**: a parallel `map_reduce` aggregation (`SUM=4194304`), a submitted graph
(`(6+1)*6=42`), a reservation admitted (committed ppm > 0) then two cleanly
rejected (bad-params + over-commit), and the engine's measured throughput
reported. Honest: the admission **math** is real and enforced at admit, but
runtime enforcement is SMP/preemption-gated (task #27) - the runtime is
single-CPU cooperative, so parallel strands interleave and a reservation is an
admitted, not-yet-scheduled guarantee; the CPU engine is the only real engine
(GPU/NPU accelerators ride the same graph/engine API as attested-firmware future
work); a free-form graph's buffer-reduce node is the documented next step.

**Phase D** brings up the **kernel's first hardware interrupt** and the terminal.
A native cell blocking on console input (`SYS_WAIT_INPUT`) parks, and the kernel
idles until a byte arrives instead of spinning - the OS's **first block-and-wake**.
Bytes land in a portable kernel-side **RX ring** (`kernel/src/input.rs`); the
reactor gains a console-read slot (`rt::read_console`) that parks a strand and, when
no queue completion is ready, blocks in `SYS_WAIT_INPUT`. The interrupt path is
boot-critical and opt-in (`arch::enable_uart_rx_irq`, called only by the Phase D
test, so the other 26 kernels are untouched): **on RISC-V it is interrupt-driven** -
the 16550 UART's IRQ (source 10) is routed through the **AIA** (S-mode APLIC in
MSI mode -> S-mode IMSIC via the `siselect`/`sireg`/`stopei` CSRs -> `sip.SEIP`),
and the kernel takes the S external interrupt to drain the UART, halting at `wfi`
(cells run with `sstatus.SIE` clear; SIE is set only to service a pending SEI after
`wfi` woke on it) - a genuine 0%-CPU park. **x86-64 and ARM64 poll** (their
IOAPIC/LAPIC and GICv3+PL011 bring-up is the documented next step; honest, not
0%-idle). On top of the raw byte substrate, librheo gained **`term`** - the
byte-stream terminal discipline: `input` (a decoder: CSI/SS3 escape sequences ->
typed `Key`s, UTF-8, control chars, async `next_key().await`), `edit` (a line
editor with insertion, cursor moves, word/line kill, history recall, completion
hook), and `render` (a buffered, minimal-diff renderer, batched writes). The
`librheoterm` test drives a read-eval loop with scripted keystrokes (typing,
backspace, cursor-left + insert, an arrow-key escape, Up-arrow history) and asserts
the exact committed lines + exit on **all three ISAs**, plus the idle-park (kernel
idled at `wfi`) on RISC-V. Honest: only RISC-V is interrupt-driven (and QEMU's 16550
loopback does not drive the APLIC line, so the deterministic test raises the UART's
MSI in the IMSIC directly - exactly the MSI the configured APLIC would send; the
byte is genuinely received and a genuine S external interrupt genuinely wakes `wfi`,
docs/LIBRHEO.md Phase D). This is "wake on input", not preemptive scheduling
(SMP/#27).

**Phase E** makes librheo the substrate for **services and a Wayland-class
compositor**: two cells share a **typed cross-cell queue pair** and pass
ownership of a **sealed buffer grant** (zero-copy), with a **flip/present
completion**. Two kernel additions, both **exposing** existing objects (no new
object): **`SYS_CONNECT` (43)** reports a cell's shared-channel end
(`ChannelInfo { chan_va, cap_id, role }`) - a cross-cell channel is **one ring
region mapped into two cells** at 24 GiB (`load::alloc_channel` +
`map_channel_into`), whose header the kernel writes once but **never drains**:
the two cells drive the SPSC rings directly over the shared frames (IO.md 6, the
queue object 3), so a message is a pure shared-memory write. **`SYS_GRANT_SHARE`
(44)** delegates a **sealed** grant to the peer cell (objects 2/5): the kernel
maps its frames **read-only** into the peer (`AddressSpace::share_ro_into`, over
the existing per-ISA leaf walk), mints a MemoryGrant cap there on the **same**
object (epoch-revocable), and reports `ShareInfo { peer_va, peer_cap_id }` -
**zero-copy shared memory, the dmabuf equivalent** (requires DELEGATE + sealed;
`SYS_GRANT` now mints `READ|WRITE|MAP|DELEGATE`). librheo gained **`ipc`**
(`Channel` - a typed cross-cell queue pair with client `send`/`await_completion`
+ server `recv`/`complete` over `SqEntry`/`CqEntry`, the cooperative
`switch_to_peer` hand-off, and `share`/`recv_buffer` for zero-copy buffer
passing) and **`display`** (`Surface` - a client drawable backed by a sealed
buffer grant, `commit` seals+delegates+sends+awaits the flip; `Compositor` - an
in-memory framebuffer, `present` composites the shared buffer + replies with the
flip completion; `InputEvent` reuses the Phase D `term::input::Key` as the HID
event-stream shape). The `librheowl` test runs **one binary as two cells** (the
test kernel wires both + the shared channel + roles): the client draws a 64x64
buffer, seals + shares it, commits a frame; the compositor maps the shared grant
read-only (zero-copy - same frames), composites it, and returns a flip completion
carrying its checksum; the client asserts that checksum **equals its own known
value** (`0x3eb4f800`) - proving zero-copy cross-cell sharing - and exits `0x42`
on **all three ISAs**. Honest: zero-copy is real (the only copy is the
compositor's own composite into its framebuffer); the channel is synchronous with
an explicit peer hand-off (a fully-symmetric async `Sender`/`Receiver` parking on
the reactor is the documented refinement); spawn-driven connect is Phase F; real
GPU (virtio-gpu scanout) is deferred - the mechanism (shared sealed buffer + typed
present queue + flip completion + input-event shape) is the deliverable. Phase F
(process/time/net + a librheo-native shell) is planned in docs/LIBRHEO.md.

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
              sched (reservations), lease, engine, graph, pty, input
              (kernel RX ring + the SYS_WAIT_INPUT park-until-input primitive -
              docs/LIBRHEO.md Phase D), svc
              (shell/resource/POSIX-file syscalls), hw (ACPI/FDT/PCIe
              discovery + the machine Inventory; block BlockDevice trait +
              virtio_blk driver), elf + load (ELF loader for native
              programs), user run loop (with per-cell syscall
              personalities), linux (the Linux personality:
              docs/LINUX-COMPAT.md), U-mode programs
              (user_progs.rs incl. the lsh shell), abi
  src/arch/   per-ISA Rust modules incl. paging.rs (one dir per ISA)
  arch/       per-ISA assembly (boot, vectors/traps, context switch, user)
  link/       linker scripts per ISA (incl. the .user text/rodata/data window)
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline,
              isolation-hw, resources, shell-smoke, hwinfo, rng, runtime,
              posix, blockfs (live virtio-blk disk), elfrun (load a native
              ELF), posixrun (native program over the POSIX syscalls),
              libcrun (a program linked against rheo-libc), jsonrun (a
              program parsing JSON with rheo-json on-OS), stdrun (a real-std
              program on-OS), librhearun (librheo Phase A: a loaded cell with
              a real mapped queue pair does heap+rng+cap + an async queue
              round-trip, docs/LIBRHEO.md), librheodata (librheo Phase B: the
              mini-DuckDB scan - typed grants + async I/O opcodes + a zero-copy
              columnar scan off a live virtio-blk disk), librheocompute (librheo
              Phase C: parallel map_reduce + a userspace graph submitted to the
              CPU engine + reservation admission), librheoterm (librheo Phase D:
              the first interrupt-driven console wakeup + the term byte-stream
              discipline - scripted editing/history/escape, idle-park on RISC-V),
              librheowl (librheo Phase E: the Wayland-class compositor demo -
              two cells share a typed cross-cell queue pair + pass a sealed
              buffer grant zero-copy + a flip completion, checksum-verified),
              coreutils (the coreutils multicall cell, with
              argv + std::fs over the VFS), linuxrun (Personality::Linux:
              bare Linux-ABI programs plus, at L2, unpatched static-glibc
              Rust + C hellos), linuxtools (L3: the unmodified upstream
              uutils/coreutils multicall cell over a ramfs, incl. threaded
              sort), linuxthreads (L4: an unpatched multi-threaded Rust std
              binary - clone/futex/TLS/join), linuxsig (L5: signal delivery -
              async raise, fault->SIGSEGV handler, SIG_DFL terminate),
              linuxproc (L6: fork/execve/wait4/cross-cell pipes - a direct
              multi-process C fixture + the P11 coreutils-suite shell),
              linuxdyn (L7: an unmodified dynamically-linked glibc C hello over
              PT_INTERP + ld-linux + fd-backed mmap), bench-core, and the
              interactive
              lsh bin (+ harness.rs, vfs_personality.rs); fixtures/ holds the
              ext4 test image (+ gen-ext4.sh); linux-fixtures/ holds the
              built-from-source glibc test binaries (rusthello/ + rustthreads/
              + hello.c + sig_{raise,segv,dfl}.c + procdemo/cecho/rsh.c +
              dhello.c; coreutils via cargo install, and the L7 ld.so/libc.so.6
              copied from the toolchain - all gitignored)
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
librheo/      the native userspace foundation library (docs/LIBRHEO.md):
              no_std+alloc, mem (grow heap + typed grants/arena/mapping)/rng
              (per-cell DRBG)/cap (typed handles)/rt (strand executor + userland
              queue reactor)/sys (syscall + on-wire queue ABI)/io (async
              File/read_at/write_at/Contract)/store (Dataset)/compute
              (map_reduce/parallel_for/scan strand workers + Engine::info +
              GraphBuilder)/sched (Reservation + lattice-rt Priority/PeriodicTask/
              TimingReport)/term (Phase D byte-stream input/edit/render)/ipc
              (Phase E cross-cell Channel + sealed-buffer share)/display (Phase E
              Surface/Compositor/InputEvent) + crt0 + the librheo-demo (Phase A),
              librheo-data (Phase B mini-DuckDB scan), librheo-compute (Phase C
              parallel compute + graph + QoS), librheo-term (Phase D terminal),
              and librheo-wl (Phase E compositor demo) programs
json/         rheo-json: a dependency-free, zero-copy JSON parser (scalar +
              SSE2 string-scan), no_std, host-tested + benchmarked
              (docs/JSON.md, comparison/json/)
services/     system service cells        (future, phase 5)
targets/      rheo-os custom target specs + the std port: rheo_os-*.json,
              patch-std.py (rust-src std patch: heap/stdio/args/env/fs arms),
              std-rheo/ (rheo sys sources + the rheo-rt crt0, the std proof
              program, and the rheo-coreutils multicall cell).
              docs/USERLAND.md M4/M5
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
