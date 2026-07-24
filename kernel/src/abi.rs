//! The user/kernel ABI shared between kernel-side dispatch and the U-mode
//! programs (kernel/src/user_progs.rs). Syscall numbers and the shared
//! parameter block; the queue-entry layout itself is in `queue`.
//!
//! Convention (all three ISAs): syscall number in the ISA's syscall-number
//! register, argument in the first argument register, return value back in
//! the first argument register. See each arch's `decode_syscall` /
//! `set_syscall_ret`.

/// Process the calling cell's queue pair (drain submissions, grant-check,
/// complete), then return to the caller. The doorbell.
pub const SYS_DOORBELL: u64 = 1;

/// Directed switch to the peer cell (cross-cell round trip, P5). Saves the
/// caller's user context, switches address space, resumes the peer where
/// it last switched. Argument and return value are ignored.
pub const SYS_SWITCH: u64 = 2;

/// Leave U-mode and return to the kernel run loop. Argument is the cell's
/// self-reported result code (0 = ok).
pub const SYS_EXIT: u64 = 3;

/// Read the shared cycle counter into the return register. Used so the
/// benchmark's user side measures the same clock the kernel calibrates,
/// without depending on per-ISA U-mode counter-enable quirks.
pub const SYS_CYCLES: u64 = 4;

// ---- Shell / resource syscalls (arg is a *const ShellIo unless noted) ----

/// Read one PTY line into ShellIo.in_buf (blocking). Returns 1 if a line
/// was read, 0 at end of input.
pub const SYS_READLINE: u64 = 5;
/// Write ShellIo.out_buf[..out_len] to the PTY. Returns 0.
pub const SYS_WRITE: u64 = 6;
/// Monotonic uptime ticks.
pub const SYS_UPTIME: u64 = 7;
/// Next per-cell random u64.
pub const SYS_RANDOM: u64 = 8;
/// Frame-pool stats: (free << 32) | total.
pub const SYS_MEMINFO: u64 = 9;
/// Number of runnable cells the kernel is tracking.
pub const SYS_PS: u64 = 10;
/// Live capability count in the calling cell's table.
pub const SYS_CAPS: u64 = 11;
/// Emit a user event (arg = event kind). Returns total events emitted.
pub const SYS_EVENT_EMIT: u64 = 12;
/// Event counts: (buffered << 32) | total.
pub const SYS_EVENT_COUNT: u64 = 13;
/// Run the demo dependency graph with input `arg`; returns its result.
pub const SYS_GRAPH: u64 = 14;
/// Admit a reservation (arg = (budget << 32) | period). Returns the
/// committed utilization in parts-per-million, or u64::MAX if refused.
pub const SYS_RESERVE: u64 = 15;
/// Acquire a lease; returns its fencing token.
pub const SYS_LEASE: u64 = 16;
/// Print the CPU report (vendor, core count, instruction-set features) to
/// the console. Kernel-formatted so feature names stay in one place.
pub const SYS_CPUINFO: u64 = 17;
/// Print the enumerated PCIe devices and their engine classification.
pub const SYS_LSPCI: u64 = 18;
/// Print the NUMA topology: per-node RAM and CPU counts.
pub const SYS_NUMA: u64 = 19;

/// Write bytes to the console from a loaded userland program (docs/USERLAND.md
/// M1). The argument is the VA of a `DebugWrite { ptr, len }` in the cell;
/// the kernel copies `len` bytes from `ptr` to the console. This is a
/// bring-up primitive - the real fd-based `write(2)` arrives with the POSIX
/// syscall surface (M2). Returns the number of bytes written.
pub const SYS_DEBUG_WRITE: u64 = 20;

/// The `SYS_DEBUG_WRITE` argument block (kept in sync with `userland`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct DebugWrite {
    pub ptr: u64,
    pub len: u64,
}

/// Shared shell I/O block, one page in the cell's `.user` data, readable
/// and writable by the kernel through its identity mapping.
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
