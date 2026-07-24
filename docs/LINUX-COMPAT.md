# Linux compatibility: the Linux personality

**Status:** Draft v0.1. Expands ARCHITECTURE.md 4.12, doctrine 10, and
POSIX-PERSONALITY.md; supersedes the "a Linux-syscall shim could be layered
on later" deferral in USERLAND.md decision 1. This document is the
**normative contract** for the personality: what runs, what does not, and
exactly how honest each syscall is.

## 1. Goal and placement

Run **unmodified Linux binaries** - unpatched Rust std programs built for
stock `*-unknown-linux-gnu` targets, glibc-linked C programs, and stock
Linux tools - as U-mode cells, without betraying the kernel's nature.

glibc is the supported libc (a deliberate decision: it is what distro
binaries actually link against, and its performance-optimized paths - ifunc
SIMD string routines, malloc arenas - are the ones users care about). musl
is out of scope.

**Where the translation lives.** In the full design this is a **personality
cell** reached over queue pairs (POSIX-PERSONALITY.md 1) - gVisor-style
translation in userspace. It is implemented *kernel-resident* here
(`kernel/src/linux/`), exactly like `svc.rs`: kernel-side handlers before
the service framework exists, running in trap context where user memory is
accessible. The bridge is architectural, not accidental:

- The personality adds **no kernel object**. PIDs, file descriptors, signal
  dispositions, the environment, futex waiter lists - all are per-cell
  synthesized state inside the module. The 10-object model and the negative
  constitution (ARCHITECTURE.md 5: fork, signals, and global namespaces are
  permanently outside the kernel *object model*) stay intact.
- Every underlying operation goes through the cell's existing grants: file
  I/O through the registered personality handler (`svc::FileOps`) backed by
  the `posix/` VFS, memory through the cell's own address space, entropy
  through the cell's DRBG. A Linux process cannot exceed its hosting cell's
  grants - "legacy software is contained exactly like everything else".
- The mechanisms the kernel proper *does* gain each pass the ARCHITECTURE.md
  6 admission rule on their own merits, independent of Linux: user
  thread-pointer save/restore (context-switch state), munmap/mprotect and
  frame reclaim (memory-grant mechanics), multi-context cells (the vcore
  model CONCURRENCY.md already specifies), U-mode FP/SIMD enablement, and
  tick-to-nanosecond conversion (the Timers arch area).
- Migration path: the dispatch seam is `linux::handle(nr, args)` called from
  the trap path. Moving the personality into a real cell later means moving
  that module behind a queue pair, submission-entry per syscall, without
  touching the Linux-facing semantics. The synchronous Linux edge then rides
  the async foundation explicitly (the vDSO-equivalent + batching noted in
  VIRTUALIZATION.md).

## 2. Dispatch and ABI

A cell carries a **personality tag** (`Native` or `Linux`), chosen when the
cell is installed. Dispatch branches on the tag **before** decoding the
syscall number: native numbers 1-30 (kernel/src/abi.rs) collide with Linux
numbers (Linux x86-64 `write` = 1 is native `SYS_DOORBELL`). A Linux cell
never reaches native dispatch and vice versa.

The syscall **register** conventions need no translation - the native ABI
already matches Linux on all three ISAs: number in `rax`/`x8`/`a7`,
arguments in `rdi,rsi,rdx,r10,r8,r9` / `x0-x5` / `a0-a5`, return in the
first argument register. Errors return as `-errno` (Linux userspace treats
-1..-4095 as error).

Syscall **numbers** are per-ISA constants in the arch layer
(`crate::arch::linux_abi::nr`), keeping the portability rule intact (zero
`cfg(target_arch)` outside `kernel/src/arch/`). There are two tables, not
three: x86-64 uses its legacy table (`arch/x86_64/linux_abi.rs`, from Linux
v6.6 `arch/x86/entry/syscalls/syscall_64.tbl`); aarch64 and riscv64 share
the asm-generic table (`arch/linux_abi_generic.rs`, from
`include/uapi/asm-generic/unistd.h`), included by both arch modules. The
same split applies to `struct stat` layouts later. Only implemented numbers
plus named-ENOSYS entries are listed - not all ~450.

## 3. The honesty policy (normative)

Every syscall the personality answers has one of four statuses:

- **full** - the Linux-observable semantics are provided.
- **partial** - works for the stated subset; the unsupported remainder
  fails loudly (an error return, never silent misbehavior). The subset is
  stated in the table.
- **recorded** - arguments are stored and success is returned, but the
  behavior is not yet enacted. Allowed ONLY where glibc startup requires
  success to proceed and the stored state is consumed by a later milestone.
  Each entry names the population of programs it can break.
- **ENOSYS** - not implemented; returns -ENOSYS. glibc/musl have documented
  fallbacks for the named entries (e.g. `clone3` -> `clone`, `rseq` ->
  unregistered, `statx` -> `newfstatat`).

Rules: success is returned only when the semantics are actually provided,
or the call is advisory by specification (`madvise`), or the entry is on
the recorded allowlist below. **Any syscall number not in the table logs
`linux: ENOSYS nr=<n>` to the serial console and returns -ENOSYS** - drift
between a new glibc and this table is always visible, never a silent hang.

### Syscall status table (grows per milestone)

Everything not listed logs `linux: ENOSYS nr=<n>` and returns -ENOSYS.

| Syscall | Status | Notes |
|---|---|---|
| read / write | full | over the per-cell fd table (console, VFS files, /dev/{null,zero,urandom}) |
| readv / writev | full | iterate the iovec array over read/write |
| openat | partial | dirfd is a C `int` (low 32 bits, sign-extended - AT_FDCWD arrives as `0xffffff9c`); AT_FDCWD honored, paths resolved by the VFS; a real (positive) dirfd → -ENOSYS (no suite util needs it). `/dev/{null,zero,urandom,random}` and `/proc/self/auxv` synthesized, else via `FileOps::open` |
| close | full | frees the slot; closes the VFS fd; reclaims a pipe when both ends close |
| lseek | full | VFS files; -ESPIPE on console/char devices/pipes |
| fstat / newfstatat | full | `struct stat` per ABI (x86-64 144 B / asm-generic 128 B); console & /dev/* report a character device; pipes S_IFIFO |
| statx | full | same fields as fstat, into the ABI-independent `struct statx`; Rust `std`'s `File::metadata` issues `statx` directly and does not fall back to `newfstatat`, so real tools require it |
| getdents64 | full | VFS directory (path stored per fd), packed as `linux_dirent64` and paged out via a per-fd cursor so a reader looping until 0 (real `ls`) terminates; directory must fit 4 KiB of records |
| getcwd | full | the per-cell cwd (default `/`) + NUL; -ERANGE if the buffer is too small |
| chdir | partial | stores the path as the cwd verbatim (absolute in practice); no existence check |
| pipe2 | partial | a single-process, non-blocking, bounded in-process pipe (4 × 8 KiB per cell); lets a tool create a pipe and fall back when `splice` is unavailable (uu_cat). Blocking / cross-context pipe semantics are L6 |
| splice | ENOSYS | uu_cat's Linux fast path probes `splice` and falls back to read/write on failure (documented fallback) |
| /proc/self/auxv | full | serves the cell's own auxv byte stream (a read-only synthetic fd); glibc/rustix read it when no PR_GET_AUXV is provided |
| dup / dup3 | partial | copies the slot; a duplicated VFS fd shares the underlying descriptor (close-once) |
| fcntl | partial | F_DUPFD/F_DUPFD_CLOEXEC/F_GETFD/F_SETFD/F_GETFL(→O_RDWR)/F_SETFL only |
| ioctl | partial | TIOCGWINSZ on a console fd → 80x24; every other request -ENOTTY |
| poll / ppoll | partial | non-blocking readiness only (never waits); answers glibc/Rust fd sanitization at startup |
| faccessat / faccessat2 | full | existence check via the VFS stat handler |
| readlinkat | partial | always -ENOENT (no symlinks in the VFS; /proc/self/exe is not read by the L3 suite - uu 0.0.29 gets argv[0] from `std::env::args`, not the auxv/execfn) |
| brk | full | heap from the loaded image end; grows/shrinks the cell's own pages |
| mmap | partial | anonymous MAP_PRIVATE only; fd-backed (L7) and MAP_FIXED → -ENOSYS |
| munmap / mprotect | full | leaf unmap+`frames::free` / leaf permission rewrite |
| madvise | full | advisory by specification: success, no action |
| exit / exit_group | full | ends the cell run with the code (exit_group == exit until threads, L4) |
| getpid / gettid | full | synthesized pid/tid 1000 |
| getppid | full | 0 (no parent) |
| getuid / geteuid / getgid / getegid | full | 1000 (no root, SECURITY-IDENTITY) |
| uname | full | sysname "Linux", release "6.6.0-rheo", machine per ISA |
| clock_gettime | partial | MONOTONIC via `arch::ticks_to_ns`; REALTIME = fixed epoch + monotonic (unsynced, disclosed) |
| clock_nanosleep / nanosleep | partial | returns immediately (0), no actual sleep |
| getrandom | full | fills from the cell's DRBG; flags ignored (never blocks) |
| sched_yield | full | no-op (single context per cell until L4) |
| prlimit64 / getrlimit | full | RLIMIT_STACK 1 MiB, RLIMIT_NOFILE 64, else unlimited; not settable |
| arch_prctl | full | x86-64 only: SET_FS/GET_FS program the FS_BASE MSR (L1) |
| set_tid_address | recorded | stores the clear-tid address, returns tid 1000; enacted with CHILD_CLEARTID at L4 |
| set_robust_list | recorded | stores the head, returns 0; futex robustness is L4 |
| rt_sigaction / rt_sigprocmask / sigaltstack | recorded | stored/ignored, returns 0; real signal delivery is L5 |
| mremap | full | shrink unmaps the tail in place; grow requires MREMAP_MAYMOVE (map a fresh region, copy, free the old); else -ENOMEM. glibc's large-block `realloc` needs it (the malloc-copy-free fallback otherwise leaks frames) |
| rseq / clone3 | ENOSYS | glibc has documented fallbacks (rseq→unregistered, clone3→clone) |
| clone / execve / fork / wait4 / futex / kill / tgkill / rt_sigreturn | ENOSYS | threads (L4), signals (L5), processes (L6). A util that hard-requires them (e.g. `sort`, which parallelizes with rayon and spawns worker threads) does not run until then |

### Planned identity/constants

- `uname` reports `sysname="Linux"`, `release="6.6.0-rheo"` (glibc refuses
  to start below its built-in minimum kernel version, so a Linux version
  string is mandatory; the "-rheo" suffix is the disclosure).
- `AT_HWCAP` advertises only CPU state the kernel has actually enabled for
  U-mode - glibc's ifunc resolvers select SIMD implementations from it, so
  over-advertising crashes string functions.
- Synthesized identity: pid 1000, uid/gid 1000, no root (SECURITY-IDENTITY:
  UID 0 does not exist; a Linux cell sees an unprivileged user).

## 4. Memory layout (Linux cells)

Same windows as native loaded cells (docs/USERLAND.md): image at 4 GiB
(`ET_EXEC` at its linked address; `ET_DYN` gets load bias 0x1_0000_0000),
stack top at 8 GiB (1 MiB mapped for Linux cells), anonymous mmap region at
12 GiB, `brk` heap starting at the image end. The initial stack carries the
System V block **plus the ELF auxiliary vector** (L1): AT_PHDR/PHENT/PHNUM,
AT_PAGESZ, AT_BASE, AT_FLAGS, AT_ENTRY, AT_UID/EUID/GID/EGID, AT_SECURE,
AT_RANDOM (16 DRBG bytes), AT_HWCAP, AT_CLKTCK, AT_PLATFORM, AT_EXECFN,
AT_NULL. No vDSO (`AT_SYSINFO_EHDR` absent - glibc falls back to real
syscalls).

## 5. Milestones

- **L0 [this doc's baseline]** - personality tag + dispatch branch, syscall
  number tables, write/exit/exit_group, the ENOSYS logger. Proof: a bare
  no_std program speaking the raw Linux ABI runs on all three ISAs
  (`linuxrun`).
- **L1** - ELF auxv, ET_DYN loading, user thread-pointer (fs_base /
  TPIDR_EL0 / tp) save+set, U-mode FP/SIMD enablement.
- **L2 [done]** - the core syscall set (table above); a per-cell fd table;
  real memory (`brk`, anonymous `mmap`/`munmap`/`mprotect` over the cell's own
  address space, via new `AddressSpace::unmap`/`protect` + per-ISA
  `paging_unmap_frame`/`paging_protect` and `frames::free`); per-ISA
  `ticks_to_ns` for `clock_gettime`. Proof (`linuxrun`): an **unpatched** Rust
  `std` hello (String/Vec/println!) and a static-glibc **C** hello, each built
  from source for the ISA's `*-unknown-linux-gnu` target, run on **all three
  ISAs** with exact stdout + exit code asserted. Accommodations, all disclosed:
  - **ET_EXEC, stock base, no relink.** A stock static-glibc binary links at a
    low VA (x86/arm 0x400000, riscv 0x10000). **All three kernels are now
    higher-half** (docs/MEMORY.md): the kernel lives in the high canonical half
    and a cell root leaves the entire low half free, so every fixture keeps
    glibc's **stock base, unmodified, no relink** - which is exactly what
    `linuxrun` proves (x86_64 rusthello at ~0x400000, aarch64 rusthello at
    ~0x400000, riscv64 rusthello at 0x14914 / chello at 0x10494, all bias 0).
    Static-PIE (ET_DYN, the documented alternative) is *not* used because the
    riscv64 glibc dev package ships no `rcrt1.o`; the loader's ET_DYN path
    (bias 0x1_0000_0000) remains for L7 dynamic linking.
  - **uname release "6.6.0-rheo"**, machine per ISA (x86_64/aarch64/riscv64).
  - **RLIMIT_STACK 1 MiB** (matches the mapped Linux stack); RLIMIT_NOFILE 64.
  - **clock_gettime** is monotonic but coarse: x86 TSC via CPUID 0x16 (else a
    1 GHz assumption), arm CNTFRQ_EL0, riscv a documented 10 MHz timebase
    (`cycle` vs `time` CSR mismatch noted); REALTIME adds a fixed boot epoch.
  - Static-glibc's NSS/getaddrinfo warnings are irrelevant to these fixtures.
- **L3 [done]** - per-cell cwd (`getcwd`/`chdir`), `statx`, `mremap`, a
  single-process `pipe2`, `/proc/self/auxv`, and the `openat` dirfd-as-`int`
  fix. Proof (`linuxtools`): the **unpatched upstream uutils/coreutils**
  multicall binary - built from crates.io (**`coreutils` = 0.0.29**, pinned),
  static-glibc ET_EXEC, stock base, no relink - runs on **all three ISAs** with
  exact stdout + exit asserted for **true, false, echo, cat, seq, head, wc,
  basename, dirname, ls, pwd** (11 utilities). The ENOSYS-driven loop drove the
  exact additions: `statx` (Rust `std` issues it directly, no fallback),
  `mremap` (glibc `realloc`), `pipe2`+`splice`-ENOSYS (uu_cat's Linux fast path
  creates a pipe then falls back to read/write), a per-fd `getdents64` cursor
  (real `ls` loops until it reads 0), and dirfd sign-extension (AT_FDCWD arrives
  as the 32-bit `0xffffff9c`). Accommodations, all disclosed:
  - **Fixture version: `coreutils` 0.0.29** (not the current 0.9.x). 0.9.x's
    multicall dispatch reads the binary name from **AT_EXECFN via
    `rustix::param::linux_execfn`**, and uutils/uucore enable rustix's
    `use-libc-auxv`, so that resolves through glibc `getauxval` via rustix's
    `weak!`/`dlsym` shim - which returns NULL in a **fully static** binary (no
    dynamic symbol table). The multicall then cannot learn its own name and
    prints usage instead of dispatching - a static-link limitation that hits
    real Linux too, not a rheo gap. 0.0.29's dispatch takes argv[0] straight
    from `std::env::args`, which the kernel supplies, so it dispatches
    correctly. (When L7 lands dynamic linking, a 0.9.x fixture can be revisited.)
  - **`sort` is dropped** from the asserted set: uu_sort parallelizes with
    rayon and unconditionally spawns worker threads (clone/futex) - L4. It is
    dropped, not faked.
  - **`cat` uses `splice`** on Linux (its fast path): `pipe2` is answered with a
    real bounded in-process pipe, `splice` returns -ENOSYS, and uu_cat falls
    back to read/write. Correct output, no lying stub.
  - **`--locked` is not used** when building the fixture (0.0.29's bundled
    Cargo.lock pins an ancient rustix that no longer builds on current nightly);
    the fixture *crate* is still version-pinned.
- **L4** - threads: multi-context cells, clone/futex, cooperative
  scheduling at syscall boundaries (a spinning thread starves its siblings
  until timer preemption lands - accepted, documented).
- **L5** - signals: delivery by trap-frame rewrite + restorer trampoline;
  faults become SIGSEGV-to-handler.
- **L6** - processes: fork (clone-cell-within-capability-bundle, eager
  copy), execve, wait4, pipes, a static-glibc shell. The
  POSIX-PERSONALITY.md P11 gate (>= 80% of the defined coreutils suite) is
  measured here.
- **L7** - dynamic linking: PT_INTERP -> ld-linux, /lib on the ext4 image,
  fd-backed private mmap. Proof: a dynamically-linked glibc hello.

## 6. Fixture build matrix (reproducibility)

All Linux test binaries are built **from source** by xtask/CI - no binaries
in git:

All fixtures are static-glibc **ET_EXEC** (`-C target-feature=+crt-static -C
relocation-model=static -no-pie`; the cross gcc is the linker so the right
glibc sysroot/crt objects are used). **All three kernels are higher-half, so
every fixture keeps glibc's stock base, no relink** (docs/MEMORY.md). Built by
xtask `build_linux_fixtures` (L2, above).

| ISA | Rust std (unpatched) | C (glibc) | link base |
|---|---|---|---|
| x86_64 | `x86_64-unknown-linux-gnu` | host `gcc` | 0x400000 (stock, higher-half) |
| aarch64 | `aarch64-unknown-linux-gnu` (linker: aarch64-linux-gnu-gcc) | `aarch64-linux-gnu-gcc` | 0x400000 (stock, higher-half) |
| riscv64 | `riscv64gc-unknown-linux-gnu` (linker: riscv64-linux-gnu-gcc) | `riscv64-linux-gnu-gcc` | 0x10000 (stock, higher-half) |

All three cross toolchains and `*-unknown-linux-gnu` rustup targets are
present in the build/CI environment, so **riscv64 genuinely passes** (no
skip); the bare Linux-ABI fixtures (L0) remain the coverage floor if a future
environment lacks a cross gcc.

**Third-party fixture crate (L3)** - the no-deps rule's "a doc must name the
crate": **`coreutils` = 0.0.29** (uutils/coreutils, crates.io), the upstream
multicall binary, built **unmodified from source** by xtask
`build_coreutils_fixture` via `cargo install coreutils --version =0.0.29`
(features `true,false,echo,cat,wc,head,seq,ls,sort,basename,dirname,pwd`;
`RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=static -C
linker=<cross-gcc> -C link-arg=-no-pie"`; not `--locked` - see L3 above). It
is a test workload only, never linked into the kernel or tools; the built
binary is `include_bytes!`d by the `linuxtools` test and its build dir is
gitignored (no binaries in git). If the crates.io fetch is unavailable the
fixture build fails loudly (the L0-L2 fixtures keep the personality covered).

| ISA | fixture target | link base |
|---|---|---|
| x86_64 | `x86_64-unknown-linux-gnu` (linker: `gcc`) | 0x400000 (stock, higher-half) |
| aarch64 | `aarch64-unknown-linux-gnu` (linker: `aarch64-linux-gnu-gcc`) | 0x400000 (stock, higher-half) |
| riscv64 | `riscv64gc-unknown-linux-gnu` (linker: `riscv64-linux-gnu-gcc`) | 0x10000 (stock, higher-half) |

## 7. Non-goals

- musl support (user decision - performance).
- Bit-exact /proc, real-time signal ordering corners, namespaces,
  cgroups, netlink, io_uring: not planned; the personality is a bridge,
  not a destination (POSIX-PERSONALITY.md 6).
- No ambient authority ever: a Linux cell's "root filesystem" is its
  per-cell VFS view; there is no global mount table, no UID 0.
