//! Raw syscalls and the on-wire queue ABI, from the cell's side (docs/
//! LIBRHEO.md). The syscall numbers and the `repr(C)` structs are duplicates
//! of `kernel/src/abi.rs` and `kernel/src/queue/mod.rs` - the established
//! pattern (a cell cannot depend on the kernel crate), kept in sync by hand.
//!
//! Arguments go in the ISA's argument registers (riscv a0.., arm x0.., x86
//! rdi/rsi/rdx), the number in the syscall-number register, the result back in
//! the first argument register.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

// ---- syscall numbers (keep in sync with kernel/src/abi.rs) ----

/// Process the calling cell's queue pair; returns the number of entries
/// completed. The doorbell.
pub const SYS_DOORBELL: u64 = 1;
/// Directed switch to the peer cell (`cur ^ 1`). On the single-CPU cooperative
/// runtime this hands the CPU between the two ends of a cross-cell channel
/// (docs/LIBRHEO.md Phase E): the producer submits then switches so the consumer
/// runs, and vice-versa. Resumes the caller where it last switched.
pub const SYS_SWITCH: u64 = 2;
/// Next per-cell random u64 (the kernel DRBG; used once to seed librheo's own
/// DRBG at startup - see `rng`).
pub const SYS_RANDOM: u64 = 8;
/// mmap_anon(len) -> base VA of `len` zeroed RW bytes (0 fails).
pub const SYS_MMAP: u64 = 21;
/// exit_group(code): leave U-mode.
pub const SYS_EXIT_GROUP: u64 = 22;
/// write(fd, buf_va, len) -> bytes written or -errno.
pub const SYS_WRITE_FD: u64 = 26;
/// queue_info(out_va) -> 0 or u64::MAX. Fills a `QueueInfo` at `out_va`.
pub const SYS_QUEUE_INFO: u64 = 31;
/// grant(out_va, len, kind, flags) -> 0 or u64::MAX. Fills a `GrantInfo`.
pub const SYS_GRANT: u64 = 32;
/// commit(cap_id, offset, len) -> 0 or non-zero on error.
pub const SYS_COMMIT: u64 = 33;
/// decommit(cap_id, offset, len) -> 0 or non-zero on error.
pub const SYS_DECOMMIT: u64 = 34;
/// seal(cap_id) -> 0 or non-zero on error.
pub const SYS_SEAL: u64 = 35;
/// munmap(va, len) -> 0. Unmaps whole pages and frees their frames.
pub const SYS_MUNMAP: u64 = 36;
/// mmap_file(fd, offset, len, flags) -> base VA (0 fails).
pub const SYS_MMAP_FILE: u64 = 37;
/// engine_info(out_va) -> 0. Fills an `EngineInfo` (docs/LIBRHEO.md Phase C).
pub const SYS_ENGINE_INFO: u64 = 38;
/// reserve_admit(out_va, budget, period, deadline, mem_floor_pages) -> 0 |
/// 1=BadParams | 2=Overcommit | 3=MemoryFloor. Fills a `ReserveInfo` on success.
pub const SYS_RESERVE_ADMIT: u64 = 39;
/// reserve_query() -> committed CPU utilization (parts-per-million).
pub const SYS_RESERVE_QUERY: u64 = 40;
/// reserve_release(cap_id) -> 0 | u64::MAX. Frees an admitted reservation.
pub const SYS_RESERVE_RELEASE: u64 = 41;
/// wait_input(buf_va, len) -> nbytes. Block until console input is available;
/// copy up to `len` bytes to `buf` and return the count (0 = end of input).
/// The OS's first block-and-wake (docs/LIBRHEO.md Phase D); `term` builds on it.
pub const SYS_WAIT_INPUT: u64 = 42;
/// connect_info(out_va) -> 0 or u64::MAX. Fills a `ChannelInfo` with the cell's
/// cross-cell shared-channel end (docs/LIBRHEO.md Phase E); `ipc` builds on it.
pub const SYS_CONNECT: u64 = 43;
/// grant_share(grant_cap_id, out_va) -> 0 or u64::MAX. Delegate a sealed grant
/// to the peer cell; fills a `ShareInfo` (peer VA + cap). Zero-copy buffer pass.
pub const SYS_GRANT_SHARE: u64 = 44;
/// spawn(path_va, path_len, argv_va, envp_va) -> child handle or u64::MAX. Load
/// an ELF into a new native cell; gated by the cell-spawn capability (Phase F).
pub const SYS_SPAWN: u64 = 45;
/// wait(handle) -> the child's exit code, or u64::MAX. Blocks cooperatively.
pub const SYS_WAIT: u64 = 46;
/// arm_timer(deadline_ns) -> 0. Block until `deadline_ns` monotonic ns elapse.
pub const SYS_ARM_TIMER: u64 = 47;
/// yield_cell() -> 0. Hand the CPU to the next runnable native cell (round-robin;
/// falls back to the `cur^1` peer where there is no process tree). The N-cell
/// generalisation of `SYS_SWITCH`, needed for service fan-out (docs/NETSTACK.md
/// the service-cell section, rheo-net N4a).
pub const SYS_YIELD: u64 = 49;
/// wait_net(buf_va, len, timeout_ns) -> frame_len. Block until a received Ethernet
/// frame is available; copy up to `len` bytes into `buf` and return the frame
/// length (0 = gave up / timed out). `timeout_ns` 0 waits indefinitely. The network
/// twin of [`SYS_WAIT_INPUT`] (docs/NETSTACK.md, the async-receive path / rheo-net
/// N2d); `net::recv` builds on it.
pub const SYS_WAIT_NET: u64 = 48;
/// uptime() -> monotonic tick reading (SYS_UPTIME). `time` converts to ns.
pub const SYS_UPTIME: u64 = 7;

// ---- raw syscall stubs (from libc/src/sys.rs) ----

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe { asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret, options(nostack)) };
    ret
}
#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
             in("a1") a1, in("a2") a2, options(nostack));
    }
    ret
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret, in("a1") a1, options(nostack));
    }
    ret
}
#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
             in("a1") a1, in("a2") a2, in("a3") a3, options(nostack));
    }
    ret
}
#[cfg(target_arch = "riscv64")]
#[inline(always)]
unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret;
    unsafe {
        asm!("ecall", in("a7") nr, inlateout("a0") a0 => ret,
             in("a1") a1, in("a2") a2, in("a3") a3, in("a4") a4, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe { asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, options(nostack)) };
    ret
}
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
             in("x1") a1, in("x2") a2, options(nostack));
    }
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, in("x1") a1, options(nostack));
    }
    ret
}
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
             in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    }
    ret
}
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret;
    unsafe {
        asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
             in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, options(nostack));
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall1(nr: u64, a0: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> u64 {
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    // Native arg4 is r10 (the `syscall` instruction clobbers rcx), matching the
    // kernel's x86-64 `decode_syscall` (rdi, rsi, rdx, r10, ...).
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2,
             in("r10") a3, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    // Native arg5 is r8 (matching the kernel's x86-64 decode: rdi, rsi, rdx,
    // r10, r8, r9).
    let ret;
    unsafe {
        asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1, in("rdx") a2,
             in("r10") a3, in("r8") a4, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

// ---- typed wrappers ----

pub fn mmap(len: usize) -> usize {
    unsafe { syscall1(SYS_MMAP, len as u64) as usize }
}
/// Reserve a typed memory grant. Fills a `GrantInfo` at `out_va`; returns 0 or
/// `u64::MAX` on failure.
pub fn grant(out_va: u64, len: usize, kind: u64, flags: u64) -> u64 {
    unsafe { syscall4(SYS_GRANT, out_va, len as u64, kind, flags) }
}
/// Back `[offset, offset+len)` of a grant with frames. 0 on success.
pub fn commit(cap_id: u32, offset: usize, len: usize) -> u64 {
    unsafe { syscall3(SYS_COMMIT, cap_id as u64, offset as u64, len as u64) }
}
/// Free the frames backing `[offset, offset+len)` of a grant. 0 on success.
pub fn decommit(cap_id: u32, offset: usize, len: usize) -> u64 {
    unsafe { syscall3(SYS_DECOMMIT, cap_id as u64, offset as u64, len as u64) }
}
/// Seal a grant immutable. 0 on success.
pub fn seal(cap_id: u32) -> u64 {
    unsafe { syscall1(SYS_SEAL, cap_id as u64) }
}
/// Unmap `[va, va+len)` and free the frames.
pub fn munmap(va: usize, len: usize) {
    unsafe { syscall2(SYS_MUNMAP, va as u64, len as u64) };
}
/// Map `len` bytes of the file open on `fd` at `offset` into the cell; returns
/// the base VA (0 fails).
pub fn mmap_file(fd: u64, offset: u64, len: usize, flags: u64) -> usize {
    unsafe { syscall4(SYS_MMAP_FILE, fd, offset, len as u64, flags) as usize }
}
/// Read the CPU engine's introspection block (kind + measured throughput +
/// preemption contract). See `compute::Engine::info`.
pub fn engine_info() -> EngineInfo {
    let mut info = EngineInfo {
        kind: 0,
        measured_cost_ticks: 0,
        preemption: 0,
    };
    unsafe {
        syscall1(SYS_ENGINE_INFO, &mut info as *mut EngineInfo as u64);
    }
    info
}
/// Admit a reservation. Fills a `ReserveInfo` at `out_va` on success; returns
/// 0 or a rejection code (1=BadParams, 2=Overcommit, 3=MemoryFloor).
pub fn reserve_admit(
    out_va: u64,
    budget: u64,
    period: u64,
    deadline: u64,
    mem_floor_pages: u64,
) -> u64 {
    unsafe {
        syscall5(
            SYS_RESERVE_ADMIT,
            out_va,
            budget,
            period,
            deadline,
            mem_floor_pages,
        )
    }
}
/// Query the cell's committed CPU utilization (parts-per-million).
pub fn reserve_query() -> u64 {
    unsafe { syscall1(SYS_RESERVE_QUERY, 0) }
}
/// Release an admitted reservation. 0 on success, `u64::MAX` if not live.
pub fn reserve_release(cap_id: u32) -> u64 {
    unsafe { syscall1(SYS_RESERVE_RELEASE, cap_id as u64) }
}
pub fn exit(code: u64) -> ! {
    unsafe { syscall1(SYS_EXIT_GROUP, code) };
    loop {}
}
/// Hand the CPU to the peer cell (docs/LIBRHEO.md Phase E). Resumes here once the
/// peer switches back. The cross-cell channel's cooperative handoff.
pub fn switch() {
    unsafe { syscall1(SYS_SWITCH, 0) };
}
/// Discover this cell's cross-cell shared-channel end in `slot`.
/// `Some(ChannelInfo)` or `None` if that slot holds no channel. Slot 0 is the
/// Phase E/J channel; a **service cell** holds one slot per client
/// (docs/NETSTACK.md the service-cell section, rheo-net N4a).
pub fn connect_slot(slot: usize) -> Option<ChannelInfo> {
    let mut info = ChannelInfo {
        chan_va: 0,
        cap_id: 0,
        role: 0,
        count: 0,
    };
    let r = unsafe {
        syscall2(
            SYS_CONNECT,
            &mut info as *mut ChannelInfo as u64,
            slot as u64,
        )
    };
    if r == u64::MAX { None } else { Some(info) }
}

/// Discover this cell's channel end at slot 0 (the Phase E/J channel).
pub fn connect() -> Option<ChannelInfo> {
    connect_slot(0)
}

/// Hand the CPU to the **next runnable native cell** in round-robin order; the
/// caller stays runnable (docs/NETSTACK.md the service-cell section, rheo-net
/// N4a). With no native process tree this is the `cur^1` [`switch`]. The reactor's
/// channel idle path uses it so a service and N clients all get the CPU - an XOR
/// hand-off cannot reach client 3 from client 2.
pub fn yield_cell() {
    unsafe { syscall1(SYS_YIELD, 0) };
}
/// Delegate a sealed grant (`cap_id`) to the peer cell; fills a `ShareInfo` at
/// `out_va`. Returns 0 or `u64::MAX`.
pub fn grant_share(cap_id: u32, out_va: u64) -> u64 {
    unsafe { syscall2(SYS_GRANT_SHARE, cap_id as u64, out_va) }
}
pub fn write(fd: u64, buf_va: u64, len: u64) -> i64 {
    unsafe { syscall3(SYS_WRITE_FD, fd, buf_va, len) as i64 }
}
/// Spawn `path` as a new native cell with `argv`/`envp` (NUL-terminated C-string
/// pointer arrays). Returns the child handle, or `u64::MAX` on failure (no
/// spawn capability, ELF not found, cell table full). See `proc::spawn`.
pub fn spawn(path_va: u64, path_len: u64, argv_va: u64, envp_va: u64) -> u64 {
    spawn_chan(path_va, path_len, argv_va, envp_va, 0)
}

/// `SYS_SPAWN` `chan_spec` flag: inherit the caller's channel slot named in bits
/// 15:8 instead of slot 0 (kernel/src/abi.rs `SPAWN_CHAN_SLOT`).
pub const SPAWN_CHAN_SLOT: u64 = 1 << 0;

/// Build a `chan_spec` naming the caller's channel `slot`.
pub const fn spawn_chan_spec(slot: usize) -> u64 {
    SPAWN_CHAN_SLOT | ((slot as u64) << 8)
}

/// Spawn like [`spawn`], with `chan_spec` naming which of this cell's channel
/// ends the child inherits (docs/NETSTACK.md the service-cell section, rheo-net
/// N4a): 0 = slot 0 (the Phase J default), else [`spawn_chan_spec`]. The child
/// always receives it at its own slot 0 with the opposite role.
pub fn spawn_chan(path_va: u64, path_len: u64, argv_va: u64, envp_va: u64, chan_spec: u64) -> u64 {
    unsafe { syscall5(SYS_SPAWN, path_va, path_len, argv_va, envp_va, chan_spec) }
}
/// Wait for the child named by `handle`; returns its exit code, or `u64::MAX` if
/// it names no child. Blocks cooperatively (the caller's other strands run).
pub fn wait(handle: u64) -> u64 {
    unsafe { syscall1(SYS_WAIT, handle) }
}
/// Block until `deadline_ns` nanoseconds of monotonic time elapse. The kernel's
/// cooperative one-shot deadline (docs/LIBRHEO.md Phase F). `time` builds on it.
pub fn arm_timer(deadline_ns: u64) {
    unsafe { syscall1(SYS_ARM_TIMER, deadline_ns) };
}
/// Monotonic uptime in raw ticks. `time::now` converts to nanoseconds.
pub fn uptime() -> u64 {
    unsafe { syscall1(SYS_UPTIME, 0) }
}
pub fn random_u64() -> u64 {
    unsafe { syscall1(SYS_RANDOM, 0) }
}
/// Block until at least one console input byte is available; copy up to `len`
/// bytes into `buf` and return the count (0 = end of input). The kernel idles
/// (WFI where the UART RX interrupt is wired, poll otherwise) while blocked.
pub fn wait_input(buf: *mut u8, len: usize) -> usize {
    unsafe { syscall2(SYS_WAIT_INPUT, buf as u64, len as u64) as usize }
}
/// Block until a received Ethernet frame is available; copy up to `len` bytes of
/// it into `buf` and return the frame length (0 = the kernel gave up: no NIC, the
/// `timeout_ns` deadline elapsed, or the bounded poll fallback expired).
/// `timeout_ns` 0 waits indefinitely. The kernel idles at WFI where the NIC's RX
/// interrupt is wired, and polls otherwise (docs/NETSTACK.md per-ISA table).
pub fn wait_net(buf: *mut u8, len: usize, timeout_ns: u64) -> usize {
    unsafe { syscall3(SYS_WAIT_NET, buf as u64, len as u64, timeout_ns) as usize }
}
/// Ring the doorbell; returns the number of completions produced.
pub fn doorbell() -> usize {
    unsafe { syscall1(SYS_DOORBELL, 0) as usize }
}
/// Ask the kernel for this cell's queue-pair region VA + capability id.
/// Returns `Some(QueueInfo)` or `None` if the cell has no mapped queue.
pub fn queue_info() -> Option<QueueInfo> {
    let mut info = QueueInfo {
        qp_va: 0,
        cap_id: 0,
    };
    let r = unsafe { syscall1(SYS_QUEUE_INFO, &mut info as *mut QueueInfo as u64) };
    if r == u64::MAX { None } else { Some(info) }
}

/// The `SYS_QUEUE_INFO` result block (kernel/src/abi.rs `QueueInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct QueueInfo {
    pub qp_va: u64,
    pub cap_id: u64,
}

// ---- on-wire queue layout (keep in sync with kernel/src/queue/mod.rs) ----

/// Submission opcodes.
pub const OP_NOP: u8 = 0;
pub const OP_ECHO: u8 = 1;
/// Async I/O opcodes (docs/LIBRHEO.md Phase B). See `io` for the typed layer.
pub const OP_OPEN: u8 = 2;
pub const OP_READ: u8 = 3;
pub const OP_WRITE: u8 = 4;
pub const OP_CLOSE: u8 = 5;
pub const OP_FSTAT: u8 = 6;
/// Submit a userspace-built dependency graph to the CPU engine (docs/LIBRHEO.md
/// Phase C). See `compute::GraphBuilder`.
pub const OP_GRAPH_SUBMIT: u8 = 7;
/// Raw-frame networking opcodes (docs/NETWORKING.md, LIBRHEO.md Phase G). See
/// `net` for the typed async layer. `OP_NET_TX` sends one Ethernet frame,
/// `OP_NET_RX` polls for one (result 0 = none), `OP_NET_MAC` reports the MAC.
pub const OP_NET_TX: u8 = 8;
pub const OP_NET_RX: u8 = 9;
pub const OP_NET_MAC: u8 = 10;
/// GPU 2D present (docs/LIBRHEO.md Phase H, docs/DISPLAY.md). See `display` for
/// the typed layer. Payload `[buf_va u64@0][w u32@8][h u32@12]`: presents the
/// cell's `w x h` RGBA framebuffer through the virtio-gpu driver (copy into the
/// resource, transfer to host, flush to scanout); `result` = bytes presented.
pub const OP_GPU_PRESENT: u8 = 11;
/// Async cross-cell channel message (docs/LIBRHEO.md Phase J). Carried over a
/// **shared** channel ring the kernel never processes (the two cells drive the
/// SPSC rings directly), so this opcode is a convention between the two ends, not
/// a `kernel_process` handler. `ipc::AsyncSender`/`AsyncReceiver` use it.
pub const OP_CHAN_MSG: u8 = 12;
/// `SqEntry.flags` bit: the op's data rides inline in the payload (IO.md 1).
pub const FLAG_INLINE: u8 = 1 << 0;
/// Durability-class flag bits (docs/IO.md). Advisory: the kernel ignores them
/// today (no durable backend in QEMU); recorded on the op for honesty.
pub const FLAG_DUR_FLUSH: u8 = 1 << 4;
pub const FLAG_DUR_FUA: u8 = 1 << 5;
/// Largest inline write payload (bytes after the `[fd u32][len u32]` header).
pub const INLINE_MAX: usize = 16;
/// Completion status codes.
pub const STATUS_OK: u32 = 0;
pub const STATUS_BAD_OPCODE: u32 = 1;
pub const STATUS_DENIED: u32 = 2;
pub const STATUS_REVOKED: u32 = 3;
pub const STATUS_EXHAUSTED: u32 = 4;
pub const STATUS_BAD_HANDLE: u32 = 5;
pub const STATUS_IO: u32 = 6;

/// The `SYS_GRANT` result block (kernel/src/abi.rs `GrantInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct GrantInfo {
    pub base: u64,
    pub cap_id: u64,
}

/// The `SYS_CONNECT` result block (kernel/src/abi.rs `ChannelInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ChannelInfo {
    pub chan_va: u64,
    pub cap_id: u64,
    /// 0 = initiator (client), 1 = acceptor (server).
    pub role: u64,
    /// How many channel slots this cell holds (docs/NETSTACK.md the service-cell
    /// section, rheo-net N4a): 1 for a Phase E/J cell, N for a service cell.
    pub count: u64,
}

/// The `SYS_GRANT_SHARE` result block (kernel/src/abi.rs `ShareInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ShareInfo {
    pub peer_va: u64,
    pub peer_cap_id: u64,
}

/// The `SYS_ENGINE_INFO` result block (kernel/src/abi.rs `EngineInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct EngineInfo {
    pub kind: u64,
    pub measured_cost_ticks: u64,
    pub preemption: u64,
}

/// The `SYS_RESERVE_ADMIT` success block (kernel/src/abi.rs `ReserveInfo`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ReserveInfo {
    pub handle: u64,
    pub committed_ppm: u64,
}

/// One node of a userspace-built dependency graph (kernel/src/abi.rs
/// `GraphNode`). `op`: 0=Const (value in `a`), 1=Add, 2=Mul, 3=Select. Each
/// Add/Mul/Select input is an immediate (`*_is_node == 0`) or an earlier node's
/// result (`*_is_node == 1`, node index in `a`/`b`).
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

/// A submission entry - 64 bytes, one cache line.
#[repr(C, align(64))]
#[derive(Copy, Clone)]
pub struct SqEntry {
    pub opcode: u8,
    pub flags: u8,
    pub engine_id: u16,
    pub cap_id: u32,
    pub flow_id: u128,
    pub user_data: u64,
    pub payload: [u8; 24],
}
const _: () = assert!(core::mem::size_of::<SqEntry>() == 64);

/// A completion entry - 32 bytes.
#[repr(C, align(32))]
#[derive(Copy, Clone)]
pub struct CqEntry {
    pub flow_id: u128,
    pub user_data: u64,
    pub status: u32,
    pub result: u32,
}
const _: () = assert!(core::mem::size_of::<CqEntry>() == 32);

/// The shared ring header (kernel/src/queue/mod.rs `QueueHeader`).
#[repr(C)]
struct QueueHeader {
    version: u32,
    depth: u32,
    sq_off: u32,
    cq_off: u32,
    sq_head: AtomicU32,
    sq_tail: AtomicU32,
    cq_head: AtomicU32,
    cq_tail: AtomicU32,
    _reserved: [u32; 8],
}

/// On-wire ABI version this build understands.
pub const QUEUE_ABI_VERSION: u32 = 1;

/// A queue-pair overlay bound to the cell's mapped ring region. Reads the
/// header's `sq_off`/`cq_off` so it stays correct if the geometry moves within
/// the ABI version; the head/tail atomics live in the region, shared with the
/// kernel's own overlay over the same physical frames.
pub struct Qp {
    base: *mut u8,
    depth: u32,
}

impl Qp {
    /// Bind to the region at `base` (the `qp_va` from `queue_info`).
    ///
    /// # Safety
    /// `base` must be this cell's mapped queue region, initialised by the
    /// kernel. Panics if the ABI version does not match.
    pub unsafe fn attach(base: *mut u8) -> Qp {
        let h = base as *const QueueHeader;
        let version = unsafe { (*h).version };
        assert!(version == QUEUE_ABI_VERSION, "queue ABI version mismatch");
        let depth = unsafe { (*h).depth };
        Qp { base, depth }
    }

    #[inline(always)]
    fn header(&self) -> *const QueueHeader {
        self.base as *const QueueHeader
    }
    #[inline(always)]
    fn sq_entries(&self) -> *mut SqEntry {
        unsafe { self.base.add((*self.header()).sq_off as usize) as *mut SqEntry }
    }
    #[inline(always)]
    fn cq_entries(&self) -> *mut CqEntry {
        unsafe { self.base.add((*self.header()).cq_off as usize) as *mut CqEntry }
    }

    /// Push one submission carrying up to 24 bytes of args and the op `flags`
    /// (e.g. [`FLAG_INLINE`]). Returns false if the ring is full.
    pub fn submit(
        &self,
        opcode: u8,
        flags: u8,
        cap_id: u32,
        flow_id: u128,
        user_data: u64,
        args: &[u8],
    ) -> bool {
        let h = self.header();
        // SAFETY: the header lives at the region base for the overlay's life.
        let (sq_head, sq_tail) = unsafe { (&(*h).sq_head, &(*h).sq_tail) };
        let head = sq_head.load(Ordering::Relaxed);
        let tail = sq_tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.depth {
            return false;
        }
        let idx = (head & (self.depth - 1)) as usize;
        let n = args.len().min(24);
        // SAFETY: idx is in-bounds; the ring is shared, mapped memory.
        unsafe {
            let slot = self.sq_entries().add(idx);
            let mut e = SqEntry {
                opcode,
                flags,
                engine_id: 0,
                cap_id,
                flow_id,
                user_data,
                payload: [0; 24],
            };
            e.payload[..n].copy_from_slice(&args[..n]);
            core::ptr::write_volatile(slot, e);
        }
        sq_head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pop one submission from the SQ - the **consumer** side of a cross-cell
    /// channel (the server end; docs/LIBRHEO.md Phase E). None if the SQ is
    /// empty. The client uses [`submit`](Self::submit) (producer); over one
    /// shared region the two drive opposite sides of the SPSC ring.
    pub fn sq_pop(&self) -> Option<SqEntry> {
        let h = self.header();
        // SAFETY: the header lives at the region base for the overlay's life.
        let (sq_head, sq_tail) = unsafe { (&(*h).sq_head, &(*h).sq_tail) };
        let tail = sq_tail.load(Ordering::Relaxed);
        let head = sq_head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail & (self.depth - 1)) as usize;
        // SAFETY: idx in-bounds; volatile read matches the producer's writer.
        let e = unsafe { core::ptr::read_volatile(self.sq_entries().add(idx)) };
        sq_tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(e)
    }

    /// Push one completion onto the CQ - the **producer** side of a cross-cell
    /// channel (the server end; docs/LIBRHEO.md Phase E). false if the CQ is
    /// full. The client uses [`reap`](Self::reap) (consumer).
    pub fn cq_push(&self, e: CqEntry) -> bool {
        let h = self.header();
        // SAFETY: the header lives at the region base for the overlay's life.
        let (cq_head, cq_tail) = unsafe { (&(*h).cq_head, &(*h).cq_tail) };
        let head = cq_head.load(Ordering::Relaxed);
        let tail = cq_tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.depth {
            return false;
        }
        let idx = (head & (self.depth - 1)) as usize;
        // SAFETY: idx in-bounds; the ring is shared, mapped memory.
        unsafe { core::ptr::write_volatile(self.cq_entries().add(idx), e) };
        cq_head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Whether the SQ holds an unconsumed entry - a **non-destructive** peek used
    /// by a service cell to measure how many client requests are in flight at once
    /// (docs/NETSTACK.md the service-cell section, rheo-net N4a). Reads the same
    /// two shared indices `sq_pop` does, and pops nothing.
    pub fn sq_pending(&self) -> bool {
        let h = self.header();
        // SAFETY: the header lives at the region base for the overlay's life.
        let (sq_head, sq_tail) = unsafe { (&(*h).sq_head, &(*h).sq_tail) };
        sq_tail.load(Ordering::Relaxed) != sq_head.load(Ordering::Acquire)
    }

    /// Whether the CQ holds an unconsumed entry - the [`sq_pending`](Self::sq_pending)
    /// twin for the other ring direction.
    pub fn cq_pending(&self) -> bool {
        let h = self.header();
        // SAFETY: as above.
        let (cq_head, cq_tail) = unsafe { (&(*h).cq_head, &(*h).cq_tail) };
        cq_tail.load(Ordering::Relaxed) != cq_head.load(Ordering::Acquire)
    }

    /// Pop one completion, or None if the completion ring is empty.
    pub fn reap(&self) -> Option<CqEntry> {
        let h = self.header();
        // SAFETY: as above.
        let (cq_head, cq_tail) = unsafe { (&(*h).cq_head, &(*h).cq_tail) };
        let tail = cq_tail.load(Ordering::Relaxed);
        let head = cq_head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail & (self.depth - 1)) as usize;
        // SAFETY: idx in-bounds; volatile read matches the kernel's writer.
        let e = unsafe { core::ptr::read_volatile(self.cq_entries().add(idx)) };
        cq_tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(e)
    }
}
