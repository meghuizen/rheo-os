//! Linux personality ABI constants for the **x86-64 legacy** syscall table
//! (docs/LINUX-COMPAT.md 2). x86-64 predates the asm-generic table and keeps
//! its historical numbers; ARM64/RISC-V share the generic table in
//! `arch/linux_abi_generic.rs` (TARGET-ARCHITECTURES.md 4, "Linux
//! personality ABI").
//!
//! Source of truth: Linux v6.6 `arch/x86/entry/syscalls/syscall_64.tbl`.
//! Only implemented / deliberately-ENOSYS numbers are listed (the honesty
//! table in docs/LINUX-COMPAT.md 3).

/// Syscall numbers (x86-64 table).
pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const CLOSE: u64 = 3;
    pub const FSTAT: u64 = 5;
    pub const POLL: u64 = 7;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const PREAD64: u64 = 17;
    pub const ACCESS: u64 = 21;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const RT_SIGTIMEDWAIT: u64 = 128;
    pub const RT_SIGQUEUEINFO: u64 = 129;
    pub const TKILL: u64 = 200;
    pub const IOCTL: u64 = 16;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const PIPE: u64 = 22;
    pub const DUP2: u64 = 33;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const CLONE: u64 = 56;
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    pub const EXECVE: u64 = 59;
    pub const EXIT: u64 = 60;
    pub const WAIT4: u64 = 61;
    pub const KILL: u64 = 62;
    pub const UNAME: u64 = 63;
    pub const FCNTL: u64 = 72;
    pub const FSYNC: u64 = 74;
    pub const FDATASYNC: u64 = 75;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    // AF_UNIX sockets (docs/LINUX-COMPAT.md L8). x86-64 legacy numbers.
    pub const SOCKET: u64 = 41;
    pub const CONNECT: u64 = 42;
    pub const ACCEPT: u64 = 43;
    pub const SENDTO: u64 = 44;
    pub const RECVFROM: u64 = 45;
    pub const SENDMSG: u64 = 46;
    pub const RECVMSG: u64 = 47;
    pub const SHUTDOWN: u64 = 48;
    pub const BIND: u64 = 49;
    pub const LISTEN: u64 = 50;
    pub const GETSOCKNAME: u64 = 51;
    pub const GETPEERNAME: u64 = 52;
    pub const SOCKETPAIR: u64 = 53;
    pub const SETSOCKOPT: u64 = 54;
    pub const GETSOCKOPT: u64 = 55;
    pub const ACCEPT4: u64 = 288;
    pub const GETRLIMIT: u64 = 97;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const GETPPID: u64 = 110;
    pub const SETPGID: u64 = 109;
    pub const GETPGID: u64 = 121;
    pub const SETSID: u64 = 112;
    pub const GETSID: u64 = 124;
    pub const SIGALTSTACK: u64 = 131;
    pub const PRCTL: u64 = 157;
    pub const ARCH_PRCTL: u64 = 158;
    pub const GETTID: u64 = 186;
    pub const FUTEX: u64 = 202;
    pub const SCHED_GETAFFINITY: u64 = 204;
    pub const GETDENTS64: u64 = 217;
    pub const SET_TID_ADDRESS: u64 = 218;
    pub const CLOCK_GETTIME: u64 = 228;
    pub const CLOCK_NANOSLEEP: u64 = 230;
    pub const EXIT_GROUP: u64 = 231;
    pub const TGKILL: u64 = 234;
    pub const OPENAT: u64 = 257;
    pub const MKDIRAT: u64 = 258;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const READLINKAT: u64 = 267;
    /// x86-64 legacy `readlink(path, buf, bufsiz)` - what glibc's `readlink()`
    /// actually issues here. The asm-generic table has no such number.
    pub const READLINK: u64 = 89;
    pub const PPOLL: u64 = 271;
    pub const RENAMEAT: u64 = 264;
    pub const FACCESSAT: u64 = 269;
    pub const SET_ROBUST_LIST: u64 = 273;
    pub const DUP3: u64 = 292;
    pub const PIPE2: u64 = 293;
    pub const PRLIMIT64: u64 = 302;
    pub const GETRANDOM: u64 = 318;
    pub const STATX: u64 = 332;
    pub const RSEQ: u64 = 334;
    pub const CLONE3: u64 = 435;
    pub const FACCESSAT2: u64 = 439;

    // epoll (docs/LINUX-COMPAT.md L8-INET). x86-64 legacy numbers.
    pub const EPOLL_CREATE: u64 = 213;
    pub const EPOLL_WAIT: u64 = 232;
    pub const EPOLL_CTL: u64 = 233;
    pub const EPOLL_PWAIT: u64 = 281;
    pub const EPOLL_CREATE1: u64 = 291;
}

/// The x86-64 `struct epoll_event` is `__attribute__((packed))` (12 bytes; the
/// `data` u64 follows the `events` u32 with no padding). ARM64/RISC-V leave it
/// naturally aligned (docs/LINUX-COMPAT.md L8-INET). Per-ISA ABI, so it lives in
/// the arch layer.
pub const EPOLL_EVENT_SIZE: usize = 12;
pub const EPOLL_EVENT_DATA_OFFSET: usize = 4;

/// The x86-64 `struct stat` (docs/LINUX-COMPAT.md L2). 144 bytes; layout per
/// Linux v6.6 `arch/x86/include/uapi/asm/stat.h`. ARM64/RISC-V use the smaller
/// asm-generic layout in `arch/linux_abi_generic.rs`; portable dispatch only
/// ever names `crate::arch::linux_abi::Stat`, so the two never mix.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Stat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atime: u64,
    st_atime_nsec: u64,
    st_mtime: u64,
    st_mtime_nsec: u64,
    st_ctime: u64,
    st_ctime_nsec: u64,
    __unused: [i64; 3],
}

impl Stat {
    /// Build a `struct stat` from the fields the personality synthesizes
    /// (docs/LINUX-COMPAT.md L2). `mode` is the full `st_mode` (type bits +
    /// permission bits); the three timestamps are all set to `mtime_ns`.
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
        let secs = mtime_ns / 1_000_000_000;
        let nsec = mtime_ns % 1_000_000_000;
        Stat {
            st_dev: 0,
            st_ino: ino,
            st_nlink: nlink,
            st_mode: mode,
            st_uid: uid,
            st_gid: gid,
            __pad0: 0,
            st_rdev: rdev,
            st_size: size as i64,
            st_blksize: blksize as i64,
            st_blocks: blocks as i64,
            st_atime: secs,
            st_atime_nsec: nsec,
            st_mtime: secs,
            st_mtime_nsec: nsec,
            st_ctime: secs,
            st_ctime_nsec: nsec,
            __unused: [0; 3],
        }
    }
}
