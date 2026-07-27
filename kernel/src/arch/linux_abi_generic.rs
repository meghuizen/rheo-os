//! Linux personality ABI constants for the **asm-generic** syscall table
//! (docs/LINUX-COMPAT.md 2) - shared by ARM64 and RISC-V 64, which use the
//! same numbers. Included by both arch modules via `#[path]`; it lives in
//! the arch layer because syscall numbers are ISA ABI, even when two ISAs
//! happen to share them (TARGET-ARCHITECTURES.md 4, "Linux personality ABI").
//!
//! Source of truth: Linux v6.6 `include/uapi/asm-generic/unistd.h`. Only the
//! numbers the personality implements or deliberately answers with ENOSYS
//! are listed (the honesty table in docs/LINUX-COMPAT.md 3) - unknown
//! numbers fall through to the logged-ENOSYS path either way.

/// Syscall numbers (asm-generic table).
pub mod nr {
    pub const GETCWD: u64 = 17;
    pub const DUP: u64 = 23;
    pub const DUP3: u64 = 24;
    pub const FCNTL: u64 = 25;
    pub const IOCTL: u64 = 29;
    pub const MKDIRAT: u64 = 34;
    pub const UNLINKAT: u64 = 35;
    pub const RENAMEAT: u64 = 38;
    pub const FACCESSAT: u64 = 48;
    pub const CHDIR: u64 = 49;
    // AF_UNIX sockets (docs/LINUX-COMPAT.md L8). asm-generic numbers, shared by
    // ARM64 and RISC-V.
    pub const SOCKET: u64 = 198;
    pub const SOCKETPAIR: u64 = 199;
    pub const BIND: u64 = 200;
    pub const LISTEN: u64 = 201;
    pub const ACCEPT: u64 = 202;
    pub const CONNECT: u64 = 203;
    pub const GETSOCKNAME: u64 = 204;
    pub const GETPEERNAME: u64 = 205;
    pub const SENDTO: u64 = 206;
    pub const RECVFROM: u64 = 207;
    pub const SETSOCKOPT: u64 = 208;
    pub const GETSOCKOPT: u64 = 209;
    pub const SHUTDOWN: u64 = 210;
    pub const SENDMSG: u64 = 211;
    pub const RECVMSG: u64 = 212;
    pub const ACCEPT4: u64 = 242;
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const PIPE2: u64 = 59;
    pub const GETDENTS64: u64 = 61;
    pub const LSEEK: u64 = 62;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const PREAD64: u64 = 67;
    pub const READV: u64 = 65;
    pub const WRITEV: u64 = 66;
    pub const PPOLL: u64 = 73;
    pub const READLINKAT: u64 = 78;
    pub const NEWFSTATAT: u64 = 79;
    pub const FSTAT: u64 = 80;
    pub const FSYNC: u64 = 82;
    pub const FDATASYNC: u64 = 83;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const SET_TID_ADDRESS: u64 = 96;
    pub const FUTEX: u64 = 98;
    pub const SET_ROBUST_LIST: u64 = 99;
    pub const NANOSLEEP: u64 = 101;
    pub const CLOCK_GETTIME: u64 = 113;
    pub const CLOCK_NANOSLEEP: u64 = 115;
    pub const SCHED_GETAFFINITY: u64 = 123;
    pub const SCHED_YIELD: u64 = 124;
    pub const KILL: u64 = 129;
    pub const TKILL: u64 = 130;
    pub const TGKILL: u64 = 131;
    pub const SIGALTSTACK: u64 = 132;
    pub const RT_SIGACTION: u64 = 134;
    pub const PRCTL: u64 = 167;
    pub const RT_SIGPROCMASK: u64 = 135;
    pub const RT_SIGTIMEDWAIT: u64 = 137;
    pub const RT_SIGQUEUEINFO: u64 = 138;
    pub const RT_SIGRETURN: u64 = 139;
    pub const UNAME: u64 = 160;
    pub const GETRLIMIT: u64 = 163;
    pub const GETPID: u64 = 172;
    pub const GETPPID: u64 = 173;
    pub const GETUID: u64 = 174;
    pub const GETEUID: u64 = 175;
    pub const GETGID: u64 = 176;
    pub const GETEGID: u64 = 177;
    pub const GETTID: u64 = 178;
    pub const BRK: u64 = 214;
    pub const MUNMAP: u64 = 215;
    pub const MREMAP: u64 = 216;
    pub const CLONE: u64 = 220;
    pub const EXECVE: u64 = 221;
    pub const MMAP: u64 = 222;
    pub const MPROTECT: u64 = 226;
    pub const MADVISE: u64 = 233;
    pub const SETPGID: u64 = 154;
    pub const GETPGID: u64 = 155;
    pub const GETSID: u64 = 156;
    pub const SETSID: u64 = 157;
    pub const WAIT4: u64 = 260;
    pub const PRLIMIT64: u64 = 261;
    pub const GETRANDOM: u64 = 278;
    pub const STATX: u64 = 291;
    pub const RSEQ: u64 = 293;
    pub const CLONE3: u64 = 435;
    /// io_uring - the async-IO submission mechanism (numbers shared with the
    /// x86-64 table, added after the split). rheo-net's async path is the
    /// queue-pair reactor, not io_uring; refused ENOSYS so libuv falls back to
    /// epoll+threadpool (docs/LINUX-COMPAT.md). Node/libuv probes `setup` first.
    pub const IO_URING_SETUP: u64 = 425;
    pub const IO_URING_ENTER: u64 = 426;
    pub const IO_URING_REGISTER: u64 = 427;
    pub const FACCESSAT2: u64 = 439;

    // Measured from the real Claude Code startup trace
    // (docs/ARCHITECTURE-DEBT.md 4.0, blocker 3).
    pub const SYSINFO: u64 = 179;
    pub const SCHED_SETSCHEDULER: u64 = 119;
    pub const SCHED_GETSCHEDULER: u64 = 120;
    pub const SCHED_GET_PRIORITY_MAX: u64 = 125;
    pub const SCHED_GET_PRIORITY_MIN: u64 = 126;
    pub const EVENTFD2: u64 = 19;
    pub const CLOSE_RANGE: u64 = 436;

    // capget: a non-root process's capability query (docs/LINUX-COMPAT.md). Node
    // probes it at startup. asm-generic number.
    pub const CAPGET: u64 = 90;

    // timerfd (docs/LINUX-COMPAT.md L8-TIMERFD). asm-generic numbers.
    pub const TIMERFD_CREATE: u64 = 85;
    pub const TIMERFD_SETTIME: u64 = 86;
    pub const TIMERFD_GETTIME: u64 = 87;

    /// Not part of this table: x86-64's legacy `open` (asm-generic ISAs have only
    /// `openat`). Named as unreachable so portable dispatch can list it, like
    /// `ACCESS` and `READLINK`.
    pub const OPEN: u64 = u64::MAX - 10;

    // epoll (docs/LINUX-COMPAT.md L8-INET). asm-generic has create1/ctl/pwait
    // only (no legacy create/wait; those are named as unreachable sentinels
    // below so portable dispatch can list them).
    pub const EPOLL_CREATE1: u64 = 20;
    pub const EPOLL_CTL: u64 = 21;
    pub const EPOLL_PWAIT: u64 = 22;

    /// Not part of this table: x86-64's legacy `epoll_create`/`epoll_wait`
    /// (asm-generic ISAs use `epoll_create1`/`epoll_pwait`). Named as unreachable
    /// sentinels so portable dispatch can list them (docs/LINUX-COMPAT.md L8-INET).
    pub const EPOLL_CREATE: u64 = u64::MAX - 7;
    pub const EPOLL_WAIT: u64 = u64::MAX - 8;

    /// Not part of this table: x86-64's `arch_prctl`. Present on every ISA's
    /// `nr` so portable dispatch can name it; a number no program can issue
    /// here (the table tops out below it), so it can never collide.
    pub const ARCH_PRCTL: u64 = u64::MAX;

    /// Not part of this table: the legacy `poll` (asm-generic ISAs use
    /// `ppoll`). Named as unreachable so portable dispatch can list it, like
    /// `ARCH_PRCTL`.
    pub const POLL: u64 = u64::MAX - 1;

    // Not part of this table: x86-64's legacy `fork`/`vfork`/`pipe`/`dup2`.
    // The asm-generic ISAs route `fork` through `clone` (no CLONE_VM), `pipe`
    // through `pipe2`, and `dup2` through `dup3`. Named as unreachable so
    // portable dispatch can list them (like `ARCH_PRCTL`); no program can issue
    // these numbers here, so they never collide (docs/LINUX-COMPAT.md L6).
    pub const FORK: u64 = u64::MAX - 2;
    pub const VFORK: u64 = u64::MAX - 3;
    pub const PIPE: u64 = u64::MAX - 4;
    pub const DUP2: u64 = u64::MAX - 5;

    /// Not part of this table: x86-64's legacy `access` (asm-generic ISAs use
    /// `faccessat`). Named as unreachable so portable dispatch can list it;
    /// no program can issue this number here (docs/LINUX-COMPAT.md L7).
    pub const ACCESS: u64 = u64::MAX - 6;
    /// No legacy `readlink` in the asm-generic table (only `readlinkat`), so this
    /// is an unreachable sentinel - the dispatch arm compiles on every ISA and
    /// matches only where the number is real.
    pub const READLINK: u64 = u64::MAX - 9;
}

/// The asm-generic `struct epoll_event` is naturally aligned (16 bytes; the
/// `data` u64 sits at offset 8 after 4 bytes of padding). x86-64 packs it to 12
/// bytes (docs/LINUX-COMPAT.md L8-INET). Per-ISA ABI, so it lives in the arch
/// layer, shared by ARM64 and RISC-V.
pub const EPOLL_EVENT_SIZE: usize = 16;
pub const EPOLL_EVENT_DATA_OFFSET: usize = 8;

/// The asm-generic `struct stat` (docs/LINUX-COMPAT.md L2), shared by ARM64
/// and RISC-V. 128 bytes; layout per Linux v6.6
/// `include/uapi/asm-generic/stat.h`. x86-64 has its own 144-byte layout in
/// `arch/x86_64/linux_abi.rs`; portable dispatch only ever names
/// `crate::arch::linux_abi::Stat`, so the two never mix.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Stat {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    __pad1: u64,
    st_size: i64,
    st_blksize: i32,
    __pad2: i32,
    st_blocks: i64,
    st_atime: i64,
    st_atime_nsec: u64,
    st_mtime: i64,
    st_mtime_nsec: u64,
    st_ctime: i64,
    st_ctime_nsec: u64,
    __unused4: u32,
    __unused5: u32,
}

impl Stat {
    /// Build a `struct stat` from the fields the personality synthesizes
    /// (docs/LINUX-COMPAT.md L2). Same signature as the x86-64 constructor, so
    /// portable dispatch is identical across ABIs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: u32,
        size: u64,
        ino: u64,
        nlink: u64,
        uid: u32,
        gid: u32,
        rdev: u64,
        blksize: u64,
        blocks: u64,
        mtime_ns: u64,
    ) -> Stat {
        let secs = (mtime_ns / 1_000_000_000) as i64;
        let nsec = mtime_ns % 1_000_000_000;
        Stat {
            st_dev: 0,
            st_ino: ino,
            st_mode: mode,
            st_nlink: nlink as u32,
            st_uid: uid,
            st_gid: gid,
            st_rdev: rdev,
            __pad1: 0,
            st_size: size as i64,
            st_blksize: blksize as i32,
            __pad2: 0,
            st_blocks: blocks as i64,
            st_atime: secs,
            st_atime_nsec: nsec,
            st_mtime: secs,
            st_mtime_nsec: nsec,
            st_ctime: secs,
            st_ctime_nsec: nsec,
            __unused4: 0,
            __unused5: 0,
        }
    }
}

/// Every x86-64-only verb is represented here by an **unreachable sentinel** so
/// one portable `match` compiles on all three ISAs. The scheme has a sharp edge:
/// two sentinels with the same value make the *second* arm dead code, silently.
/// That happened - `READLINK` was added as `MAX - 7`, colliding with
/// `EPOLL_CREATE`, which made `epoll_create` unreachable on this table. Clippy
/// reports it as an unreachable pattern, but only if clippy runs after the
/// constant is added, and the boot tests did not exercise the shadowed arm.
///
/// So the invariant is now checked at compile time instead of by review.
const _: () = {
    let all = [
        nr::POLL,
        nr::FORK,
        nr::VFORK,
        nr::PIPE,
        nr::DUP2,
        nr::ACCESS,
        nr::READLINK,
        nr::EPOLL_CREATE,
        nr::EPOLL_WAIT,
        nr::OPEN,
    ];
    let mut i = 0;
    while i < all.len() {
        let mut j = i + 1;
        while j < all.len() {
            assert!(all[i] != all[j], "duplicate asm-generic syscall sentinel");
            j += 1;
        }
        i += 1;
    }
};
