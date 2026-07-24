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
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MPROTECT: u64 = 10;
    pub const MUNMAP: u64 = 11;
    pub const BRK: u64 = 12;
    pub const RT_SIGACTION: u64 = 13;
    pub const RT_SIGPROCMASK: u64 = 14;
    pub const RT_SIGRETURN: u64 = 15;
    pub const IOCTL: u64 = 16;
    pub const READV: u64 = 19;
    pub const WRITEV: u64 = 20;
    pub const SCHED_YIELD: u64 = 24;
    pub const MREMAP: u64 = 25;
    pub const MADVISE: u64 = 28;
    pub const DUP: u64 = 32;
    pub const NANOSLEEP: u64 = 35;
    pub const GETPID: u64 = 39;
    pub const CLONE: u64 = 56;
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
    pub const GETRLIMIT: u64 = 97;
    pub const GETUID: u64 = 102;
    pub const GETGID: u64 = 104;
    pub const GETEUID: u64 = 107;
    pub const GETEGID: u64 = 108;
    pub const GETPPID: u64 = 110;
    pub const SIGALTSTACK: u64 = 131;
    pub const ARCH_PRCTL: u64 = 158;
    pub const GETTID: u64 = 186;
    pub const FUTEX: u64 = 202;
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
}
