//! The Linux personality (docs/LINUX-COMPAT.md). In the full design this is
//! a personality *cell* reached over queue pairs (POSIX-PERSONALITY.md 1);
//! it is kernel-resident here, like `svc.rs` - kernel-side handlers before
//! the service framework exists, running in trap context where the calling
//! cell's user memory is accessible. It adds no kernel object: PIDs, fds,
//! signal state, brk/mmap bookkeeping are per-cell synthesized state in this
//! module (`LinuxState`), and every underlying operation goes through the
//! cell's existing grants (the `svc::FileOps` VFS personality, the cell's
//! address space via `crate::user`, its DRBG).
//!
//! Dispatch reaches this module only for cells tagged `Personality::Linux`
//! (kernel/src/user.rs) - the branch happens *before* the syscall number is
//! interpreted, because native numbers collide with Linux numbers. Syscall
//! numbers are per-ISA constants from `arch::linux_abi` (two tables: x86-64
//! legacy, asm-generic shared by ARM64/RISC-V); this module is portable.
//!
//! The honesty policy (docs/LINUX-COMPAT.md 3) is enforced here: a syscall
//! returns success only when its observable semantics are provided; every
//! number without a handler logs `linux: ENOSYS nr=<n>` to the serial
//! console and returns -ENOSYS, so glibc/tool drift is visible, never a
//! silent hang.

pub mod dirent;
pub mod epoll;
pub mod errno;
pub mod fd;
pub mod inetsock;
pub mod mem;
pub mod pipe;
pub mod proc;
pub mod signal;
pub mod stack;
pub mod thread;
pub mod unixsock;

pub use signal::{FaultOutcome, deliver_fault};

use crate::arch::TrapFrame;
use crate::arch::linux_abi::nr;
use crate::user::MAX_CELLS;
use core::ptr::{addr_of, addr_of_mut};

/// Per-cell Linux personality state (docs/LINUX-COMPAT.md L2). Fixed-size, so
/// the kernel stays allocation-free.
/// Longest per-cell current working directory (docs/LINUX-COMPAT.md L3).
const CWD_MAX: usize = 256;

pub struct LinuxState {
    fds: fd::FdTable,
    brk_start: usize,
    brk_cur: usize,
    mmap_cursor: usize,
    tid_addr: u64,
    robust_list: u64,
    /// The cell's current working directory (getcwd/chdir; AT_FDCWD base).
    cwd: [u8; CWD_MAX],
    cwd_len: usize,
    initialized: bool,
}

impl LinuxState {
    const fn new() -> LinuxState {
        LinuxState {
            fds: fd::FdTable::new(),
            brk_start: 0,
            brk_cur: 0,
            mmap_cursor: 0,
            tid_addr: 0,
            robust_list: 0,
            cwd: [0; CWD_MAX],
            cwd_len: 0,
            initialized: false,
        }
    }
}

static mut LINUX_STATE: [LinuxState; MAX_CELLS] = [const { LinuxState::new() }; MAX_CELLS];

fn state(idx: usize) -> &'static mut LinuxState {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { &mut (*addr_of_mut!(LINUX_STATE))[idx] }
}

/// Initialize the Linux state for cell `idx`: console fds 0/1/2, the heap
/// base at the loaded image end, the mmap cursor at the per-cell region base.
/// Called by the test kernel after `load::load_elf_linux`, before `run`.
pub fn install_cell(idx: usize, image_end: usize) {
    let st = state(idx);
    st.fds.init_console();
    st.fds.set_auxv(stack::last_auxv());
    st.cwd_len = 1;
    st.cwd[0] = b'/';
    let brk =
        (image_end + crate::mm::frames::FRAME_SIZE - 1) & !(crate::mm::frames::FRAME_SIZE - 1);
    st.brk_start = brk;
    st.brk_cur = brk;
    st.mmap_cursor = mem::mmap_base();
    st.tid_addr = 0;
    st.robust_list = 0;
    st.initialized = true;
    // Seed the multi-context thread table with context 0 (docs/LINUX-COMPAT.md
    // L4), reusing the cell's installed frame.
    thread::init_cell(idx);
    // Seed the process entry: this is the top of the process tree (pid 1000,
    // docs/LINUX-COMPAT.md L6).
    proc::init_top(idx);
}

/// Clear all per-cell Linux state (called from `user::reset`).
pub fn reset() {
    for i in 0..MAX_CELLS {
        *state(i) = LinuxState::new();
    }
    thread::reset();
    signal::reset();
    pipe::reset();
    proc::reset();
    unixsock::reset();
    inetsock::reset();
    epoll::reset();
}

/// Deep-copy cell `from`'s Linux state into cell `to` (the `fork` inheritance
/// step, docs/LINUX-COMPAT.md L6): the child gets the parent's fd table, cwd,
/// brk/mmap bookkeeping, and auxv, then references the same pipes.
pub(crate) fn dup_state(from: usize, to: usize) {
    // SAFETY: single CPU, synchronous; `from != to`; both indices are in range.
    unsafe {
        let base = addr_of_mut!(LINUX_STATE) as *mut LinuxState;
        core::ptr::copy_nonoverlapping(base.add(from), base.add(to), 1);
    }
    state(to).fds.inherit_pipe_ends();
}

/// Reset cell `cell`'s memory bookkeeping for a fresh `execve` image
/// (docs/LINUX-COMPAT.md L6): new heap base + mmap cursor + auxv snapshot. The
/// fd table and cwd are kept (POSIX `execve` semantics) **except** descriptors
/// marked `FD_CLOEXEC`, which are closed here - that is what close-on-exec means,
/// and `execve` used to keep every descriptor regardless. The caller resets the
/// signal dispositions separately.
pub(crate) fn exec_reinit(cell: usize, image_end: usize) {
    let st = state(cell);
    let closed = st.fds.close_cloexec();
    if closed > 0 {
        crate::println!("linux: execve closed {closed} close-on-exec fd(s)");
    }
    st.fds.set_auxv(stack::last_auxv());
    let brk =
        (image_end + crate::mm::frames::FRAME_SIZE - 1) & !(crate::mm::frames::FRAME_SIZE - 1);
    st.brk_start = brk;
    st.brk_cur = brk;
    st.mmap_cursor = mem::mmap_base();
    st.tid_addr = 0;
    st.robust_list = 0;
    st.initialized = true;
}

// ------------------------------------------------------------- stdout tap

static mut STDOUT_TAP: Option<fn(&[u8])> = None;

/// Install a tap that receives every byte written to fd 1/2 (docs/
/// LINUX-COMPAT.md L2). The kernel keeps no always-on stdout buffer (honest);
/// the `linuxrun` test installs a tap into a capture buffer to assert exact
/// stdout. Pass None to remove it.
pub fn set_stdout_tap(tap: Option<fn(&[u8])>) {
    // SAFETY: single-threaded test setup between runs.
    unsafe { *addr_of_mut!(STDOUT_TAP) = tap };
}

/// Feed console output to the installed tap, if any.
pub(crate) fn tap_stdout(bytes: &[u8]) {
    // SAFETY: read of a function pointer set at test setup.
    if let Some(f) = unsafe { *addr_of!(STDOUT_TAP) } {
        f(bytes);
    }
}

// ------------------------------------------------------------- dispatch

/// What the dispatcher should do after a Linux syscall: resume the current
/// context with a return value, end the whole cell, or switch to another
/// context of the same cell (docs/LINUX-COMPAT.md L4).
pub enum Ctl {
    /// Write the value to the syscall return register and resume.
    Ret(u64),
    /// End the cell's run (`exit_group`, or the last thread's `exit`).
    Exit(u64),
    /// Resume a different context of this cell (futex/yield/thread-exit). The
    /// frame already carries its saved state and pending return value; the
    /// thread scheduler swapped FP/TLS before returning this.
    Switch(*mut TrapFrame),
}

/// Encode a negative errno as the u64 return-register value (Linux userspace
/// treats -1..-4095 as error).
pub(crate) fn err(e: i64) -> Ctl {
    Ctl::Ret((-e) as u64)
}

/// Turn an i64 result (>= 0 value, or -errno) into a `Ctl::Ret`.
pub(crate) fn ret(v: i64) -> Ctl {
    Ctl::Ret(v as u64)
}

/// Handle one Linux syscall for cell `cur`. `args` are the six raw argument
/// registers (already Linux-ordered by `arch::decode_syscall`); `frame` is the
/// calling context's saved state (the parent for `clone`).
pub fn handle(cur: usize, nr_val: u64, args: &[u64; 6], frame: *mut TrapFrame) -> Ctl {
    // Every pointer argument below is an address the **cell** chose, and the
    // kernel services this trap with the cell's root active, in which all of
    // kernel RAM is mapped supervisor-RWX. So the whole Linux ABI surface is
    // bounded here, at the single dispatch point, rather than at ~60 individual
    // dereferences (docs/ENGINEERING.md 12). A rejected address is `-EFAULT`,
    // exactly as Linux reports it.
    if !ptr_args_ok(nr_val, args) {
        return err(errno::EFAULT);
    }
    let st = state(cur);
    match nr_val {
        // -- I/O over the fd table (pipe fds may block cross-cell, L6) --
        nr::READ => sys_read(cur, st, args[0] as i64, args[1], args[2]),
        nr::WRITE => sys_write(cur, st, args[0] as i64, args[1], args[2], frame),
        nr::READV => ret(sys_readv(st, args[0] as i64, args[1], args[2], false)),
        nr::WRITEV => ret(sys_readv(st, args[0] as i64, args[1], args[2], true)),
        nr::OPENAT => ret(st
            .fds
            .openat(dirfd(args[0]), args[1], strlen(args[1]), args[2])),
        nr::CLOSE => ret(st.fds.close(args[0] as i64)),
        nr::LSEEK => ret(st.fds.lseek(args[0] as i64, args[1] as i64, args[2])),
        // pread64(fd, buf, count, offset): a positioned read (ld.so reads ELF
        // headers with it, docs/LINUX-COMPAT.md L7). VFS files only.
        nr::PREAD64 => ret(st
            .fds
            .pread(args[0] as i64, args[1], args[2], args[3] as i64)),
        nr::FSTAT => ret(st.fds.fstat(args[0] as i64, args[1])),
        nr::NEWFSTATAT => ret(sys_newfstatat(st, args)),
        nr::STATX => ret(sys_statx(st, args)),
        nr::GETDENTS64 => ret(st.fds.getdents64(args[0] as i64, args[1], args[2])),
        nr::DUP => ret(st.fds.dup(args[0] as i64)),
        nr::DUP3 => ret(st.fds.dup3(args[0] as i64, args[1] as i64)),
        nr::FCNTL => ret(st.fds.fcntl(args[0] as i64, args[1], args[2])),
        nr::IOCTL => ret(sys_ioctl(st, args[0] as i64, args[1], args[2])),
        nr::FACCESSAT | nr::FACCESSAT2 => ret(sys_faccessat(args[1])),
        // access(path, mode): x86-64 legacy; ld.so probes /etc/ld.so.preload
        // etc. The path is arg0 (no dirfd), docs/LINUX-COMPAT.md L7.
        nr::ACCESS => ret(sys_faccessat(args[0])),
        nr::READLINKAT => err(errno::ENOENT), // no symlinks in the VFS; /proc/self/exe unused
        nr::POLL | nr::PPOLL => ret(sys_poll(st, args[0], args[1])),

        // -- working directory (docs/LINUX-COMPAT.md L3) --
        nr::GETCWD => ret(sys_getcwd(st, args[0], args[1])),
        nr::CHDIR => ret(sys_chdir(st, args[0])),

        // -- memory --
        nr::BRK => Ctl::Ret(mem::brk(st, args[0])),
        // mmap(addr, len, prot, flags, fd, offset): anonymous + file-backed
        // MAP_PRIVATE (+ MAP_FIXED), docs/LINUX-COMPAT.md L7.
        nr::MMAP => ret(mem::mmap(
            st,
            args[0],
            args[1],
            args[2],
            args[3],
            args[4] as i64,
            args[5],
        )),
        nr::MREMAP => ret(mem::mremap(st, args[0], args[1], args[2], args[3])),
        nr::MUNMAP => ret(mem::munmap(args[0], args[1])),
        nr::MPROTECT => ret(mem::mprotect(args[0], args[1], args[2])),
        nr::MADVISE => Ctl::Ret(0), // advisory by specification

        // -- threads (multi-context cell, docs/LINUX-COMPAT.md L4) --
        // clone(flags, child_stack, parent_tid, {child_tid, tls}) - the last
        // two are swapped on CLONE_BACKWARDS ISAs (ARM64); the order is arch
        // ABI (crate::arch::CLONE_BACKWARDS), decoded here so thread::clone
        // stays portable.
        nr::CLONE => {
            // A `clone` without CLONE_VM is `fork` (a new process); with it, a
            // new thread in the same address space (docs/LINUX-COMPAT.md L6).
            if proc::is_fork(args[0]) {
                ret(proc::fork(cur, frame))
            } else {
                let (child_tid, tls) = if crate::arch::CLONE_BACKWARDS {
                    (args[4], args[3])
                } else {
                    (args[3], args[4])
                };
                ret(thread::clone(
                    cur, frame, args[0], args[1], args[2], child_tid, tls,
                ))
            }
        }
        // futex(uaddr, op, val, timeout, ...): arg 3 is the timespec (now honoured).
        nr::FUTEX => thread::futex(cur, args[0], args[1], args[2] as u32, args[3]),

        // -- processes (docs/LINUX-COMPAT.md L6) --
        // `fork`/`vfork` (x86-64 have dedicated numbers; asm-generic routes
        // through CLONE above). vfork is treated as fork (eager copy, no COW
        // share) - safe, just less lazy.
        nr::FORK | nr::VFORK => ret(proc::fork(cur, frame)),
        nr::EXECVE => proc::execve(cur, args[0], args[1], args[2], frame),
        nr::WAIT4 => proc::wait4(cur, args[0] as i64, args[1], args[2]),
        // pipe(fd) (x86-64 legacy) == pipe2(fd, 0); pipe2 is the generic form.
        nr::PIPE => ret(st.fds.pipe2(args[0], 0)),
        nr::PIPE2 => ret(st.fds.pipe2(args[0], args[1])),
        // dup2(old,new) (x86-64 legacy) == dup3(old,new,0).
        nr::DUP2 => ret(st.fds.dup3(args[0] as i64, args[1] as i64)),

        // -- AF_UNIX sockets (docs/LINUX-COMPAT.md L8) - no kernel object; the
        //    byte transport is the L6 cross-cell ring (linux::pipe). --
        nr::SOCKET => ret(st.fds.socket(args[0], args[1])),
        nr::SOCKETPAIR => ret(st.fds.socketpair(args[0], args[1], args[3])),
        nr::BIND => ret(st.fds.bind(args[0] as i64, args[1], args[2])),
        nr::LISTEN => ret(st.fds.listen(args[0] as i64)),
        // accept4's flags (arg3): SOCK_CLOEXEC honoured, SOCK_NONBLOCK deferred
        // (docs/LINUX-COMPAT.md, the `fcntl` row). Plain `accept` passes 0.
        nr::ACCEPT => ret(st.fds.accept(args[0] as i64, args[1], args[2], 0)),
        nr::ACCEPT4 => ret(st.fds.accept(args[0] as i64, args[1], args[2], args[3])),
        nr::CONNECT => ret(st.fds.connect(args[0] as i64, args[1], args[2])),
        nr::GETSOCKNAME => ret(st.fds.getsockname(args[0] as i64, args[1], args[2], false)),
        nr::GETPEERNAME => ret(st.fds.getsockname(args[0] as i64, args[1], args[2], true)),
        // sendto/recvfrom: a UDP datagram socket routes to the loopback datagram
        // path (with addresses); a connected stream socket ignores the address and
        // routes to the blocking write/read path (cross-cell wake + SIGPIPE).
        nr::SENDTO => {
            if st.fds.is_dgram(args[0] as i64) {
                ret(st
                    .fds
                    .sendto(args[0] as i64, args[1], args[2], args[4], args[5]))
            } else {
                sys_write(cur, st, args[0] as i64, args[1], args[2], frame)
            }
        }
        nr::RECVFROM => {
            if st.fds.is_dgram(args[0] as i64) {
                ret(st
                    .fds
                    .recvfrom(args[0] as i64, args[1], args[2], args[4], args[5]))
            } else {
                sys_read(cur, st, args[0] as i64, args[1], args[2])
            }
        }
        // epoll (L8-INET): a minimal level-triggered readiness surface.
        // epoll_create(size) takes a size *hint*, not flags; only epoll_create1
        // carries EPOLL_CLOEXEC.
        nr::EPOLL_CREATE => ret(st.fds.epoll_create(0)),
        nr::EPOLL_CREATE1 => ret(st.fds.epoll_create(args[0])),
        nr::EPOLL_CTL => ret(st
            .fds
            .epoll_ctl(args[0] as i64, args[1], args[2] as i64, args[3])),
        nr::EPOLL_WAIT | nr::EPOLL_PWAIT => {
            ret(st.fds.epoll_wait(args[0] as i64, args[1], args[2] as usize))
        }
        // sendmsg/recvmsg: gather/scatter over msg_iov (non-blocking; the fixture
        // uses read/write). SCM_RIGHTS ancillary data is deferred (L8).
        nr::SENDMSG => ret(sys_sendmsg(st, args[0] as i64, args[1], true)),
        nr::RECVMSG => ret(sys_sendmsg(st, args[0] as i64, args[1], false)),
        nr::SETSOCKOPT | nr::SHUTDOWN => Ctl::Ret(0),
        nr::GETSOCKOPT => ret(sys_getsockopt(args[3], args[4])),
        // process groups / sessions: recorded (single-session model, no job
        // control); the shell queries them but does not depend on the effect.
        nr::SETPGID | nr::SETSID => Ctl::Ret(0),
        nr::GETPGID | nr::GETSID => Ctl::Ret(proc::pid(cur) as u64),

        // -- process / thread lifetime --
        nr::EXIT => thread::exit_thread(cur, args[0]),
        nr::EXIT_GROUP => proc::exit_group(cur, args[0]),

        // -- identity (docs/LINUX-COMPAT.md 3: uid/gid 1000, no root) --
        nr::GETPID => Ctl::Ret(proc::pid(cur) as u64),
        nr::GETTID => Ctl::Ret(thread::gettid(cur)),
        nr::GETPPID => Ctl::Ret(proc::ppid(cur) as u64),
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ctl::Ret(1000),
        nr::UNAME => ret(sys_uname(args[0])),

        // -- time / entropy / scheduling --
        nr::CLOCK_GETTIME => ret(sys_clock_gettime(args[0], args[1])),
        nr::CLOCK_NANOSLEEP | nr::NANOSLEEP => Ctl::Ret(0), // return immediately
        nr::GETRANDOM => ret(sys_getrandom(args[0], args[1])),
        nr::SCHED_YIELD => thread::sched_yield(cur),
        nr::SCHED_GETAFFINITY => ret(sys_sched_getaffinity(args[1], args[2])),
        nr::PRCTL => ret(sys_prctl(args[0])),

        // -- resource limits --
        nr::PRLIMIT64 => ret(sys_prlimit64(args[1], args[2], args[3])),
        nr::GETRLIMIT => ret(sys_getrlimit(args[0], args[1])),

        // -- x86-64 thread pointer (ARM64/RISC-V set theirs in userspace) --
        nr::ARCH_PRCTL => sys_arch_prctl(cur, args[0], args[1]),

        // -- recorded (stored, success returned; enacted in a later milestone,
        //    docs/LINUX-COMPAT.md 3). Real signals are L5; threads are L4. --
        nr::SET_TID_ADDRESS => {
            st.tid_addr = args[0];
            // Record the calling context's clear_child_tid and report its tid
            // (docs/LINUX-COMPAT.md L4).
            Ctl::Ret(thread::set_tid_address(cur, args[0]) as u64)
        }
        nr::SET_ROBUST_LIST => {
            st.robust_list = args[0];
            Ctl::Ret(0)
        }

        // -- signals (real delivery by trap-frame rewrite, docs/LINUX-COMPAT.md
        //    L5). Dispositions are per-cell; masks/pending per-context. --
        nr::RT_SIGACTION => ret(signal::rt_sigaction(
            cur, args[0], args[1], args[2], args[3],
        )),
        nr::RT_SIGPROCMASK => signal::rt_sigprocmask(cur, args[0], args[1], args[2], frame),
        nr::SIGALTSTACK => ret(signal::sigaltstack(cur, args[0], args[1])),
        nr::RT_SIGRETURN => signal::rt_sigreturn(cur, frame),
        nr::KILL => signal::kill(cur, args[0] as i64, args[1], frame),
        nr::TGKILL => signal::tgkill(cur, args[0] as i64, args[1] as i64, args[2], frame),
        nr::TKILL => signal::tkill(cur, args[0] as i64, args[1], frame),
        nr::RT_SIGQUEUEINFO => {
            signal::rt_sigqueueinfo(cur, args[0] as i64, args[1], args[2], frame)
        }
        nr::RT_SIGTIMEDWAIT => ret(signal::rt_sigtimedwait()),

        other => {
            crate::println!("linux: ENOSYS nr={other}");
            err(errno::ENOSYS)
        }
    }
}

/// Bound every cell-supplied pointer argument of Linux syscall `nr_val`
/// against the calling cell's user VA range (docs/ENGINEERING.md 12). `false`
/// means the call is refused `-EFAULT` before any handler runs.
///
/// The table is deliberately in one place: the mapping from a syscall number to
/// "which arguments are pointers, and how long" is ABI knowledge, and spreading
/// it over the handlers is what let the whole surface go unchecked. Where the
/// length is itself an argument it is used, so a bogus length is rejected too -
/// that is what bounds `readv`'s and `poll`'s otherwise **unbounded**
/// `iovcnt`/`nfds` array walks. Otherwise the minimum is one byte: the address
/// bound is the security property, and guessing a struct size too large would
/// reject a legitimate caller.
///
/// `nr::*` resolves per ISA (`arch::linux_abi`), so this stays portable.
fn ptr_args_ok(nr_val: u64, args: &[u64; 6]) -> bool {
    // Required (must be a valid user range) / optional (0 = "not supplied").
    let rd = |i: usize, n: u64| crate::user::user_buf(args[i], n as usize).is_some();
    let wr = |i: usize, n: u64| crate::user::user_buf_mut(args[i], n as usize).is_some();
    let rd_opt = |i: usize, n: u64| args[i] == 0 || rd(i, n);
    let wr_opt = |i: usize, n: u64| args[i] == 0 || wr(i, n);
    match nr_val {
        // -- fd I/O: the length is an argument, so use it --
        nr::READ | nr::PREAD64 | nr::GETDENTS64 => wr(1, args[2]),
        nr::WRITE => rd(1, args[2]),
        // The iovec array itself; each entry's own base/len is checked by the
        // per-entry `read`/`write` below it (fd.rs), which routes through the
        // same helpers.
        nr::READV | nr::WRITEV => rd(1, args[2].saturating_mul(16)),
        nr::OPENAT | nr::FACCESSAT | nr::FACCESSAT2 => rd(1, 1),
        nr::ACCESS | nr::CHDIR => rd(0, 1),
        nr::FSTAT => wr(1, 1),
        nr::NEWFSTATAT => rd(1, 1) && wr(2, 1),
        nr::STATX => rd(1, 1) && wr(4, 1),
        nr::IOCTL => wr_opt(2, 8), // only TIOCGWINSZ writes; others ignore it
        nr::POLL | nr::PPOLL => args[1] == 0 || wr(0, args[1].saturating_mul(8)),
        nr::GETCWD => wr(0, args[1]),

        // -- threads / processes --
        // The futex word, plus - **only for the WAIT commands** - the optional
        // `struct timespec` timeout in arg 3. This has to be command-aware: for
        // FUTEX_WAKE and friends arg 3 is a plain *count*, and real callers reach
        // the syscall with that register simply left dirty (glibc's and Rust's wake
        // wrappers pass three arguments). Validating it unconditionally as a
        // pointer refuses a legitimate `FUTEX_WAKE` with -EFAULT, which silently
        // stops every waiter from being woken - observed as rayon-threaded `sort`
        // producing no output at all on ARM64 while the other two ISAs passed.
        nr::FUTEX => {
            const FUTEX_WAIT: u64 = 0;
            const FUTEX_WAIT_BITSET: u64 = 9;
            let cmd = args[1] & 0x7f;
            let takes_timespec = cmd == FUTEX_WAIT || cmd == FUTEX_WAIT_BITSET;
            rd(0, 4) && (!takes_timespec || rd_opt(3, 16))
        }
        nr::CLONE => wr_opt(2, 4), // parent_tid; child_tid is validated on use
        nr::EXECVE => rd(0, 1) && rd(1, 8) && rd_opt(2, 8),
        nr::WAIT4 => wr_opt(1, 4),
        nr::PIPE | nr::PIPE2 => wr(0, 8),
        nr::SET_TID_ADDRESS | nr::SET_ROBUST_LIST => wr_opt(0, 4),

        // -- sockets --
        nr::SOCKETPAIR => wr(3, 8),
        nr::BIND | nr::CONNECT => rd(1, 1),
        nr::ACCEPT | nr::ACCEPT4 | nr::GETSOCKNAME | nr::GETPEERNAME => {
            wr_opt(1, 1) && wr_opt(2, 4)
        }
        nr::SENDTO => rd(1, args[2]) && rd_opt(4, 1),
        nr::RECVFROM => wr(1, args[2]) && wr_opt(4, 1),
        nr::SENDMSG | nr::RECVMSG => rd(1, 1),
        nr::EPOLL_CTL => rd_opt(3, 1),
        nr::EPOLL_WAIT | nr::EPOLL_PWAIT => wr(1, 1),
        nr::GETSOCKOPT => wr_opt(3, 1) && wr_opt(4, 4),

        // -- identity / time / entropy / limits --
        nr::UNAME => wr(0, 6 * 65), // struct utsname: six 65-byte fields
        nr::CLOCK_GETTIME => wr(1, 16),
        nr::GETRANDOM => wr(0, args[1]),
        nr::SCHED_GETAFFINITY => wr(2, args[1]),
        nr::PRLIMIT64 => rd_opt(2, 16) && wr_opt(3, 16),
        nr::GETRLIMIT => wr(1, 16),

        // -- signals --
        nr::RT_SIGACTION => rd_opt(1, 1) && wr_opt(2, 1),
        nr::RT_SIGPROCMASK => rd_opt(1, 8) && wr_opt(2, 8),
        nr::SIGALTSTACK => rd_opt(0, 1) && wr_opt(1, 1),
        nr::RT_SIGQUEUEINFO => rd(2, 1),

        // Everything else takes no user pointer (or resolves one itself through
        // the same helpers): brk/mmap/munmap/mprotect/mremap take VAs the memory
        // code range-checks, arch_prctl(ARCH_SET_FS) takes a *value*, and the
        // identity/recorded/ENOSYS calls take scalars.
        _ => true,
    }
}

/// A Linux `*at` directory fd is a C `int`: only the low 32 bits are
/// meaningful, and callers routinely load AT_FDCWD (-100) with a 32-bit `mov`
/// that zero-extends to `0xffffff9c`. Sign-extend the low 32 bits so AT_FDCWD
/// and real (positive) fds are interpreted exactly as the Linux kernel does.
fn dirfd(v: u64) -> i64 {
    v as i32 as i64
}

/// Length of the NUL-terminated C string at user VA `va` (bounded).
fn strlen(va: u64) -> usize {
    if va == 0 {
        return 0;
    }
    // A NUL-terminated string is the one argument shape whose length the caller
    // does not state, so the entry check (`ptr_args_ok`) can only bound its first
    // byte. The scan therefore carries its own bound: it stops at the last byte
    // still inside the cell's readable range, so a string placed at the very top
    // of that range cannot walk the scan into the kernel half
    // (docs/ENGINEERING.md 12).
    let limit = crate::user::user_read_span(va, 4096);
    // SAFETY: trap context, and `[va, va+limit)` was range-checked above as
    // readable in the calling cell, whose address space is active.
    unsafe {
        let p = va as *const u8;
        let mut n = 0usize;
        while n < limit && p.add(n).read() != 0 {
            n += 1;
        }
        n
    }
}

/// SIGPIPE (asm-generic and x86 agree).
const SIGPIPE: u64 = 13;

/// read(fd, buf, count) with cross-cell pipe blocking (docs/LINUX-COMPAT.md L6).
/// A non-pipe fd goes straight through the fd table. A pipe read of an empty
/// buffer parks the caller when a peer can run (the writer), else returns
/// -EAGAIN (single-process fallback, matching L3).
fn sys_read(cur: usize, st: &mut LinuxState, fd: i64, buf: u64, count: u64) -> Ctl {
    // O_NONBLOCK (set through `fcntl(F_SETFL)`) means "never park me": skip the
    // cooperative block below and let the would-block case report -EAGAIN
    // (docs/LINUX-COMPAT.md, the `fcntl` row).
    let nb = st.fds.is_nonblock(fd);
    if let Some((idx, writer)) = st.fds.pipe_end(fd) {
        if writer {
            return err(errno::EBADF); // the write end is not readable
        }
        return match pipe::read(idx, buf, count) {
            pipe::ReadNb::Done(n) => ret(n),
            pipe::ReadNb::WouldBlock => {
                if !nb && proc::runnable_peer_exists(cur) {
                    proc::block_pipe_read(cur, buf, count, idx)
                } else {
                    err(errno::EAGAIN)
                }
            }
        };
    }
    // A connected socket reads its rx ring the same way (L8): the transport is an
    // L6 ring, so the cross-cell block/wake path is identical.
    if let Some(idx) = st.fds.sock_rx(fd) {
        return match pipe::read(idx, buf, count) {
            pipe::ReadNb::Done(n) => ret(n),
            pipe::ReadNb::WouldBlock => {
                if !nb && proc::runnable_peer_exists(cur) {
                    proc::block_pipe_read(cur, buf, count, idx)
                } else {
                    err(errno::EAGAIN)
                }
            }
        };
    }
    ret(st.fds.read(fd, buf, count))
}

/// write(fd, buf, count) with cross-cell pipe blocking + SIGPIPE
/// (docs/LINUX-COMPAT.md L6). Writing to a pipe with no readers raises SIGPIPE
/// (default disposition terminates the writer - the normal end of
/// `seq ... | head`); an ignored/handled SIGPIPE yields -EPIPE.
fn sys_write(
    cur: usize,
    st: &mut LinuxState,
    fd: i64,
    buf: u64,
    count: u64,
    frame: *mut TrapFrame,
) -> Ctl {
    let nb = st.fds.is_nonblock(fd);
    if let Some((idx, writer)) = st.fds.pipe_end(fd) {
        if !writer {
            return err(errno::EBADF); // the read end is not writable
        }
        return match pipe::write(idx, buf, count) {
            pipe::WriteNb::Done(n) => ret(n),
            pipe::WriteNb::WouldBlock => {
                if !nb && proc::runnable_peer_exists(cur) {
                    proc::block_pipe_write(cur, buf, count, idx)
                } else {
                    err(errno::EAGAIN)
                }
            }
            pipe::WriteNb::Epipe => {
                match signal::kill(cur, proc::pid(cur) as i64, SIGPIPE, frame) {
                    // Ignored/handled SIGPIPE (no termination) -> the write reports
                    // -EPIPE; a terminating/handler delivery returns that control.
                    Ctl::Ret(_) => err(errno::EPIPE),
                    other => other,
                }
            }
        };
    }
    // A connected socket writes its tx ring the same way (L8), with the same
    // cross-cell block/wake + SIGPIPE-on-no-readers behaviour.
    if let Some(idx) = st.fds.sock_tx(fd) {
        return match pipe::write(idx, buf, count) {
            pipe::WriteNb::Done(n) => ret(n),
            pipe::WriteNb::WouldBlock => {
                if !nb && proc::runnable_peer_exists(cur) {
                    proc::block_pipe_write(cur, buf, count, idx)
                } else {
                    err(errno::EAGAIN)
                }
            }
            pipe::WriteNb::Epipe => {
                match signal::kill(cur, proc::pid(cur) as i64, SIGPIPE, frame) {
                    Ctl::Ret(_) => err(errno::EPIPE),
                    other => other,
                }
            }
        };
    }
    ret(st.fds.write(fd, buf, count))
}

/// sendmsg/recvmsg over a socket fd (docs/LINUX-COMPAT.md L8): gather/scatter the
/// `msg_iov` array through the fd's non-blocking read/write. `msg_name` is ignored
/// (connected stream socket) and `msg_control` (SCM_RIGHTS) is **not** processed -
/// fd-passing is deferred; a non-empty control buffer is left untouched.
fn sys_sendmsg(st: &mut LinuxState, fd: i64, msg_va: u64, write: bool) -> i64 {
    #[repr(C)]
    struct MsgHdr {
        msg_name: u64,
        msg_namelen: u32,
        _pad: u32,
        msg_iov: u64,
        msg_iovlen: u64,
        msg_control: u64,
        msg_controllen: u64,
        msg_flags: i32,
    }
    if msg_va == 0 {
        return -errno::EFAULT;
    }
    // SAFETY: `msg_va` is a caller-provided `struct msghdr`.
    let hdr = unsafe { &*(msg_va as *const MsgHdr) };
    sys_readv(st, fd, hdr.msg_iov, hdr.msg_iovlen, write)
}

/// getsockopt(fd, level, optname, optval, optlen): report a zeroed value for the
/// common options (SO_ERROR etc.) so glibc's post-connect probe succeeds.
fn sys_getsockopt(optval_va: u64, optlen_va: u64) -> i64 {
    if optval_va != 0 && optlen_va != 0 {
        // SAFETY: caller-provided optlen (in) + optval buffer.
        let len = unsafe { (optlen_va as *const u32).read() } as usize;
        let n = len.min(4);
        unsafe { core::ptr::write_bytes(optval_va as *mut u8, 0, n) };
    }
    0
}

/// readv/writev: iterate the iovec array, calling the fd read/write per entry.
/// `write` selects the direction. Returns the total transferred or the first
/// error.
fn sys_readv(st: &mut LinuxState, fd: i64, iov_va: u64, iovcnt: u64, write: bool) -> i64 {
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct IoVec {
        base: u64,
        len: u64,
    }
    let mut total = 0i64;
    for i in 0..iovcnt as usize {
        // SAFETY: trap context; the iovec array lies in the cell's memory.
        let iov = unsafe { (iov_va as *const IoVec).add(i).read() };
        if iov.len == 0 {
            continue;
        }
        let r = if write {
            st.fds.write(fd, iov.base, iov.len)
        } else {
            st.fds.read(fd, iov.base, iov.len)
        };
        if r < 0 {
            return if total > 0 { total } else { r };
        }
        total += r;
        if (r as u64) < iov.len {
            break; // short transfer ends the vector
        }
    }
    total
}

/// poll/ppoll: non-blocking readiness check. The cell must never park, so
/// this returns immediately. For each pollfd a valid descriptor reports the
/// requested IN/OUT events as ready (console/regular fds are always ready
/// here); a closed descriptor reports POLLNVAL. Used by glibc/Rust startup to
/// verify the standard fds (`sanitize_standard_fds`), which is why answering
/// it matters - an ENOSYS here makes Rust std `abort` before `main`.
fn sys_poll(st: &mut LinuxState, fds_va: u64, nfds: u64) -> i64 {
    const POLLIN: i16 = 0x001;
    const POLLOUT: i16 = 0x004;
    const POLLNVAL: i16 = 0x020;
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    let mut ready = 0i64;
    for i in 0..nfds as usize {
        // SAFETY: the pollfd array lies in the calling cell's memory.
        let p = unsafe { &mut *(fds_va as *mut PollFd).add(i) };
        if p.fd < 0 {
            p.revents = 0;
            continue;
        }
        p.revents = if st.fds.is_open(p.fd as i64) {
            p.events & (POLLIN | POLLOUT)
        } else {
            POLLNVAL
        };
        if p.revents != 0 {
            ready += 1;
        }
    }
    ready
}

/// newfstatat(dirfd, path, statbuf, flags). AT_EMPTY_PATH (0x1000) with an
/// empty path degenerates to fstat(dirfd); otherwise an absolute path is
/// stat'd through the VFS.
fn sys_newfstatat(st: &mut LinuxState, args: &[u64; 6]) -> i64 {
    const AT_EMPTY_PATH: u64 = 0x1000;
    let path_len = strlen(args[1]);
    if path_len == 0 && args[3] & AT_EMPTY_PATH != 0 {
        return st.fds.fstat(dirfd(args[0]), args[2]);
    }
    let Some(o) = crate::svc::file_ops() else {
        return -errno::ENOENT;
    };
    let mut native = crate::abi::Stat { size: 0, kind: 0 };
    let r = (o.stat)(args[1], path_len as u64, &mut native as *mut _ as u64);
    if r < 0 {
        return r;
    }
    let mode = dirent::mode_for_kind(native.kind);
    let blocks = native.size.div_ceil(512);
    let stat =
        crate::arch::linux_abi::Stat::new(mode, native.size, 1, 1, 1000, 1000, 0, 4096, blocks, 0);
    // SAFETY: statbuf is a writable VA in the calling cell.
    unsafe { (args[2] as *mut crate::arch::linux_abi::Stat).write(stat) };
    0
}

/// The Linux `struct statx` (`include/uapi/linux/stat.h`). ABI-independent
/// (fixed layout on every ISA, unlike `struct stat`), so it lives in the
/// portable personality, not `arch::linux_abi`. 256 bytes; only the basic-stats
/// fields are filled (docs/LINUX-COMPAT.md L3).
#[repr(C)]
#[derive(Copy, Clone, Default)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    __reserved: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    __spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    __spare2: u64,
    __spare3: [u64; 12],
}

/// statx(dirfd, path, flags, mask, statxbuf): the modern stat. AT_EMPTY_PATH
/// with an empty path stats the fd (`dirfd`); otherwise the (absolute) path is
/// stat'd through the VFS. Rust `std`'s `File::metadata` issues `statx` directly
/// and does not fall back to `newfstatat`, so this must be answered for real
/// tools (docs/LINUX-COMPAT.md L3).
fn sys_statx(st: &mut LinuxState, args: &[u64; 6]) -> i64 {
    const AT_EMPTY_PATH: u64 = 0x1000;
    const STATX_BASIC_STATS: u32 = 0x0000_07ff;
    let path_len = strlen(args[1]);
    let (mode, size) = if path_len == 0 && args[2] & AT_EMPTY_PATH != 0 {
        match st.fds.mode_size(dirfd(args[0])) {
            Ok(v) => v,
            Err(e) => return e,
        }
    } else {
        let Some(o) = crate::svc::file_ops() else {
            return -errno::ENOENT;
        };
        let mut native = crate::abi::Stat { size: 0, kind: 0 };
        let r = (o.stat)(args[1], path_len as u64, &mut native as *mut _ as u64);
        if r < 0 {
            return r;
        }
        (dirent::mode_for_kind(native.kind), native.size)
    };
    let stx = Statx {
        stx_mask: STATX_BASIC_STATS,
        stx_blksize: 4096,
        stx_nlink: 1,
        stx_uid: 1000,
        stx_gid: 1000,
        stx_mode: mode as u16,
        stx_ino: 1,
        stx_size: size,
        stx_blocks: size.div_ceil(512),
        ..Default::default()
    };
    let Some(out) = crate::user::user_out::<Statx>(args[4]) else {
        return -errno::EFAULT;
    };
    // SAFETY: `out` was validated non-null, `Statx`-aligned and inside the
    // calling cell's user VA range; its address space is active.
    unsafe { out.write(stx) };
    0
}

/// ioctl: TIOCGWINSZ on a console fd reports an 80x24 window; every other
/// request is -ENOTTY (glibc's isatty/terminal probing then treats the fd as
/// a non-tty, so stdio is fully buffered and flushed at exit).
fn sys_ioctl(st: &mut LinuxState, fd: i64, req: u64, arg: u64) -> i64 {
    const TIOCGWINSZ: u64 = 0x5413;
    if req == TIOCGWINSZ && st.fds.is_console(fd) {
        #[repr(C)]
        struct WinSize {
            row: u16,
            col: u16,
            xpixel: u16,
            ypixel: u16,
        }
        // SAFETY: `arg` is a writable VA in the calling cell.
        unsafe {
            (arg as *mut WinSize).write(WinSize {
                row: 24,
                col: 80,
                xpixel: 0,
                ypixel: 0,
            })
        };
        return 0;
    }
    -errno::ENOTTY
}

/// faccessat/faccessat2: existence check via the VFS stat handler.
fn sys_faccessat(path_va: u64) -> i64 {
    let Some(o) = crate::svc::file_ops() else {
        return -errno::ENOENT;
    };
    let mut native = crate::abi::Stat { size: 0, kind: 0 };
    let r = (o.stat)(
        path_va,
        strlen(path_va) as u64,
        &mut native as *mut _ as u64,
    );
    if r < 0 { -errno::ENOENT } else { 0 }
}

/// getcwd(buf, size): copy the cell's cwd plus a NUL terminator into `buf`.
/// The Linux raw syscall returns the number of bytes written including the NUL;
/// -ERANGE if the buffer is too small (docs/LINUX-COMPAT.md L3).
fn sys_getcwd(st: &LinuxState, buf: u64, size: u64) -> i64 {
    let need = st.cwd_len + 1;
    if buf == 0 || (size as usize) < need {
        return -errno::ERANGE;
    }
    // SAFETY: `buf` is a writable range of at least `need` bytes in the cell.
    unsafe {
        core::ptr::copy_nonoverlapping(st.cwd.as_ptr(), buf as *mut u8, st.cwd_len);
        *(buf as *mut u8).add(st.cwd_len) = 0;
    }
    need as i64
}

/// chdir(path): set the cell's cwd. The path is stored verbatim (absolute paths
/// only in practice); path resolution against it is done by `openat`/AT_FDCWD.
fn sys_chdir(st: &mut LinuxState, path_va: u64) -> i64 {
    let len = strlen(path_va).min(CWD_MAX);
    if len == 0 {
        return -errno::ENOENT;
    }
    // SAFETY: `path_va` is a readable C string in the cell (bounded above).
    unsafe { core::ptr::copy_nonoverlapping(path_va as *const u8, st.cwd.as_mut_ptr(), len) };
    st.cwd_len = len;
    0
}

/// uname: fill `struct utsname` (six 65-byte fields). release is "6.6.0-rheo"
/// (glibc refuses to start below its built-in minimum kernel version; the
/// "-rheo" suffix is the disclosure, docs/LINUX-COMPAT.md 3).
fn sys_uname(buf: u64) -> i64 {
    const FIELD: usize = 65;
    let fields = [
        "Linux",
        "rheo",
        "6.6.0-rheo",
        "#1 rheo",
        crate::arch::LINUX_UNAME_MACHINE,
        "",
    ];
    for (i, f) in fields.iter().enumerate() {
        // SAFETY: `buf` points at a 6*65-byte utsname in the cell's memory.
        unsafe {
            let dst = (buf as *mut u8).add(i * FIELD);
            core::ptr::write_bytes(dst, 0, FIELD);
            let n = f.len().min(FIELD - 1);
            core::ptr::copy_nonoverlapping(f.as_ptr(), dst, n);
        }
    }
    0
}

/// The nanosecond reading a Linux cell sees from `clock_gettime` - the **cell's
/// own clock domain**. MONOTONIC is `arch::ticks_to_ns(time::monotonic())`;
/// REALTIME is that plus a fixed epoch (2023-11-14T22:13:20Z), because there is no
/// synced time source (docs/TIME-IDENTITY.md) - disclosed, not hidden.
///
/// This is the single definition of that domain, because more than one thing now
/// depends on it: `clock_gettime` reports it, and a `futex` absolute timeout is
/// computed by the *program* from it, so the kernel must compare deadlines in the
/// same domain (docs/ENGINEERING.md 11 - clock domains are not interchangeable;
/// on RISC-V this is the `cycle` CSR, not the timer's `time` CSR).
pub(crate) fn cell_clock_ns(realtime: bool) -> u64 {
    /// The fixed REALTIME epoch, in nanoseconds.
    const BOOT_EPOCH_NS: u64 = 1_700_000_000 * 1_000_000_000;
    let ns = crate::arch::ticks_to_ns(crate::time::monotonic());
    if realtime { ns + BOOT_EPOCH_NS } else { ns }
}

/// clock_gettime(clk_id, timespec). MONOTONIC uses the cycle counter via
/// `arch::ticks_to_ns`; REALTIME adds a fixed boot epoch (unsynced - the
/// value is monotonic but not true wall time, documented).
fn sys_clock_gettime(clk_id: u64, ts_va: u64) -> i64 {
    const CLOCK_REALTIME: u64 = 0;
    let ns = cell_clock_ns(clk_id == CLOCK_REALTIME);
    let secs = ns / 1_000_000_000;
    let nsec = ns % 1_000_000_000;
    // SAFETY: `ts_va` is a writable timespec (two i64) in the cell's memory.
    unsafe {
        let p = ts_va as *mut i64;
        p.write(secs as i64);
        p.add(1).write(nsec as i64);
    }
    0
}

/// getrandom(buf, count, flags): fill from the cell's DRBG (docs/
/// TIME-IDENTITY.md 4). Flags are ignored - the source never blocks.
fn sys_getrandom(buf: u64, count: u64) -> i64 {
    // SAFETY: `buf` is a writable range in the calling cell.
    let dst = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, count as usize) };
    crate::rng::derive_cell_drbg().fill_bytes(dst);
    count as i64
}

/// sched_getaffinity(pid, cpusetsize, mask): report a single online CPU. The
/// cooperative multi-context model runs on one core (docs/LINUX-COMPAT.md L4),
/// so `available_parallelism` reads 1 and thread pools (rayon) stay small and
/// deterministic. Returns the number of bytes written to `mask`.
fn sys_sched_getaffinity(cpusetsize: u64, mask: u64) -> i64 {
    let n = (cpusetsize as usize).min(128);
    if mask == 0 || n == 0 {
        return -errno::EINVAL;
    }
    // SAFETY: `mask` is a writable range of `n` bytes in the calling cell.
    unsafe {
        core::ptr::write_bytes(mask as *mut u8, 0, n);
        *(mask as *mut u8) = 1; // CPU 0 online
    }
    n as i64
}

/// prctl(option, ...): only thread naming (PR_SET_NAME/PR_GET_NAME) is
/// accepted, as a cosmetic no-op - rayon names its worker threads and treats a
/// failure as fatal (docs/LINUX-COMPAT.md L4, allowlisted). Every other option
/// is -ENOSYS.
fn sys_prctl(option: u64) -> i64 {
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    match option {
        PR_SET_NAME | PR_GET_NAME => 0,
        _ => -errno::ENOSYS,
    }
}

#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;

/// The limit the personality reports for a resource (docs/LINUX-COMPAT.md 3):
/// STACK = the size of the stack actually mapped (glibc sizes thread stacks
/// from it, so reporting more than is mapped would hand every thread a stack
/// that faults), NOFILE = the fd table size, everything else unlimited.
fn rlimit_for(resource: u64) -> RLimit {
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    /// The mapped initial-stack size - the one number, not a second guess at it.
    const STACK_BYTES: u64 = (stack::LINUX_STACK_PAGES * crate::mm::frames::FRAME_SIZE) as u64;
    match resource {
        RLIMIT_STACK => RLimit {
            cur: STACK_BYTES,
            max: STACK_BYTES,
        },
        RLIMIT_NOFILE => RLimit {
            cur: fd::NFD as u64,
            max: fd::NFD as u64,
        },
        _ => RLimit {
            cur: RLIM_INFINITY,
            max: RLIM_INFINITY,
        },
    }
}

/// prlimit64(pid, resource, new, old): report the (fixed) limit into `old`;
/// `new` is ignored (limits are not settable).
fn sys_prlimit64(resource: u64, _new: u64, old: u64) -> i64 {
    if old != 0 {
        // SAFETY: `old` is a writable rlimit in the cell's memory.
        unsafe { (old as *mut RLimit).write(rlimit_for(resource)) };
    }
    0
}

/// getrlimit(resource, old): report the (fixed) limit.
fn sys_getrlimit(resource: u64, old: u64) -> i64 {
    if old != 0 {
        // SAFETY: `old` is a writable rlimit in the cell's memory.
        unsafe { (old as *mut RLimit).write(rlimit_for(resource)) };
    }
    0
}

// arch_prctl codes (x86-64 asm/prctl.h).
const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;

/// arch_prctl(code, addr) - x86-64 only (docs/LINUX-COMPAT.md L1). glibc's
/// x86-64 startup sets the thread pointer with `ARCH_SET_FS` before any TLS
/// access. ARM64/RISC-V never reach here (no such number in their table; they
/// set the thread pointer in userspace). SET_FS programs the FS_BASE MSR;
/// GET_FS writes the current base to `*addr`.
fn sys_arch_prctl(cur: usize, code: u64, addr: u64) -> Ctl {
    match code {
        ARCH_SET_FS => {
            // Record it as this context's TLS base so it is reloaded when the
            // context is next scheduled (docs/LINUX-COMPAT.md L4), then program
            // the MSR now for the running context.
            thread::set_current_fs_base(cur, addr);
            crate::arch::set_user_fs_base(addr);
            Ctl::Ret(0)
        }
        ARCH_GET_FS => {
            // SAFETY: trap context; `addr` is a writable VA in the cell.
            unsafe { (addr as *mut u64).write(crate::arch::user_fs_base()) };
            Ctl::Ret(0)
        }
        _ => err(errno::EINVAL),
    }
}
