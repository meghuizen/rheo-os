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
pub fn write(fd: u64, buf_va: u64, len: u64) -> i64 {
    unsafe { syscall3(SYS_WRITE_FD, fd, buf_va, len) as i64 }
}
pub fn random_u64() -> u64 {
    unsafe { syscall1(SYS_RANDOM, 0) }
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
