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
| read / write | full | over the per-cell fd table (console, VFS files, /dev/{null,zero,urandom}). **Console input (stdin) blocks** (docs/ARCHITECTURE-DEBT.md 2.4): it used to answer 0 on an empty console, i.e. *end of input*, which is indistinguishable from a real EOF and so a lie to every reader. A blocking descriptor now parks (`proc::Block::Console`) until a byte is buffered or input genuinely ends; a non-blocking one reports -EAGAIN. Bytes come from the same kernel RX ring the UART interrupt fills, so blocking and non-blocking reads cannot disagree about what has arrived. On a machine with a live serial console and no input, a blocking stdin read waits - as it does on Linux |
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
| pipe2 / pipe | full | `O_CLOEXEC` **and** `O_NONBLOCK` honored on both ends at creation (docs/ARCHITECTURE-DEBT.md 2.4). A **cross-cell** bounded ring buffer (16 global pipes × 64 KiB, `kernel/src/linux/pipe.rs`), the two ends held by different cells after `fork` (L6). Read/write block **cooperatively** with cross-cell wake at syscall boundaries; EOF when all write ends close, SIGPIPE/-EPIPE when all read ends close. `dup`/`fork` refcount the ends. Single-process use (both ends in one cell, no peer to run) falls back to non-blocking -EAGAIN, keeping uu_cat's `splice`-fallback path (L3) working. `pipe` (x86-64 legacy) == `pipe2(...,0)` |
| splice | ENOSYS | uu_cat's Linux fast path probes `splice` and falls back to read/write on failure (documented fallback) |
| /proc/self/auxv | full | serves the cell's own auxv byte stream (a read-only synthetic fd); glibc/rustix read it when no PR_GET_AUXV is provided |
| dup / dup3 | partial | copies the slot; a duplicated VFS fd shares the underlying descriptor (close-once) |
| fcntl | partial | F_DUPFD/F_DUPFD_CLOEXEC (the CLOEXEC form really sets it), F_GETFD/F_SETFD (**FD_CLOEXEC tracked**, honored by `execve`), F_GETFL (the **real** access mode plus O_NONBLOCK while set), F_SETFL (**O_NONBLOCK honored**: a would-block read/write returns -EAGAIN instead of parking the cell or reporting 0; **O_APPEND and O_ASYNC are refused -EINVAL** - not repositioned on write, no SIGIO), F_GETPIPE_SZ on a pipe (the ring's real capacity). File locking (F_GETLK/F_SETLK/F_SETLKW + the OFD forms) → **-ENOLCK** (no lock manager). **Every other command → -EINVAL** and a console line; it used to `_ => 0`, i.e. report success for anything unimplemented. **Creation-time `O_NONBLOCK`/`SOCK_NONBLOCK` is now honoured too** (`open`/`socket`/`socketpair`/`accept4`/`pipe2`, docs/ARCHITECTURE-DEBT.md 2.4). It could not be while `poll` reported every fd ready: a non-blocking program's poll-then-read loop would be told "ready", read -EAGAIN, and spin. It landed with the waiting `poll` in one slice |
| ioctl | partial | TIOCGWINSZ on a console fd → 80x24; every other request -ENOTTY |
| poll / ppoll | full for POLLIN/POLLOUT | **Real readiness, and a real wait** (docs/ARCHITECTURE-DEBT.md 2.4). `revents` is computed per `FdKind` by one shared definition (`linux::poll_revents` over `pollin_ready`/`pollout_ready`): a pipe or local socket from its ring, a **remote** UDP/TCP socket from the `svc::SocketOps` bridge (whose readiness probe pumps the datapath), the console from the RX ring or end of input, a VFS file/`/dev/null`/`/dev/zero` always, an epoll fd from its own watches, a closed fd POLLNVAL. The **timeout is honoured**: 0 is a pure probe, a negative value waits indefinitely, and a positive one is a deadline in the cell's own clock domain. Waiting is a `proc::Block::Poll` registration, so the caller leaves the CPU and the scheduler idles on what the watched descriptors can be woken by. `ppoll`'s `struct timespec` is read (NULL = indefinite). **What it used to be:** readiness was never consulted at all - every open fd was reported ready for whatever was asked and the timeout ignored - and two things depended on that accident, which is why they were fixed in one slice (see the `fcntl` row and L8-INET-REMOTE). *Scope:* POLLERR/POLLHUP/POLLPRI are not reported (a hung-up pipe surfaces as POLLIN readable, which is what a reader acts on); a set larger than 64 descriptors keeps the non-blocking probe, because the request is copied into a fixed kernel array to be judged while another cell's address space is active; a wait whose watched descriptors have **no** wake source answers immediately rather than parking on an impossible condition |
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
| futex | partial | FUTEX_WAIT/WAKE (+ WAIT_BITSET/WAKE_BITSET as plain WAIT/WAKE; PRIVATE ignored); WAIT re-checks the word and parks the caller, WAKE moves up to `val` waiters to ready (L4). **The timeout is honored** (it used to be treated as infinite, so `pthread_cond_timedwait` hung): a `struct timespec` is read from arg 3 - **relative** for FUTEX_WAIT, **absolute** for FUTEX_WAIT_BITSET, in CLOCK_MONOTONIC or (with FUTEX_CLOCK_REALTIME) CLOCK_REALTIME - and an elapsed deadline returns **-ETIMEDOUT**. Deadlines are compared in the **cell's own clock domain** (`linux::cell_clock_ns`, what its `clock_gettime` reports), because that is the domain the program computed the deadline in; the CPU parks on them through the kernel timer arbiter's `FutexWait` slot, never `arch::timer_*` directly. A WAIT with **no timeout and no runnable sibling** can never be satisfied: it returns **-EAGAIN** (so the caller re-checks, which is what every real caller does) and logs one console line, instead of returning 0 - "you were woken" - which was a lie about a wakeup that never happened. **Priority inheritance is still a TODO** (FIFO wake) |
| getppid | full | the parent's pid (0 for the top of the process tree) |
| getuid / geteuid / getgid / getegid | full | 1000 (no root, SECURITY-IDENTITY) |
| uname | full | sysname "Linux", release "6.6.0-rheo", machine per ISA |
| clock_gettime | partial | MONOTONIC via `arch::ticks_to_ns`; REALTIME = fixed epoch + monotonic (unsynced, disclosed) |
| gettimeofday | full | The legacy wall-clock read, same REALTIME domain as `clock_gettime`, reported as seconds + microseconds. `tz` (obsolete) ignored; a NULL `tv` is a no-op success. **libuv calls this directly and *asserts* it returns 0** (`uv_gettimeofday` → `GetCurrentTimeInMicroseconds`), so an unimplemented number aborts Node at startup - it was the first gap the real `node` binary hit (docs/LINUX-COMPAT.md, the real-Node section) |
| clock_getres | full | The clock's resolution. This OS's clock derives from the cycle counter (`arch::ticks_to_ns`), so it is nanosecond-granular: reports `{0 s, 1 ns}` for any `clk_id` (all clocks share the one source). A NULL `res` is success (a clock-existence probe). V8 reads it at init |
| time (x86-64) | full | The legacy whole-second wall clock, same REALTIME domain. Returns the seconds; writes `*tloc` too if non-NULL. x86-64 only - asm-generic glibc uses `clock_gettime` (an unreachable sentinel on those tables). V8/libuv call it |
| clock_nanosleep / nanosleep | full for the blocking case | **A real sleep** (docs/ARCHITECTURE-DEBT.md 2.4); it used to return 0 immediately. The deadline is computed and compared in the **cell's own clock domain** (`cell_clock_ns` - the domain the program's own `clock_gettime` reports), parked on as a `proc::Block::Timer`, so a sibling process runs while one sleeps. `clock_nanosleep`'s `TIMER_ABSTIME` is honoured; a deadline already in the past returns 0 at once. `rem` is **never written**, because no signal is delivered to a parked process, so the sleep cannot be interrupted - stated rather than implied. Note that glibc routes `nanosleep` through `clock_nanosleep` on every ISA here, so the latter is the number that matters (fixing only the former would be invisible - observed) |
| getrandom | full | fills from the cell's DRBG; flags ignored (never blocks) |
| sched_yield | full | switches to the next ready context of this cell (L4); with no ready sibling it crosses to the next runnable **process** (`proc::yield_cell`, the `SYS_YIELD` hand-off), so a yield is a real preemption point and not a no-op. Returns 0. Only when the caller is the sole runnable process is it picked again |
| sched_getaffinity | partial | reports a single online CPU (bit 0) so `available_parallelism` reads 1 and thread pools (rayon) stay small/deterministic (L4) |
| prlimit64 / getrlimit | full | RLIMIT_STACK is **whatever the loader actually mapped** for this cell, read from `LinuxState.stack_pages` - 8 MiB by default, more if the image's `PT_GNU_STACK` asked for more (see the stack section). glibc sizes *thread* stacks from this number, so it must be the mapped size and not a recomputation. RLIMIT_NOFILE 64, else unlimited; not settable |
| arch_prctl | full | x86-64 only: SET_FS/GET_FS program the FS_BASE MSR (L1); the base is recorded per context and reloaded on a context switch (L4) |
| prctl | partial | PR_SET_NAME/PR_GET_NAME accepted as a cosmetic no-op (rayon names its workers and treats failure as fatal, L4); every other option -ENOSYS |
| set_tid_address | full | records the calling context's clear-tid address, returns its tid (L4; enacted by CHILD_CLEARTID on thread exit) |
| set_robust_list | recorded | stores the head, returns 0; robust-futex unwinding on abnormal thread exit is not enacted (no suite util depends on it) |
| rt_sigaction | full | per-cell disposition table for signals 1..64; SIG_DFL/SIG_IGN honored; stores/returns the old action; SIGKILL/SIGSTOP -EINVAL. The kernel `struct sigaction` layout is ISA-specific (x86-64 carries `sa_restorer`, asm-generic does not: `arch::SIGACTION_HAS_RESTORER`) (L5) |
| rt_sigprocmask | full | per-context blocked mask (BLOCK/UNBLOCK/SETMASK); SIGKILL/SIGSTOP never blockable; a now-unblocked pending signal is delivered before return (L5) |
| sigaltstack | full | per-context alternate signal stack; honored for SA_ONSTACK handlers; -EPERM while executing on it (L5) |
| rt_sigreturn | full | restores the interrupted `TrapFrame` + signal mask from the signal frame on the user stack; resumes where the signal interrupted (L5) |
| kill | full for live processes | Any live pid, the caller's group (`0`), or every process except the top of the tree (`-1`, standing in for init); any other negative pid is `-ESRCH` (no `setpgid`, so no groups exist). `sig == 0` is a real existence probe. A signal to **another** process is resolved against *its* disposition table and recorded pending, then delivered when the scheduler switches into it (`signal::on_resume`) - a frame rewrite needs the target's own stack and address space, and that is the only moment they are live. An uncaught fatal default makes a non-running target a zombie its parent reaps. **Does not interrupt a blocked syscall**: a target parked in `read`/`poll` gets the signal when that wait completes, not with `EINTR` - documented, and a separate slice |
| tgkill / tkill / rt_sigqueueinfo | partial | self-targeting only (own pid 1000 / own tids): `raise`/`abort` paths. A signal to a *non-running* sibling **context** is recorded pending, not force-delivered - no fixture needs it. Non-self tgid `-ESRCH` |
| rt_sigtimedwait | partial | never blocks; returns -EAGAIN so callers loop/bail rather than hang (no fixture waits in it) (L5) |
| mremap | full | shrink unmaps the tail in place; grow requires MREMAP_MAYMOVE (map a fresh region, copy, free the old); else -ENOMEM. glibc's large-block `realloc` needs it (the malloc-copy-free fallback otherwise leaks frames) |
| clone3 | full | Decodes `struct clone_args` (>= `CLONE_ARGS_SIZE_VER0` = 64 bytes) and routes to the **same** thread/process path as legacy `clone` - the one shape difference being that `stack`+`stack_size` name the base and length, so the child SP is `stack + stack_size`. `size` is checked before the pointer is dereferenced (size 0 -> EINVAL), then each field reads through `uaccess` (EFAULT if unreadable), matching Linux. glibc's `pthread_create` falls back to `clone` on ENOSYS, but a runtime that issues `clone3` **directly** with no fallback (Bun's JavaScriptCore/Zig threading) gets a hard thread-spawn failure from ENOSYS - which is why this is now implemented rather than refused (GOAL-BUN) |
| rseq | ENOSYS | glibc's fallback is "no restartable sequences", so ENOSYS is the correct answer and a success would mislead. **Dispatched** to the refusal rather than falling through the unknown-number path, so the log no longer says `ENOSYS nr=<n>` as if the number were unrecognised (docs/ARCHITECTURE-DEBT.md 4.0) |
| io_uring_setup / _enter / _register | ENOSYS, deliberately | The async-IO submission mechanism. This OS's async path is the queue-pair reactor, not io_uring, so refusing it is a design statement, not a gap. Node 22's libuv probes `io_uring_setup` at startup and falls back to epoll+threadpool on ENOSYS (observed in the real `node` trace: `io_uring_setup` then epoll_create1/epoll_ctl/epoll_pwait). **Dispatched** to the refusal rather than falling through the unknown-number path, the clone3/rseq class. Numbers 425/426/427 are shared across the x86-64 and asm-generic tables (added after the split) |
| open (x86-64 legacy) | full | Routed to `openat` with `AT_FDCWD` - the same call. It had been missing, and glibc issues `open` in preference to `openat` on x86-64, so every `open` was refused on **that ISA and nowhere else** (docs/ENGINEERING.md 11, the two-numbers hazard). An unreachable sentinel on the asm-generic tables |
| eventfd2 | full for the wakeup contract | A 64-bit counter as a per-cell fd over a per-personality registry (`linux::eventfd`), **no kernel object** - the counter lives in the registry, not the descriptor, so `dup`/`fork` share it. Write adds (refusing `u64::MAX` and refusing to overflow), read drains, `EFD_SEMAPHORE` yields 1 and decrements, a zero counter is **not** readable (so poll/epoll report it unready and a blocking read parks under the pipe's runnable-peer rule), sub-8-byte transfers are `-EINVAL`, an unknown flag bit is `-EINVAL` rather than dropped. Scope: a **sibling context** writing it does not wake a context parked on it, which is the L4 cell-level block limitation, not an eventfd one |
| timerfd_create / settime / gettime | full for the event-loop use | A timer as a per-cell fd over a per-personality registry (`linux::timerfd`), **no kernel object** - the armed deadline is an ordinary cell-clock wait, the same kind `nanosleep` parks on. One-shot and periodic; `TFD_TIMER_ABSTIME`, `CLOCK_MONOTONIC`/`CLOCK_REALTIME`; an all-zero `it_value` disarms. `read` returns the expiration count and consumes it (a periodic timer advances to its next future expiry); a blocking read on a not-yet-expired timer **parks on the deadline** (the scheduler idles on the timer, no runnable-peer needed, unlike an eventfd), a non-blocking one is `-EAGAIN`. `POLLIN` once expired, never `POLLOUT` - so an epoll loop wakes when the timer fires (the libuv timer source). `write` is `-EINVAL`. Scope: the cell-level block limit (L4) and no `TFD_TIMER_CANCEL_ON_SET` (the cell clock does not step) |
| sysinfo | partial, honestly | `uptime` from the cell's own clock domain, `totalram`/`freeram` from the frame pool, `procs` from the live process count, `mem_unit` 1. `sharedram`/`bufferram`/`totalhigh`/`freehigh`/swap/`loads` are **0 because they are 0** - no page cache, no highmem, no swap, no load average is computed. Bun sizes its heap from these, so a placeholder would be worse than a refusal |
| sched_setscheduler / getscheduler / get_priority_{max,min} | partial, honestly | One class exists: `SCHED_OTHER`, cooperative round-robin. Setting it at priority 0 succeeds *because it is already in force*; `SCHED_FIFO`/`RR`/`BATCH`/`IDLE` are refused `-EPERM` (the unprivileged-Linux errno every caller handles) rather than accepted and dropped - a real-time guarantee this scheduler cannot keep must not be reported as granted. `getscheduler` reports `SCHED_OTHER`; the priority range is 0..0 |
| close_range | full | Closes every open descriptor in the inclusive range, skipping already-closed slots as Linux does. `CLOSE_RANGE_CLOEXEC` marks instead of closing; `CLOSE_RANGE_UNSHARE` is refused `-EINVAL` (this personality has no fd table shared separately from the address space) rather than ignored |
| capget | full for the query, honestly empty | The identity-class answer for a non-root process (the same class as `getuid`/`getgid`): our synthesized identity is unprivileged (uid 1000, no capabilities), so every capability mask - effective/permitted/inheritable - is reported **0**, not a stub claiming capabilities the process does not have. The version-probe protocol is honoured: an unknown version writes the supported version (`_LINUX_CAPABILITY_VERSION_3`) back into the header and returns `-EINVAL`; a NULL data pointer is a probe that succeeds without filling; V1 fills one `cap_user_data_t`, V2/V3 fill two (64-bit caps). Node probes it nine times at startup - it was the only syscall the real `node --version` trace issued that the personality did not dispatch. `capset` is not offered (an unprivileged process cannot raise capabilities) |
| fork / vfork | full | clone-cell-within-capability-bundle (docs/POSIX-PERSONALITY.md 2): a new `user` cell in the parent's bundle, the parent's committed pages **eager-copied** (COW deferred + documented), `LinuxState`/fd table/cwd/signal dispositions deep-copied, a child pid synthesized; child returns 0, parent returns the pid. Only the calling thread is duplicated (POSIX). Reached via `clone` without `CLONE_VM` on every ISA (glibc's `fork`), or the x86-64 `fork`/`vfork` numbers. Over the `MAX_CELLS` (16) cap → -EAGAIN. `vfork` is treated as `fork` (eager copy, no COW share - safe, just less lazy) (`kernel/src/linux/proc.rs`) |
| execve | full | replaces the calling cell's image with one **streamed** from the VFS (`load::exec_elf_from_vfs`: only the ELF header + phdrs are buffered; each `PT_LOAD` segment is read page-by-page straight into its destination frame, so the kernel holds no whole-image buffer). Keeps the same cell/pid, cwd, and fd table **except descriptors marked FD_CLOEXEC, which are closed** (it used to keep every fd regardless); resets signal handlers to default and starts single-threaded. ET_EXEC + static-PIE ET_DYN, stock base |
| wait4 / waitpid | full | the parent blocks cooperatively (switching to a runnable child) until a child exits, then reaps: WIFEXITED/WEXITSTATUS for a normal exit, WIFSIGNALED for a signal-killed child; frees the child cell + its frames. `pid > 0` waits for that child, `pid <= 0` for any; WNOHANG honored; -ECHILD with no children. SIGCHLD is not queued to a handler (the parent reaps directly; documented) |
| dup2 | full | (x86-64 legacy) == `dup3(old, new, 0)`; a pipe end is refcounted |
| setpgid / setsid | recorded | returns 0 (single-session model, no job control); the shell queries process groups but does not depend on the effect |
| getpgid / getsid | partial | returns the caller's pid (one group/session per process) |
| socket / socketpair | partial | AF_UNIX (L8) and AF_INET/AF_INET6 (L8-INET), SOCK_STREAM + SOCK_DGRAM (AF_UNIX SOCK_DGRAM deferred); other families -EAFNOSUPPORT. `SOCK_CLOEXEC` honored; `SOCK_NONBLOCK` is the deferral in the `fcntl` row |
| bind / listen / accept / accept4 | partial | per-cell synthesized registries; **local only** - a remote listener needs NIC flow-steering grants (L8-INET-REMOTE deferral). `accept` is non-blocking (-EAGAIN on an empty backlog) |
| connect | partial | AF_UNIX + **loopback** INET over the L6 ring pair; a **non-loopback IPv4** destination is handed to the registered `svc::SocketOps` bridge - a real remote TCP handshake over the NIC, reporting 0 / -ECONNREFUSED / -ETIMEDOUT (L8-INET-REMOTE). Non-loopback **IPv6** → -ENETUNREACH. With no bridge registered, every non-loopback address → -ENETUNREACH |
| sendto / recvfrom | partial | datagram sockets: **loopback** over the in-kernel queue, **non-loopback IPv4** over the `svc::SocketOps` bridge (real UDP on the wire: ARP next hop, IPv4+UDP checksums, source-address reporting; the receive **parks** on `net_rx::wait_frame`, or drains without parking when the fd is O_NONBLOCK). A loopback datagram sent to a port where **nothing is bound** now returns **-ECONNREFUSED** rather than reporting the datagram sent - Linux reports that as an ICMP port-unreachable on a later operation of a connected socket, and there is no ICMP over this in-kernel queue, so it is reported on the send itself (earlier than Linux for an unconnected `sendto`, and deliberately so: the silent success made glibc's resolver - which falls back to 127.0.0.1:53 with no /etc/resolv.conf - fail for a reason nothing pointed at). A full destination queue is still a normal UDP **drop** and is reported sent. A stream socket ignores the address and routes to read/write. No `MSG_*` flags |
| send / recv / read / write on a socket | partial | loopback/AF_UNIX over the L6 rings; a connected **remote** TCP socket forwards to `SocketOps::tcp_send`/`tcp_recv` - implemented but **unproven in QEMU** (SLIRP has no TCP responder), see L8-INET-REMOTE |
| sendmsg / recvmsg | partial | gather/scatter over `msg_iov`, non-blocking; **no SCM_RIGHTS** ancillary data (L8 deferral) |
| getsockname / getpeername | full | real `sockaddr_in`/`sockaddr_in6`/`sockaddr_un`; a remote socket reports the datapath's own IPv4 and the true peer address |
| setsockopt / shutdown | recorded | returns 0, stores nothing (SO_REUSEADDR/TCP_NODELAY succeed as no-ops) |
| getsockopt | partial | zero-filled answer (SO_ERROR reads as 0) |
| epoll_create1 / epoll_ctl / epoll_wait / epoll_pwait | partial | level-triggered EPOLLIN/EPOLLOUT only. `epoll_wait`/`epoll_pwait` now **honour their timeout and genuinely park** (docs/ARCHITECTURE-DEBT.md 2.4) - they used to return 0 at once, which turns every epoll loop into a spin - over the same per-`FdKind` readiness the `poll` row describes, including a real `tcp_pending` probe for a remote TCP socket (it used to always report readable, because `svc::SocketOps` had no way to ask). Still deferred: EPOLLET, EPOLLONESHOT, EPOLLEXCLUSIVE, EPOLLRDHUP/EPOLLPRI, and a nested epoll (an epoll fd watched by another epoll reports no wake source) |

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
stack top at 8 GiB (a Linux cell's stack is sized from its own `PT_GNU_STACK`
`p_memsz`, at least 8 MiB and at most 64 MiB - see below), anonymous mmap region at
**80..252 GiB** (raised from a 4 GiB window boxed below the queue region, so a
`MAP_NORESERVE` reservation as large as JavaScriptCore's **128 GiB** Gigacage fits -
demand-filled, costing frames only for the pages actually touched; GOAL-BUN), `brk`
heap starting at the image end. The initial stack carries the
System V block **plus the ELF auxiliary vector** (L1): AT_PHDR/PHENT/PHNUM,
AT_PAGESZ, AT_BASE, AT_FLAGS, AT_ENTRY, AT_UID/EUID/GID/EGID, AT_SECURE,
AT_RANDOM (16 DRBG bytes), AT_HWCAP, AT_CLKTCK, AT_PLATFORM, AT_EXECFN,
AT_NULL. No vDSO (`AT_SYSINFO_EHDR` absent - glibc falls back to real
syscalls).

### Limits (and why demand paging is the real answer)

Most of what a Linux cell maps is still committed **eagerly** - the whole image, the
whole initial stack, every anonymous `mmap` - so a program costs frames in proportion
to its *size*, not to what it *uses*. **File-backed `mmap` is the exception and is
now demand-paged** (see "Demand paging" below), which is what lets `ld.so` map a real
`libc` without paying for it. Three numbers therefore still bound what can run:

| Constant | Value | What it bounds |
|---|---|---|
| `frames::POOL_FRAMES` | 131072 = **512 MiB** | all physical memory the kernel hands out |
| `frames::USER_RESERVE_FRAMES` | 4096 = **16 MiB** | held back from cell-driven allocation, so the kernel's own allocations (page tables, driver rings, a `fork` copy) can never fail |
| `user::MAX_FRAMES_PER_CELL` | 98304 = **384 MiB** | one cell's fairness cap on the charged mapping paths |
| `stack::LINUX_STACK_PAGES` | 2048 = **8 MiB** | the initial stack an image that asks for nothing gets - a floor, not the size |
| `stack::LINUX_STACK_MAX_PAGES` | 16384 = **64 MiB** | ceiling on a `PT_GNU_STACK` request; above it the request is clamped and logged |

These were 128 MiB / 8 MiB / 96 MiB / 1 MiB - sized for static-glibc fixtures of a
few hundred KiB. A ~100 MB binary exhausted them before reaching `main`. QEMU runs
`-m 1G` on every ISA and the pool base sits 64 MiB into RAM, so ~960 MiB is
available; the pool deliberately does **not** take it all, because firmware places
blobs near the top of RAM (QEMU's RISC-V `virt` puts the device tree the kernel
parses at ~`0xBFE0_0000` with `-m 1G`) and a pool that reached one would overwrite
it. Raising further means checking that first.

**This is a limit raise, not a design change**, and it does not remove the
underlying constraint - it moves it. The proper fix is demand paging, below.

### Demand paging

A **resumable user page fault** commits a page on first touch. `on_user_trap` calls
`linux::fill_fault` *before* the L5 fault-to-signal branch: if the personality can
fill the page it returns the same frame and the faulting instruction re-executes;
otherwise the fault becomes SIGSEGV exactly as before. The handler
(`linux::mem::fault`) asks three questions in order, and the order is the whole
correctness argument:

1. **Is anything mapped here?** No VMA record means a genuine SIGSEGV - the
   commonest one being a null dereference.
2. **Is the page already present?** Then this fault was a *permission* refusal, not a
   missing page. `FaultCause` carries no read/write bit, so the page tables are the
   source of truth (`AddressSpace::is_mapped` over `arch::paging_mapped`). Guessing
   here repopulates and re-faults forever, with no diagnostic - measured at 78,780
   fills in the revert probe.
3. **Does the mapping permit any access?** A `PROT_NONE` record is a *reservation*
   (glibc reserves large arenas that way and commits sub-ranges with `mprotect`);
   populating one hands out memory the program deliberately made inaccessible.

**What is demand-paged today: file-backed `MAP_PRIVATE` `mmap`, and the ELF image
itself.** The mapping owns a
VFS handle in `linux::filemap` rather than remembering the caller's fd, because
`ld.so` closes the fd immediately after `mmap` - on Linux a mapping references the
*file*. That registry is global, fixed-size and refcounted (the `pipe`/`epoll`/
`eventfd` pattern, **no kernel object**), with one reference per live `Vma` record:
taken at `fork` (`VmaList::inherit_files` beside `fds::inherit_pipe_ends`) and given
back at exit and at `munmap`. Both halves matter - without the `fork` addref a
child's exit frees an entry the *parent* still maps, and the parent's next untouched
page reads zeros with no fault and no log.

**The image.** `load::load_elf_linux` used to copy every `PT_LOAD` page into a fresh
frame, so a program cost frames in proportion to its size - and since the image is
already resident in kernel memory, that was a *second* copy of the whole program.
For an `ET_EXEC` binary with a `PT_INTERP` the kernel loads the main program itself,
so this path (not `mmap`) is where a large image's memory lands. It now **records**
the segments it can leave to the fault handler, in a `load::SegRecorder`, and copies
only the ones it cannot. Two conditions must hold, and each was found by a segment
that broke without it:

- **`p_filesz == p_memsz`.** A segment with a `.bss` tail is part file and part zero
  inside one record, and getting that boundary wrong produced a null dereference in
  a static Rust binary. The honest scope is: whole-file segments are demand-paged,
  the rest copied. Static glibc has exactly one such segment, tens of KiB.
- **`p_offset` congruent to `p_vaddr` mod the page size.** Paging fills whole pages,
  so the page holding a VA must line up with a page-aligned file offset. Every real
  toolchain emits this; a hand-built ELF that does not is copied.

Each refusal prints which segment and why, so "loaded eagerly" is never silent.
Because the bytes come from kernel memory rather than a file, `filemap` gained a
second store kind (`Store::Mem`), and because a segment's content ends mid-page,
`Vma::file_len` says how far a record is backed - past it the pages are zero, not
"whatever is next in the file".

**`user::reset` must run before the load, not after.** It clears the registry the
loader registers the image in, so the old order left the records naming a released
entry and every page came back zeroed - which on RISC-V is an illegal instruction at
the entry point and nothing more informative. `filemap::alive` now makes a
recurrence say so rather than hand out blank memory.

**`execve` and the interpreter are demand-paged too.** Both stream from the VFS, and
the obstacle was never the streaming - it was that recording against the *caller's*
fd would name a file closed on return. So a mapping opens its **own** handle over the
path, which is what `mmap` already does and for the same reason. `SegRecorder` holds up
to two stores, because a dynamically linked program is two files (the program and
`ld.so`); a third file is something `ld.so` maps itself through `mmap`, already lazy.
`exec_reinit` records, as `install_cell` does - it was the only caller, so an
`execve`d image used to record nothing.

One trap is worth naming, because it was hit: `exec_elf_from_vfs` is shared with the
**native** `SYS_SPAWN`, and a native cell has no VMA list, so a lazy image left its
child with an address space full of holes and no diagnostic. The eager and lazy loads
are now separate functions - the eager one keeps the old name so an unaware caller
gets a correct image, and `exec_elf_from_vfs_demand` says in its name that the caller
must map what it returns.

Measured: `linuxdyn` records 1 program + 1 `ld.so` segment (35 pages recorded, 4
copied), and `linuxproc`'s fork+execve phase records 221 pages and copies 21. Both
paths run inside a syscall where a test can measure nothing directly, so
`load::recorded_pages()`/`eager_pages()` are the witnesses.

Measured on riscv64: `rusthello`'s 201 image pages cost **16 frames** at load
(the one eager zero-tail segment plus page tables) instead of 201. `linuxrun`
asserts that inequality on all three ISAs, so "the recorded pages were not
committed" is checked rather than believed.

**Two things the kernel had to gain for this to be safe.** A cell hands the kernel
pointers into its own memory, and with presence lazy the *kernel* becomes a reader of
a page nothing has faulted in - a load fault at a kernel PC, which is not resumable
here (this is why Linux has `copy_from_user` and a fixup table). The F1 hardening
already routes every cell-supplied pointer through one set of helpers, so those gained
"ensure present" beside "in range": asked once at the seam, not at ~60 dereferences.
Placement was the whole problem - in the bare range predicates it cost a ~2,900x
amplification, because `unmap_range` uses them only to *bound* a range and so
materialised every page immediately before freeing it; on the dereference helpers it
costs 0 kernel pre-faults, measured every run. Second, x86-64's ring-3 fault resume
used `sysretq`, which *consumes* RCX and R11 - fine while signal delivery was the only
fault resume (a handler entry does not re-execute anything), fatal for the first path
that genuinely re-executes. Faults now resume through `iret_resume`.

Proof: `mmapdp` in `linuxproc` on all three ISAs - 64 file pages mapped, exactly 5
filled, each carrying its own per-page file byte (so the offset arithmetic holds at
the top of the mapping), 100 rereads of a filled page costing nothing, a write to a
filled read-only page still SIGSEGV, a page still filling from the file after a forked
sharer exited, and the registry back where it started. Plus `linuxdyn`, where `ld.so`
maps a real 1.5-2.1 MB `libc` and an unmodified dynamic glibc binary runs.

**`fork` is copy-on-write.** It used to copy every committed page, so a process paid
its whole resident set to fork - more, for a large program, than its image ever cost.
Now `AddressSpace::fork_from` shares the parent's pages read-only into the child and
marks both sides copy-on-write; each page is privated on the first write to it, in the
same `linux::mem::fault` handler. Measured on riscv64: a fork of a 2406-page (9.4 MiB)
process shares 2406 pages, copies 0, and consumes 12 frames of child page tables -
200x. Three pieces carry it:

- **A per-frame reference count** in `frames` (`share`/`refs`, and `free` is now a
  decrement - which is what lets every pre-COW caller stay unchanged). A page that
  cannot be shared (outside the pool, or at the count ceiling) is copied, so nothing
  is silently aliased.
- **A software PTE bit per ISA** (`arch::paging_cow_protect_user`/`_at`/`_clear`: Sv39
  RSW bit 8, AArch64 bit 55, x86-64 bit 52). The mark lives in the page table, not the
  VMA list, because a fork shares the stack and the `brk` heap too and neither has a
  VMA record - a COW test routed through the VMA list would refuse the first stack
  write after every fork.
- **The parent is write-protected too**, which is the half that fails silently: without
  it the parent writes through to memory the child now sees, wrong values with no fault.

Proven by `cowfork` in `linuxproc` on all three ISAs - a 256-page dirty heap forked,
three isolation properties (the child sees the parent's pre-fork values; neither side's
writes reach the other), with the kernel's own `mm::fork_pages()`/`fork_frames()` as the
oracle. Both halves observed failing when reverted.

**The stack grows on fault.** `setup_stack` maps only its top page - the one the kernel
writes argv/envp/auxv into - and `install_cell`/`exec_reinit` register the rest of the
`PT_GNU_STACK` request as an anonymous read-write reservation (`mem::reserve_stack`). A
touch below the top page faults a fresh zeroed frame in through the same handler; a touch
below the *reservation* hits no VMA and is a SIGSEGV, so the guard page falls out of the
bound rather than needing its own page. An image asking for a 64 MiB stack used to pay 64
MiB before `main`; it now pays one page plus what it touches. Proven by `stackx`
(linuxproc, all three ISAs): a 12 MiB request whose 9280 KiB of writes appear as 2380
demand fills where an eager stack shows none (observed failing at 59 when the eager
mapping is restored), with the RLIMIT_STACK and touch-through assertions unchanged.

That closes the last eager path in the memory model: image, file `mmap`, `fork`, and the
stack are all lazy. **Still eager, and named:** a segment with a `.bss` tail (copied
whole because its file/zero boundary sits inside one record), and every **native** cell's
image (no VMA list to map records with). Both ride the same handler;
docs/ARCHITECTURE-DEBT.md 4.0 blocker 2 tracks them.

### Touching a cell's memory: the `uaccess` seam

Every kernel access to a cell's memory goes through one module, `kernel/src/uaccess.rs`.
This is not decoration: a cell hands the kernel raw virtual addresses, and lazy mapping
makes *readiness* a moving target - demand paging made a page's presence lazy, COW makes
its writability lazy on top of that, and a fault taken in kernel mode is not resumable
here (which is why Linux has `copy_from_user`/`copy_to_user` and a fixup table). Before
this seam, ~98 sites touched cell memory and 51 dereferenced the raw VA with only a
bounds check performed elsewhere, so each lazy feature re-opened a 98-site audit and half
the sites had no guard to extend. The module offers bounds-only predicates
(`readable`/`writable`, kept separate because folding presence into them cost a measured
~2,900x amplification), resolve-and-hand-back (`buf`/`slice`/`out_ptr`), and
resolve-and-perform (`read`/`write`/`copy_in`/`copy_out`/`fill`) - the last so a site
cannot forget to resolve. A new lazy feature changes one function there and nothing
else. **Doctrine note:** the frame refcount, `share`, `cow_protect` and fault delivery
are kernel *mechanism*; the COW *policy* is personality code and, like seL4's
user-level page-fault handling, can move behind a userspace process server later without
rewriting the mechanism. It is pre-resolution, not a fixup table - which is sound while
the kernel is the only thing running, and is where SMP (task #27) will need a real
fixup path.

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
  - **RLIMIT_STACK = the stack the loader actually mapped.** The size comes from
    the image's own `PT_GNU_STACK` `p_memsz` (`elf::stack_size` ->
    `LinuxImage.stack_want` -> `stack::stack_pages_for`), floored at
    `LINUX_STACK_PAGES` = 8 MiB (the glibc default, for an image that asks for
    nothing) and capped at `LINUX_STACK_MAX_PAGES` = 64 MiB, clamped **and
    logged** above that - the stack is mapped eagerly and charged to the cell's
    frame budget, so obeying an arbitrary request would exhaust the pool at load.
    The reported limit is read from the one `LinuxState.stack_pages` the loader
    set, not recomputed: glibc sizes *thread* stacks from RLIMIT_STACK, so
    reporting more than is mapped hands every thread a stack that faults.
    `PT_GNU_STACK`'s executable-stack flag is deliberately not honoured (W^X is
    structural). Proven by `stackx` in `linuxproc`. RLIMIT_NOFILE 64.
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
  - **`clone3` is implemented** (GOAL-BUN): it decodes `struct clone_args` and
    routes to the same context-creation path as legacy `clone`. glibc's
    `pthread_create` would fall back to `clone` on ENOSYS, but Bun's
    JavaScriptCore/Zig threading issues `clone3` **directly** with no fallback, so
    ENOSYS there is a hard thread-spawn failure. `rseq` stays ENOSYS (glibc's
    fallback is "no restartable sequences").
  - **Futex timeouts are honored** (they were not at L4, which is what made
    `pthread_cond_timedwait` hang) - see the `futex` row in the status table for
    the clock domains and the deadline path. Proof: `condwait.c` in the
    `linuxthreads` test does two `pthread_cond_timedwait`s on a never-signalled
    condvar (one CLOCK_REALTIME, one CLOCK_MONOTONIC), each of which must return
    ETIMEDOUT no earlier than its own deadline, with the kernel asserting the
    deadlines really went through the timer arbiter's `FutexWait` slot. Without
    the fix the fixture hangs until the boot test's 120 s timeout - observed.
  - **Frame pool** is 131072 frames (512 MiB) with a 384 MiB per-cell budget;
    demand-commit keeps N thread stacks + arenas within it (`linuxthreads` runs
    5 contexts comfortably). See "Limits" below for why the numbers are what they
    are.
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
    copies every committed page (including the whole Linux stack, >= 8 MiB); `execve`
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
  - **`execve` of a dynamic binary is now wired** (GOAL-DISK-2): the streaming
    `execve` path (`load::exec_elf_from_vfs_demand` → `exec_elf_inner`) parses
    `PT_INTERP`, reads the interpreter path from the program's fd at the segment
    offset (the streaming path holds only the header buffer, not the whole
    image), and streams the interpreter at `LINUX_INTERP_BASE` demand-paged - the
    same handling `load_elf_linux` gives the initial-load path, factored so the
    two share `stream_elf_at`. `linuxdyn` now proves **both**: phase 1 loads
    `dhello` directly (initial-load), phase 2 `execve`s `/bin/dhello` from the VFS
    (streaming), each asserting exact stdout + exit 12 on all three ISAs, with the
    recorded-page witness confirming program + interpreter are both demand-paged.
  - **`execve` off a live ext4 disk is done** (GOAL-DISK-2b): `linuxdyn` phase 3
    mounts a real ext4 image on a virtio-blk disk through `ext4fs`/`ext4plus` + the
    block cache and `execve`s `/bin/dhello` from it - the program, `ld.so` and
    `libc.so.6` all stream off the disk on demand (447-590 block-cache fills per
    ISA, exact stdout + exit 12, all three ISAs), none resident whole. That is the
    shell-launches-a-dynamic-binary-off-disk shape the real target needs.
  - **Multi-library resolution is done** (GOAL-DYN-MULTILIB, task #169). A binary
    that links a **second** shared library (`dmath`: a C `sqrt` program, so `libc`
    + `libm`) now runs - `linuxdyn`'s multi-library phase asserts `dmath: sqrt16=4`
    + exit 4 on all three ISAs. The defect was **not** in ld.so or in version
    resolution: it was the `stat`/`fstat` block reporting `st_ino = 1` and
    `st_dev = 0` for **every** file, because the kernel↔VFS bridge (`abi::Stat`)
    carried no inode - the VFS `NodeId` was dropped. glibc's `ld.so` dedups shared
    objects by `(st_dev, st_ino)`, so after mapping `libm` it opened `libc`,
    `fstat`'d it, saw the same `(0, 1)`, concluded "libc.so.6 is already loaded"
    (as the libm map), never mapped real `libc`, and then failed
    `version 'GLIBC_2.34' not found` searching `libm` for a `libc` version - which
    is exactly why the error named `libm` while looking up a `libc`-provided
    version. Single-library `dhello` never hit it: one file's inode is never
    *compared* against another's. The fix plumbs the real `NodeId` from
    `posix::Metadata` through a widened `abi::Stat.ino` into every Linux
    `st_ino`/`stx_ino` (`fstat`/`newfstatat`/`statx`); `st_dev` stays constant, so
    distinct inodes are sufficient for the dedup. Recorded as a scar in
    docs/ENGINEERING.md 11 ("a field left constant is a field that lies").
    `MAX_MAPPED_FILES` was raised to **64** alongside it, for the dozen-library
    shape a production binary has. A **four-library** proof rides the same phase:
    `dcpp` (a dynamic C++ hello) links **libstdc++ + libgcc_s + libc + libm** and
    runs C++ runtime init (static constructors, iostream setup, exception-unwind
    tables) - the production shape a real application has - printing
    `dcpp: hello from dynamic C++ (23)` + exit 23 on x86_64 and aarch64
    (riscv64 skips-with-reason: no cross-g++ in the build environment). No new
    kernel gap surfaced: the inode fix plus the existing loader scale from two
    libraries to a real four-library C++ binary unchanged.
  - Accommodations, disclosed: a dynamic **Rust** `std` hello is additionally
    skewed (rustc's bundled std targets a newer glibc than the cross sysroot), so
    a version-consistent multi-lib fixture uses cross-gcc-built C. **MAP_SHARED of
    a file** stays unmodeled (ld.so uses PRIVATE).

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

- **L8-INET [done]** - **AF_INET / AF_INET6 sockets over the loopback interface**
  (127.0.0.1 / ::1). This extends the socket surface from AF_UNIX to the
  internet domain so **unmodified networked Linux binaries run**, and - like every
  prior milestone - adds **no kernel object**: INET sockets are per-cell fds and
  the byte transport reuses the L6 cross-cell ring (`kernel/src/linux/inetsock.rs`).
  The socket **syscall numbers are the same** as AF_UNIX (`socket`/`bind`/... - the
  family is handled inside); epoll adds a few numbers to the two `arch/*/linux_abi`
  tables (x86-64 legacy `epoll_create1`=291.. / asm-generic `epoll_create1`=20..;
  per-ISA ABI).
  - **Loopback scope at L8-INET (superseded for remote by L8-INET-REMOTE, below).**
    The kernel is **allocation-free**; the native transports (`net::tcp`/`net::udp`)
    are `no_std`+**alloc** userspace crates and **cannot** be linked into the
    `kernel/` library. For **loopback** a TCP connection between two local endpoints
    reduces to a **reliable, in-order byte stream** - exactly the L6 ring pair that
    already backs AF_UNIX SOCK_STREAM - and UDP to an in-order **datagram queue**.
    So INET sockets run over loopback deterministically and network-free, keying the
    address namespace by `(is_v6, port)`. This proves the socket **ABI**. At L8-INET
    a non-loopback destination was refused `-ENETUNREACH`; **L8-INET-REMOTE** lifts
    that over a bridge, and the loopback path below is **unchanged byte-for-byte**
    (`linuxinet` still asserts the same transcript).
  - **Syscalls**: `socket(AF_INET|AF_INET6, SOCK_STREAM|SOCK_DGRAM)`, `bind`,
    `listen`, `accept`/`accept4`, `connect`, `send`/`recv`/`read`/`write` on a
    connected stream (via the same cross-cell block + SIGPIPE path as pipes),
    `sendto`/`recvfrom` (datagrams over loopback, with source-address reporting),
    `getsockname`/`getpeername` (real `sockaddr_in`/`sockaddr_in6`),
    `setsockopt`/`getsockopt` (accept-and-ignore / zeroed - SO_REUSEADDR,
    TCP_NODELAY succeed as no-ops), `shutdown` (no-op). A minimal **epoll** -
    `epoll_create1`, `epoll_ctl` (ADD/MOD/DEL), `epoll_wait`/`epoll_pwait` -
    reports **level-triggered** `EPOLLIN`/`EPOLLOUT` readiness on socket/pipe fds.
    The `struct epoll_event` layout is per-ISA (x86-64 packs it, ARM64/RISC-V
    align it: `arch::linux_abi::EPOLL_EVENT_{SIZE,DATA_OFFSET}`).
  - **Proof (`linuxinet`, all three ISAs, exact stdout + exit)**: an unmodified
    static-glibc C fixture (`inet.c`, built from source by xtask, never committed):
    (1) a TCP `socket`/`bind`/`listen`/`accept` server + `socket`/`connect` client
    over **127.0.0.1** exchanging "hello"/"world"; (2) an **epoll** watch on the
    client socket reporting `EPOLLIN` ready once the server has written; (3) a UDP
    `sendto`/`recvfrom` over 127.0.0.1; (4) a TCP exchange over **::1**
    (AF_INET6). Prints exactly `tcp4: hello` / `epoll: ready` / `tcp4: world` /
    `udp4: ping` / `tcp6: hi` / `inet OK`, exit 0.
  - **Accommodations, disclosed**: **loopback only** (remote INET is a later
    phase, above). **epoll is level-triggered only** - `EPOLLET`, `EPOLLONESHOT`,
    `EPOLLEXCLUSIVE`, `EPOLLRDHUP`/`EPOLLPRI` are not implemented (masked to
    `EPOLLIN|EPOLLOUT`), and `epoll_wait` is **non-blocking** (readiness computed
    at call time; a blocking cross-cell wait is a later refinement, like the
    AF_UNIX blocking `accept`). `setsockopt`/`getsockopt` **store nothing** - the
    common options are accepted so glibc proceeds; TCP_NODELAY/SO_REUSEADDR have no
    observable effect on loopback. **No dual-stack**: v4 and v6 are separate port
    namespaces (no IPV4_MAPPED). UDP is best-effort (a datagram to an unbound
    port is dropped, `sendto` still reports success). Stream `accept` is
    non-blocking (as in AF_UNIX).

- **L8-INET-REMOTE [done]** - **real remote networking: an unmodified Linux binary
  reaches the network over the NIC** (rheo-net **N4b**, docs/NETSTACK.md N4b). This
  lifts the L8-INET `-ENETUNREACH` refusal for non-loopback addresses, and again
  adds **no kernel object** and **no new syscall**: the socket numbers, the fd
  table and the per-cell synthesized state are all as before.
  - **The mechanism: a bridge, not a stack (`svc::SocketOps`).** Doctrine
    (docs/ARCHITECTURE.md 6, docs/NETWORKING.md) puts IP/UDP/TCP in **userspace**,
    and `kernel/` is allocation-free, so the kernel can hold **no network stack** -
    exactly the constraint that keeps it holding no filesystem. The answer is the
    same one `svc::FileOps` already uses: a table of **function pointers a service
    registers**, with all policy outside. `kernel/src/svc.rs` gains
    **`SocketOps`** + `set_socket_ops`/`socket_ops` (10 entries: `local_ip`,
    `udp_bind`/`udp_close`/`udp_send`/`udp_recv`/`udp_pending`,
    `tcp_connect`/`tcp_send`/`tcp_recv`/`tcp_close`), and
    `kernel/src/linux/fd.rs` forwards **non-loopback** operations to it. Two new
    `FdKind` variants carry the bridge handles (`InetUdpRemote`,
    `InetTcpRemote`); **loopback keeps `InetDgram`/`InetConn` and the L6 ring
    fast path untouched**. With no bridge registered the answer is still
    `-ENETUNREACH`, so every other Linux kernel behaves exactly as before.
  - **Who registers it.** Today `tests/src/inet_personality.rs` (the sibling of
    `vfs_personality.rs`, the same pattern): it links **`rheo-net`** in its
    librheo-free **codec posture** (`--no-default-features`) and drives
    `hw::virtio_net` directly. The protocol work is entirely the stack's -
    `eth` framing, `arp` request/reply, `ip` headers + checksum, `udp`
    build/parse + pseudo-header checksum, and the full RFC 793
    `tcp::Connection` state machine (its synchronous
    `poll(now)`/`on_wire_segment(now, bytes)` seam drives cleanly from kernel
    context). The documented end state is a network **service cell** reached
    over a queue pair (rheo-net N4a); the table is shaped to accept that
    substitution.
  - **Blocking.** A remote receive **parks**: the bridge blocks in
    `net_rx::wait_frame_slice` - the N2d park-until-frame primitive - so on
    riscv64/aarch64 the kernel genuinely halts at WFI until the NIC's RX
    interrupt fires, and on x86-64 it falls back to the same documented bounded
    kernel poll (no MSI-X through the virtio-pci config tunnel). Never a
    re-submit spin.
  - **Proof (`linuxnet`, all three ISAs, exact stdout + exit 0)**: an unmodified
    static-glibc C fixture (`inetremote.c`, built from source by xtask, never
    committed) (1) hand-builds a **DNS query** and `sendto`s it to QEMU SLIRP's
    built-in responder at **10.0.2.3:53**, then `recvfrom`s the reply and checks
    its **structure** - our transaction id echoed, the QR bit set, the sender
    being 10.0.2.3:53 (never a specific resolved address: SLIRP proxies to the
    host resolver, so an A record's value is not deterministic); and (2)
    `connect()`s to a **closed port on the gateway** (10.0.2.2:9) - a real
    three-way handshake goes out and SLIRP's **reset** comes back, which
    `tcp::Connection` turns into `ECONNREFUSED`. The kernel additionally asserts
    the receive really parked (`net_rx::irq_count() > 0` + `did_idle()` on the
    interrupt-driven ISAs). With no netdev attached the kernel
    skips-with-reason.
  - **Name resolution through glibc's own resolver** (`resolve.c`, the second
    `linuxnet` fixture). Hand-building a DNS packet proves the datapath but not
    the thing every real program actually does, which is call `getaddrinfo`. That
    did not work, and the reason was **missing configuration, reported as
    success**: there was no `/etc/resolv.conf` anywhere in the tree, so glibc fell
    back to its built-in nameserver `127.0.0.1:53`; that address is **loopback**,
    so the query went to the in-kernel datagram queue where nothing listens - and
    the send **reported the datagram sent anyway**. `getaddrinfo` then failed with
    no signal pointing at any of it.
    Two changes fix it: the `linuxnet`-class kernels seed
    `/etc/{nsswitch.conf,hosts,resolv.conf}` into the ramfs
    (`vfs_personality::seed_resolver_files`, nameserver `10.0.2.3` - SLIRP's
    resolver, a **non-loopback** address, so the query rides the proven remote UDP
    path), and a loopback datagram to a port with **no bound endpoint** now
    returns `-ECONNREFUSED` (the `sendto` row above). The fixture asserts, in
    order: that refusal (deterministic, network-free); `rheo.test` resolving to
    **10.9.8.7** from the seeded `/etc/hosts` - a closed-form answer, asserted
    exactly, proving the `files` backend read the seeded configuration with no
    wire involved; and then a **live** `getaddrinfo` of a real public name, whose
    address is a property of the host's resolver and is therefore **reported, never
    asserted or printed** (one line from a fixed pair: resolved, or cleanly not).
    On this development host it resolves on all three ISAs.
    **What this used to rest on, and what it rests on now.** It used to work
    *because of a bug*: `poll` reported every open fd ready without consulting
    readiness, so glibc's resolver fell through to its `recvfrom` - and that
    `recvfrom` blocked only because creation-time `SOCK_NONBLOCK` was being
    dropped. Both were fixed in one slice (docs/ARCHITECTURE-DEBT.md 2.4), because
    fixing either alone breaks the resolver: a readiness-computing `poll` that does
    not wait makes it give up, and an honoured `SOCK_NONBLOCK` without a waiting
    `poll` makes it spin on -EAGAIN. It now works for the right reason: the
    resolver's `poll` **blocks** until the socket is readable - a remote socket's
    readiness names `idle::NET`, so the scheduler idles on the NIC, and asking the
    `svc::SocketOps` bridge for readiness pumps the receive path - and its
    non-blocking `recvfrom` then succeeds. `linuxnet` is the regression test for
    exactly this, on all three ISAs.
  - **What works remotely, precisely.** **UDP: fully** - `sendto`/`recvfrom`,
    `connect`+`send`/`recv`, source-address reporting, real ARP next-hop
    resolution (the destination on our own /24, else the gateway), real IPv4 +
    UDP checksums, a blocking receive that parks. **TCP: connect is real and
    proven** (SYN on the wire, RTO retransmit inside the budget, a real reset
    turned into `ECONNREFUSED`, `ETIMEDOUT` at the deadline). TCP **data
    transfer is implemented** (`tcp_send`/`tcp_recv` over the same
    `tcp::Connection`, `read`/`write` on the fd) but **not proven**. Be precise
    about why, because the old wording ("SLIRP has no TCP responder")
    **overstated** it: SLIRP *does* proxy **outbound** TCP - the same proxying
    that makes the live DNS above work for UDP - so a real remote TCP data
    exchange is reachable in principle. What SLIRP does not offer is a
    **deterministic** peer: there is no built-in TCP service to talk to, and
    anything reachable through the proxy is a property of the host, not of this
    code. So the gap is "no deterministic peer is arranged", not "the wire cannot
    carry it". Closing it means arranging one: a host-side sink started by xtask
    and reached at `10.0.2.2:<port>`, a `guestfwd`ed listener, or the N4a service
    cell talking to a peer cell - each a live phase that must degrade
    with a printed reason.
    One real defect on that path **has** been fixed: `op_tcp_send` never
    processed inbound segments, so the peer's ACKs never reached the state
    machine, `snd_una` never advanced, the send queue filled and `write` returned
    0 → `EAGAIN` **forever** - any body larger than the send queue deadlocked. The
    send path now drains the NIC before accepting data. That fix is **reasoned and
    code-reviewed, not proven**: it needs the same peer the data path needs.
  - **Deferred, disclosed**: **IPv6 remote** (`AF_INET6` to a non-loopback
    address is still `-ENETUNREACH` - the N4b datapath is IPv4); no remote
    **listener**/`accept` (an inbound connection needs NIC flow-steering
    grants, docs/NETWORKING.md); remote handles are **not reference-counted**
    across `dup`/`fork` (the first close releases them); one datapath instance
    for the whole machine, with fixed-size registries (4 UDP endpoints, 4 TCP
    connections, a 4-entry ARP cache); no `SO_RCVTIMEO` - one documented receive
    bound (2 s) and connect bound (3 s) apply, except that a descriptor made
    non-blocking with `fcntl(F_SETFL, O_NONBLOCK)` passes a **zero** deadline, so
    it drains without parking and reports `-EAGAIN`; no DHCP
    (the SLIRP identity 10.0.2.15/gateway 10.0.2.2 is fixed - DHCP as a
    userspace service is a later phase); `epoll` on a remote TCP socket reports
    "always readable" (the N4b datapath has no non-blocking readiness probe),
    while remote UDP readiness is real.

- **L8-EVENTFD [done]** - **`eventfd2`, plus the six other syscalls the real
  Claude Code binary was measured issuing** (docs/ARCHITECTURE-DEBT.md 4.0,
  blocker 3). The seven were taken from that binary's startup `strace`, not
  guessed. Six are advisory - a program keeps going without them, or glibc has a
  documented fallback. `eventfd2` is not: it is the epoll event loop's only
  wakeup path, so refusing it does not degrade the program, it removes the
  mechanism.

  **No kernel object.** An eventfd is a per-cell fd (`fd::FdKind::EventFd`)
  indexing a per-personality registry (`kernel/src/linux/eventfd.rs`), exactly as
  an epoll instance and a pipe are. The counter lives in the **registry**, not in
  the descriptor: `dup` and `fork` produce a second descriptor for the *same*
  object, and a counter copied per descriptor gives two counters that silently
  stop waking each other (docs/ENGINEERING.md 11). Sharing is the whole point of
  the object, so the shared state *is* the object.

  Blocking is the pipe's machinery, reused: `proc::Block::EventFdRead`, judged by
  `satisfiable` from kernel state (the waiter's address space is not active
  there), completed by `complete_block` once it is, classified as an `idle::PEER`
  wake source, and named in the deadlock diagnostic.

  The other six are in the syscall table above (legacy `open`, `sysinfo`,
  `sched_setscheduler` + `getscheduler` + `get_priority_{max,min}`,
  `close_range`, `clone3`, `rseq`). Two are worth repeating here because they are
  *honesty* fixes rather than features: `sched_setscheduler` accepts the policy
  already in force and refuses real-time with `-EPERM` instead of accepting and
  dropping it, and `sysinfo` reports the real frame pool rather than zeros.

  Proven by the `sysx` fixture in `linuxproc` on all three ISAs, asserting each
  refusal **as** a refusal. Four narrow reverts were each observed failing.

  *Scope (honest):* a **sibling context** writing an eventfd does not wake a
  context parked on it - a blocking eventfd read parks the whole **cell** under
  the same `runnable_peer_exists` rule a pipe read uses, which is the L4
  cell-level block limitation (task #27), not an eventfd-specific one. Within one
  context, and across processes, the semantics are exact.

- **L8-TIMERFD [done]** - **`timerfd_create`/`settime`/`gettime`**, the timer
  source of libuv, and thus of Node.js and much of the async/JS world
  (GOAL-TIMERFD). An event loop arms a timerfd for its nearest deadline, adds it
  to its epoll set, and `epoll_wait` returns when it fires; without timerfd a
  program loses its timer wakeup, not merely a convenience.

  **No kernel object**, the eventfd pattern exactly: a timerfd is a per-cell fd
  (`fd::FdKind::TimerFd`) indexing a per-personality registry
  (`kernel/src/linux/timerfd.rs`), and its expiry is an ordinary **cell-clock
  deadline** - the same wait `nanosleep` (`proc::Block::Timer`) parks on and the
  same the scheduler already halts for through the timer arbiter's `CellSleep`
  slice. So timerfd composes the existing time machinery: it adds no new wake
  source and does not touch the deadline arithmetic. The armed deadline lives in
  the **registry**, not the descriptor, so `dup`/`fork` alias one timer (the
  eventfd reasoning).

  Blocking reuses the sleep machinery: `proc::Block::TimerFdRead`, judged by
  `satisfiable` from kernel state (the registry + the cell clock), completed by
  `complete_block` writing the expiration count, classified `idle::TIMER`. Unlike
  an eventfd read, a blocking timerfd read needs **no runnable peer** - the wake
  source is the clock, so a single-threaded program parks correctly and the
  scheduler idles on the timer, exactly as `nanosleep` does. For epoll, the
  timerfd's per-fd source is `idle::TIMER` and its readiness is "expired", so the
  existing poll/epoll idle path (timer slices that re-check readiness) wakes the
  loop with no change to that path.

  A one-shot fires once; a periodic timer's `read` returns the elapsed count and
  advances its deadline to the next future tick. `write` is `-EINVAL` (a timerfd
  is not writable). Proven by the `timerx` fixture in `linuxpoll` on all three
  ISAs: a blocking read parks on a 20 ms one-shot and returns exactly one
  expiration, then epoll_wait wakes on a second one-shot, and the disarmed timer
  reads zero.

  *Scope (honest):* the L4 cell-level block limit (a sibling context does not
  independently wait), and no `TFD_TIMER_CANCEL_ON_SET` - the cell clock does not
  step, so there is nothing to cancel on.

- **A real JavaScript engine runs [done]** (GOAL-JS). The `jsdemo` fixture is a
  pure-Rust JavaScript engine - the **`boa_engine`** crate (crates.io, pinned in
  `tests/linux-fixtures/jsdemo/Cargo.lock`) - built static-glibc for each ISA and
  run unmodified under the Linux personality by `linuxrun`. It is the on-goal
  proxy for Node.js/Claude Code: a complete language runtime (lexer, parser,
  bytecode compiler, register VM, heap, garbage collector) exercising the L2-L8
  syscall surface and the demand-paged loader at scale (~9.5 MB image, ~1580
  demand-paged pages, ~18 frames committed at load). It evaluates real JavaScript
  - a function, `Array.prototype.reduce`, an arrow-function closure, string
  concatenation - and prints `js: rheo:42` (exit 0) on **all three ISAs**. This is
  not V8 and not Node's libuv loop; it is a genuine JS interpreter executing on
  the OS, the strongest evidence to date that the personality carries a real
  language runtime, and the honest step short of Node itself (whose V8 + libuv +
  ~100 MB binary is the remaining distance).

- **The syscall surface for real Node is closed; the remaining distance is V8's
  JIT, not the syscalls** (measured, not guessed). An `strace` of the real
  `node` binary (v22, dynamic, 124 MB) evaluating JavaScript (`node -e
  'console.log(40+2)'`) issues 49 distinct syscalls; every one is now dispatched
  or **deliberately** refused. `node --version` (the loader + startup path)
  issues only syscalls the personality already handled once `capget` landed - it
  was the single call falling through the unknown-number log. Real JS execution
  adds `io_uring_setup`/`_enter` (refused ENOSYS, libuv falls back to
  epoll+threadpool - the trace shows exactly that fallback), `clone3` (refused,
  glibc falls back to `clone`), and `epoll_pwait` (already dispatched, shares the
  `epoll_wait` arm). **The one thing the personality's doctrine refuses is V8's
  JIT code space**: the trace contains a single `mprotect(..., PROT_READ |
  PROT_WRITE | PROT_EXEC)` - V8's writable-executable code region - which the W^X
  invariant (docs/ARCHITECTURE.md 5, enforced structurally in
  `kernel/src/linux/mem.rs`) refuses with `-EPERM`. Every other `PROT_EXEC`
  mapping in the trace is a file-backed `PROT_READ|PROT_EXEC` shared-library
  segment (legitimate W^X). So the honest path to Node on this OS is V8's
  **`--jitless`** mode (the Ignition bytecode interpreter, no executable
  allocation - the same interpreter shape `boa` already proves runs), *not*
  relaxing W^X. The remaining engineering is mechanical, not doctrinal: stream
  the ~124 MB binary + its shared-library set off a live ext4 disk (the
  `linuxdyn` disk path, demand-paged) and confirm V8 initialises within the
  QEMU-TCG boot budget. A W^X-clean JIT (write-then-flip RW→RX code space, or a
  MAP_JIT-style dual mapping) is the follow-on that would let V8 optimise rather
  than only interpret.

- **The real Node.js binary runs unmodified on the OS and prints its answer**
  (GOAL-NODE [done], task #174, the `linuxnode` test). The actual
  `/opt/node22/bin/node` (v22, dynamic, 124 MB, shipping V8 + libuv) streams off a
  live ext4 disk over virtio-blk-pci (`ext4fs`/`ext4plus` + the block cache,
  ~15,000 block-cache fills - none of the binary or its libraries resident whole),
  its `ld-linux-x86-64.so.2` links all **seven** shared libraries (glibc +
  libstdc++ + libgcc_s), V8 initialises, libuv runs its event loop, it evaluates
  `console.log("rheo:"+(40+2))`, prints exactly `rheo:42`, and **exits 0** - on
  x86-64 (arm64/riscv64 have no node build and skip-with-reason). It runs
  `--jitless` so V8's Ignition interpreter needs no writable-executable code page
  (W^X, ARCHITECTURE.md 5, the one `mprotect(RWX)` V8 would issue is refused;
  host-verified that `--jitless` avoids it). This is the production JavaScript
  runtime Claude Code runs on, executing unmodified. Reaching it took two things,
  both measured by running the real binary and seeing exactly where it stopped:
  four legacy calls (`gettimeofday` - which libuv *asserts* on -, `clock_getres`,
  `time`, and io_uring refused deliberately), and **per-context blocking** (below),
  which was the real blocker - not a missing syscall.

- **Per-context blocking** (docs/LINUX-COMPAT.md L4, the scheduler change that made
  Node run). A cell holds up to 8 execution contexts (threads); before this, a
  proc-level blocking syscall (`epoll_wait`/`poll`/`nanosleep`/pipe/eventfd/
  console/`wait4`) parked the **whole cell**, freezing every sibling thread - fine
  for a single-threaded program, fatal for an event loop: Node's main thread blocks
  on `epoll_wait` for an eventfd a V8 worker must write, and parking the cell froze
  the worker, so the scheduler reported a genuine `DEADLOCK`. Now the block
  condition lives **per context** (`thread.rs` `pblock`, judged and completed by
  `proc.rs`): when a context blocks, the scheduler runs a `Ready` **sibling**
  context first, then a sibling that is **already satisfiable** (Node's teardown:
  main writes the eventfd, then futex-waits for the worker parked on it - the
  worker is resumed and `FUTEX_WAKE`s main), and only parks the whole cell (the
  pre-existing cross-cell path) when every context is blocked with no sibling
  runnable or satisfiable. A **single-context** cell has no sibling and falls
  straight to that cross-cell park - byte-for-byte the old behaviour, which is why
  the entire existing Linux suite (threads/futex/poll/signals/processes) stays
  green. The futex path integrates with it: a futex WAIT with no `Ready` sibling
  resumes a satisfiable proc-blocked sibling rather than spinning `EAGAIN`. Still
  cooperative and single-CPU (a compute-bound thread starves siblings until timer
  preemption, #27); one context per cell may block on `poll` at a time (the copied
  `pollset` is per-cell - `epoll`, which Node uses, has no such limit).

- **The real Bun binary loads and initialises to the concurrency frontier**
  (GOAL-BUN, task #175, the `linuxbun` test - a **partial** pass). The actual
  `/root/.bun/bin/bun` (v1.3, dynamic, 99 MB, JavaScriptCore + a Zig runtime)
  streams off a live ext4 disk over virtio-blk-pci (~3,500 block-cache fills, none
  resident whole), its `ld-linux` links the whole library set, and JSC initialises
  **including its 128 GiB Gigacage** (a single `MAP_NORESERVE` reservation the
  kernel now demand-fills - the mmap window was raised to 80..252 GiB for it, and a
  failed eager commit no longer leaks a phantom VMA), spawns a worker thread via
  **`clone3`** (now implemented), and sets up its libuv event loop. It runs
  `BUN_JSC_useJIT=0` (host-verified to issue zero RWX mappings, the JSC equivalent
  of `--jitless`). Then it `abort()`s (SIGABRT) **before evaluating** - and the
  cause is measured, not guessed: **every one of its 205 syscalls came from the
  main thread; the worker it spawned never got the CPU**. Our scheduler is
  cooperative single-CPU (it switches to a sibling only when the current context
  blocks), and Bun's main thread requires the worker to have made progress
  *concurrently* before it ever blocks. That is the **preemptive-SMP frontier**
  (task #132), not a missing syscall or a memory bug - the entire load path
  (streaming, demand paging, 7-library dynamic linking, the 128 GiB Gigacage,
  `clone3`, the event loop) works. The `linuxbun` harness accepts this specific,
  tightly-bounded partial (exit 134 **and** empty output). x86-64 only (no
  arm64/riscv64 bun build).

  **That attribution has since been tested, and it does not hold.** Timer preemption
  landed (docs/SUBSTRATE.md 15, S3'), the worker measurably **does** get the CPU when
  it is enabled - 66 preemptions taken, all of them to a sibling context of Bun's own
  cell - and Bun aborts **identically with preemption disabled**: same exit 134, same
  empty output, at the same point. So "the worker never got the CPU" was a true
  observation and a *wrong diagnosis*: it was the first difference anyone measured
  between Bun and Node, not the cause of the abort. What the cause is, is now
  genuinely unknown, and saying so is better than substituting the next plausible
  guess (docs/ENGINEERING.md 1). The prediction "when #132 lands it should print
  `rheo:42`" is withdrawn as disproven rather than quietly restated about a later
  milestone.

  The `linuxbun` boot therefore stays **cooperative** - that is the scheduler its
  partial is characterised against. Enabling preemption is not a no-op: Bun gets
  *further*, all the way to printing its startup banner, and then fails differently.
  Widening an accepted partial to cover a second unexplained failure would turn a
  bounded disclosure into a blanket one.

  The experiment paid for itself in four real defects, each fixed:

  - **The vector-register file was saved after the scheduler's bookkeeping** rather
    than before it. The kernel is soft-float, but that bounds the floating point it
    *emits*, not the vector registers `compiler_builtins`' `mem*` routines and
    ordinary struct moves use on x86-64 - so anything between the interrupt and the
    save clobbers what is about to be saved, and a preemption arrives at an arbitrary
    instruction inside the cell's own vector code. The symptom was not a fault at the
    switch: it was the *resumed* context computing with someone else's registers,
    which showed up as Bun dying with `Illegal instruction` at a nonsense address. The
    save is now the first action on every preemption path (a fourth path into the
    `SYS_YIELD` FP scar, docs/LIBRHEO.md).
  - **`getrusage` was refused**, and Bun printed `Sys: 8589934ms` from the `-ENOSYS`
    return reinterpreted as microseconds. A fabricated measurement is worse than a
    refusal, and a zeroed struct would be worse for the same reason, so it now reports
    the counters this kernel has (elapsed CPU as `utime` - there is no user/system
    split to report, and guessing a ratio would invent the distinction - the cell's own
    committed frames as `maxrss`, its fault count) with the rest 0 *because they are 0*.
  - **`MADV_DONTDUMP`/`MADV_DODUMP` were refused** where this OS can provide their
    entire observable effect: it produces no core dumps. JSC marks the 128 GiB
    Gigacage `MADV_DONTDUMP`, which is the sane thing to do with mostly-untouched
    address space.
  - **V8's JIT now runs**, through the capability-gated W^X exception
    (docs/ARCHITECTURE.md 5.1): `linuxnode` mints a `MemoryGrant` capability carrying
    `WRITE | EXECUTE` into the cell and runs the real `node` with **no `--jitless`**,
    so V8 tiers up to Sparkplug, gets its writable-executable code page, evaluates and
    exits 0. Every other kernel in the suite mints nothing of the sort and its RWX
    request is refused `-EPERM` with a printed reason exactly as before. The trace
    below is what forced that design and is kept as its evidence.
  - **V8's JIT reaches baseline compilation and dies at one call** *without* the
    capability. Running `node`
    *without* `--jitless` produces a V8 fatal whose native stack trace names the exact
    site: `Runtime_BytecodeBudgetInterrupt_Ignition` ->
    `BaselineBatchCompiler::CompileBatch` -> `Compiler::CompileBaseline` ->
    `BaselineCompiler::Build` -> `Factory::CodeBuilder::BuildInternal` ->
    `MemoryAllocator::AllocatePage` -> `SetPermissionsOnExecutableMemoryChunk` ->
    `v8::base::OS::SetPermissions`, which fatals on `Check failed: 12 == errno`. So
    V8 gets all the way through Ignition and into tiering up to Sparkplug before the
    single `mprotect(PROT_WRITE|PROT_EXEC)` is refused - the claim "the one
    `mprotect(RWX)` V8 would issue is refused" is now a cited trace rather than an
    assertion. Two things follow. V8 *requires* `ENOMEM` from a failed
    `SetPermissions` and fatals on anything else, so our `-EPERM` (the errno a
    hardened Linux returns) produces a hard abort rather than V8's own graceful
    path - and returning `ENOMEM` to steer a program down a nicer path would be
    fabricating a reason, so it is not done. And unmodified Node 22 cannot use a
    W->X flip or a dual mapping: `v8_enable_write_protect_code_memory` is a
    **compile-time** option in this build, so JIT here needs either a rebuilt V8 or a
    doctrine change to W^X (ARCHITECTURE.md 5), which is a decision the admission rule
    in ARCHITECTURE.md 6 reserves and which docs/ARCHITECTURE-DEBT.md 4.0 already
    flags as "deliberately not decided".
  - **The timer wheel's bulk re-file path skipped its trailing bucket expiry.** Taken
    when more than a level-0 revolution has elapsed with nothing serviced, it left an
    already-due timer to fire on the *next* service - after timers with later
    deadlines. It broke the one property the wheel exists to guarantee, and only after
    a long stall, so it presented as a rare load-dependent flake in `substrate`
    (observed failing while the host was building three ISAs) rather than as a bug. A
    transport would have applied a later RTO before an earlier one.

  **Node under preemption was intermittent - about one run in eight died with SIGSEGV
  and no output - and the cause was found and fixed.** It is worth recording as a
  worked example, because the wrong conclusion was available and cheap: "preemption is
  inherently risky, leave it off".

  The bug was on x86-64, in the choice of *return instruction*. `SYSRET` takes its RIP
  from RCX and its RFLAGS from R11 - it **consumes** them - which is exactly right for
  returning from a `syscall`, since the instruction is defined to clobber both, and
  exactly wrong for resuming a context that was stopped somewhere else. `syscall_entry`
  therefore never saved RCX/R11 into the frame at all, and `sysret_resume`
  reconstructed them from the rip/rflags slots.

  Before preemption that was airtight: every frame a *syscall* trap handed back was
  either the caller's own or a peer that had itself last stopped at a syscall, so
  RCX/R11 were clobberable in both cases. **A preempted context's frame is neither** -
  it is captured at an arbitrary instruction with those registers live - and if a
  sibling's `SYS_YIELD` (or any switching syscall) later selects it, the syscall
  trampoline is the path it comes back through. The symptom is not a fault at the
  switch: it is the resumed context computing with two wrong registers, which is why it
  presented as an occasional segfault with no pattern. This is the third appearance of
  the same family - the `iret_resume`-for-faults fix and the FP-save-ordering fix above
  are the other two - and the family is "a resume path that is correct for one
  provenance being used for another".

  The fix is by **provenance, not by tagging**: after the dispatcher returns, the
  trampoline compares the frame it is about to resume against the frame it entered on,
  and resumes a *different* one through `IRET`. There is no flag for a future producer
  of frames to forget to set. `syscall_entry` additionally writes the rcx/r11 slots with
  the values SYSRET would synthesise, so a syscall-origin frame resumes bit-identically
  either way and the comparison is the only thing that decides. The syscall fast path
  keeps SYSRET; only a switch pays for IRET. ARM64 and RISC-V need no equivalent, since
  `eret`/`sret` restore the whole register file and consume nothing.

  Proven in both directions by a new phase of the `preempt` kernel: a cell pins
  sentinels in RCX and R11 **inside one `asm!` block that also contains its spin loop**
  (so the compiler cannot spill and reload them around the window - the same discipline
  the FP/SIMD `SYS_YIELD` proof needed) while a sibling yields in a loop. With the fix
  the sentinels survive 54 cross-cell syscall resumes; with the comparison reverted the
  phase fails **deterministically**, which is what turns a one-in-eight flake into a
  property. The phase skips-with-reason on the two ISAs where the hazard cannot exist.

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

The **L8 socket fixtures** (`tests/linux-fixtures/{af_unix,inet}.c`, static-glibc
ET_EXEC via the same recipe as `chello`): `af_unix.c` (the `linuxunix` proof -
socketpair+fork + bind/listen/connect/accept over AF_UNIX) and `inet.c` (the
`linuxinet` proof - TCP + UDP + epoll over 127.0.0.1 and TCP over ::1, the
loopback AF_INET/AF_INET6 surface).

The **L7 dynamic fixtures** (`tests/linux-fixtures/dhello.c` + `dmath.c`, the
`linuxdyn` proof) are built **dynamically** - stock ET_DYN/PIE, no
`-static`/`-no-pie` (gcc's default) - so their `PT_INTERP` names the real
`ld-linux`. `dhello` links only `libc`; `dmath` (a `sqrt` program built
`-fno-builtin -lm`) links `libm` **as well**, so ld.so must load two shared
libraries and resolve one's versions against the other (the multi-library case,
GOAL-DYN-MULTILIB); `dcpp` (a C++ hello built with g++) links **libstdc++ +
libgcc_s + libc + libm** - four libraries plus C++ runtime init, the production
shape. Their runtime dependencies (the dynamic linker + `libc.so.6` + `libm.so.6`
+ `libstdc++.so.6` + `libgcc_s.so.1`) are **not built** but **copied from the
cross toolchain** at build time by xtask `build_dyn_fixture` into the gitignored
fixture build dir (never committed), and the `linuxdyn` test seeds them into a
ramfs `/lib` so ld.so resolves them. `dcpp` needs a cross-g++, absent for riscv64
in this environment, so its phase skips-with-reason there:

| ISA | dynamic C (gcc, PIE) | ld.so source (interp path) | libc.so.6 source |
|---|---|---|---|
| x86_64 | host `gcc` | `/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` (interp `/lib64/ld-linux-x86-64.so.2`) | `/lib/x86_64-linux-gnu/libc.so.6` |
| aarch64 | `aarch64-linux-gnu-gcc` | `/usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1` (interp `/lib/ld-linux-aarch64.so.1`) | `/usr/aarch64-linux-gnu/lib/libc.so.6` |
| riscv64 | `riscv64-linux-gnu-gcc` | `/usr/riscv64-linux-gnu/lib/ld-linux-riscv64-lp64d.so.1` (interp `/lib/ld-linux-riscv64-lp64d.so.1`) | `/usr/riscv64-linux-gnu/lib/libc.so.6` |

If a runtime `.so` is missing for an ISA, that ISA's dynamic fixture is
**skipped-with-reason** (a 1-byte placeholder is written; `linuxdyn` detects it
and skips), keeping the static L2-L6 coverage. `libm.so.6` (beside `libc.so.6`
in the same sysroot lib dir) gates only the **multi-library** phase the same way:
missing → that phase skips, single-library coverage stays. All three toolchains
are present in the build/CI environment here, so **all three ISAs genuinely
pass**. Note:
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
