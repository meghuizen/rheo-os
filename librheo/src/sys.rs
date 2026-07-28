//! Raw syscalls and the on-wire queue ABI, from the cell's side (docs/
//! LIBRHEO.md). The syscall numbers and the `repr(C)` structs are duplicates
//! of `kernel/src/abi.rs` and `kernel/src/queue/mod.rs` - the established
//! pattern (a cell cannot depend on the kernel crate), kept in sync by hand.
//!
//! Arguments go in the ISA's argument registers (riscv a0.., arm x0.., x86
//! rdi/rsi/rdx), the number in the syscall-number register, the result back in
//! the first argument register.

use core::arch::asm;
use core::sync::atomic::Ordering;

// ---- the ABI: one definition, re-exported ------------------------------------
//
// Syscall numbers, queue opcodes, status codes and every `repr(C)` result block
// live in the `rheo-abi` crate - the same crate `kernel::abi` and
// `kernel::queue` re-export - so this file cannot drift from the kernel
// (docs/ARCHITECTURE-DEBT.md 3.1). It used to restate all of it by hand under a
// "keep in sync" comment: geometry drift was caught by the version check, but a
// field-meaning change or a moved syscall number was a wrong number with no
// fault. `pub use` keeps every existing path (`sys::SYS_DOORBELL`,
// `sys::SqEntry`, ...) working.
pub use rheo_abi::{
    BufReduceDesc, ChannelInfo, CpuFeatures, CqEntry, EngineInfo, FLAG_DUR_FLUSH, FLAG_DUR_FUA,
    FLAG_INLINE, GrantInfo, GraphNode, INLINE_MAX, OP_CHAN_MSG, OP_CLOSE, OP_ECHO, OP_FSTAT,
    OP_GPU_PRESENT, OP_GRAPH_SUBMIT, OP_NET_MAC, OP_NET_RX, OP_NET_TX, OP_NOP, OP_OPEN, OP_READ,
    OP_WRITE, QUEUE_ABI_VERSION, QueueHeader, QueueInfo, ReserveInfo, SIMD_AVX2, SIMD_AVX512F,
    SIMD_AVX512VNNI, SIMD_NEON, SIMD_SSE2, SPAWN_CHAN_SLOT, STATUS_BAD_HANDLE, STATUS_BAD_OPCODE,
    STATUS_DENIED, STATUS_EXHAUSTED, STATUS_IO, STATUS_OK, STATUS_REVOKED, SYS_ARM_TIMER,
    SYS_COMMIT, SYS_CONNECT, SYS_CPUINFO, SYS_CYCLES, SYS_DECOMMIT, SYS_DOORBELL, SYS_ENGINE_INFO,
    SYS_EXIT_GROUP, SYS_GRANT, SYS_GRANT_SHARE, SYS_MMAP, SYS_MMAP_FILE, SYS_MUNMAP,
    SYS_QUEUE_INFO, SYS_RANDOM, SYS_RESERVE_ADMIT, SYS_RESERVE_QUERY, SYS_RESERVE_RELEASE,
    SYS_SEAL, SYS_SPAWN, SYS_SWITCH, SYS_UPTIME, SYS_VCORE_INFO, SYS_WAIT, SYS_WAIT_INPUT,
    SYS_WAIT_NET, SYS_WRITE_FD, SYS_YIELD, ShareInfo, SqEntry, TIMER_CLIENT_CELL_SLEEP,
    TIMER_CLIENT_PACER, TileGemmDesc, VcoreInfo, spawn_chan_spec,
};

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
/// [`munmap`] keeping the kernel's answer: 0 accepted, `u64::MAX` refused. The
/// kernel only tears down frames this cell owns - a typed grant it holds a MAP
/// capability on, or its own anon/file mmap regions - so a shared channel ring, a
/// peer's shared grant and the queue region are all refused
/// (docs/ENGINEERING.md 12).
pub fn munmap_checked(va: usize, len: usize) -> u64 {
    unsafe { syscall2(SYS_MUNMAP, va as u64, len as u64) }
}
/// Map `len` bytes of the file open on `fd` at `offset` into the cell; returns
/// the base VA (0 fails).
pub fn mmap_file(fd: u64, offset: u64, len: usize, flags: u64) -> usize {
    unsafe { syscall4(SYS_MMAP_FILE, fd, offset, len as u64, flags) as usize }
}
/// Read the CPU engine's introspection block (kind + measured throughput +
/// preemption contract). See `compute::Engine::info`.
pub fn engine_info() -> EngineInfo {
    engine_info_at(0).1
}

/// Read engine `index` from the kernel's engine table (0 = CPU, then the
/// PCIe-enumerated GPUs; docs/GPU-HARDWARE.md 9). Returns
/// `(engine_count, info)`; for an out-of-range index the info block is
/// zeroed and only the count is meaningful.
pub fn engine_info_at(index: u64) -> (u64, EngineInfo) {
    let mut info = EngineInfo {
        kind: 0,
        measured_cost_ticks: 0,
        preemption: 0,
        vendor: 0,
    };
    let n = unsafe { syscall2(SYS_ENGINE_INFO, &mut info as *mut EngineInfo as u64, index) };
    (n, info)
}

/// Number of engines the kernel has registered (CPU + recognised GPUs).
pub fn engine_count() -> u64 {
    let mut info = EngineInfo {
        kind: 0,
        measured_cost_ticks: 0,
        preemption: 0,
        vendor: 0,
    };
    unsafe {
        syscall2(
            SYS_ENGINE_INFO,
            &mut info as *mut EngineInfo as u64,
            u64::MAX,
        )
    }
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
/// one-shot deadline (docs/LIBRHEO.md Phase F). `time` builds on it.
pub fn arm_timer(deadline_ns: u64) {
    arm_timer_as(deadline_ns, TIMER_CLIENT_CELL_SLEEP);
}

/// [`arm_timer`] in a chosen kernel timer-arbiter slot (docs/NETSTACK.md 21): a
/// paced transport passes [`TIMER_CLIENT_PACER`] so its continuously re-armed
/// send deadline never contends with the cell's own `sleep`.
pub fn arm_timer_as(deadline_ns: u64, client: u64) {
    unsafe { syscall2(SYS_ARM_TIMER, deadline_ns, client) };
}
/// Monotonic uptime in raw ticks. `time::now` converts to nanoseconds.
pub fn uptime() -> u64 {
    unsafe { syscall1(SYS_UPTIME, 0) }
}
pub fn random_u64() -> u64 {
    unsafe { syscall1(SYS_RANDOM, 0) }
}
/// Monotonic cycle counter (raw ticks) for in-cell micro-benchmarks (the SIMD
/// probe times tiers with this). Under QEMU icount this is not wall-clock, so
/// it is honest only for relative ordering on real hardware (docs/TILES.md 4).
pub fn cycles() -> u64 {
    unsafe { syscall1(SYS_CYCLES, 0) }
}

/// This cell's CPU feature report (docs/TILES.md 4): the raw CPUID bitmask, the
/// portable `SIMD_*` tier mask of kernel-enabled+validated widths, and the
/// vendor string. The `tile::simd` probe reads `simd` to choose its dispatch.
pub fn cpu_features() -> CpuFeatures {
    let mut f = CpuFeatures {
        features: 0,
        simd: 0,
        vendor: [0; 16],
    };
    unsafe { syscall1(SYS_CPUINFO, &mut f as *mut CpuFeatures as u64) };
    f
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

/// Ask the kernel which **vcore** this is and how many the cell holds
/// (docs/SUBSTRATE.md pillar 3, docs/CONCURRENCY.md).
///
/// The number every per-vcore structure in a cell keys on - the strand executor, its
/// local run queue, the ring [`queue_info`] reports - and the one thing a cell cannot
/// work out for itself: there is no register it may read that says "you are context 1
/// of your cell". Only the kernel knows, because the kernel decided.
pub fn vcore_info() -> Option<VcoreInfo> {
    let mut info = VcoreInfo { index: 0, count: 1 };
    let r = unsafe { syscall1(SYS_VCORE_INFO, &mut info as *mut VcoreInfo as u64) };
    if r == u64::MAX { None } else { Some(info) }
}

/// This vcore's index, or 0 when the kernel does not answer.
///
/// Shaped as `fn() -> usize` so it can be handed straight to
/// `runtime::strand::set_vcore_hook`, which is the whole reason the verb exists: the
/// runtime is *told* its index rather than inventing one, and this is the telling.
pub fn vcore_index() -> usize {
    vcore_info().map(|i| i.index as usize).unwrap_or(0)
}

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
