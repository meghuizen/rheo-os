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
pub mod errno;
pub mod fd;
pub mod mem;
pub mod stack;

use crate::arch::linux_abi::nr;
use crate::user::MAX_CELLS;
use core::ptr::{addr_of, addr_of_mut};

/// Per-cell Linux personality state (docs/LINUX-COMPAT.md L2). Fixed-size, so
/// the kernel stays allocation-free.
pub struct LinuxState {
    fds: fd::FdTable,
    brk_start: usize,
    brk_cur: usize,
    mmap_cursor: usize,
    tid_addr: u64,
    robust_list: u64,
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
    let brk =
        (image_end + crate::mm::frames::FRAME_SIZE - 1) & !(crate::mm::frames::FRAME_SIZE - 1);
    st.brk_start = brk;
    st.brk_cur = brk;
    st.mmap_cursor = mem::mmap_base();
    st.tid_addr = 0;
    st.robust_list = 0;
    st.initialized = true;
}

/// Clear all per-cell Linux state (called from `user::reset`).
pub fn reset() {
    for i in 0..MAX_CELLS {
        *state(i) = LinuxState::new();
    }
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

/// What the dispatcher should do after a Linux syscall: resume the cell with
/// a return value, or end its run with an exit code.
pub enum Ctl {
    /// Write the value to the syscall return register and resume.
    Ret(u64),
    /// End the cell's run (`exit` / `exit_group`) with this code.
    Exit(u64),
}

/// Encode a negative errno as the u64 return-register value (Linux userspace
/// treats -1..-4095 as error).
fn err(e: i64) -> Ctl {
    Ctl::Ret((-e) as u64)
}

/// Turn an i64 result (>= 0 value, or -errno) into a `Ctl::Ret`.
fn ret(v: i64) -> Ctl {
    Ctl::Ret(v as u64)
}

/// Handle one Linux syscall for cell `cur`. `args` are the six raw argument
/// registers (already Linux-ordered by `arch::decode_syscall`).
pub fn handle(cur: usize, nr_val: u64, args: &[u64; 6]) -> Ctl {
    let st = state(cur);
    match nr_val {
        // -- I/O over the fd table --
        nr::READ => ret(st.fds.read(args[0] as i64, args[1], args[2])),
        nr::WRITE => ret(st.fds.write(args[0] as i64, args[1], args[2])),
        nr::READV => ret(sys_readv(st, args[0] as i64, args[1], args[2], false)),
        nr::WRITEV => ret(sys_readv(st, args[0] as i64, args[1], args[2], true)),
        nr::OPENAT => ret(st
            .fds
            .openat(args[0] as i64, args[1], strlen(args[1]), args[2])),
        nr::CLOSE => ret(st.fds.close(args[0] as i64)),
        nr::LSEEK => ret(st.fds.lseek(args[0] as i64, args[1] as i64, args[2])),
        nr::FSTAT => ret(st.fds.fstat(args[0] as i64, args[1])),
        nr::NEWFSTATAT => ret(sys_newfstatat(st, args)),
        nr::GETDENTS64 => ret(st.fds.getdents64(args[0] as i64, args[1], args[2])),
        nr::DUP => ret(st.fds.dup(args[0] as i64)),
        nr::DUP3 => ret(st.fds.dup3(args[0] as i64, args[1] as i64)),
        nr::FCNTL => ret(st.fds.fcntl(args[0] as i64, args[1], args[2])),
        nr::IOCTL => ret(sys_ioctl(st, args[0] as i64, args[1], args[2])),
        nr::FACCESSAT | nr::FACCESSAT2 => ret(sys_faccessat(args[1])),
        nr::READLINKAT => err(errno::ENOENT), // /proc/self/exe etc. - L3
        nr::POLL | nr::PPOLL => ret(sys_poll(st, args[0], args[1])),

        // -- memory --
        nr::BRK => Ctl::Ret(mem::brk(st, args[0])),
        nr::MMAP => ret(mem::mmap(st, args[1], args[2], args[3])),
        nr::MUNMAP => ret(mem::munmap(args[0], args[1])),
        nr::MPROTECT => ret(mem::mprotect(args[0], args[1], args[2])),
        nr::MADVISE => Ctl::Ret(0), // advisory by specification

        // -- process lifetime --
        nr::EXIT | nr::EXIT_GROUP => Ctl::Exit(args[0]),

        // -- identity (docs/LINUX-COMPAT.md 3: pid/uid/gid 1000, no root) --
        nr::GETPID | nr::GETTID => Ctl::Ret(1000),
        nr::GETPPID => Ctl::Ret(0),
        nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => Ctl::Ret(1000),
        nr::UNAME => ret(sys_uname(args[0])),

        // -- time / entropy / scheduling --
        nr::CLOCK_GETTIME => ret(sys_clock_gettime(args[0], args[1])),
        nr::CLOCK_NANOSLEEP | nr::NANOSLEEP => Ctl::Ret(0), // return immediately
        nr::GETRANDOM => ret(sys_getrandom(args[0], args[1])),
        nr::SCHED_YIELD => Ctl::Ret(0),

        // -- resource limits --
        nr::PRLIMIT64 => ret(sys_prlimit64(args[1], args[2], args[3])),
        nr::GETRLIMIT => ret(sys_getrlimit(args[0], args[1])),

        // -- x86-64 thread pointer (ARM64/RISC-V set theirs in userspace) --
        nr::ARCH_PRCTL => sys_arch_prctl(args[0], args[1]),

        // -- recorded (stored, success returned; enacted in a later milestone,
        //    docs/LINUX-COMPAT.md 3). Real signals are L5; threads are L4. --
        nr::SET_TID_ADDRESS => {
            st.tid_addr = args[0];
            Ctl::Ret(1000)
        }
        nr::SET_ROBUST_LIST => {
            st.robust_list = args[0];
            Ctl::Ret(0)
        }
        nr::RT_SIGACTION | nr::RT_SIGPROCMASK | nr::SIGALTSTACK => Ctl::Ret(0),

        other => {
            crate::println!("linux: ENOSYS nr={other}");
            err(errno::ENOSYS)
        }
    }
}

/// Length of the NUL-terminated C string at user VA `va` (bounded).
fn strlen(va: u64) -> usize {
    if va == 0 {
        return 0;
    }
    // SAFETY: trap context; `va` is a VA in the calling cell. Bounded scan.
    unsafe {
        let p = va as *const u8;
        let mut n = 0usize;
        while n < 4096 && p.add(n).read() != 0 {
            n += 1;
        }
        n
    }
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
        return st.fds.fstat(args[0] as i64, args[2]);
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

/// clock_gettime(clk_id, timespec). MONOTONIC uses the cycle counter via
/// `arch::ticks_to_ns`; REALTIME adds a fixed boot epoch (unsynced - the
/// value is monotonic but not true wall time, documented).
fn sys_clock_gettime(clk_id: u64, ts_va: u64) -> i64 {
    const CLOCK_REALTIME: u64 = 0;
    // A fixed epoch so REALTIME is plausible (2023-11-14T22:13:20Z); there is
    // no synced time source (docs/TIME-IDENTITY.md), so this is disclosed.
    const BOOT_EPOCH_SECS: u64 = 1_700_000_000;
    let ns = crate::arch::ticks_to_ns(crate::time::monotonic());
    let mut secs = ns / 1_000_000_000;
    let nsec = ns % 1_000_000_000;
    if clk_id == CLOCK_REALTIME {
        secs += BOOT_EPOCH_SECS;
    }
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

#[repr(C)]
struct RLimit {
    cur: u64,
    max: u64,
}

const RLIM_INFINITY: u64 = u64::MAX;

/// The limit the personality reports for a resource (docs/LINUX-COMPAT.md 3):
/// STACK 1 MiB (glibc sizes thread stacks from it), NOFILE = the fd table
/// size, everything else unlimited.
fn rlimit_for(resource: u64) -> RLimit {
    const RLIMIT_STACK: u64 = 3;
    const RLIMIT_NOFILE: u64 = 7;
    match resource {
        RLIMIT_STACK => RLimit {
            cur: 1024 * 1024,
            max: 1024 * 1024,
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
fn sys_arch_prctl(code: u64, addr: u64) -> Ctl {
    match code {
        ARCH_SET_FS => {
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
