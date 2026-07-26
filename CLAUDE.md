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
**L8 has begun** (docs/LINUX-COMPAT.md L8, docs/NETSTACK.md rheo-net Phase N1d):
**AF_UNIX (Unix domain) sockets** - `socket`/`socketpair`/`bind`/`listen`/
`accept`/`connect`/`sendmsg`/`recvmsg` on SOCK_STREAM, sockets as per-cell fds
whose byte transport reuses the **L6 cross-cell ring** (a connection is two rings,
one per direction) plus a global name registry (`kernel/src/linux/unixsock.rs`) -
no new kernel object, the L6 `pipe2` precedent. The `linuxunix` test runs an
unmodified static-glibc AF_UNIX C fixture (socketpair+fork + bind/listen/connect/
accept over an abstract name) on **all three ISAs**. SCM_RIGHTS fd-passing and
SOCK_DGRAM are documented deferrals.

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
`SYS_MMAP` frame leak; DDR always real and **PMEM real where a QEMU nvdimm is
exposed** (Phase J: x86-64 q35 via the ACPI NFIT + a separate pmem allocator,
distinct from the DDR pool; arm/riscv `virt` expose no nvdimm so PMEM
skips-with-reason to DDR - docs/MEMORY.md 2.1), HBM/CXL/Remote emulated-as-DDR,
device-BAR refused, NUMA single-node - all honest/documented, per-cell grant
tables as fixed statics, every commit/decommit/seal grant-checked), and **real
async I/O opcodes
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
test, so the other kernels are untouched): **interrupt-driven on RISC-V and
ARM64**. RISC-V routes the 16550 UART's IRQ (source 10) through the **AIA** (S-mode
APLIC in MSI mode -> S-mode IMSIC via the `siselect`/`sireg`/`stopei` CSRs ->
`sip.SEIP`) and takes the S external interrupt to drain the UART, halting at `wfi`
(cells run with `sstatus.SIE` clear; SIE set only to service a pending SEI after
`wfi` woke on it). ARM64 routes the PL011 UART's IRQ (SPI 33) through the **GICv3**
(GICD + the boot CPU's GICR, CPU interface via `ICC_*_EL1`) and takes it in the
current-EL-SPx vector slot, draining the byte and EOI'ing via `ICC_EOIR1_EL1`;
cells run at EL0 with IRQ masked and the kernel unmasks (`daifclr`) only after
`wfi`. Both are genuine 0%-CPU parks. **x86-64 stays a poll** - under QEMU's TCG +
`kernel-irqchip=split` the LAPIC ISR/IRR are not modeled and IOAPIC-routed lines do
not re-deliver reliably, so faking it would be dishonest (its *timer* IS
interrupt-driven; only the IOAPIC-routed UART is affected). On top of the raw byte
substrate, librheo gained **`term`** - the
byte-stream terminal discipline: `input` (a decoder: CSI/SS3 escape sequences ->
typed `Key`s, UTF-8, control chars, async `next_key().await`), `edit` (a line
editor with insertion, cursor moves, word/line kill, history recall, completion
hook), and `render` (a buffered, minimal-diff renderer, batched writes). The
`librheoterm` test drives a read-eval loop with scripted keystrokes (typing,
backspace, cursor-left + insert, an arrow-key escape, Up-arrow history) and asserts
the exact committed lines + exit on **all three ISAs**, plus the idle-park (kernel
idled at `wfi`) on **RISC-V and ARM64**. Honest: RISC-V and ARM64 are
interrupt-driven, each with a device-loopback caveat (QEMU's 16550/PL011 loopback
does not drive the interrupt-controller line, so the deterministic test raises the
controller line directly - RISC-V the IMSIC MSI, ARM64 `GICD_ISPENDR` for SPI 33 -
exactly the interrupt the device would raise; the byte is genuinely delivered and a
genuine interrupt genuinely wakes `wfi`, docs/LIBRHEO.md Phase D). This is "wake on
input", not preemptive scheduling
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
compositor's own composite into its framebuffer); the synchronous channel is an
explicit peer hand-off (the compositor uses it); a **fully-symmetric async
`Sender`/`Receiver` parking on the reactor** is now done as **Phase J** (below);
spawn-driven connect is Phase F; real
GPU (virtio-gpu scanout) is deferred - the mechanism (shared sealed buffer + typed
present queue + flip completion + input-event shape) is the deliverable.

A **Phase J polish trio** refines librheo Phases E/F (additive userspace, no new
kernel object). **(1) Symmetric async IPC**: `ipc::Channel::split()` yields an
async `AsyncSender`/`AsyncReceiver` that **park on the strand reactor** (a channel
slot in `rt`, mirroring the console/timer/wait slots) instead of spinning on
`switch_to_peer`. A strand that `recv`s parks, the vcore runs the cell's other
strands, and only when all have parked does `block_on` hand the CPU to the peer
(`SYS_SWITCH`) and deliver the message. The in-cell wait is a genuine reactor park;
the cell-boundary hand-off stays a cooperative switch under the single-CPU model (a
truly parallel producer/consumer awaits SMP #27) - honest. The `librheoipc` test
runs one binary as two cells that **ping-pong 8 typed messages** over the async
Sender/Receiver; the consumer asserts the exact sequence **and**
`rt::chan_wakeups() == 8` (every message a genuine reactor park+wake, not a spin),
exiting `0x42` on **all three ISAs**. **(2) Cross-cell stdout pipelines**:
**`SYS_SPAWN` now propagates the parent's channel to the spawned child** (maps the
same frames RW via `AddressSpace::share_rw_into`, mints a channel cap into the
shared bundle, records the child's end with the opposite role - no new kernel
object, it composes Cell + QueuePair). `proc::spawn_piped` returns a `Pipe {
child, tx, rx }`: the child streams its output over the async channel (not through
the kernel), the parent reads it with `rx`, then reaps the child. Honest: the pipe
connects a spawned child to its **parent** (a valid `cur^1` pair; the child frees
the shared frames on exit after the parent drained them); two sibling spawned
stages await a directed switch/SMP (#27). The `librheopipe` test spawns
`/bin/pipesrc`, which streams `"ABCDEFGHIJKL"` back over the inherited channel; the
orchestrator reconstructs+verifies it and exits `0x42` on **all three ISAs**.
**(3) The full `term` line editor in `lrsh`**: the librheo-native shell now drives
the Phase D editor - `KeyReader` (parking on input) + `LineEditor` (in-line cursor
edits, backspace, word/line kill, **Up/Down history**, a **Tab command-name
completion hook**) + the buffered `Renderer` - instead of a raw line read; committed
lines still run builtins or spawn `/bin/<cmd>`. The `librheoproc` shell scenario
feeds scripted keystrokes (typing, a backspace edit `child 9`->`child 8`, Up-arrow
history recall, `ec`<Tab>->`echo`) and asserts the committed-command evidence
(`child 8` ran twice, no `child 9` command, completion produced `echo`) + exit
`0x42` on **all three ISAs**.

**Phase F** closes librheo as a **complete foundation**: a native **process
model**, **time**, a **librheo-native shell**, an **embedded** proof, and honest
benchmarks. Three kernel additions **expose** the Cell object (1) / arm-timer verb
(no new object; per-cell synthesized state in `kernel/src/nproc.rs`, mirroring
`linux::proc` for `Personality::Native` cells). **`SYS_SPAWN` (45)** - gated by a
**cell-spawn capability** (`ObjectKind::Cell` + WRITE - no ambient authority) -
streams an ELF from the VFS into a **new** native cell with its own address space +
queue pair (sharing the parent's cap bundle like `fork`), builds its SysV stack
from the caller's argv/envp, and returns a child handle; **`SYS_WAIT` (46)** blocks
the parent cooperatively (generalizing the L6 cross-cell run loop), runs the child,
and reaps its exit code (a faulted native child is reaped with `FAULT_EXIT`=139 -
native cells have no signals); **`SYS_ARM_TIMER` (47)** is a one-shot deadline,
now the OS's **second interrupt**, **interrupt-driven on all three ISAs** (the
kernel arms the per-ISA timer and halts at `wfi`/`hlt` until it fires - a genuine
0%-CPU park: RISC-V Sstc `stimecmp`, ARM64 CNTV virtual timer via the GICv3, x86-64
LAPIC LVT one-shot in x2APIC mode; opt-in via `arch::enable_timer_irq`, with a
cooperative deadline-check fallback where not wired).
librheo gained **`proc`** (`spawn`/`Child::wait().await`/`args`/`env`/`identity`),
**`time`** (monotonic `Instant`/`now` + async `sleep`/`timeout`/`interval` over the
reactor's timer slot), and a **`net`** stub (deferred - networking is a service).
It is **feature-gated**: `default=["full"]`; an **embedded** cell builds
`--no-default-features` (spine only: cap/rt/mem/sys) - `librheo-embed` does a direct
queue round-trip and is **~9x smaller** loadable than a full binary. **`lrsh`** is
the librheo-native shell (builtins + `spawn`/`wait` of native coreutils over the
Phase D console path; **Phase J** wires in the full `term` line editor - see
below). The `librheoproc` test proves it on **all three ISAs**: an
orchestrator spawns `/bin/echo` + three `/bin/child` cells (argv fan-out), reduces
exit codes to 12, and a `time::sleep` wakes on the timer (asserting a genuine
`wfi`/`hlt` idle-park on all three ISAs); `lrsh` runs a scripted keystroke
session through the term editor (committed-command evidence + exit `0x42`); and the spine-only `librheo-embed`
round-trips. Benchmarks (icount, per TOOLING.md): full async round-trip ~1,433
(x86-64) / ~2,048 (riscv64) instructions, spawn+wait ~263k (x86-64) / ~539k
(riscv64) - process create is dominated by ELF stream-load + child crt0, the honest
price of a new address space. Honest deferrals: the
**x86-64 UART RX interrupt** (poll fallback - its QEMU TCG split-irqchip
IOAPIC/LAPIC does not re-deliver reliably; the timer + the riscv/arm UART RX are
all interrupt-driven), and the `net` stack - docs/LIBRHEO.md has the full A-F
accounting. **librheo A-F is complete.**

**Phase G** turns the Phase F `net` stub into the real **NIC data path - raw
Ethernet frames over a virtio-net driver** (docs/NETWORKING.md, LIBRHEO.md Phase
G); the IP/TCP/QUIC stack stays a **service**, deferred. A hand-written
**virtio-net driver** (`kernel/src/hw/virtio_net.rs`) mirrors virtio-blk over the
**two transports** - virtio-mmio on arm/riscv `virt`, virtio-pci on x86-64 q35
(via the `VIRTIO_PCI_CAP_PCI_CFG` config tunnel, no BAR mapping) - with reset +
**minimal** feature negotiation (`VIRTIO_F_VERSION_1` + `VIRTIO_NET_F_MAC`; no
mergeable-rx-buffers or checksum/GSO offload), an **RX** and a **TX** split
virtqueue, the 12-byte v1 `virtio_net_hdr`, and the MAC from device config; DMA
uses **physical** addresses (`virt_to_phys`), polled (a device RX IRQ is a later
refinement). Three **queue opcodes** (`OP_NET_TX`/`OP_NET_RX`/`OP_NET_MAC`, no new
kernel object) bridge a cell's async submissions to the driver in `kernel_process`,
completing with the strand token - the Phase B `io` model. librheo's **`net`** is
now real: `mac`/`send`/`recv` of raw frames (`connect`/`listen` stay `Unsupported`
- IP/TCP is a service). The `librheonet` test proves it on **all three ISAs**: a
librheo cell reads the NIC MAC, sends a **broadcast ARP request** for the SLIRP
gateway `10.0.2.2`, and **receives SLIRP's ARP reply** (a deterministic,
network-free RX proof over QEMU `-netdev user`), asserting ethertype + opcode +
sender IP and exiting `0x42`. Deferred: the full transport stack (IP/TCP/QUIC/TLS
in a cell), a socket `ObjectKind` + steering grants, header/payload split, and the
device RX interrupt. **librheo A-G is complete.**

**Phase H** brings up a **real GPU: a virtio-gpu 2D driver wired to the Phase E
compositor** (docs/DISPLAY.md, LIBRHEO.md Phase H); VIRGL/3D and the full display
pipeline stay deferred. A hand-written **virtio-gpu 2D driver**
(`kernel/src/hw/virtio_gpu.rs`, the plain 2D / VIRGL-off subset of virtio spec
5.7) mirrors virtio-net/blk over the **two transports** - virtio-mmio on
arm/riscv `virt`, virtio-pci on x86-64 q35 (via the `VIRTIO_PCI_CAP_PCI_CFG`
config tunnel, no BAR mapping) - with reset + **minimal** feature negotiation
(`VIRTIO_F_VERSION_1` only; no VIRGL/EDID), a single **controlq**, and every 2D
command a `virtio_gpu_ctrl_hdr` + body submitted as a **2-descriptor chain**
(`[readable command][writable response]`, the virtio-blk request/status shape)
polled for its `RESP_OK_*` code. Bring-up: `GET_DISPLAY_INFO` -> `CREATE_2D`
(resource 1, `B8G8R8A8_UNORM`, **128x128**) -> `ATTACH_BACKING` (a kernel-side
framebuffer of **16 frame-pool frames**, one `virtio_gpu_mem_entry` per frame, so
no contiguous alloc is needed) -> `SET_SCANOUT` (scanout 0); a present is
`TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`. All rings/buffers/framebuffer come from
the frame pool (only static is a small `Option<VirtioGpu>`); DMA by **physical**
address (`virt_to_phys`). One **queue opcode** (`OP_GPU_PRESENT`, no new kernel
object - it extends the queue object with a mechanism) bridges a cell's async
present to the driver in `kernel_process`, copying the cell's framebuffer into the
resource then transfer+flush. librheo's **`display`** gained `Gpu`/`Scanout` (draw
into a framebuffer grant, `present().await` to the real device); the Phase E
in-memory `Compositor` is unchanged (still proves zero-copy). The `librheogpu`
test proves it on **all three ISAs**: a librheo cell draws a known 128x128 RGBA
frame and presents it, exiting `0x42`. QEMU runs **headless** (`-display none`),
so - like virtio-net's network-free ARP proof - the proof is the **genuine driver
round-trip**: **all six 2D commands return OK** from the real device model
(`GET_DISPLAY_INFO` reports QEMU's default 1280x800 even headless; then create-2d,
attach, set-scanout, transfer, flush). No claim of visible output; the 2D scanout
command round-trip + compositor present wiring is the deliverable. **librheo A-H
is complete.**

**rheo-net N2d** (docs/NETSTACK.md 16) makes the network **receive** side as async
as the send side - the OS's **third interrupt source**. Before it,
`librheo::net::recv` was a re-poll (`OP_NET_RX` returned "nothing available" and the
cell submitted it again), and the reactor had no network slot, so a cell waiting for
a packet **spun a core**. Three pieces: **`SYS_WAIT_NET` (48)** - a park-until-frame
verb in the shape of `SYS_WAIT_INPUT` (`kernel/src/net_rx.rs`, portable), mechanism
only, **no new kernel object** (it exposes the same virtio-net driver the `OP_NET_*`
opcodes bridge to), taking a `timeout_ns` deadline so a transport can wait for "a
frame **or** the RTO, whichever comes first" (the per-ISA timer gained
`timer_arm`/`timer_expired`/`timer_disarm`, and `timer_wait` is now built on them);
a **NIC RX interrupt**, wired from the virtio-mmio slot the driver records
(`arch::enable_virtio_net_irq`, opt-in like the Phase D UART IRQ, so the other 47
kernels boot unchanged) - the handler ACKs the device
(`InterruptStatus`/`InterruptACK`) and counts the arrival, the TX ring now sets
`VRING_AVAIL_F_NO_INTERRUPT` (transmit stays polled), and on RISC-V
`riscv_user_trap` now services a device interrupt taken **in U-mode** (S-mode
interrupts are always enabled there) instead of reading it as a fault; and a
**reactor network slot** (`net_rx_req` beside console/timer/wait/channel) so
`net::recv`/`recv_timeout` **park** and wake on the frame, with `net::try_recv`
keeping the non-blocking drain a batching transport needs. The kernel's RX ring
*is* the receive virtqueue's 16 pre-posted device-DMA'd buffers (a second ring would
only add a copy - documented). The `netwait` test proves it on **all three ISAs**: a
cell parks on `net::recv`, is woken by SLIRP's **ARP reply**, parks again for a **TCP
reset**, and finally parks with a 20 ms deadline on an empty queue; asserted are one
reactor wakeup **per** receive (`rt::net_wakeups()` - one park + one wake, never N
re-polls), a witness strand that ran **while** the receiver was parked, and
kernel-side `net_rx::irq_count() > 0` + `did_idle()` on the interrupt-driven ISAs.
Per-ISA honesty (the wait has **three modes**, chosen by portable logic over the
`arch` predicates - `net_rx::IdleMode`): **riscv64** (APLIC-S source `1+slot` in MSI
mode -> IMSIC -> `sip.SEIP`) and **aarch64** (GICv3 SPI `16+slot`) are genuinely
**NIC-interrupt-driven** and halt at `wfi` - a real 0%-CPU park. **x86-64 has no NIC
RX interrupt** (its NIC is virtio-*pci* driven through the `VIRTIO_PCI_CAP_PCI_CFG`
tunnel with no mapped BAR for an MSI-X table, and legacy INTx rides the same QEMU-TCG
IOAPIC path that does not re-deliver) but it *does* have a genuine LAPIC timer, so its
wait is a **timer-backed idle** (N4c): poll the receive queue, `hlt` for a **500 us**
one-shot slice, re-poll - a real halt at ~1% duty cycle, not a spin, with
`interrupt_driven()` still reporting **false** (only `did_idle()`/`idle_mode()` report
the halt, so a timer wake is never dressed up as a NIC interrupt). `timeout_ns` is a
**monotonic deadline in every mode** - `POLL_BUDGET` is only a backstop for an
indefinite wait on the last-resort poll path and can never truncate a caller's timeout.
MSI-X through the config tunnel, interrupt coalescing, and zero-copy receive are the
documented next steps.

**rheo-net N4a** (docs/NETSTACK.md 17) is the **network service cell + concurrent
fan-out** - the keystone every remaining network scenario rides on (app-protocol
servers, the remote-INET bridge for Linux binaries = **N4b**, onion routing,
DHCP/zeroconf/NTP). Doctrine puts the stack in userspace, so a long-lived **service
cell** must serve **many** client cells; Phase E only proved one-to-one. Two hard
blockers had to go: a cell held **exactly one** channel end (three spawned children
would all inherit the *same* ring - a race, not a fan-out), and `SYS_SWITCH` is a
*directed* `cur^1` hand-off (from client cell 2 it reaches cell 3, never the service
at cell 0 - a livelock). N4a takes the **minimal mechanism extension** of the existing
spawn/channel path - **no new kernel object**, everything composing Cell (object 1)
with QueuePair (object 3), exactly the L6-pipe / Phase-J precedents: a **per-cell
channel table** (`MAX_CELL_CHANNELS` = 4, a fixed static array - the kernel stays
allocation-free), each slot its own ring region at `channel_slot_va(slot)`, with
`SYS_CONNECT(out, slot)` gaining the slot argument + a `count`; **`SYS_SPAWN` gaining
a `chan_spec`** (`SPAWN_CHAN_SLOT | slot << 8` = hand the child the caller's slot
`slot`, always landing at the child's **own slot 0** so a client binary is
slot-agnostic; `chan_spec` 0 is the byte-for-byte Phase J default, and
`AddressSpace::share_rw_into` gained the matching `dst_base`); and **`SYS_YIELD`
(49)** - hand the CPU to the next runnable native cell, round-robin, the caller
staying runnable. `SYS_YIELD` is the same cooperative cross-cell scheduler
`SYS_WAIT`/child-exit already drive (`kernel/src/nproc.rs`) exposed as a plain yield,
transfers no authority (one capability bundle), and **degenerates to `cur^1`** where
the caller has no native process tree, so the Phase E/J two-cell path is unchanged;
the reactor's channel idle path now uses it. On top, `net::service` ships the
framework: a **`Service`** binding one channel end per client and running **one
strand per client** (each parked on its own `AsyncReceiver`; the reactor scans slots
in order, which is what round-robins them), a thin **`Client`** for the spawned cell,
and a word-wide protocol (`Request { op, client, seq }` packed in the message tag,
argument/result in its `u32`): `OP_ECHO` (a per-client keyed transform), `OP_RESOLVE`
(a catalogue name id -> an IPv4, answered from the **network-free** tiers of
`net::dns` - a `HostsTable` + a TTL `Cache`), `OP_BYE`. librheo gained per-slot
reactor channels (`attach_channel_slot`, `chan_send_on`/`chan_recv_on`),
`ipc::Channel::open_slot`, `proc::spawn_on_channel`, and two witnesses -
`rt::chan_wakeups_on(slot)` + `rt::chan_max_pending()` (over a new **non-destructive**
`Qp::sq_pending`/`cq_pending` peek). The `netservice` test proves it on **all three
ISAs**: one service cell serves **three** client cells, each over its **own** channel,
and exits `0x42` only if every client got its **distinct correct** response (each
predicts its echo + its own name `10.1.1.1`/`10.2.2.2`/`10.3.3.3` and exits `id+1`,
codes asserted `1,2,3` - so each was really reaped), `served == [4,3,3]`, the
**interleave witness** `order == [0,1,2, 0,1,2, 0,1,2, 0]` (strand k reaches round r
only after strands `0..k` did - no strand monopolised the vcore), the **in-flight
witness** `max_in_flight == 3` (all three requests queued at the same instant, before
the first reply), and `wakeups == [4,3,3]` (one genuine reactor park+wake per message,
never a spin). The core is **deterministic and network-free**; a **bonus live** ARP for
the SLIRP gateway, performed *inside client 0's serving strand* (parking on the wire
per N2d while siblings run), resolves on all three ISAs and **degrades honestly** to
`REPLY_NONE` with no NIC. Honest: **concurrent, not parallel** (one CPU, cooperative -
a service strand cannot compute while a client computes; SMP is task #27); fan-out is
**parent-shaped** (the service spawns its clients - a **name-based rendezvous** for
unrelated cells is the genuinely new capability and a documented follow-on); the
protocol is word-wide (bigger requests ride a shared sealed grant, the Phase E
mechanism); and 4 clients is the fixed-array ceiling.

**rheo-net N5a** (docs/NETSTACK.md 19) adds the first **application protocols** -
**HTTP/1.1 + HTTP/2, client and server** - the gateway most remaining scenarios ride
on (WAF/DPI inspects HTTP, S3-style storage *is* HTTP, Arrow Flight is gRPC over
HTTP/2). Both live in the crate's **always-compiled** half (HTTP is parsing plus
synchronous state machines, so it needs neither librheo nor the NIC and links in
either posture); **no kernel change, no new object/verb, no new dependency, no
`cfg(target_arch)`**. **`net::http1`** parses **zero-copy** - method, target, reason
phrase and every header name/value are `&[u8]` slices *of the caller's buffer* (the
rheo-json `Cow::Borrowed` discipline; case-insensitivity applied at compare time), so
a WAF datapath classifies a request without allocating per header; the owned
`OwnedRequest`/`OwnedResponse` the `Client`/`Server` helpers return are the *only*
copy. Framing covers `Content-Length` + **chunked** both directions, bodiless
statuses, and the RFC 9112 persistence rule. **Request smuggling is a parser
property, not a filter**: `Content-Length` *and* `Transfer-Encoding` (either order),
duplicate `Content-Length` (even when equal), `5, 5` / `+5` / `0x5`, a non-`chunked`
final coding, bare LF, `Host : x`, obs-fold, non-token names, control bytes in
values, oversized/too-many headers, a double space in the request line and a
non-1.x version are each rejected **with their own error** - 22 shapes asserted,
plus 4 chunked-framing rejections (a non-empty trailer is refused, not dropped).
`http1::scan` reuses the `json/src/scan.rs` idiom - scalar oracle + a branchless
wide path + fuzz equivalence - but the wide path is **SWAR** (`u64` load/compare/
mask/ctz, 8 bytes per step) rather than SSE2, so it stays portable with no
`cfg(target_arch)`; a target-specific SIMD kernel is deferred. **`net::http2`** ships
the frame layer (DATA/HEADERS/SETTINGS/WINDOW_UPDATE/PING/RST_STREAM/GOAWAY/
CONTINUATION, PRIORITY parsed-and-ignored), the preface, the stream state machine,
**connection- and stream-level flow control** (a send is bounded by
`min(conn, stream, max_frame)`, the remainder queued until a WINDOW_UPDATE credits
it; the *connection* window correctly starts at 65535 and is changed only by
WINDOW_UPDATE, never by `SETTINGS_INITIAL_WINDOW_SIZE`), and **HPACK** - static
table, dynamic table with size updates, and the RFC 7541 Appendix B **Huffman code
generated mechanically from the authoritative RFC text** (blob-hash cross-checked;
the generator asserted prefix-freedom + canonicality, which is what lets the decoder
be a per-length canonical lookup rather than a tree), with the padding/EOS rules
enforced. `conn` has the **same synchronous seam as `net::tcp`** (`on_bytes` /
`take_out` / `next_event`, no I/O inside), which is what makes h2 provable with no
live peer and transport-agnostic. For h2 over TLS, N5a added a **minimal RFC 7301
ALPN** to the N3b handshake (offer in ClientHello, selection echoed in
EncryptedExtensions, both sides must agree) - with an empty list the ClientHello is
byte-for-byte N3b's, so the RFC 8448 KAT is untouched; `h2` is therefore
**negotiated, not assumed**. The `nethttp` test kernel proves it all on **all three
ISAs** (pure compute, no netdev): the h1 codec with the **zero-copy borrow asserted
by pointer range**, the 22 smuggling rejections, the **SWAR == scalar** oracle over
20,000 fuzz buffers, an **h1 client talking to our h1 server over real `net::tcp`**
across the in-cell `VirtualLink` (POST+body byte-exact, a chunked response
reassembled exactly, a **second request on the same never-closed connection**, a
404), **HPACK against RFC 7541 Appendix C** - C.1 integers, C.2.1-C.2.4, and the
**C.3.1-C.3.3 / C.4.1-C.4.3 sequences** (C.4 Huffman) decoded to the RFC's exact
header lists *and* re-encoded to the RFC's exact bytes with the dynamic table sizes
**55/57/110/164** checked (indices 62/63 are only reachable if both tables evolve
identically), Huffman edge cases, **h2** (preface + SETTINGS acked both ways,
HEADERS+DATA, a **flow-control-gated body** - 16 of 39 bytes arrive, the remainder
asserted still queued, the server's WINDOW_UPDATE releases it - a second concurrent
stream, RST_STREAM/PING-ACK/GOAWAY, and four protocol errors asserted to fail), and
**HTTPS**: one h1 exchange through the N3b TLS record layer with the plaintext
asserted absent from the ciphertext, a tampered record still rejected, and ALPN
negotiating `http/1.1` and `h2`. The **live GET is skipped with a reason** - SLIRP
has no HTTP server, so there is nothing deterministic to fetch and nothing is faked.
Deferred: **HTTP/3** (it is HTTP over QUIC = N7), trailers, server push, PRIORITY as
a scheduler, `CONNECT`/`Upgrade` (incl. `h2c` upgrade), `100-continue`, content
codings, a resumable chunked decoder (the one-shot re-scan is O(n^2) per body), a
bound-to-port server *cell* (that composes N4a fan-out with the N6 inbound
steering), and gRPC / Arrow Flight / Kafka (N5b/N5c) on top of this h2.

**rheo-net N4b** (docs/NETSTACK.md 18, docs/LINUX-COMPAT.md L8-INET-REMOTE) is
**real remote networking for unmodified Linux binaries** - the biggest functional
unlock left. L8-INET gave `AF_INET`/`AF_INET6` sockets but **loopback only**: a
non-loopback destination was refused `-ENETUNREACH`, because a *local* TCP connection
degenerates to the L6 ring the kernel already has while a *remote* one needs the real
segment/RTO machinery, and the kernel is **allocation-free**. N4b's key: that is true
of the `kernel/` **library**, not of a *kernel binary* - a test kernel declares its own
`#[global_allocator]` and already links alloc crates. So the kernel gains **a bridge,
not a stack**, and **no kernel object**: `svc::SocketOps` (10 `fn` pointers -
`local_ip`, `udp_bind`/`close`/`send`/`recv`/`pending`, `tcp_connect`/`send`/`recv`/
`close` - plus `set_socket_ops`), mirroring `svc::FileOps` line for line (the pattern
that keeps the kernel **filesystem-free** while serving `open`/`read`/`write`), and
`kernel/src/linux/fd.rs` forwards every **non-loopback** operation to it via two new
`FdKind` variants (`InetUdpRemote`/`InetTcpRemote`). **Loopback is byte-for-byte
unchanged** (`linuxinet` still asserts its transcript), and with no bridge registered
the answer stays `-ENETUNREACH`, so all 49 pre-existing kernels behave identically.
The registrant is `tests/src/inet_personality.rs` (the sibling of
`vfs_personality.rs`), which links **`rheo-net` in a new librheo-free `codec`
posture** - librheo supplies a *cell's* `_start`/panic handler/global allocator, which
a kernel binary cannot link, so the crate now has a default `hosted` feature gating
the async endpoints (`arp::resolve`, `udp::UdpEndpoint`, `icmp`, `dns`, `timer`,
`local`, `service`) and `--no-default-features` leaves the pure synchronous layers
(`eth`/`ip`/`arp` packets/`udp` codec/`tcp`/`cc`/`shard`/`wire` framing). Everything on
the wire is the stack's own code - `eth` framing, `arp` request/reply + a 4-entry cache
with real next-hop routing, `ip` headers + checksum, `udp` build/parse + pseudo-header
checksum, and the full RFC 793 `tcp::Connection`, whose **synchronous**
`poll(now)`/`on_wire_segment(now, bytes)` seam (written for the N2a in-cell link)
drives straight from a syscall trap. Three small documented driver accessors were added
(`virtio_net::send_frame_slice`/`recv_frame_slice`/`mac_addr`) plus a
`net_rx::wait_frame_slice` twin, so a remote receive **parks** on the N2d
park-until-frame primitive - a genuine WFI idle on riscv64/aarch64, the documented
bounded poll on x86-64. The `linuxnet` test proves it on **all three ISAs**: an
**unmodified static-glibc C binary** (`inetremote.c`) hand-builds a DNS query,
`sendto`s it to SLIRP's responder `10.0.2.3:53` and `recvfrom`s the reply, asserting
its **structure** (txid echoed, QR set, sender `10.0.2.3:53` - never a resolved
address, which SLIRP proxies non-deterministically), then `connect()`s to a closed
gateway port `10.0.2.2:9` where SLIRP's **real reset** becomes `ECONNREFUSED`; each
phase prints one line from a small fixed set so the transcript stays exact while
nothing is fabricated, and the kernel also asserts the receive genuinely parked
(`net_rx::irq_count() > 0` + `did_idle()`). Honest scope: **UDP remote is complete**;
**TCP connect is real and proven** (SYN on the wire, RTO retransmit, RST -> refused,
deadline -> `ETIMEDOUT`) while TCP **data transfer is implemented but unproven under
QEMU** - SLIRP offers no TCP responder, so it is untested code until a phase adds one;
**IPv6 remote** stays `-ENETUNREACH`; no remote **listener** (inbound needs NIC
steering grants); remote handles are not refcounted across `dup`/`fork`; fixed
registries (4 UDP / 4 TCP / 4 ARP); one documented 2 s receive + 3 s connect bound (no
`SO_RCVTIMEO`/`O_NONBLOCK`); no DHCP (the SLIRP identity 10.0.2.15/gw 10.0.2.2 is
fixed); and moving the datapath into the **N4a service cell** awaits N4a's deferred
name-based rendezvous.

**rheo-net N4c** (docs/NETSTACK.md 20) is **host configuration** - the two questions a
host must answer before anything works, *who am I on this link* and *what time is it*.
Four pieces, all ordinary userspace over UDP/ARP, **no kernel object, no new verb, no
new dependency, no `cfg(target_arch)`**: **`net::hostcfg`**, the host-config store
(address / netmask / gateway / DNS servers / search domains / hostname / source) owning
the on-link-vs-gateway `next_hop` decision - `HostConfig::slirp()` is now the **one**
place QEMU's guest/gateway/resolver addresses are named, and
`udp::UdpEndpoint::from_host_config` / `dns::Config` / `net::service` read the store
instead of carrying literals (a link-local claim deliberately **clears** the gateway);
**`net::dhcp`**, a DHCP client (RFC 2131 - the BOOTP codec + magic cookie + TLV
options, DISCOVER->OFFER->REQUEST->ACK->BOUND, T1/T2 renewal + rebinding + expiry with
the RFC defaults and a `T1 > T2` clamp, NAK, DECLINE, RELEASE, and the broadcast
`0.0.0.0 -> 255.255.255.255` **no-ARP** framing that is the whole special case);
**`net::zeroconf`**, IPv4 link-local (RFC 3927 - the `0.0.0.0`-sender ARP **probe**,
conflict re-pick, bounded announce, defend-once-then-yield) plus **mDNS** (RFC 6762
over the **`dns` codec unchanged** - which is why N4c made that codec
posture-independent: QU bit, cache-flush bit, TTL-0 goodbye, `.local` scoping, the RFC
1112 `01:00:5e` MAC mapping); and **`net::ntp`**, an SNTP/NTPv4 client whose answer is
a **bounded interval** (half-width `delay/2` widened by the server's root distance),
adjusting a *userspace offset* and never a system clock. Codecs + state machines are
always compiled; only the four async drivers are `hosted`. The `nethostcfg` test proves
it on **all three ISAs**: a deterministic **network-free** core (a complete DISCOVER
byte oracle with every uncovered byte asserted zero, the state-machine walk driven by
OFFER/ACK from our *own* encoder, renewal/rebind/expiry/NAK, seven rejections each with
its own error, DECLINE/RELEASE, the store read back by `dns::Config` + `UdpEndpoint`,
the link-local KAT + conflict protocol, mDNS oracles, and the NTP KAT - offset exactly
**+250 ms**, delay **1.5 s**, half-width **750 ms**/**1.75 s** - plus nine rejections
and the KoD backoff), then four **bonus live** phases: SLIRP **does** run a DHCP
server, so a real DISCOVER->OFFER->REQUEST->ACK yields `10.0.2.15/24` on every ISA
(reported, never asserted - it is a property of the QEMU backend, and nothing ever
synthesises a lease), while NTP and mDNS skip-with-reason and the link-local probes go
out and report *absence of evidence*. N4c also **fixed two real defects**: every live
wait is now bounded by a **duration** rather than a drain count (a drain is an
interrupt park on one ISA and a poll on another, so a count meant different things per
ISA and blew the test budget) - `wire::recv_frame_timeout` -> `net::recv_timeout` ->
`SYS_WAIT_NET`, with counts that remain counting **frames**, which is what forced the
kernel wait's deadline-not-spin-count semantics and its timer-backed idle above; and
`LinkLocal::announce` used to serve both the bounded announcement sequence and an
unbounded post-claim *defence*, so once claimed it returned a frame forever and a
driver's `while let Some(f) = ll.announce()` never terminated (announcing and
`defend()` are now separate, and the boundedness is asserted). The test also asserts
the kernel's **wait mode** per ISA: `NicInterrupt` + `did_idle()` on riscv64/aarch64
(3-4 genuine device interrupts per run), `TimerIdle` + `did_idle()` +
`interrupt_driven() == false` on x86-64. Deferred: DHCPv6/SLAAC, DNS-SD, mDNS
known-answer suppression + name probing + the RFC timing schedule, IGMP/MLD, PTP/NTS
and any clock discipline, and running the four clients inside the N4a service cell
(needs N4a's name-based rendezvous).

Deferred (documented): cross-host/cluster, PTP/NTS time sync, attested
firmware + real GPU/NPU engines, elastic-grant pressure events, the Verus
proofs, and the hardware-lab performance numbers. **SMP** (docs/SMP.md,
task #27) now has its foundation and a real RISC-V secondary: the portable
**per-CPU state + kernel spinlock** (`kernel/src/smp.rs`: a `SpinLock<T>` +
a per-CPU registry with `this_cpu()`, zero-impact on the single-CPU path)
and a **genuine RISC-V secondary hart running kernel code** - brought up via
SBI HSM `hart_start` onto the shared kernel address space, it claims a
per-CPU registry slot, marks itself online, and writes a shared counter
through the cross-core spinlock, which the primary reads back and asserts
(the `smp` test, riscv64). ARM64 and x86-64 make a genuine, guarded bring-up
attempt and skip-with-reason: ARM64's PSCI `CPU_ON` (`smc #0`) empirically
**traps to EL1** (no EL3/EL2 firmware in this QEMU config; the SMC is guarded
so the trap is observed, not fatal - CPU detection there is likewise
EL1-limited to the boot CPU), and x86-64 APs need a 16-bit real-mode
INIT-SIPI-SIPI trampoline below 1 MiB (not implemented; ACPI still enumerates
the 4 APs). Still deferred: **preemptive multi-core scheduling** (the runtime
stays single-CPU cooperative - the secondary does proof-of-life work and
parks, it is not yet fed runnable cells) and making the shared kernel
`static mut` state SMP-safe end to end.

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
              (frames + frames_pmem real-nvdimm allocator + grants), time (clock), rng (ChaCha20 DRBG +
              hwrng seeding), event streams,
              sched (reservations), lease, engine, graph, pty, smp
              (per-CPU state + a kernel SpinLock + RISC-V SBI-HSM secondary-hart
              bring-up - docs/SMP.md), input
              (kernel RX ring + the SYS_WAIT_INPUT park-until-input primitive -
              docs/LIBRHEO.md Phase D), net_rx (the SYS_WAIT_NET
              park-until-frame primitive + the NIC RX interrupt sink + the three
              deadline-honouring wait modes: a NIC-interrupt park, a timer-backed
              idle where only the timer interrupt exists, else a bounded poll -
              docs/NETSTACK.md 16), svc
              (shell/resource/POSIX-file syscalls + the N4b SocketOps
              remote-INET bridge - a FileOps-shaped fn-pointer table a service
              registers, so the kernel stays network-stack-free -
              docs/NETSTACK.md 18), hw (ACPI/FDT/PCIe
              discovery + the machine Inventory; block BlockDevice trait +
              virtio_blk driver; virtio_net raw-frame NIC driver -
              docs/NETWORKING.md; virtio_gpu 2D display driver -
              docs/DISPLAY.md), elf + load (ELF loader for native
              programs), user run loop (with per-cell syscall
              personalities + the per-cell channel table: one end per client for
              a service cell, docs/NETSTACK.md 17), nproc (native process model:
              SYS_SPAWN/WAIT + SYS_YIELD round-robin yield + the cooperative
              cross-cell scheduler - docs/LIBRHEO.md Phase F, docs/NETSTACK.md 17),
              linux (the Linux personality:
              docs/LINUX-COMPAT.md), U-mode programs
              (user_progs.rs incl. the lsh shell), abi
  src/arch/   per-ISA Rust modules incl. paging.rs (one dir per ISA)
  arch/       per-ISA assembly (boot, vectors/traps, context switch, user)
  link/       linker scripts per ISA (incl. the .user text/rodata/data window)
tests/        in-QEMU test kernels: cap-invariants, queue-pipeline,
              isolation-hw, resources, pmem (Phase J: a MemKind::Pmem grant
              backed by a real QEMU nvdimm - x86-64 via the ACPI NFIT; arm/riscv
              skip-with-reason - docs/MEMORY.md 2.1), smp (per-CPU state + kernel spinlock +
              a real RISC-V secondary hart; ARM64/x86-64 skip-with-reason -
              docs/SMP.md), shell-smoke, hwinfo, rng, runtime,
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
              the interrupt-driven console wakeup + the term byte-stream
              discipline - scripted editing/history/escape, idle-park on RISC-V +
              ARM64; x86-64 poll),
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
              PT_INTERP + ld-linux + fd-backed mmap), librheoproc (librheo Phase
              F: native spawn/wait + one-shot timer + the lrsh shell + the
              embedded spine-only cell), librheonet (librheo Phase G: raw-frame
              networking - virtio-net driver + net::send/recv/mac, an ARP round
              trip via SLIRP), netwait (rheo-net N2d: true async receive - the NIC
              RX interrupt + SYS_WAIT_NET park; a cell parks on net::recv, wakes on
              SLIRP's ARP reply + a TCP reset, then parks on a deadline, with one
              reactor wakeup per receive and a genuine kernel idle-park on
              riscv64/aarch64), librheogpu (librheo Phase H: a real GPU -
              virtio-gpu 2D driver + display::Scanout present, the create-2d/
              attach/set-scanout/transfer/flush round trip, headless-honest),
              librheoipc (librheo Phase J: symmetric async IPC - two cells
              ping-pong typed messages over the async Sender/Receiver, each recv
              a genuine reactor park), librheopipe (librheo Phase J: a cross-cell
              stdout pipeline - an orchestrator spawns a producer child that
              inherits its channel and streams its output back), netservice
              (rheo-net N4a: the network service cell + concurrent fan-out - one
              service cell serves 3 client cells, each over its own cross-cell
              channel, one strand per client; distinct correct responses + the
              round-robin interleave/in-flight/park-wake witnesses + reaping,
              deterministic core plus a bonus live ARP), linuxnet (rheo-net N4b:
              remote INET for unmodified Linux binaries - an unmodified
              static-glibc C binary does a real DNS round trip to SLIRP's
              resolver 10.0.2.3:53 and a real remote TCP connect over the NIC,
              through the svc::SocketOps bridge + inet_personality.rs), nethttp
              (rheo-net N5a: HTTP/1.1 + HTTP/2 - the zero-copy h1 codec with its
              22 request-smuggling rejections + the SWAR-vs-scalar scan oracle, an
              h1 client<->server exchange over the in-cell TCP VirtualLink
              (POST+body, chunked, keep-alive, 404), the RFC 7541 Appendix C HPACK
              known-answer vectors incl. Huffman, the h2 connection (preface,
              SETTINGS, concurrent streams, WINDOW_UPDATE-gated body, RST/PING/
              GOAWAY), and one h1 exchange through the TLS 1.3 record layer with
              ALPN; pure compute, live GET skipped-with-reason), nethostcfg
              (rheo-net N4c: host configuration - the DHCP byte oracle + full
              state-machine walk (renewal/rebind/expiry/NAK/DECLINE/RELEASE +
              seven rejections), the hostcfg store read back by dns::Config and
              udp::UdpEndpoint, IPv4 link-local (0.0.0.0 ARP probe, conflict
              re-pick, bounded announce, defend-once-then-yield), mDNS over the
              DNS codec, and the NTP offset/delay KAT as a bounded interval;
              deterministic core plus four duration-bounded live phases (SLIRP's
              real DHCP lease reported, NTP/mDNS skip-with-reason) and the
              per-ISA wait-mode assertion - NIC-interrupt park on riscv64/aarch64,
              timer-backed idle on x86-64),
              bench-core, and the interactive
              lsh bin (+ harness.rs, vfs_personality.rs, inet_personality.rs -
              the N4b remote-INET datapath registered as svc::SocketOps);
              fixtures/ holds the
              ext4 test image (+ gen-ext4.sh); linux-fixtures/ holds the
              built-from-source glibc test binaries (rusthello/ + rustthreads/
              + hello.c + sig_{raise,segv,dfl}.c + procdemo/cecho/rsh.c +
              dhello.c + af_unix.c + inet.c + inetremote.c; coreutils via cargo
              install, and the L7 ld.so/libc.so.6
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
              (Phase E cross-cell Channel + sealed-buffer share + Phase J symmetric
              async Sender/Receiver on the reactor + rheo-net N4a multi-slot
              Channel::open_slot: one end per client for a service cell)/display (Phase E
              Surface/Compositor/InputEvent + Phase H Scanout/Gpu real GPU present
              over OP_GPU_PRESENT - docs/DISPLAY.md)/proc (Phase F spawn/wait/args/
              env + Phase J spawn_piped: a spawned child inherits the parent's
              channel as its stdout pipe + rheo-net N4a spawn_on_channel: a service
              hands each spawned client its own channel slot)/time (Phase F clock + async
              sleep/timeout)/net (Phase G raw-frame
              send/try_recv/mac over OP_NET_* + the N2d parking recv/recv_timeout
              over SYS_WAIT_NET - docs/NETWORKING.md, docs/NETSTACK.md 16) +
              crt0 (feature-gated: default=full, --no-default-features=embedded
              spine) + the librheo-demo (Phase A), librheo-data (Phase B
              mini-DuckDB scan), librheo-compute (Phase C parallel compute + graph
              + QoS), librheo-term (Phase D terminal), librheo-wl (Phase E
              compositor demo), Phase F: librheo-orch (spawn/wait/timer proof),
              lrsh (the librheo-native shell), librheo-echo/librheo-child (native
              coreutils it spawns), librheo-embed (the embedded spine-only cell),
              librheo-net (Phase G ARP round trip over virtio-net), librheo-netwait
              (rheo-net N2d parked receive: woken by a real frame, then by a
              deadline), librheo-gpu
              (Phase H virtio-gpu 2D present round trip), librheo-ipc (Phase J
              two-cell async Sender/Receiver ping-pong), and librheo-pipe/
              librheo-pipesrc (Phase J cross-cell stdout pipeline: a spawned
              producer child streams its output to the parent over the channel)
              programs
net/          rheo-net: the greenfield network stack as portable userspace
              (docs/NETSTACK.md, docs/NETWORKING.md) - no_std+alloc, no per-ISA
              code, built for the bare targets as loaded ELF cells over
              librheo's raw-frame path: eth/arp/ip (N1a), udp/icmp/wire (N1b),
              dns caching resolver (N1c), trace (N1e), local fast path (N1d),
              tcp + timer wheel (N2a), cc Reno/CUBIC (N2b), smoltcp_cell +
              shard (N2c), crypto (N3a) + tls (N3b, both feature-gated),
              service (N4a: the service-cell framework - one channel end +
              one strand per client, so one service cell serves many clients
              concurrently), hostcfg + dhcp + zeroconf (link-local + mDNS) + ntp
              (N4c: host configuration - the store the stack reads for its
              address/netmask/gateway/resolvers/search domains, a DHCP client, RFC
              3927 link-local, mDNS over the dns codec unchanged, and an SNTP
              client whose answer is a bounded interval; docs/NETSTACK.md 20),
              and http1 + http2 (N5a: HTTP/1.1 - a zero-copy,
              smuggling-hardened codec + chunked framing + a transport-agnostic
              Client/Server; HTTP/2 - frames, streams, both flow-control levels,
              and HPACK with the RFC-generated Appendix B Huffman table - both in
              the always-compiled half, docs/NETSTACK.md 19). Two postures: `hosted` (default - the
              librheo-driven async endpoints) and the librheo-free **codec**
              posture (`--no-default-features`: eth/ip/arp/udp/tcp/cc/shard/wire
              only), which is what links beside a *kernel* for the N4b
              SocketOps bridge (docs/NETSTACK.md 18).
              Plus the netcore/netl4/netdns/nettrace/netlocal/
              nettcp/nettcpcc/netsmoltcp/netcrypto/nettls/nethttp/nethostcfg
              demo bins and
              netsvc-demo + netsvc-client (the N4a service + its clients)
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
