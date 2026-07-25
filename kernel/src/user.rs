//! The user-mode run loop and syscall dispatch (BUILD-ORDER.md step 5).
//! Portable: the arch trampolines save/restore U-mode state and call
//! `on_user_trap`; this module owns the policy - which cell is current,
//! what each syscall does, and how a cell's run ends.
//!
//! Cross-cell switching (`SYS_SWITCH`) is a directed hand-off between two
//! user cells with a real address-space switch each way - the mechanism
//! the P5 benchmark measures against seL4's cross-vspace IPC.
//!
//! Global mutable state with raw pointers is deliberate here: this is the
//! per-CPU scheduler, there is one CPU at this stage, and traps are
//! synchronous, so there is no concurrent access to guard against.

use crate::abi::{
    GrantInfo, QueueInfo, ReserveInfo, SYS_COMMIT, SYS_CYCLES, SYS_DECOMMIT, SYS_DOORBELL,
    SYS_EXIT, SYS_EXIT_GROUP, SYS_GRANT, SYS_MMAP, SYS_MMAP_FILE, SYS_MUNMAP, SYS_QUEUE_INFO,
    SYS_RESERVE_ADMIT, SYS_RESERVE_QUERY, SYS_RESERVE_RELEASE, SYS_SEAL, SYS_SWITCH,
    SYS_WAIT_INPUT,
};
use crate::arch::{self, FaultCause, MapPerm, TrapFrame, TrapKind};
use crate::capability::{BUDGET_UNLIMITED, CapTable, MAP, ObjectKind, ObjectTable, READ, WRITE};
use crate::mm::{AddressSpace, frames};
use crate::queue::{self, QueuePair};
use crate::sched::{Admission, AdmitError, Reservation};

/// Base VA of the per-cell anonymous mmap region (docs/USERLAND.md M2): 12 GiB,
/// above the image (1-4 GiB) and stack (8 GiB), free in every cell root.
const MMAP_BASE: usize = 0x3_0000_0000;
static mut MMAP_NEXT: usize = MMAP_BASE;

/// Why a cell's run ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Exited(u64),
    Faulted(usize),
}

/// Which syscall ABI a cell speaks (docs/LINUX-COMPAT.md 2). Dispatch
/// branches on this BEFORE interpreting the syscall number - native numbers
/// 1-30 collide with Linux numbers (Linux x86-64 `write` = 1 is native
/// `SYS_DOORBELL`), so the tag, not the number, decides the table.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Personality {
    /// The native rheo-os ABI (kernel/src/abi.rs) - the default.
    Native,
    /// The Linux syscall ABI, translated by `crate::linux`.
    Linux,
}

#[derive(Copy, Clone)]
struct RunCell {
    aspace: *const AddressSpace,
    caps: *mut CapTable,
    objects: *const ObjectTable,
    qp: *const QueuePair,
    frame: *mut TrapFrame,
    outcome: Option<Outcome>,
    present: bool,
    personality: Personality,
    /// Base VA of the cell's mapped queue-pair region, reported by
    /// `SYS_QUEUE_INFO` (docs/LIBRHEO.md). 0 = the cell has no mapped queue.
    qp_va: u64,
    /// 32-bit ABI id of the cell's QueuePair capability, reported alongside.
    qp_cap_id: u32,
    /// Next free VA for a typed memory-grant reservation (`SYS_GRANT`,
    /// docs/LIBRHEO.md Phase B). Per-cell so two cells' grants never collide.
    grant_next: usize,
    /// Next free VA for a file mmap (`SYS_MMAP_FILE`).
    filemmap_next: usize,
}

const EMPTY: RunCell = RunCell {
    aspace: core::ptr::null(),
    caps: core::ptr::null_mut(),
    objects: core::ptr::null(),
    qp: core::ptr::null(),
    frame: core::ptr::null_mut(),
    outcome: None,
    present: false,
    personality: Personality::Native,
    qp_va: 0,
    qp_cap_id: 0,
    grant_next: GRANT_BASE,
    filemmap_next: FILEMMAP_BASE,
};

/// Base VA of a native cell's typed memory-grant reservations (docs/LIBRHEO.md
/// Phase B): 32 GiB, above the image (1-4), stack (8), anon mmap (12+), and
/// queue (16) regions, free in every cell root. Reservations are pure address
/// space (48-bit VA), so multi-GiB grants cost nothing until committed.
const GRANT_BASE: usize = 0x8_0000_0000;
/// Base VA of a native cell's file mmaps (`SYS_MMAP_FILE`): 20 GiB.
const FILEMMAP_BASE: usize = 0x5_0000_0000;

/// Typed memory grants a native cell holds (docs/LIBRHEO.md Phase B). Fixed
/// per-cell table, like the Linux fd table - no allocation. A grant is a typed
/// address-space reservation + the minted MemoryGrant capability that gates
/// commit/decommit/seal.
#[derive(Copy, Clone)]
struct GrantSlot {
    in_use: bool,
    base: usize,
    len: usize,
    /// `mm::grant::MemKind` discriminant (0=DDR..5=Remote), validated and
    /// recorded at `SYS_GRANT`. Only DDR is real in QEMU; HBM/CXL/PMEM/Remote
    /// are backed by DDR frames (emulated, honest); DeviceBar has no backing and
    /// is refused. Recorded for future NUMA/placement differentiation; the
    /// commit path treats every backed kind as DDR today.
    #[allow(dead_code)]
    kind: u8,
    sealed: bool,
    cap_id: u32,
}

const EMPTY_GRANT: GrantSlot = GrantSlot {
    in_use: false,
    base: 0,
    len: 0,
    kind: 0,
    sealed: false,
    cap_id: 0,
};

const MAX_GRANTS_PER_CELL: usize = 16;
static mut CELL_GRANTS: [[GrantSlot; MAX_GRANTS_PER_CELL]; MAX_CELLS] =
    [[EMPTY_GRANT; MAX_GRANTS_PER_CELL]; MAX_CELLS];

fn cell_grants(cur: usize) -> &'static mut [GrantSlot; MAX_GRANTS_PER_CELL] {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_GRANTS))[cur] }
}

/// One admitted CPU reservation a native cell holds (docs/LIBRHEO.md Phase C,
/// docs/ARCHITECTURE.md 3 object 7). Fixed per-cell table, like the grant
/// table. The admission MATH (EDF utilization) is real and enforced at admit;
/// actual run-queue enforcement is SMP/preemption work (task #27), documented.
#[derive(Copy, Clone)]
struct ResSlot {
    in_use: bool,
    /// The admitted reservation (carries the util to free on release).
    res: Reservation,
    cap_id: u32,
}

const EMPTY_RES: ResSlot = ResSlot {
    in_use: false,
    res: Reservation::ZERO,
    cap_id: 0,
};

const MAX_RES_PER_CELL: usize = 8;
static mut CELL_RES: [[ResSlot; MAX_RES_PER_CELL]; MAX_CELLS] =
    [[EMPTY_RES; MAX_RES_PER_CELL]; MAX_CELLS];

/// Per-cell EDF admission controller (docs/SCHEDULING.md 4): tracks the cell's
/// committed CPU utilization and refuses a set it cannot guarantee.
const EMPTY_ADMISSION: Admission = Admission::new();
static mut CELL_ADMISSION: [Admission; MAX_CELLS] = [EMPTY_ADMISSION; MAX_CELLS];

fn cell_res(cur: usize) -> &'static mut [ResSlot; MAX_RES_PER_CELL] {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_RES))[cur] }
}

fn cell_admission(cur: usize) -> &'static mut Admission {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_ADMISSION))[cur] }
}

/// Number of runnable cell slots. Bumped 8 -> 16 for the Linux personality's
/// **processes** (docs/LINUX-COMPAT.md L6): a shell plus several concurrent
/// pipeline stages (each `fork` claims a slot until it is reaped). The native
/// `SYS_SWITCH` test's `cur ^ 1` pairing (cells 0/1) is unaffected.
pub const MAX_CELLS: usize = 16;

static mut CELLS: [RunCell; MAX_CELLS] = [EMPTY; MAX_CELLS];
static mut CURRENT: usize = 0;
static mut EXITED: usize = 0;
/// The cell `run` was entered with (docs/LINUX-COMPAT.md L6): the top of the
/// Linux process tree. Only its exit ends the whole run; a forked child's exit
/// makes it a zombie and reschedules another cell (`linux::proc`).
static mut TOP_CELL: usize = 0;

fn cells() -> &'static mut [RunCell; MAX_CELLS] {
    // SAFETY: single CPU, synchronous traps; no aliasing run concurrently.
    unsafe { &mut *core::ptr::addr_of_mut!(CELLS) }
}

/// Clear the run table (call before installing a fresh set of cells).
pub fn reset() {
    *cells() = [EMPTY; MAX_CELLS];
    // SAFETY: single CPU, between runs.
    unsafe {
        *core::ptr::addr_of_mut!(MMAP_NEXT) = MMAP_BASE;
        *core::ptr::addr_of_mut!(CELL_GRANTS) = [[EMPTY_GRANT; MAX_GRANTS_PER_CELL]; MAX_CELLS];
        *core::ptr::addr_of_mut!(CELL_RES) = [[EMPTY_RES; MAX_RES_PER_CELL]; MAX_CELLS];
        *core::ptr::addr_of_mut!(CELL_ADMISSION) = [EMPTY_ADMISSION; MAX_CELLS];
    }
    crate::linux::reset();
}

/// The index of the currently running cell (docs/LINUX-COMPAT.md L2). The
/// Linux personality keys its per-cell state (fd table, brk, mmap cursor) on
/// this.
pub fn current_index() -> usize {
    unsafe { *core::ptr::addr_of!(CURRENT) }
}

/// The installed frame pointer for cell `idx`. The Linux personality's thread
/// table uses this as its initial (thread 0) execution context
/// (docs/LINUX-COMPAT.md L4); clone-created threads get kernel-owned frames.
pub fn cell_frame(idx: usize) -> *mut TrapFrame {
    cells()[idx].frame
}

/// Run `f` against the current cell's address space, then re-activate it so
/// any new/removed mappings take effect (TLB flush). The mechanism the Linux
/// personality's memory syscalls (brk/mmap/munmap/mprotect) build on; mirrors
/// how `mmap_anon` publishes fresh mappings.
pub fn with_current_aspace<R>(f: impl FnOnce(&mut AddressSpace) -> R) -> R {
    let cell = cells()[current_index()];
    // SAFETY: single CPU; the running cell's address space is uniquely owned
    // for the duration of the synchronous trap.
    let aspace = unsafe { &mut *(cell.aspace as *mut AddressSpace) };
    let r = f(aspace);
    aspace.activate();
    r
}

/// Map fresh zeroed frames for `[va, va+len)` in the current cell with `perm`.
/// `va` and `len` need not be page-aligned; whole overlapping pages are mapped.
pub fn map_anon_at(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            let pa = frames::alloc();
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Unmap every whole page in `[va, va+len)` in the current cell and free the
/// frames. Pages that were not mapped are skipped.
pub fn unmap_range(va: usize, len: usize) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            if let Some(pa) = aspace.unmap(a) {
                frames::free(pa);
            }
            a += frames::FRAME_SIZE;
        }
    });
}

/// Commit every whole page in `[va, va+len)` in the current cell with `perm`:
/// pages already mapped are reprotected in place (their frame and contents
/// kept); pages not yet mapped get a fresh zeroed frame. This is the
/// demand-commit path for `mprotect` making a reserved region accessible
/// (docs/LINUX-COMPAT.md L4) - glibc reserves large `PROT_NONE` regions
/// (per-thread malloc arenas, thread-stack guards) and commits sub-ranges as it
/// grows, so eager backing on `mmap` would exhaust the frame pool.
pub fn commit_range(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            // Unmap returns the existing frame (if any) so a reprotect keeps
            // the page's contents; otherwise allocate a fresh zeroed frame.
            let pa = aspace.unmap(a).unwrap_or_else(frames::alloc);
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Change the permission of every whole page in `[va, va+len)` in the current
/// cell, keeping the frames.
pub fn protect_range(va: usize, len: usize, perm: MapPerm) {
    if len == 0 {
        return;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let end = va + len;
    with_current_aspace(|aspace| {
        let mut a = base;
        while a < end {
            aspace.protect(a, perm);
            a += frames::FRAME_SIZE;
        }
    });
}

/// Back `len` bytes of fresh zeroed RW pages into the current cell and return
/// the base VA (0 on empty request). A bump allocator over the anon region -
/// the primitive the libc's malloc grows its heap with (docs/USERLAND.md M2).
fn mmap_anon(cur: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let cell = cells()[cur];
    // SAFETY: single CPU; the running cell's address space is uniquely owned
    // for the duration of the run, and adding not-present -> present leaf
    // entries to the active root is safe (re-activated below to publish them).
    let aspace = unsafe { &mut *(cell.aspace as *mut AddressSpace) };
    let pages = len.div_ceil(frames::FRAME_SIZE);
    let base = unsafe { *core::ptr::addr_of!(MMAP_NEXT) };
    for i in 0..pages {
        let pa = frames::alloc();
        aspace.map_user_frame(base + i * frames::FRAME_SIZE, pa, MapPerm::UserRw);
    }
    unsafe {
        *core::ptr::addr_of_mut!(MMAP_NEXT) = base + pages * frames::FRAME_SIZE;
    }
    // Publish the new mappings on the active root (fence / tlbi / cr3 reload).
    aspace.activate();
    base
}

fn page_up(x: usize) -> usize {
    (x + frames::FRAME_SIZE - 1) & !(frames::FRAME_SIZE - 1)
}

/// `SYS_GRANT`: reserve `len` bytes of typed grant address space, mint a
/// MemoryGrant capability into the cell's table, and write `GrantInfo { base,
/// cap_id }` at `out_va`. Returns 0, or `u64::MAX` on failure. The reservation
/// costs no frames (demand commit); `kind` names the memory type
/// (`mm::grant::MemKind` discriminant). DeviceBar (4) has no backing here and is
/// refused; the other non-DDR kinds are emulated on DDR (documented).
fn grant_create(cur: usize, out_va: u64, len: usize, kind: u64, _flags: u64) -> u64 {
    if len == 0 || kind > 5 || kind == 4 {
        return u64::MAX; // empty / unknown kind / device-BAR (no backing)
    }
    let bytes = page_up(len);
    let cell = cells()[cur];
    if cell.caps.is_null() || cell.objects.is_null() {
        return u64::MAX;
    }
    // Mint a real MemoryGrant capability (READ|WRITE|MAP) so commit/decommit/
    // seal are capability-gated. SAFETY: single CPU, synchronous trap; the
    // cell's tables are uniquely owned for the trap. `objects` is installed as
    // `*const` but the test kernel owns it as a mutable static; creating a new
    // object here needs `&mut`, which this cast recovers.
    let cap_id = unsafe {
        let objects = &mut *(cell.objects as *mut ObjectTable);
        let caps = &mut *cell.caps;
        let Ok(obj) = objects.create(ObjectKind::MemoryGrant) else {
            return u64::MAX;
        };
        match caps.mint(objects, obj, READ | WRITE | MAP, BUDGET_UNLIMITED) {
            Ok(h) => h.raw_low32(),
            Err(_) => return u64::MAX,
        }
    };
    // Record the reservation in the per-cell grant table.
    let table = cell_grants(cur);
    let Some(slot) = table.iter_mut().find(|s| !s.in_use) else {
        return u64::MAX;
    };
    let base = cells()[cur].grant_next;
    cells()[cur].grant_next = base + bytes;
    *slot = GrantSlot {
        in_use: true,
        base,
        len: bytes,
        kind: kind as u8,
        sealed: false,
        cap_id,
    };
    // SAFETY: `out_va` is a user VA in the running cell's active address space,
    // sized for a `GrantInfo` (the cell passes its own stack slot).
    unsafe {
        (out_va as *mut GrantInfo).write(GrantInfo {
            base: base as u64,
            cap_id: cap_id as u64,
        });
    }
    0
}

/// Find the grant slot addressed by `cap_id` after grant-checking that the cell
/// still holds a valid MemoryGrant capability with the MAP right (revocation /
/// budget enforced here). Returns `(base, len, sealed)` or None.
fn grant_resolve(cur: usize, cap_id: u32) -> Option<(usize, usize, bool)> {
    let cell = cells()[cur];
    if cell.caps.is_null() || cell.objects.is_null() {
        return None;
    }
    // SAFETY: single CPU, synchronous trap; tables uniquely owned.
    let ok = unsafe {
        let objects = &*cell.objects;
        let caps = &mut *cell.caps;
        caps.grant_check_low32(objects, cap_id, MAP).is_ok()
    };
    if !ok {
        return None;
    }
    let slot = cell_grants(cur)
        .iter()
        .find(|s| s.in_use && s.cap_id == cap_id)?;
    Some((slot.base, slot.len, slot.sealed))
}

/// `SYS_COMMIT`: back `[offset, offset+len)` of the grant with fresh zeroed RW
/// frames. Refused on a sealed grant or an out-of-range span. Returns 0 or
/// `u64::MAX`.
fn grant_commit(cur: usize, cap_id: u32, offset: usize, len: usize) -> u64 {
    let Some((base, glen, sealed)) = grant_resolve(cur, cap_id) else {
        return u64::MAX;
    };
    if sealed || offset.saturating_add(len) > glen {
        return u64::MAX;
    }
    commit_range(base + offset, len, MapPerm::UserRw);
    0
}

/// `SYS_DECOMMIT`: free the frames backing `[offset, offset+len)`; the
/// reservation and capability remain. Refused on a sealed grant.
fn grant_decommit(cur: usize, cap_id: u32, offset: usize, len: usize) -> u64 {
    let Some((base, glen, sealed)) = grant_resolve(cur, cap_id) else {
        return u64::MAX;
    };
    if sealed || offset.saturating_add(len) > glen {
        return u64::MAX;
    }
    unmap_range(base + offset, len);
    0
}

/// `SYS_SEAL`: make the grant immutable - its committed pages become read-only
/// (shareable), and further commit/decommit are refused. Returns 0 or
/// `u64::MAX`.
fn grant_seal(cur: usize, cap_id: u32) -> u64 {
    let Some((base, glen, _)) = grant_resolve(cur, cap_id) else {
        return u64::MAX;
    };
    protect_range(base, glen, MapPerm::UserRo);
    if let Some(slot) = cell_grants(cur)
        .iter_mut()
        .find(|s| s.in_use && s.cap_id == cap_id)
    {
        slot.sealed = true;
    }
    0
}

/// `SYS_MMAP_FILE`: map `len` bytes of the file open on `fd` into the current
/// cell at a fresh file-mmap VA, reading the file range page-by-page into fresh
/// frames (MAP_PRIVATE; a short read leaves the page tail zero). The bytes are
/// read through the registered `svc::FileOps` (the same VFS the POSIX
/// personality uses) into each frame via the kernel linear map, then the frame
/// is mapped read-only. Returns the base VA, or 0 on failure.
fn mmap_file(cur: usize, fd: u64, offset: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let Some(ops) = crate::svc::file_ops() else {
        return 0;
    };
    let bytes = page_up(len);
    let base = cells()[cur].filemmap_next;
    cells()[cur].filemmap_next = base + bytes;
    let pages = bytes / frames::FRAME_SIZE;
    with_current_aspace(|aspace| {
        for i in 0..pages {
            let va = base + i * frames::FRAME_SIZE;
            let pa = frames::alloc(); // zeroed
            let file_off = offset as i64 + (i * frames::FRAME_SIZE) as i64;
            let want = (len - i * frames::FRAME_SIZE).min(frames::FRAME_SIZE);
            // Read into the frame through the kernel linear map (the frame is
            // not yet user-mapped, so this cannot alias user memory). A short
            // read (EOF) leaves the tail zero. The VFS handler runs in kernel
            // context, so a kernel VA is a valid destination.
            let kva = arch::phys_to_virt(pa) as u64;
            (ops.lseek)(fd, file_off, 0); // SEEK_SET
            (ops.read)(fd, kva, want as u64);
            aspace.map_user_frame(va, pa, MapPerm::UserRo);
        }
    });
    base
}

/// `SYS_RESERVE_ADMIT`: run the EDF schedulability test for a CPU reservation
/// (budget/period/deadline) plus an advisory memory floor, and on success mint a
/// Reservation capability into the cell's table and write `ReserveInfo { handle,
/// committed_ppm }` at `out_va` (docs/LIBRHEO.md Phase C, object 7). Returns 0 on
/// success or a rejection code (1=BadParams, 2=Overcommit, 3=MemoryFloor) - a
/// refused reservation returns cleanly, never faults. The admission MATH is real
/// and enforced here; run-queue enforcement is SMP/preemption work (task #27).
fn reserve_admit(
    cur: usize,
    out_va: u64,
    budget: u64,
    period: u64,
    deadline: u64,
    mem_floor_pages: u64,
) -> u64 {
    let cell = cells()[cur];
    if cell.caps.is_null() || cell.objects.is_null() {
        return 1; // no capability tables - treat as bad params
    }
    // Advisory memory floor: the reservation is honored only if the pool can
    // currently cover it (QEMU has no bandwidth/IO backend, so CPU is the real
    // guarantee; the floor is a documented advisory check, not a hold).
    let (free, _) = frames::stats();
    if mem_floor_pages > free as u64 {
        return 3;
    }
    // Find a free reservation slot before mutating the admission total.
    if cell_res(cur).iter().all(|s| s.in_use) {
        return 1;
    }
    let res = match cell_admission(cur).admit(budget, period, deadline) {
        Ok(r) => r,
        Err(AdmitError::BadParams) => return 1,
        Err(AdmitError::Overcommit) => return 2,
    };
    // Mint a Reservation capability (READ) so query/release are capability-gated,
    // mirroring the grant path. SAFETY: single CPU, synchronous trap; the cell's
    // tables are uniquely owned for the trap (the `*const` objects table is owned
    // mutably by the test kernel, recovered here to create a new object).
    let cap_id = unsafe {
        let objects = &mut *(cell.objects as *mut ObjectTable);
        let caps = &mut *cell.caps;
        let Ok(obj) = objects.create(ObjectKind::Reservation) else {
            cell_admission(cur).release(&res);
            return 1;
        };
        match caps.mint(objects, obj, READ, BUDGET_UNLIMITED) {
            Ok(h) => h.raw_low32(),
            Err(_) => {
                cell_admission(cur).release(&res);
                return 1;
            }
        }
    };
    let slot = cell_res(cur).iter_mut().find(|s| !s.in_use).unwrap();
    *slot = ResSlot {
        in_use: true,
        res,
        cap_id,
    };
    let committed = cell_admission(cur).committed_ppm();
    // SAFETY: `out_va` is a user VA in the running cell's active address space,
    // sized for a `ReserveInfo` (the cell passes its own stack slot).
    unsafe {
        (out_va as *mut ReserveInfo).write(ReserveInfo {
            handle: cap_id as u64,
            committed_ppm: committed,
        });
    }
    0
}

/// `SYS_RESERVE_RELEASE`: free an admitted reservation, returning its
/// utilization to the cell's admission controller (the RAII drop path). Returns
/// 0, or `u64::MAX` if the handle names no live reservation. Grant-checks the
/// Reservation capability (READ) so a forged/revoked handle is rejected.
fn reserve_release(cur: usize, cap_id: u32) -> u64 {
    let cell = cells()[cur];
    if cell.caps.is_null() || cell.objects.is_null() {
        return u64::MAX;
    }
    // SAFETY: single CPU, synchronous trap; tables uniquely owned.
    let ok = unsafe {
        let objects = &*cell.objects;
        let caps = &mut *cell.caps;
        caps.grant_check_low32(objects, cap_id, READ).is_ok()
    };
    if !ok {
        return u64::MAX;
    }
    let Some(slot) = cell_res(cur)
        .iter_mut()
        .find(|s| s.in_use && s.cap_id == cap_id)
    else {
        return u64::MAX;
    };
    let res = slot.res;
    slot.in_use = false;
    cell_admission(cur).release(&res);
    0
}

/// Register a runnable cell in slot `idx`. The pointers must outlive the
/// run (the test kernels own the backing storage as statics).
///
/// # Safety
/// All pointers must be valid and uniquely owned for the duration of the
/// run; `frame` must have been produced by `arch::trapframe_new`.
pub unsafe fn install(
    idx: usize,
    aspace: *const AddressSpace,
    caps: *mut CapTable,
    objects: *const ObjectTable,
    qp: *const QueuePair,
    frame: *mut TrapFrame,
) {
    cells()[idx] = RunCell {
        aspace,
        caps,
        objects,
        qp,
        frame,
        outcome: None,
        present: true,
        personality: Personality::Native,
        qp_va: 0,
        qp_cap_id: 0,
        grant_next: GRANT_BASE,
        filemmap_next: FILEMMAP_BASE,
    };
    *cell_grants(idx) = [EMPTY_GRANT; MAX_GRANTS_PER_CELL];
}

/// Record the mapped queue-pair region VA and capability id for cell `idx`
/// (docs/LIBRHEO.md). `SYS_QUEUE_INFO` reports these so a loaded librheo cell
/// can bind its ring. Call after `install`, before `run`.
pub fn set_queue_info(idx: usize, qp_va: u64, cap_id: u32) {
    assert!(cells()[idx].present, "set_queue_info on empty slot {idx}");
    cells()[idx].qp_va = qp_va;
    cells()[idx].qp_cap_id = cap_id;
}

/// Tag an installed cell with a syscall personality (call after `install`,
/// before `run`). Native is the default; a Linux cell's traps are handled by
/// `crate::linux` instead of the native dispatch.
pub fn set_personality(idx: usize, p: Personality) {
    assert!(cells()[idx].present, "set_personality on empty slot {idx}");
    cells()[idx].personality = p;
}

/// Run starting from cell `idx` until some cell exits or faults. Returns
/// which cell ended the run and how. Cross-cell switches keep running
/// inside the trampoline; only an exit or fault unwinds back here.
pub fn run(idx: usize) -> (usize, Outcome) {
    let cell = cells()[idx];
    assert!(cell.present, "run of empty cell slot {idx}");
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = idx;
        *core::ptr::addr_of_mut!(TOP_CELL) = idx;
        (*cell.aspace).activate();
        arch::enter_user_first(cell.frame);
    }
    // enter_user_first returns via return_to_kernel after an exit/fault.
    // Restore the kernel address space so setup code can again reach all
    // of RAM (a cell root only maps that cell's user pages).
    arch::paging_activate_kernel();
    let exited = unsafe { *core::ptr::addr_of!(EXITED) };
    (
        exited,
        cells()[exited].outcome.expect("no outcome recorded"),
    )
}

/// Turn a Linux personality `Ctl` into the frame to resume (or a null-frame
/// unwind for the top cell's exit). Shared by the syscall path and the
/// synchronous-fault path (docs/LINUX-COMPAT.md L6).
///
/// `Ctl::Switch` covers both a same-cell thread switch (L4) and a cross-cell
/// process switch (L6): in the latter `crate::linux::proc` has already updated
/// `CURRENT`, activated the target address space, and swapped FP/TLS, so here
/// the trampoline just resumes the returned frame.
fn linux_ctl(ctl: crate::linux::Ctl, frame: *mut TrapFrame) -> *mut TrapFrame {
    match ctl {
        crate::linux::Ctl::Ret(v) => {
            arch::set_syscall_ret(unsafe { &mut *frame }, v);
            frame
        }
        crate::linux::Ctl::Exit(code) => finish(Outcome::Exited(code)),
        crate::linux::Ctl::Switch(next) => next,
    }
}

/// Record why the current cell's run ended and signal an unwind by
/// returning a null frame (the arch trampoline calls return_to_kernel).
fn finish(outcome: Outcome) -> *mut TrapFrame {
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
    cells()[cur].outcome = Some(outcome);
    unsafe {
        *core::ptr::addr_of_mut!(EXITED) = cur;
    }
    core::ptr::null_mut()
}

/// The cell `run` was entered with - the top of the Linux process tree
/// (docs/LINUX-COMPAT.md L6). Only its exit unwinds `run`.
pub fn top_cell() -> usize {
    unsafe { *core::ptr::addr_of!(TOP_CELL) }
}

/// Whether cell `idx` currently holds a runnable/live cell.
pub fn cell_present(idx: usize) -> bool {
    cells()[idx].present
}

/// The address-space pointer installed for cell `idx` (the Linux process
/// scheduler's `fork`/`execve` build on it).
pub fn cell_aspace(idx: usize) -> *const AddressSpace {
    cells()[idx].aspace
}

/// Repoint cell `idx` at a new address space (after `execve` replaces the image,
/// docs/LINUX-COMPAT.md L6).
pub fn set_cell_aspace(idx: usize, aspace: *const AddressSpace) {
    cells()[idx].aspace = aspace;
}

/// Repoint cell `idx` at a new context-0 frame (after `execve`).
pub fn set_cell_frame(idx: usize, frame: *mut TrapFrame) {
    cells()[idx].frame = frame;
}

/// Make cell `idx` the current cell and activate its address space - the Linux
/// process scheduler's cross-cell switch (docs/LINUX-COMPAT.md L6), the same
/// mechanism the native `SYS_SWITCH` uses, driven from `crate::linux::proc`
/// instead of a syscall arm.
pub fn switch_to_cell(idx: usize) {
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = idx;
        (*cells()[idx].aspace).activate();
    }
}

/// Install a **forked** child in slot `idx` sharing the parent's capability
/// bundle (POSIX fork = clone-cell-within-capability-bundle, docs/POSIX-
/// PERSONALITY.md 2). The address space and frame are kernel-owned by
/// `crate::linux::proc`; personality is Linux.
///
/// # Safety
/// `aspace`/`frame` must outlive the child's run; `parent` must be present.
pub unsafe fn install_forked(
    idx: usize,
    aspace: *const AddressSpace,
    frame: *mut TrapFrame,
    parent: usize,
) {
    let p = cells()[parent];
    cells()[idx] = RunCell {
        aspace,
        caps: p.caps,
        objects: p.objects,
        qp: p.qp,
        frame,
        outcome: None,
        present: true,
        personality: Personality::Linux,
        qp_va: 0,
        qp_cap_id: 0,
        grant_next: GRANT_BASE,
        filemmap_next: FILEMMAP_BASE,
    };
}

/// Free cell slot `idx` (a reaped zombie). The slot becomes reusable by a
/// future `fork`.
pub fn free_cell(idx: usize) {
    cells()[idx] = EMPTY;
}

/// The trap dispatcher, called from each arch's U-mode trampoline. Returns
/// the frame to resume (the same cell, or the peer on a switch), or a null
/// pointer to signal an unwind back to the kernel (exit or fault) - the
/// arch wrapper is responsible for calling return_to_kernel on null, so
/// that per-ISA state (e.g. x86 GS) is restored symmetrically first.
///
/// Dereferencing the raw `frame` (the state the trampoline just saved) is
/// the whole point of the dispatcher.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn on_user_trap(
    kind: TrapKind,
    cause: FaultCause,
    fault_addr: usize,
    frame: *mut TrapFrame,
) -> *mut TrapFrame {
    if kind == TrapKind::Fault {
        let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
        // A Linux cell with an installed, unblocked handler for the fault's
        // signal gets synchronous delivery (SIGSEGV/SIGBUS/SIGILL/SIGFPE) by
        // trap-frame rewrite (docs/LINUX-COMPAT.md L5); otherwise it terminates
        // reporting 128+signo. A NATIVE cell fault is always terminal
        // (Outcome::Faulted) - signal delivery is behind the Linux branch only.
        if cells()[cur].personality == Personality::Linux {
            return match crate::linux::deliver_fault(cur, cause, fault_addr, frame) {
                crate::linux::FaultOutcome::Resume(f) => f,
                // A default (uncaught) fatal fault ends the process. For the top
                // cell this unwinds `run` (reporting 128+signo); a forked child
                // becomes a WIFSIGNALED zombie its parent reaps
                // (docs/LINUX-COMPAT.md L6).
                crate::linux::FaultOutcome::Terminate(signo) => {
                    linux_ctl(crate::linux::proc::exit_signaled(cur, signo), frame)
                }
            };
        }
        return finish(Outcome::Faulted(fault_addr));
    }

    let (nr, args) = arch::decode_syscall(unsafe { &*frame });
    let arg = args[0];
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };

    // Linux cells never reach native dispatch: the personality tag decides
    // the syscall table before the number means anything (the ABIs collide).
    if cells()[cur].personality == Personality::Linux {
        return linux_ctl(crate::linux::handle(cur, nr, &args, frame), frame);
    }

    match nr {
        SYS_DOORBELL => {
            let cell = cells()[cur];
            // SAFETY: the pointers were validated at install time. During the
            // trap the cell's address space is active, so a queue mapped at a
            // user VA (a loaded librheo cell) is reachable here.
            let n = unsafe { queue::kernel_process(&*cell.qp, &mut *cell.caps, &*cell.objects) };
            arch::set_syscall_ret(unsafe { &mut *frame }, n as u64);
            frame
        }
        SYS_QUEUE_INFO => {
            let cell = cells()[cur];
            let ret = if cell.qp_va == 0 {
                u64::MAX
            } else {
                // SAFETY: `arg` is a user VA in the running cell's active
                // address space, sized for a `QueueInfo` (16 bytes); the cell
                // passes the address of its own stack slot.
                unsafe {
                    (arg as *mut QueueInfo).write(QueueInfo {
                        qp_va: cell.qp_va,
                        cap_id: cell.qp_cap_id as u64,
                    });
                }
                0
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret);
            frame
        }
        SYS_CYCLES => {
            arch::set_syscall_ret(unsafe { &mut *frame }, arch::cycles());
            frame
        }
        SYS_SWITCH => {
            let peer = cur ^ 1;
            let peer_cell = cells()[peer];
            assert!(peer_cell.present, "SYS_SWITCH with no peer cell");
            unsafe {
                *core::ptr::addr_of_mut!(CURRENT) = peer;
                (*peer_cell.aspace).activate();
            }
            peer_cell.frame
        }
        SYS_EXIT | SYS_EXIT_GROUP => finish(Outcome::Exited(arg)),
        SYS_MMAP => {
            let base = mmap_anon(cur, args[0] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, base as u64);
            frame
        }
        // Typed memory grants exposed to the cell (docs/LIBRHEO.md Phase B).
        SYS_GRANT => {
            let r = grant_create(cur, args[0], args[1] as usize, args[2], args[3]);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_COMMIT => {
            let r = grant_commit(cur, args[0] as u32, args[1] as usize, args[2] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_DECOMMIT => {
            let r = grant_decommit(cur, args[0] as u32, args[1] as usize, args[2] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_SEAL => {
            let r = grant_seal(cur, args[0] as u32);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_MUNMAP => {
            unmap_range(args[0] as usize, args[1] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, 0);
            frame
        }
        SYS_MMAP_FILE => {
            let base = mmap_file(cur, args[0], args[1], args[2] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, base as u64);
            frame
        }
        // Reservations exposed to the cell (docs/LIBRHEO.md Phase C, object 7).
        SYS_RESERVE_ADMIT => {
            let r = reserve_admit(cur, args[0], args[1], args[2], args[3], args[4]);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_RESERVE_QUERY => {
            let ppm = cell_admission(cur).committed_ppm();
            arch::set_syscall_ret(unsafe { &mut *frame }, ppm);
            frame
        }
        SYS_RESERVE_RELEASE => {
            let r = reserve_release(cur, args[0] as u32);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        // Block until console input is available (docs/LIBRHEO.md Phase D). The
        // OS's first block-and-wake: the kernel idles here (WFI where the UART
        // RX interrupt is wired, poll otherwise) until a byte arrives.
        SYS_WAIT_INPUT => {
            let n = crate::input::wait_input(args[0], args[1] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, n as u64);
            frame
        }
        // Shell / resource / file syscalls are handled by the system-service
        // module; an unrecognised number faults the cell.
        other => match crate::svc::handle(other, &args) {
            Some(ret) => {
                arch::set_syscall_ret(unsafe { &mut *frame }, ret);
                frame
            }
            None => finish(Outcome::Faulted(0)),
        },
    }
}

/// Number of runnable cells the kernel is tracking (shell `ps`).
pub fn cell_count() -> usize {
    cells().iter().filter(|c| c.present).count()
}

/// Live capability count in the current cell's table (shell `caps`).
pub fn current_caps_live() -> usize {
    let cur = unsafe { *core::ptr::addr_of!(CURRENT) };
    let cell = cells()[cur];
    if cell.caps.is_null() {
        0
    } else {
        // SAFETY: caps was validated at install time.
        unsafe { (*cell.caps).live_count() }
    }
}
