//! The user/kernel ABI, as seen from the kernel side.
//!
//! Everything a separately-compiled cell must agree with byte-for-byte - the
//! syscall numbers, the queue opcodes, the ring layout, and every `repr(C)`
//! block a syscall writes into a cell's memory - lives in the **`rheo-abi`**
//! crate and is re-exported here unchanged. It used to be written out twice by
//! hand, once here and once in `librheo/src/sys.rs`, kept in step by comments
//! saying "keep in sync": geometry drift was caught, but a field-meaning change
//! or a moved syscall number produced wrong numbers with no fault
//! (docs/ARCHITECTURE-DEBT.md 3.1). Now there is one definition, so divergence
//! is a compile error.
//!
//! What stays here is what is *not* cross-crate: [`ShellIo`] and [`Params`] are
//! shared only with the U-mode programs in `kernel/src/user_progs.rs`, which are
//! compiled into this same crate.
//!
//! Convention (all three ISAs): syscall number in the ISA's syscall-number
//! register, arguments in the argument registers, return value back in the first
//! one. See each arch's `decode_syscall` / `set_syscall_ret`.

pub use rheo_abi::{
    BufReduceDesc, ChannelInfo, CpuFeatures, DEADLOCK_EXIT, DebugWrite, EngineInfo, FAULT_EXIT,
    GrantInfo, GraphNode, MAX_CELL_CHANNELS, QueueInfo, ReserveInfo, SIMD_AVX2, SIMD_AVX512F,
    SIMD_AVX512VNNI, SIMD_NEON, SIMD_SSE2, SPAWN_CHAN_SLOT, SYS_ARM_TIMER, SYS_CAPS, SYS_CLOSE,
    SYS_COMMIT, SYS_CONNECT, SYS_CPUINFO, SYS_CYCLES, SYS_DEBUG_WRITE, SYS_DECOMMIT, SYS_DOORBELL,
    SYS_ENGINE_INFO, SYS_EVENT_COUNT, SYS_EVENT_EMIT, SYS_EXIT, SYS_EXIT_GROUP, SYS_FSTAT,
    SYS_GETDENTS, SYS_GRANT, SYS_GRANT_SHARE, SYS_GRAPH, SYS_LEASE, SYS_LSEEK, SYS_LSPCI,
    SYS_MEMINFO, SYS_MMAP, SYS_MMAP_FILE, SYS_MUNMAP, SYS_NUMA, SYS_OPEN, SYS_PS, SYS_QUEUE_INFO,
    SYS_RANDOM, SYS_READ, SYS_READLINE, SYS_RESERVE, SYS_RESERVE_ADMIT, SYS_RESERVE_QUERY,
    SYS_RESERVE_RELEASE, SYS_SEAL, SYS_SPAWN, SYS_STAT, SYS_SWITCH, SYS_UPTIME, SYS_WAIT,
    SYS_WAIT_INPUT, SYS_WAIT_NET, SYS_WRITE, SYS_WRITE_FD, SYS_YIELD, ShareInfo, Stat,
    TIMER_CLIENT_CELL_SLEEP, TIMER_CLIENT_PACER, TileGemmDesc, spawn_chan_spec,
};

/// Shared shell I/O block, one page in the cell's `.user` data, readable and
/// writable by the kernel through its identity mapping. Kernel-internal: the
/// only other reader is `kernel/src/user_progs.rs`, in this crate.
pub const SHELL_BUF: usize = 256;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ShellIo {
    pub in_buf: [u8; SHELL_BUF],
    pub in_len: u64,
    pub out_buf: [u8; SHELL_BUF],
    pub out_len: u64,
    /// Per-cell ChaCha20 DRBG state (docs/TIME-IDENTITY.md 4). The kernel
    /// seeds `rng_key` from the root DRBG when it builds the cell; the cell
    /// then draws random bytes as a library call over this state - no
    /// syscall on the fast path. `rng_pos == 32` means the output buffer is
    /// spent and the next draw re-keys (fast key erasure / forward secrecy).
    pub rng_key: [u8; 32],
    pub rng_out: [u8; 32],
    pub rng_pos: u64,
}

impl ShellIo {
    pub const ZERO: ShellIo = ShellIo {
        in_buf: [0; SHELL_BUF],
        in_len: 0,
        out_buf: [0; SHELL_BUF],
        out_len: 0,
        rng_key: [0; 32],
        rng_out: [0; 32],
        rng_pos: 32,
    };
}

/// Per-cell parameter/result block, one page, mapped U|RW into the cell
/// and readable by the kernel through its identity mapping. The kernel
/// fills the inputs before entry; the user writes the outputs before
/// SYS_EXIT.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Params {
    /// Input: which micro-op the worker should run (see WORKLOAD_*).
    pub workload: u64,
    /// Input: iteration count for the worker loop (or, for the prober,
    /// the target user VA to poke).
    pub iters: u64,
    /// Input: user VA of this cell's shared QueuePair.
    pub qp_addr: u64,
    /// Input: capability id (32-bit ABI form) for the queue.
    pub cap_id: u64,
    /// Output: total counter ticks the worker measured across `iters`.
    pub ticks: u64,
    /// Output: operations the worker completed (sanity check).
    pub ops: u64,
    /// Output: self-reported status (0 = ok).
    pub status: u64,
}

impl Params {
    pub const ZERO: Params = Params {
        workload: 0,
        iters: 0,
        qp_addr: 0,
        cap_id: 0,
        ticks: 0,
        ops: 0,
        status: 0,
    };
}

/// Worker measures the syscall doorbell round trip: submit one NOP, ring,
/// reap one completion, per iteration.
pub const WORKLOAD_ROUNDTRIP: u64 = 1;
/// Worker measures the bare syscall floor: SYS_CYCLES, nothing else.
pub const WORKLOAD_SYSCALL: u64 = 2;
/// Worker measures the cross-cell directed switch: SYS_SWITCH per iteration.
pub const WORKLOAD_CROSSCELL: u64 = 3;
