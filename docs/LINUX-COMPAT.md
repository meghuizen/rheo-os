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
| pread64 | partial | positioned read of a VFS file (lseek+read); ld.so reads ELF headers with it (L7). Non-VFS fds → -EBADF |
| readv / writev | full | iterate the iovec array over read/write |
| openat | partial | dirfd is a C `int` (low 32 bits, sign-extended - AT_FDCWD arrives as `0xffffff9c`); AT_FDCWD honored, paths resolved by the VFS; a real (positive) dirfd → -ENOSYS (no suite util needs it). `/dev/{null,zero,urandom,random}` and `/proc/self/auxv` synthesized, else via `FileOps::open` |
| close | full | frees the slot; closes the VFS fd; reclaims a pipe when both ends close |
| lseek | full | VFS files; -ESPIPE on console/char devices/pipes |
| fstat / newfstatat | full | `struct stat` per ABI (x86-64 144 B / asm-generic 128 B); console & /dev/* report a character device; pipes S_IFIFO |
| statx | full | same fields as fstat, into the ABI-independent `struct statx`; Rust `std`'s `File::metadata` issues `statx` directly and does not fall back to `newfstatat`, so real tools require it |
| getdents64 | full | VFS directory (path stored per fd), packed as `linux_dirent64` and paged out via a per-fd cursor so a reader looping until 0 (real `ls`) terminates; directory must fit 4 KiB of records |
| getcwd | full | the per-cell cwd (default `/`) + NUL; -ERANGE if the buffer is too small |
| chdir | partial | stores the path as the cwd verbatim (absolute in practice); no existence check |
| pipe2 / pipe | full | a **cross-cell** bounded ring buffer (16 global pipes × 64 KiB, `kernel/src/linux/pipe.rs`), the two ends held by different cells after `fork` (L6). Read/write block **cooperatively** with cross-cell wake at syscall boundaries; EOF when all write ends close, SIGPIPE/-EPIPE when all read ends close. `dup`/`fork` refcount the ends. Single-process use (both ends in one cell, no peer to run) falls back to non-blocking -EAGAIN, keeping uu_cat's `splice`-fallback path (L3) working. `pipe` (x86-64 legacy) == `pipe2(...,0)` |
| splice | ENOSYS | uu_cat's Linux fast path probes `splice` and falls back to read/write on failure (documented fallback) |
| /proc/self/auxv | full | serves the cell's own auxv byte stream (a read-only synthetic fd); glibc/rustix read it when no PR_GET_AUXV is provided |
| dup / dup3 | partial | copies the slot; a duplicated VFS fd shares the underlying descriptor (close-once) |
| fcntl | partial | F_DUPFD/F_DUPFD_CLOEXEC/F_GETFD/F_SETFD/F_GETFL(→O_RDWR)/F_SETFL only |
| ioctl | partial | TIOCGWINSZ on a console fd → 80x24; every other request -ENOTTY |
| poll / ppoll | partial | non-blocking readiness only (never waits); answers glibc/Rust fd sanitization at startup |
| faccessat / faccessat2 | full | existence check via the VFS stat handler |
| access | full | x86-64 legacy; path in arg0, no dirfd. ld.so probes /etc/ld.so.preload etc. (L7); existence check via the VFS |
| readlinkat | partial | always -ENOENT (no symlinks in the VFS; /proc/self/exe is not read by the L3 suite - uu 0.0.29 gets argv[0] from `std::env::args`, not the auxv/execfn) |
| brk | full | heap from the loaded image end; grows/shrinks the cell's own pages |
| mmap | partial | anonymous MAP_PRIVATE **and file-backed MAP_PRIVATE** (L7: read `[offset, offset+len)` from a VFS fd into fresh frames, partial last page zero-filled); **MAP_FIXED** places at the caller's addr, replacing existing pages (ld.so reserves a library's span then overlays each segment). MAP_SHARED of a file is not modeled (ld.so uses PRIVATE). Anonymous mappings are always zeroed - a MAP_FIXED anon overlay of a file-backed reservation discards the file bytes (the library bss). A PROT_NONE mapping only **reserves** address space (no frames); accessible mappings are committed eagerly (demand-commit, L4) |
| munmap / mprotect | full | leaf unmap+`frames::free` / leaf permission rewrite. `mprotect` making a reserved range accessible **commits** fresh frames (glibc grows a PROT_NONE-reserved arena/stack this way, L4); PROT_NONE decommits |
| madvise | full | advisory by specification: success, no action |
| exit | full | ends the calling **thread** (L4): CHILD_CLEARTID clear+futex-wake, then switch to the next ready context; ends the cell only if it was the last |
| exit_group | full | ends the whole process (all contexts). The top cell unwinds the run; a forked child becomes a zombie (its frames reclaimed) until the parent `wait4`s it (L6) |
| getpid | full | synthesized pid/tgid: 1000 for the top process, 1001+ per forked child (L6) |
| gettid | full | per-context tid (L4): the main thread is 1000, clone children 1001+ |
| clone | partial | the pthread-create flag set (CLONE_VM/FS/FILES/SIGHAND/THREAD/SETTLS/PARENT_SETTID/CHILD_CLEARTID): a new context in the same address space with its own stack/TLS, returns 0 in the child / tid in the parent (L4). Not `fork` (L6); >`MAX_CONTEXTS` (8) per cell → -EAGAIN. Arg order is arch ABI (`CLONE_BACKWARDS` on ARM64/RISC-V) |
| futex | partial | FUTEX_WAIT/WAKE (+ WAIT_BITSET/WAKE_BITSET as plain WAIT/WAKE; PRIVATE ignored); WAIT re-checks the word and parks the caller, WAKE moves up to `val` waiters to ready (L4). Any timeout treated as infinite; **priority inheritance is a documented TODO** (FIFO wake; no RT-reservation mutexes in the suite) |
| getppid | full | the parent's pid (0 for the top of the process tree) |
| getuid / geteuid / getgid / getegid | full | 1000 (no root, SECURITY-IDENTITY) |
| uname | full | sysname "Linux", release "6.6.0-rheo", machine per ISA |
| clock_gettime | partial | MONOTONIC via `arch::ticks_to_ns`; REALTIME = fixed epoch + monotonic (unsynced, disclosed) |
| clock_nanosleep / nanosleep | partial | returns immediately (0), no actual sleep |
| getrandom | full | fills from the cell's DRBG; flags ignored (never blocks) |
| sched_yield | full | switches to the next ready context (L4); returns 0 (no-op if it is the only runnable context) |
| sched_getaffinity | partial | reports a single online CPU (bit 0) so `available_parallelism` reads 1 and thread pools (rayon) stay small/deterministic (L4) |
| prlimit64 / getrlimit | full | RLIMIT_STACK 1 MiB, RLIMIT_NOFILE 64, else unlimited; not settable |
| arch_prctl | full | x86-64 only: SET_FS/GET_FS program the FS_BASE MSR (L1); the base is recorded per context and reloaded on a context switch (L4) |
| prctl | partial | PR_SET_NAME/PR_GET_NAME accepted as a cosmetic no-op (rayon names its workers and treats failure as fatal, L4); every other option -ENOSYS |
| set_tid_address | full | records the calling context's clear-tid address, returns its tid (L4; enacted by CHILD_CLEARTID on thread exit) |
| set_robust_list | recorded | stores the head, returns 0; robust-futex unwinding on abnormal thread exit is not enacted (no suite util depends on it) |
| rt_sigaction | full | per-cell disposition table for signals 1..64; SIG_DFL/SIG_IGN honored; stores/returns the old action; SIGKILL/SIGSTOP -EINVAL. The kernel `struct sigaction` layout is ISA-specific (x86-64 carries `sa_restorer`, asm-generic does not: `arch::SIGACTION_HAS_RESTORER`) (L5) |
| rt_sigprocmask | full | per-context blocked mask (BLOCK/UNBLOCK/SETMASK); SIGKILL/SIGSTOP never blockable; a now-unblocked pending signal is delivered before return (L5) |
| sigaltstack | full | per-context alternate signal stack; honored for SA_ONSTACK handlers; -EPERM while executing on it (L5) |
| rt_sigreturn | full | restores the interrupted `TrapFrame` + signal mask from the signal frame on the user stack; resumes where the signal interrupted (L5) |
| kill / tgkill / tkill / rt_sigqueueinfo | partial | self-targeting only (own pid 1000 / own tids): `raise`/`abort` paths. Delivery is by trap-frame rewrite; a signal to a *non-running* sibling context is recorded pending (not force-delivered) - no L5 fixture needs it, documented. Non-self pid/tgid -ESRCH (L5) |
| rt_sigtimedwait | partial | never blocks; returns -EAGAIN so callers loop/bail rather than hang (no fixture waits in it) (L5) |
| mremap | full | shrink unmaps the tail in place; grow requires MREMAP_MAYMOVE (map a fresh region, copy, free the old); else -ENOMEM. glibc's large-block `realloc` needs it (the malloc-copy-free fallback otherwise leaks frames) |
| rseq / clone3 | ENOSYS | glibc has documented fallbacks (rseq→unregistered, clone3→clone); verified via the ENOSYS logger that glibc/rust fall back to `clone`, so clone3 stays ENOSYS |
| fork / vfork | full | clone-cell-within-capability-bundle (docs/POSIX-PERSONALITY.md 2): a new `user` cell in the parent's bundle, the parent's committed pages **eager-copied** (COW deferred + documented), `LinuxState`/fd table/cwd/signal dispositions deep-copied, a child pid synthesized; child returns 0, parent returns the pid. Only the calling thread is duplicated (POSIX). Reached via `clone` without `CLONE_VM` on every ISA (glibc's `fork`), or the x86-64 `fork`/`vfork` numbers. Over the `MAX_CELLS` (16) cap → -EAGAIN. `vfork` is treated as `fork` (eager copy, no COW share - safe, just less lazy) (`kernel/src/linux/proc.rs`) |
| execve | full | replaces the calling cell's image with one **streamed** from the VFS (`load::exec_elf_from_vfs`: only the ELF header + phdrs are buffered; each `PT_LOAD` segment is read page-by-page straight into its destination frame, so the kernel holds no whole-image buffer). Keeps the same cell/pid, fd table (close-on-exec is not tracked - documented), and cwd; resets signal handlers to default and starts single-threaded. ET_EXEC + static-PIE ET_DYN, stock base |
| wait4 / waitpid | full | the parent blocks cooperatively (switching to a runnable child) until a child exits, then reaps: WIFEXITED/WEXITSTATUS for a normal exit, WIFSIGNALED for a signal-killed child; frees the child cell + its frames. `pid > 0` waits for that child, `pid <= 0` for any; WNOHANG honored; -ECHILD with no children. SIGCHLD is not queued to a handler (the parent reaps directly; documented) |
| dup2 | full | (x86-64 legacy) == `dup3(old, new, 0)`; a pipe end is refcounted |
| setpgid / setsid | recorded | returns 0 (single-session model, no job control); the shell queries process groups but does not depend on the effect |
| getpgid / getsid | partial | returns the caller's pid (one group/session per process) |

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
    correctly. (L7 has since landed **dynamic** linking: a dynamically-linked
    0.9.x multicall would have a dynamic symbol table, so glibc `getauxval` /
    rustix `linux_execfn` resolves AT_EXECFN and the multicall dispatches by
    name - a 0.9.x dynamic fixture is now feasible as future work; the pinned
    0.0.29 static fixture remains the L3 proof.)
  - **`sort` was dropped** at L3 (uu_sort parallelizes with rayon and spawns
    worker threads via clone/futex) and is **re-enabled at L4** - it now runs
    and its exact sorted output is asserted, proving a real threaded upstream
    coreutil works on the multi-context cell.
  - **`cat` uses `splice`** on Linux (its fast path): `pipe2` is answered with a
    real bounded in-process pipe, `splice` returns -ENOSYS, and uu_cat falls
    back to read/write. Correct output, no lying stub.
  - **`--locked` is not used** when building the fixture (0.0.29's bundled
    Cargo.lock pins an ancient rustix that no longer builds on current nightly);
    the fixture *crate* is still version-pinned.
- **L4 (done)** - threads: **multi-context cells** (the CONCURRENCY.md vcore
  model made real for a Linux cell). A cell holds up to `MAX_CONTEXTS` = **8**
  execution contexts (a `TrapFrame` + run state + FP save area each), scheduled
  **cooperatively, round-robin, at syscall boundaries** on the single CPU. All
  contexts share one address space and one kernel stack (cheap switch, no page-
  table reload); FP/SIMD state is saved/restored eagerly per switch and the
  per-thread TLS base is reloaded (x86-64 FS_BASE; ARM64 TPIDR_EL0 / RISC-V `tp`
  ride in the frame). Added: `clone` (pthread flag set, new context with its own
  stack/TLS, arch-specific arg order via `CLONE_BACKWARDS`), `futex`
  (WAIT/WAKE + the _BITSET variants), per-context `gettid`, thread `exit` vs
  `exit_group`, CHILD_CLEARTID clear+wake on thread exit, `set_tid_address`,
  real `sched_yield`, `sched_getaffinity`/`prctl`(name) for rayon. Memory gained
  **demand-commit** (PROT_NONE `mmap` reserves without frames; `mprotect`
  commits) so glibc's per-thread 64 MiB PROT_NONE arenas don't exhaust the frame
  pool. PIDs/TIDs/futex waiter lists stay per-cell synthesized state - no kernel
  object (`kernel/src/linux/thread.rs`). Proof: an unpatched multi-threaded Rust
  `std` binary (`std::thread` ×4 + `mpsc` + `Mutex` + `Arc<AtomicUsize>` + join)
  runs on **all three ISAs** with exact stdout/exit asserted (`linuxthreads`
  test), and `sort` is re-enabled in `linuxtools`.
  - **Cooperative, no preemption (accepted, documented)**: a compute-bound
    thread that never issues a syscall starves its siblings. The fix is timer
    preemption (task #27); L4 is correct for syscall-driven workloads (glibc
    mutexes/condvars/channels block via futex).
  - **Priority inheritance is a TODO**: futex wake is plain FIFO. CONCURRENCY.md
    mandates PI for RT-reservation mutexes; no reservation-holding threads exist
    in the suite, so this is deferred with a code TODO.
  - **`clone3` stays ENOSYS**: verified via the logger that glibc/rust fall back
    to `clone`.
  - **Frame pool** stays 32768 frames (128 MiB); demand-commit keeps N thread
    stacks + arenas within it (`linuxthreads` runs 5 contexts comfortably).
- **L5 [done]** - signals: **synthesized POSIX signal delivery, no kernel
  object** (docs/POSIX-PERSONALITY.md: signals are event delivery, not a
  primitive). Dispositions are a per-cell table (SIG_DFL/SIG_IGN honored);
  masks/pending/altstack are per-context (`kernel/src/linux/signal.rs`).
  Delivery is a **saved-`TrapFrame` rewrite** in trap context: a Linux
  `rt_sigframe` (siginfo + ucontext with the interrupted GPRs/PC/SP + the saved
  mask) is built on the user stack (or the sigaltstack for SA_ONSTACK), the
  frame's PC is pointed at the handler with arg0=signo (+ siginfo\*/ucontext\*),
  and its return address at the **restorer**: glibc's `SA_RESTORER` on x86-64,
  or an **injected 2-instruction `rt_sigreturn` trampoline page** (mapped at
  `arch::SIGTRAMP_VA` by `linux::stack::setup_stack`) on ARM64/RISC-V, which
  have no SA_RESTORER path. `rt_sigreturn` restores the frame + mask.
  **Synchronous faults become signals**: `user::on_user_trap` maps the per-ISA
  fault cause (vector / ESR EC / scause -> `arch::FaultCause`) to
  SIGSEGV/SIGBUS/SIGILL/SIGFPE and, for a Linux cell with an installed,
  unblocked handler, delivers it by frame rewrite (`siginfo.si_addr` = the fault
  address); otherwise (SIG_DFL, SIG_IGN, or the signal blocked) the cell
  terminates reporting 128+signo. **A native cell fault stays terminal**
  (`Outcome::Faulted`) - delivery is behind the Linux branch only. `kill`/
  `tgkill`/`tkill`/`raise` deliver to self; the x86-64 ring-3 fault path in
  `vectors.S` now captures the full `TrapFrame` and resumes via `sysret`
  (ARM64/RISC-V already carried the frame through the fault path). Proof
  (`linuxsig`, all three ISAs, exact stdout + exit): `sig_raise` (async
  `sigaction`+`raise(SIGUSR1)` -> handler -> `rt_sigreturn` resume, exit 0),
  `sig_segv` (`sigaction(SIGSEGV)` + a null write -> handler `_exit(0)` instead
  of a killed cell), `sig_dfl` (`raise(SIGABRT)`, no handler -> terminate 134).
  - **Scope**: delivery is to self / synchronous faults. Signal targeting of a
    *non-running* sibling context is recorded pending but not force-delivered
    (no L5 fixture needs it; full cross-thread delivery is future work).
  - **FP/SIMD across a handler is not saved/restored** (the ucontext carries
    GPRs/PC/SP + mask, not the vector state); L5 fixtures do not rely on FP
    liveness across a handler. Documented; eager per-signal FP save is future.
- **L6 [done]** - processes: **fork / execve / wait4 / cross-cell pipes**, plus
  a shell running the P11 coreutils suite. All are per-cell synthesized state -
  **no new kernel object** (docs/LINUX-COMPAT.md 1). The kernel mechanisms added
  each pass the ARCHITECTURE.md 6 admission rule on their own merits, like L4's
  multi-context cells: a page-table user-leaf walk (`arch::paging_for_each_user_leaf`)
  behind `AddressSpace::fork_from`/`free_user_frames` (memory-grant mechanics -
  eager copy + reclaim), a **generalized run loop** where a cell blocking
  (`wait4`, an empty/full pipe) or exiting hands the CPU to the next runnable
  cell via the same address-space switch the native `SYS_SWITCH` uses (the
  native path is byte-for-byte unchanged - `crate::linux::proc` drives the
  cross-cell switch behind the `Personality::Linux` branch), and a streaming
  `execve` loader (`load::exec_elf_from_vfs`). `MAX_CELLS` 8 → **16** (a shell
  plus concurrent pipeline stages); the frame pool stays 32768 (fork eager-copies
  and `wait4`/`exit`/`execve` reclaim leaf frames, so a suite of pipelines stays
  bounded - intermediate page-table frames leak a small, documented amount per
  dead process).
  - Proof (`linuxproc`, all three ISAs, exact stdout + exit): **(A)** a direct
    multi-process static-glibc C fixture (`procdemo`) - `pipe2` + `fork`; the
    child `dup2`s the pipe write end to stdout and `execve`s a second
    static-glibc binary (`/bin/cecho`, served from the VFS); the parent drains
    the pipe, `wait4`s, and prints a deterministic transcript, exiting with a
    code derived from the child status. This proves fork + execve + wait4 + pipe
    end-to-end. **(B)** the **P11 gate**: `rsh`, a minimal from-scratch
    static-glibc shell (a full dash/busybox cross-build was out of budget; `rsh`
    is honest - it exercises exactly the L6 primitives: `fork`/`execve`/`wait4`/
    `pipe2`/`dup2` with pipelines and `&&`/`||`), forks + execs the **unpatched
    upstream uutils/coreutils** multicall (L3's `coreutils` 0.0.29, from the VFS)
    to run the suite below.
  - **P11 coreutils suite** (`rsh -c "<cmdline>"`, exact stdout + exit each),
    measured **12/12 = 100 %** on x86_64, aarch64, riscv64 (gate >= 80 %; kill
    threshold < 60 %): `seq 1 5 | wc -l` (5), `echo hi | cat` (hi),
    `true && echo ok` (ok), `false || echo rescued` (rescued),
    `true && echo ok || echo no` (ok), `false && echo no || echo yes` (yes),
    `basename /a/b/c.txt` (c.txt), `dirname /a/b/c.txt` (/a/b),
    `seq 1 4 | head -n2` (1,2), `echo one two three | wc -w` (3), `pwd` (/),
    `echo hello | wc -c` (6). Each pipeline stage `seq ...` runs as
    `coreutils seq ...` (the multicall dispatches on the argument), so the
    suite is the real Linux Rust coreutils driven by a shell over the L6
    process primitives.
  - Accommodations, all disclosed: **eager copy, COW deferred** - `fork`
    copies every committed page (including the 1 MiB Linux stack); `execve`
    frees it immediately, so the transient cost is bounded. **Cross-thread
    SIGCHLD is not queued** - the parent reaps via `wait4` directly (no L6
    fixture installs a SIGCHLD handler). **Close-on-exec is not tracked** -
    `execve` keeps all fds open; a pipeline child closes its unused ends
    explicitly (the shell does). **Cooperative, single CPU** (inherited from
    L4): a compute-bound process starves peers until it hits a syscall
    boundary - correct for the syscall-driven suite.
- **L7 [done]** - dynamic linking: running an **unmodified, dynamically-linked
  glibc binary**. This closes "unmodified Linux binaries run" for the common
  dynamic case. Three pieces, all per-cell / loader mechanics - **no new kernel
  object**:
  - **PT_INTERP + dual-load** (`kernel/src/{elf,load}.rs`): `load_elf_linux`
    parses the `PT_INTERP` path (`Elf::interp`), and when present loads BOTH the
    main program (ET_DYN, bias `LINUX_DYN_BASE` 4 GiB) AND the interpreter
    `ld-linux-*.so` (ET_DYN, bias `LINUX_INTERP_BASE` 64 GiB, well clear of the
    image/stack/mmap region), streaming the interpreter from the VFS exactly as
    the program's own file I/O resolves it. Execution starts in ld.so; the auxv
    (`linux/stack.rs`) carries `AT_BASE` = the interpreter's load bias,
    `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` = the **main program's**, and `AT_ENTRY` =
    the **main program's** entry (a new `LinuxImage::at_entry`). **No kernel
    relocation processing** - ld.so self-relocates, then relocates the program +
    libc, including initial-exec TLS and IRELATIVE/ifunc relocations, which run
    to completion on all three ISAs (no `__tls_get_addr` general-dynamic path is
    exercised by the hello).
  - **fd-backed `mmap`** (`kernel/src/linux/mem.rs`): file-backed MAP_PRIVATE
    (read the file range into fresh frames, partial last page zero-filled) plus
    **MAP_FIXED** (place at the caller's addr, freeing+replacing existing pages).
    ld.so does exactly this: `mmap(NULL, span, PROT_READ, MAP_PRIVATE, fd, 0)`
    to reserve a library's whole span, then MAP_FIXED-overlays each segment
    (text r-x at its file offset, data rw) and an **anonymous** MAP_FIXED for the
    bss. The bss overlay must yield **zeroed** frames - discarding the file bytes
    the reservation mapped there - or libc's bss (its stdio/malloc locks) keeps
    file garbage and self-deadlocks; `mem::mmap`'s anon path frees the existing
    frames and maps fresh zeroed ones (distinct from `mprotect`'s content-
    preserving demand-commit). Added `pread64` (ld.so reads ELF headers with it)
    and x86-64 legacy `access` (ld.so's /etc/ld.so.preload probe).
  - **/lib populated with the real per-ISA glibc**: the `linuxdyn` test seeds a
    ramfs with the toolchain's actual `ld-linux-*.so` (at its `PT_INTERP` path)
    and `libc.so.6` (`/lib`, found via `LD_LIBRARY_PATH=/lib` in envp). The `.so`
    blobs are copied from the cross toolchains at build time by xtask
    `build_dyn_fixture` (x86-64 from the host `/lib/x86_64-linux-gnu`, aarch64
    from `/usr/aarch64-linux-gnu/lib`, riscv64 from `/usr/riscv64-linux-gnu/lib`)
    and are **never committed** (the fixture build dir is gitignored). If a
    runtime `.so` cannot be located for an ISA, `build_dyn_fixture` writes a
    1-byte placeholder and `linuxdyn` **skips-with-reason** for that ISA (the
    static L2-L6 coverage stays the floor); all three toolchains are present in
    the build/CI environment here, so **all three ISAs genuinely pass**.
  - Proof (`linuxdyn`, all three ISAs, exact stdout + exit): a stock
    **dynamically-linked (non-static) glibc C hello** (`dhello`, ET_DYN/PIE,
    built with the ISA's gcc and NO `-static`/`-no-pie` - the default), loaded
    with `/lib` seeded from the toolchain, prints `hello from dynamic glibc` and
    exits 12. The syscall log surfaces the real dynamic-startup sequence (brk,
    access, openat/pread64/fstat on /lib, mmap fd-backed + MAP_FIXED, mprotect
    for RELRO, arch_prctl/set_tid_address/set_robust_list, rseq→ENOSYS,
    prlimit64) then main's write + exit_group.
  - Accommodations, disclosed: **`execve` of a dynamic binary is not wired** -
    the streaming `execve` path stays static/static-PIE only; the `linuxdyn`
    proof loads the dynamic binary directly. A dynamic **Rust** `std` hello is
    not built (it additionally needs `libgcc_s.so.1`/`libm.so.6` seeded); the C
    hello is the L7 proof. **MAP_SHARED of a file** stays unmodeled (ld.so uses
    PRIVATE).

- **L8 [done]** - **AF_UNIX (Unix domain) sockets** - the first slice of the
  socket surface (docs/NETSTACK.md rheo-net Phase N1d). Like every prior
  milestone this adds **no kernel object**: sockets are per-cell fds
  (`kernel/src/linux/fd.rs`) and the byte transport reuses the L6 cross-cell
  ring buffer (`kernel/src/linux/pipe.rs`) - a SOCK_STREAM connection is **two
  rings, one per direction**, exactly the shape a bidirectional pipe-pair has.
  The only new global state is a **name registry + per-listener accept queue**
  (`kernel/src/linux/unixsock.rs`), per-personality synthesized state just like
  the pipe table, so cross-cell block/wake reuse the L6 pipe scheduler
  unchanged. The socket syscall numbers are wired into all three
  `arch/*/linux_abi` tables (x86-64 legacy `socket`=41.. / asm-generic
  `socket`=198..; per-ISA ABI, allowed in the arch layer).
  - **Syscalls**: `socket`, `socketpair`, `bind`, `listen`, `accept`/`accept4`,
    `connect`, `getsockname`/`getpeername`, `sendto`/`recvfrom` (routed to the
    blocking write/read path), `sendmsg`/`recvmsg` (iovec gather/scatter),
    `setsockopt`/`getsockopt` (accept + ignore / zeroed), `shutdown` (no-op).
    `read`/`write` on a connected socket fd go through the same cross-cell block
    + SIGPIPE path as pipes. **Abstract-namespace** names (`\0`-prefixed
    `sun_path`) are supported (keyed verbatim); pathname names key on the
    `sun_path` up to its first NUL.
  - **Proof (`linuxunix`, all three ISAs, exact stdout + exit)**: an unmodified
    static-glibc C fixture (`af_unix.c`, built from source by xtask, never
    committed) exercises both paths - (1) `socketpair(AF_UNIX, SOCK_STREAM)` +
    `fork`, where the parent and the forked child (two cells) send + recv
    "ping"/"pong" in both directions over the two direction rings (the L6
    cross-cell block/wake), and (2) `socket`/`bind`/`listen`/`connect`/`accept`
    over an **abstract** name, a single-process loopback that sends + recvs
    "hello"/"world". Prints exactly `pair: pong` / `conn: hello` / `back: world`
    / `af_unix OK`, exit 0.
  - **Accommodations, disclosed**: **SCM_RIGHTS fd-passing is deferred** - the
    seam is `sendmsg`'s `msg_control` (passing an fd would dup it into the peer
    cell's fd table over the connection); it is **not faked** (a non-empty
    control buffer is left untouched). **SOCK_DGRAM is refused**
    (`-EPROTONOSUPPORT`) - datagram boundary preservation is not implemented
    (stream only). **`accept` is non-blocking** (returns `-EAGAIN` on an empty
    backlog): the loopback proof connects before accepting, so it never blocks;
    a blocking cross-cell accept server (park the acceptor, wake on a queued
    connection) is a later refinement. `getpeername` reports family-only for an
    unnamed peer. `bind` implies the name is connectable (the registry carries
    the backlog); `listen` validates the socket is bound.

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

The Rust std column covers two fixtures per ISA, built with the same recipe: the
single-threaded `rusthello` (L2) and the multi-threaded `rustthreads` (L4,
`tests/linux-fixtures/rustthreads`, the `linuxthreads` proof).

The C column additionally covers the three **L5 signal fixtures**
(`tests/linux-fixtures/sig_{raise,segv,dfl}.c`, static-glibc ET_EXEC via the
same recipe as `chello`), the `linuxsig` proof: `sig_raise` (async
`raise(SIGUSR1)` -> handler -> `rt_sigreturn` resume), `sig_segv` (a null write
delivered to a SIGSEGV handler), and `sig_dfl` (`raise(SIGABRT)` with no handler
-> terminate 134).

The **L6 process fixtures** (`tests/linux-fixtures/{procdemo,cecho,rsh}.c`,
static-glibc ET_EXEC via the same recipe), the `linuxproc` proof: `procdemo`
(pipe2 + fork + dup2 + execve + wait4), `cecho` (its `execve` target, loaded
from the VFS), and `rsh` (a minimal from-scratch POSIX-ish shell - pipelines +
`&&`/`||` over fork/execve/wait4/pipe2/dup2 - for the P11 gate). `rsh` execs the
L3 `coreutils` 0.0.29 multicall (already in the fixture matrix) from a ramfs, so
the P11 suite is the real upstream Rust coreutils driven by a shell.

The **L7 dynamic fixture** (`tests/linux-fixtures/dhello.c`, the `linuxdyn`
proof) is the one binary built **dynamically** - stock ET_DYN/PIE, no
`-static`/`-no-pie` (gcc's default) - so its `PT_INTERP` names the real
`ld-linux`. Its runtime dependencies (the dynamic linker + `libc.so.6`) are
**not built** but **copied from the cross toolchain** at build time by xtask
`build_dyn_fixture` into the gitignored fixture build dir (never committed), and
the `linuxdyn` test seeds them into a ramfs `/lib` so ld.so resolves them:

| ISA | dynamic C (gcc, PIE) | ld.so source (interp path) | libc.so.6 source |
|---|---|---|---|
| x86_64 | host `gcc` | `/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` (interp `/lib64/ld-linux-x86-64.so.2`) | `/lib/x86_64-linux-gnu/libc.so.6` |
| aarch64 | `aarch64-linux-gnu-gcc` | `/usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1` (interp `/lib/ld-linux-aarch64.so.1`) | `/usr/aarch64-linux-gnu/lib/libc.so.6` |
| riscv64 | `riscv64-linux-gnu-gcc` | `/usr/riscv64-linux-gnu/lib/ld-linux-riscv64-lp64d.so.1` (interp `/lib/ld-linux-riscv64-lp64d.so.1`) | `/usr/riscv64-linux-gnu/lib/libc.so.6` |

If a runtime `.so` is missing for an ISA, that ISA's dynamic fixture is
**skipped-with-reason** (a 1-byte placeholder is written; `linuxdyn` detects it
and skips), keeping the static L2-L6 coverage. All three toolchains are present
in the build/CI environment here, so **all three ISAs genuinely pass**. Note:
`rsh` (below) is a purpose-built shell, not dash/busybox - a full shell
cross-build for three ISAs was out of budget; `rsh` is honest about what it
exercises (the L6 process
primitives) and its source is in the tree.

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
