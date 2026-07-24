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

| Syscall | Status | Notes |
|---|---|---|
| write | partial | fds 1/2 to the console; other fds -EBADF until the L2 fd table |
| exit | full | ends the cell run with the code |
| exit_group | full | same as exit until threads (L4) |
| everything else | ENOSYS | logged to serial |

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
- **L2** - the core syscall set; unified fd table. Proof: **unpatched** Rust
  std hello (stock `*-unknown-linux-gnu`, `+crt-static`) and glibc-static C
  hello run.
- **L3** - cwd/openat/dirfd, /dev + /proc/self synthesis. Proof: the
  **unpatched uutils coreutils** multicall binary (crates.io, pinned,
  static-glibc) runs.
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

| ISA | Rust std (unpatched) | C (glibc) |
|---|---|---|
| x86_64 | `x86_64-unknown-linux-gnu` + `-C target-feature=+crt-static` | host `gcc -static` |
| aarch64 | `aarch64-unknown-linux-gnu` + crt-static (linker: aarch64-linux-gnu-gcc) | `aarch64-linux-gnu-gcc -static` |
| riscv64 | `riscv64gc-unknown-linux-gnu` + crt-static (needs riscv cross gcc) | `riscv64-linux-gnu-gcc -static` |

riscv64's cross toolchain is acquired via the distro package
(`gcc-riscv64-linux-gnu`); if an environment cannot install it, the
libc-based fixtures **skip with a stated reason** on riscv64 and the bare
Linux-ABI fixtures (L0) remain the riscv64 coverage floor. Named
third-party fixture crates (the no-deps rule's "a doc must name the
crate"): **uutils/coreutils** (crates.io, version pinned at L3), used
unmodified as a test workload, never linked into the kernel or tools.

## 7. Non-goals

- musl support (user decision - performance).
- Bit-exact /proc, real-time signal ordering corners, namespaces,
  cgroups, netlink, io_uring: not planned; the personality is a bridge,
  not a destination (POSIX-PERSONALITY.md 6).
- No ambient authority ever: a Linux cell's "root filesystem" is its
  per-cell VFS view; there is no global mount table, no UID 0.
