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
/// cpuinfo(out_va) -> 0. With `out_va == 0`, print the CPU report (vendor,
/// core count, instruction-set features) to the console (the shell builtin).
/// With `out_va != 0`, write a [`CpuFeatures`] there instead - the machine-
/// readable form a cell reads to pick its SIMD path (docs/TILES.md 4). Mechanism
/// only: it exposes the already-discovered `hw::Inventory` CPU report + the FP
/// widths the kernel validated at boot; no new object (ARCHITECTURE.md 6).
pub const SYS_CPUINFO: u64 = 17;
/// Print the enumerated PCIe devices and their engine classification.
pub const SYS_LSPCI: u64 = 18;
/// Print the NUMA topology: per-node RAM and CPU counts.
pub const SYS_NUMA: u64 = 19;

// Portable SIMD-tier bits reported in `CpuFeatures::simd` - the widths the
// kernel actually **enabled and validated** for U-mode (not just what CPUID
// claims), so a cell dispatches only to a path whose register state the kernel
// saves across cell switches (docs/TILES.md 4). x86 tiers are cumulative
// (AVX-512 implies AVX2 implies SSE2); ARM64 reports NEON; RISC-V reports none
// (scalar F/D only until RVV). A cell still runs its own functionality +
// benchmark probe before trusting a tier.
/// x86 SSE2 (the hard-float x86 baseline).
pub const SIMD_SSE2: u64 = 1 << 0;
/// x86 AVX2 (x86-64-v3).
pub const SIMD_AVX2: u64 = 1 << 1;
/// x86 AVX-512F (x86-64-v4).
pub const SIMD_AVX512F: u64 = 1 << 2;
/// x86 AVX-512-VNNI (Zen4 / Sapphire Rapids int8 `dpbusd`).
pub const SIMD_AVX512VNNI: u64 = 1 << 3;
/// ARM64 NEON (Advanced SIMD).
pub const SIMD_NEON: u64 = 1 << 4;

/// Machine-readable CPU feature report a cell reads via `SYS_CPUINFO(out_va)`
/// (docs/TILES.md 4). `features` is the raw per-ISA CPUID bitmask
/// (`arch::cpu_feature_names()`-indexed, for diagnostics); `simd` is the
/// portable `SIMD_*` tier mask of kernel-enabled+validated widths a cell
/// dispatches on; `vendor` is the ASCII vendor string, NUL-padded.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct CpuFeatures {
    pub features: u64,
    pub simd: u64,
    pub vendor: [u8; 16],
}

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

// ---- POSIX personality syscalls (docs/USERLAND.md M2) ----
//
// Multi-argument, fd-based: the arguments are read from a0..a5 (see each
// arch's `decode_syscall`). Return values are `i64` in the return register:
// >= 0 on success (fd / byte count / offset / base VA), or a negated errno.
// The memory/process calls are handled in the kernel; the file calls are
// forwarded to a registered personality handler (`svc::FileOps`).

/// mmap_anon(len) -> base VA of `len` bytes of fresh zeroed RW pages (0 fails).
pub const SYS_MMAP: u64 = 21;
/// exit_group(code): leave U-mode, like `SYS_EXIT`.
pub const SYS_EXIT_GROUP: u64 = 22;
/// open(path_va, path_len, flags) -> fd or -errno.
pub const SYS_OPEN: u64 = 23;
/// close(fd) -> 0 or -errno.
pub const SYS_CLOSE: u64 = 24;
/// read(fd, buf_va, len) -> bytes read or -errno.
pub const SYS_READ: u64 = 25;
/// write(fd, buf_va, len) -> bytes written or -errno.
pub const SYS_WRITE_FD: u64 = 26;
/// lseek(fd, offset, whence) -> new offset or -errno.
pub const SYS_LSEEK: u64 = 27;
/// stat(path_va, path_len, statbuf_va) -> 0 or -errno. Fills a `Stat` at
/// `statbuf_va` (docs/USERLAND.md M5).
pub const SYS_STAT: u64 = 28;
/// fstat(fd, statbuf_va) -> 0 or -errno. Like `SYS_STAT` but by open fd.
pub const SYS_FSTAT: u64 = 29;
/// getdents(path_va, path_len, buf_va, buf_len) -> bytes written or -errno.
/// Packs directory entries into `buf` (docs/USERLAND.md M5): each record is
/// `[u32 kind][u32 name_len][name bytes]` with no padding, read sequentially
/// by the std `ReadDir`. `kind`: 0 regular, 1 dir, 2 symlink, 3 other.
pub const SYS_GETDENTS: u64 = 30;

// ---- librheo native foundation (docs/LIBRHEO.md, Phase A) ----

/// queue_info(out_va) -> 0, or u64::MAX if the cell has no queue pair. Writes
/// a `QueueInfo { qp_va, cap_id }` at `out_va`: the base VA of the cell's
/// mapped queue-pair region and the 32-bit ABI id of its minted QueuePair
/// capability. librheo's reactor calls this once at startup to bind the ring
/// and address the doorbell (a native, explicit alternative to an auxv entry).
pub const SYS_QUEUE_INFO: u64 = 31;

/// The `SYS_QUEUE_INFO` result block (kept in sync with librheo's `sys` arm).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QueueInfo {
    /// Base VA of the cell's queue-pair region (the `QueueHeader`).
    pub qp_va: u64,
    /// 32-bit ABI id of the cell's QueuePair capability (`Handle::raw_low32`).
    pub cap_id: u64,
}

// ---- typed memory grants exposed to userspace (docs/LIBRHEO.md Phase B,
// docs/MEMORY.md, docs/ARCHITECTURE.md 3 object 5) ----
//
// These EXPOSE the existing memory-grant kernel object to a native cell as
// mechanism (ARCHITECTURE.md 6 admission rule); they add no new object. A grant
// is a typed reservation of address space that is demand-committed with frames.
// The reservation, the typed kind, and the seal flag are per-cell state (a fixed
// static table, like the Linux fd table); each SYS_GRANT also mints a real
// MemoryGrant capability into the cell's table, and commit/decommit/seal
// grant-check it (MAP right) before touching pages.

/// grant(out_va, len, kind, flags) -> 0, or u64::MAX on failure. Reserves
/// `len` bytes of address space of typed `kind` (no frames yet - demand
/// commit), mints a MemoryGrant capability, and writes a `GrantInfo { base,
/// cap_id }` at `out_va`. `kind` is a [`MemKind`] discriminant.
pub const SYS_GRANT: u64 = 32;
/// commit(cap_id, offset, len) -> 0 or -errno. Backs `[offset, offset+len)` of
/// the grant with fresh zeroed RW frames (demand paging without a fault
/// handler). Refused on a sealed grant.
pub const SYS_COMMIT: u64 = 33;
/// decommit(cap_id, offset, len) -> 0 or -errno. Returns the frames backing
/// `[offset, offset+len)` to the pool (the reservation and cap stay).
pub const SYS_DECOMMIT: u64 = 34;
/// seal(cap_id) -> 0 or -errno. Makes the grant immutable (its committed pages
/// become read-only, shareable) - the zero-copy-buffer / dmabuf precursor.
pub const SYS_SEAL: u64 = 35;
/// munmap(va, len) -> 0. Unmaps whole pages in `[va, va+len)` and frees their
/// frames (the real unmap the anon `SYS_MMAP` lacked - fixes the frame leak).
pub const SYS_MUNMAP: u64 = 36;
/// mmap_file(fd, offset, len, flags) -> base VA (0 fails). Maps `len` bytes of
/// the file open on `fd` (via the registered `svc::FileOps`) into the cell,
/// read into fresh frames (MAP_PRIVATE semantics), for mmap-ing a dataset.
pub const SYS_MMAP_FILE: u64 = 37;

/// The `SYS_GRANT` result block (kept in sync with librheo's `mem` arm).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GrantInfo {
    /// Base VA of the reserved grant region.
    pub base: u64,
    /// 32-bit ABI id of the minted MemoryGrant capability.
    pub cap_id: u64,
}

// ---- compute & QoS exposed to userspace (docs/LIBRHEO.md Phase C,
// docs/ARCHITECTURE.md 3 objects 4/6/7) ----
//
// These EXPOSE the existing engine, dependency-graph, and reservation kernel
// objects to a native cell as mechanism (ARCHITECTURE.md 6 admission rule);
// they add no new object. Graph submission rides the queue pair (opcode
// `OP_GRAPH_SUBMIT`, docs/IO.md 1); engine introspection and reservation
// admission are plain syscalls.

/// engine_info(out_va, index) -> engine count. Writes an `EngineInfo` for
/// engine `index`: index 0 is the CPU engine the kernel runs graphs on
/// (throughput MEASURED at attach - attest-by-measurement, object 4);
/// indices beyond it are GPUs enumerated from PCIe (docs/GPU-HARDWARE.md),
/// registered with their declared op-boundary preemption contract and a
/// zero measured cost until a driver cell can execute on them. Out-of-range
/// indices write nothing; the returned count is the enumeration bound.
pub const SYS_ENGINE_INFO: u64 = 38;
/// reserve_admit(out_va, budget, period, deadline, mem_floor_pages) -> 0 on
/// success, else a rejection code (1=BadParams, 2=Overcommit, 3=MemoryFloor).
/// Runs the EDF schedulability test (object 7); on success mints a Reservation
/// capability and writes a `ReserveInfo { handle, committed_ppm }` at `out_va`.
pub const SYS_RESERVE_ADMIT: u64 = 39;
/// reserve_query() -> the cell's committed CPU utilization in parts-per-million.
pub const SYS_RESERVE_QUERY: u64 = 40;
/// reserve_release(cap_id) -> 0. Releases an admitted reservation, freeing its
/// utilization (the RAII drop path). `u64::MAX` if the handle is not live.
pub const SYS_RESERVE_RELEASE: u64 = 41;

// ---- console input / the first block-and-wake (docs/LIBRHEO.md Phase D) ----

/// wait_input(buf_va, len) -> nbytes. **Block** until at least one console
/// input byte is available, copy up to `len` bytes into the cell buffer at
/// `buf_va`, and return the count (0 = end of input). The OS's first
/// block-and-wake: a native cell with nothing to do parks here and the kernel
/// idles (WFI/HLT where the ISA's UART RX interrupt is wired, per-ISA in
/// `kernel/src/arch`; a poll otherwise). The RX bytes come from a kernel-side
/// ring (`kernel/src/input.rs`) so keystrokes typed while a cell computes are
/// not lost. librheo's `term` builds its async input on this.
pub const SYS_WAIT_INPUT: u64 = 42;

// ---- services & IPC: cross-cell connect + buffer-grant passing
// (docs/LIBRHEO.md Phase E, docs/IO.md 6, docs/ARCHITECTURE.md 3 objects 2/3/5) ----
//
// These EXPOSE the existing queue-pair (object 3) and memory-grant (object 5)
// kernel objects as the Wayland-class services substrate; they add no new object
// (they pass the ARCHITECTURE.md 6 admission rule as mechanism). A *cross-cell*
// queue pair is one shared ring region mapped into two cells at the same channel
// VA (the two ends of an IO.md-6 typed connection); the kernel initialises the
// header once and reports each cell's end, but never drains it - the two cells
// drive the SPSC rings directly over the shared frames (no kernel_process). A
// buffer grant is passed by *delegating* a sealed grant (object 2 delegate,
// object 5 seal->immutable/shareable): the kernel maps the same frames into the
// peer read-only and mints a MemoryGrant capability there - zero-copy shared
// memory, the dmabuf equivalent.

/// connect_info(out_va) -> 0, or u64::MAX if the cell has no cross-cell channel.
/// Writes a `ChannelInfo { chan_va, cap_id, role }`: the base VA of the cell's
/// mapped shared-channel region, the 32-bit ABI id of its minted QueuePair
/// capability for the channel, and its role (0 = initiator/client, 1 =
/// acceptor/server) so one binary can serve both ends. librheo's `ipc::Channel`
/// calls this once to bind the shared ring (IO.md 6: connect = capability
/// exchange yielding a typed queue pair).
pub const SYS_CONNECT: u64 = 43;
// ---- native process model + timers (docs/LIBRHEO.md Phase F,
// docs/ARCHITECTURE.md 3 object 1 Cell, verb set "create/destroy cell" +
// "arm timer/doorbell") ----
//
// These EXPOSE the existing Cell object (1) and the arm-timer verb to a native
// cell as mechanism (ARCHITECTURE.md 6 admission rule); they add no new object.
// Spawning is gated by a **cell-spawn capability** (an `ObjectKind::Cell` cap
// carrying WRITE) so a cell without it cannot create cells - no ambient
// authority. A spawned child is a fresh `Personality::Native` cell with its own
// address space + mapped queue pair, sharing the parent's capability bundle
// (like `fork`), running librheo. Native child faults stay terminal (no
// signals); the parent reaps a fault as an exit code.

/// spawn(path_va, path_len, argv_va, envp_va) -> child handle (>= 0), or
/// u64::MAX on failure (no spawn capability, ELF not found, cell table full).
/// Loads the ELF at `path` from the VFS into a NEW native cell, builds its
/// initial stack from the NUL-terminated C-string arrays at `argv_va`/`envp_va`
/// (copied out of the caller before the child's space is built), maps it a queue
/// pair + mints a queue capability, and records the caller as its parent. The
/// child is runnable but does not run until the parent `SYS_WAIT`s (or exits).
/// The returned handle is passed to `SYS_WAIT`.
pub const SYS_SPAWN: u64 = 45;
/// wait(handle) -> the child's exit code (0..=255). **Blocks** cooperatively
/// (the parent's other strands run meanwhile, driven by librheo's reactor) until
/// the child named by `handle` exits, then reaps it and frees its slot. Returns
/// u64::MAX if `handle` names no child of the caller. A native child that faults
/// is reaped with a sentinel exit code (`FAULT_EXIT`), never a signal.
pub const SYS_WAIT: u64 = 46;
/// arm_timer(deadline_ns) -> 0. **Blocks** until `deadline_ns` nanoseconds of
/// monotonic time elapse from the call, then returns. The "arm timer" verb: a
/// one-shot deadline. Honors docs/POWER.md - the kernel only waits when a real
/// deadline was requested. Cooperative on every ISA today (the deadline is
/// checked against the monotonic clock; a true per-ISA timer IRQ is documented
/// future work, docs/LIBRHEO.md Phase F), so this is an honest deadline wait, not
/// a 0%-CPU idle. librheo's `time::sleep`/`timeout` build on it.
pub const SYS_ARM_TIMER: u64 = 47;

/// The exit code a native child is reaped with when it faults (native cells have
/// no signal delivery, docs/LIBRHEO.md Phase F): 128 + a SIGSEGV-shaped 11.
pub const FAULT_EXIT: u64 = 139;

/// grant_share(grant_cap_id, out_va) -> 0, or u64::MAX on failure. Delegate a
/// **sealed** memory grant to the peer cell: the kernel maps the grant's frames
/// into the peer read-only, mints a MemoryGrant capability there referencing the
/// same kernel object (so an epoch revoke kills the peer's copy too), and writes
/// a `ShareInfo { peer_va, peer_cap_id }` at `out_va`. Requires the grant cap to
/// carry DELEGATE and the grant to be sealed (immutable = shareable, object 5).
/// This is zero-copy cross-cell buffer passing (the dmabuf equivalent): the
/// client fills+seals a buffer, shares it, sends the handle over the channel;
/// the server reads the same frames with no copy.
pub const SYS_GRANT_SHARE: u64 = 44;

/// The `SYS_CONNECT` result block (kept in sync with librheo's `ipc` arm).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ChannelInfo {
    /// Base VA of the cell's mapped shared-channel region (the `QueueHeader`).
    pub chan_va: u64,
    /// 32-bit ABI id of the cell's QueuePair capability for the channel.
    pub cap_id: u64,
    /// 0 = initiator (client: SQ producer, CQ consumer); 1 = acceptor (server:
    /// SQ consumer, CQ producer). The two ends drive opposite sides of the SPSC.
    pub role: u64,
}

/// The `SYS_GRANT_SHARE` result block (kept in sync with librheo's `ipc` arm).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ShareInfo {
    /// Base VA at which the grant's frames were mapped read-only in the peer.
    pub peer_va: u64,
    /// 32-bit ABI id of the MemoryGrant capability minted in the peer's table.
    pub peer_cap_id: u64,
}

/// The `SYS_ENGINE_INFO` result block (kept in sync with librheo's `compute`
/// arm). `kind`: 0=CPU, 1=GPU (enumerated from PCIe - recognised and
/// registered, executable only via its future driver cell,
/// docs/GPU-HARDWARE.md 5). `preemption`: 0=per-instruction,
/// 1=per-op-boundary (the declared accelerator contract). `vendor` is the
/// PCI vendor ID for a device engine (0x10DE NVIDIA, 0x1002 AMD, 0x8086
/// Intel, 0x1AF4 virtio), 0 for the CPU. The syscall takes
/// `(out_va, index)` and returns the engine count; index 0 is always the
/// CPU engine, so the old single-engine call is unchanged.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct EngineInfo {
    pub kind: u64,
    pub measured_cost_ticks: u64,
    pub preemption: u64,
    pub vendor: u64,
}

/// The `SYS_RESERVE_ADMIT` success block (kept in sync with librheo's `sched`
/// arm). `handle` is the 32-bit ABI id of the minted Reservation capability;
/// `committed_ppm` is the cell's total committed utilization after admission.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ReserveInfo {
    pub handle: u64,
    pub committed_ppm: u64,
}

/// Graph-node op 4 (BufReduce) descriptor (docs/TILES.md 6): `node.a` is the
/// cell VA of this struct. The engine returns the wrapping u64 sum over
/// `elems` elements of `dtype` (0=I8, 1=U8, 2=I32; signed sign-extends).
/// Validation caps: `va != 0`, `0 < elems <= 1<<20`, `dtype <= 2` - anything
/// else completes STATUS_DENIED. Kept in sync with librheo's `sys` mirror.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BufReduceDesc {
    pub va: u64,
    pub elems: u64,
    pub dtype: u32,
    pub _pad: u32,
}
const _: () = assert!(core::mem::size_of::<BufReduceDesc>() == 24);

/// Graph-node op 5 (TileGemm) descriptor (docs/TILES.md 6): `node.a` is the
/// cell VA of this struct. The engine zeroes C, runs the int8->i32 GEMM
/// whole (the node is the tile loop), and returns the FNV-1a hash of C's
/// logical m x n window - the deterministic receipt; the buffer carries the
/// real output. Strides are in elements. Validation caps: VAs non-zero,
/// `1 <= m,n,k <= 256`, strides >= the matching dims, `dtype_in == 0` (I8)
/// and `dtype_acc == 2` (I32) exactly - the kernel path is integer-only
/// (the aarch64 kernel target is soft-float). Worst node = 16.7M MACs,
/// worst graph = 32 nodes: the documented drain bound.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TileGemmDesc {
    pub a_va: u64,
    pub b_va: u64,
    pub c_va: u64,
    pub m: u32,
    pub n: u32,
    pub k: u32,
    pub a_stride: u32,
    pub b_stride: u32,
    pub c_stride: u32,
    pub dtype_in: u32,
    pub dtype_acc: u32,
}
const _: () = assert!(core::mem::size_of::<TileGemmDesc>() == 56);

/// One node of a userspace-built dependency graph (docs/LIBRHEO.md Phase C,
/// docs/ARCHITECTURE.md 3 object 6), kept in sync with librheo's `compute` arm.
/// A cell writes an array of these into one of its buffers and submits it with
/// `OP_GRAPH_SUBMIT`; the kernel validates the edges (topological), runs it on
/// the CPU engine, and writes each node's `u64` result back. `op`: 0=Const
/// (value in `a`), 1=Add, 2=Mul, 3=Select. For Add/Mul/Select each input is an
/// immediate (`*_is_node == 0`, value in `a`/`b`) or an earlier node's result
/// (`*_is_node == 1`, node index in `a`/`b`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GraphNode {
    pub op: u32,
    pub a_is_node: u32,
    pub b_is_node: u32,
    pub _pad: u32,
    pub a: u64,
    pub b: u64,
}
const _: () = assert!(core::mem::size_of::<GraphNode>() == 32);

/// The `stat`/`fstat` result block (kept in sync with the std `fs` arm in
/// targets/std-rheo/fs.rs). `kind`: 0 regular, 1 dir, 2 symlink, 3 other.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Stat {
    pub size: u64,
    pub kind: u64,
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
