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
    CapInfo, ChannelInfo, GrantInfo, MAX_CELL_CHANNELS, QueueInfo, ReserveInfo, SYS_ARM_TIMER,
    SYS_CAP_DERIVE, SYS_CAP_DROP, SYS_CAP_INFO, SYS_CAP_REVOKE, SYS_COMMIT, SYS_CONNECT,
    SYS_CYCLES, SYS_DECOMMIT, SYS_DOORBELL, SYS_EXIT, SYS_EXIT_GROUP, SYS_GRANT, SYS_GRANT_SHARE,
    SYS_MMAP, SYS_MMAP_FILE, SYS_MUNMAP, SYS_QUEUE_INFO, SYS_RESERVE_ADMIT, SYS_RESERVE_QUERY,
    SYS_RESERVE_RELEASE, SYS_SEAL, SYS_SPAWN, SYS_SWITCH, SYS_WAIT, SYS_WAIT_INPUT, SYS_WAIT_NET,
    SYS_YIELD, ShareInfo,
};
use crate::arch::{self, FaultCause, MapPerm, TrapFrame, TrapKind};
use crate::capability::{
    self, BUDGET_UNLIMITED, CapError, CapTable, DELEGATE, MAP, ObjectKind, ObjectTable, READ, WRITE,
};
use crate::mm::{AddressSpace, frames};
use crate::queue::{self, QueuePair};
use crate::sched::{Admission, AdmitError, Reservation};

/// Base VA of the per-cell anonymous mmap region (docs/USERLAND.md M2): 12 GiB,
/// above the image (1-4 GiB) and stack (8 GiB), free in every cell root.
const MMAP_BASE: usize = 0x3_0000_0000;
static mut MMAP_NEXT: usize = MMAP_BASE;

// Errno values the capability verbs report. Small local constants rather than a
// dependency on `linux::errno`: these are the *native* ABI, and the native
// surface must not be defined by the compatibility personality.
const EPERM: u32 = 1;
const EFAULT: u32 = 14;
const EINVAL: u32 = 22;
const ENOENT: u32 = 2;
const ENOSPC: u32 = 28;

/// Map a capability-layer error onto the errno a cell sees.
///
/// Each one is distinguishable on purpose. A cell that asked to widen a
/// capability and a cell that passed a stale handle have made *different*
/// mistakes, and collapsing them to one code is what turns a debuggable
/// refusal into a mystery (docs/ENGINEERING.md 7).
fn cap_errno(e: CapError) -> u32 {
    match e {
        // A handle that names no live slot, or whose generation does not match.
        CapError::BadHandle => ENOENT,
        // The right was not held - including a widening attempt, which is the
        // monotonic-attenuation invariant refusing (ARCHITECTURE.md 8.2).
        CapError::InsufficientRights | CapError::WidenAttempt | CapError::NotDelegatable => EPERM,
        // The object's epoch moved: someone revoked it.
        CapError::Revoked => EINVAL,
        // A finite budget ran out, or a table/object table is full.
        CapError::Exhausted | CapError::TableFull | CapError::TooManyObjects => ENOSPC,
    }
}

// ======================================================================
// User-pointer validation (docs/ENGINEERING.md 12)
// ======================================================================
//
// Every syscall out-parameter, every queue payload VA and every buffer a
// personality handler is handed is an address the **cell** chose. The kernel
// services the trap in S-mode/EL1/ring 0 *with the calling cell's root active*,
// and every cell root maps all of kernel RAM supervisor-RWX (the linear map),
// so dereferencing such an address unchecked is an arbitrary kernel read or
// write with a cell-steerable value. Nothing here walks page tables: the check
// is a null test, an alignment test, an overflow-checked add and one or two
// compares - a few instructions on the syscall path, no per-call page-table
// walk (the P1 grant check's budget, docs/ARCHITECTURE.md).

/// Exclusive upper bound of a cell's low-half user VA range - the portable
/// "below the kernel half" bound, `2^38` = 256 GiB.
///
/// Derivation: of the three ISAs, RISC-V **Sv39** has the narrowest user half -
/// a 39-bit VA whose low (user) portion is `[0, 2^38)`, everything above being
/// the sign-extended kernel half (docs/MEMORY.md). ARM64's TTBR0 (48-bit) and
/// x86-64's 4-level user half (47-bit) are far larger, so `2^38` is the
/// portable minimum and is *below* the kernel half on all three. Every VA the
/// loader hands a cell lives far below it: image 1-4 GiB, stack 8 GiB
/// ([`crate::load::USER_STACK_TOP`]), anon mmap 12 GiB ([`MMAP_BASE`]), queue
/// 16 GiB ([`crate::load::USER_QUEUE_VA`]), file mmap 20 GiB
/// ([`FILEMMAP_BASE`]), channels 24 GiB ([`crate::load::USER_CHANNEL_VA`]),
/// grants 32 GiB ([`GRANT_BASE`]), and the Linux ELF interpreter 64 GiB
/// ([`crate::load::LINUX_INTERP_BASE`]) - the highest, still 4x below.
pub const USER_VA_MAX: u64 = 1 << 38;

// The layout above is asserted at compile time, so moving a region without
// revisiting this bound cannot compile.
const _: () = assert!((crate::load::LINUX_INTERP_BASE as u64) < USER_VA_MAX);
const _: () = assert!((GRANT_BASE as u64) < USER_VA_MAX);
const _: () = assert!((crate::load::USER_CHANNEL_VA as u64) < USER_VA_MAX);
const _: () = assert!((FILEMMAP_BASE as u64) < USER_VA_MAX);
const _: () = assert!((crate::load::USER_QUEUE_VA as u64) < USER_VA_MAX);
const _: () = assert!((MMAP_BASE as u64) < USER_VA_MAX);

/// The cell-memory accessors live in [`crate::uaccess`] now - the single seam every
/// lazy-mapping feature has to teach, rather than ~98 sites (see that module's header).
/// These aliases keep the existing spelling at call sites that only need bounds +
/// presence; new code should name `uaccess` directly, and anything that *performs* an
/// access should use its `read`/`write`/`copy_in`/`copy_out` tier.
pub use crate::uaccess::{
    buf as user_buf, buf_mut as user_buf_mut, in_ptr as user_in, out_ptr as user_out, prefaults,
    read_span as user_read_span, readable as user_read_ok, slice as user_slice,
    writable as user_write_ok,
};

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
    /// The cell's cross-cell shared-channel ends, reported by `SYS_CONNECT`
    /// (docs/LIBRHEO.md Phase E; the multi-slot table is docs/NETSTACK.md the
    /// service-cell section, rheo-net N4a). Slot 0 is the Phase E/J channel; a
    /// **service cell** holds one slot per client, which is what makes fan-out
    /// possible. Fixed array - the kernel allocates nothing.
    chan: [ChanEnd; MAX_CELL_CHANNELS],
}

/// One cross-cell channel end a cell holds (docs/NETSTACK.md, rheo-net N4a).
#[derive(Copy, Clone)]
struct ChanEnd {
    /// Base VA of the mapped shared ring region. 0 = this slot is empty.
    va: u64,
    /// 32-bit ABI id of the QueuePair capability authorising this end.
    cap_id: u32,
    /// 0 = initiator (client), 1 = acceptor (server).
    role: u64,
}

const EMPTY_CHAN: ChanEnd = ChanEnd {
    va: 0,
    cap_id: 0,
    role: 0,
};

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
    chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
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

// Live typed-memory grants a cell may hold at once (docs/TILES.md 12): a
// fixed per-cell table, not a proof-relevant limit, so sized for real
// workloads - a tile attention program holds ~11 buffers, a warehouse or
// compositor cell more. Raised 16 -> 64 (still small: ~40 B/slot). Whether
// 64 suffices for the largest real cell is an open sizing question flagged
// in docs/TILES.md 12.
const MAX_GRANTS_PER_CELL: usize = 64;
static mut CELL_GRANTS: [[GrantSlot; MAX_GRANTS_PER_CELL]; MAX_CELLS] =
    [[EMPTY_GRANT; MAX_GRANTS_PER_CELL]; MAX_CELLS];

fn cell_grants(cur: usize) -> &'static mut [GrantSlot; MAX_GRANTS_PER_CELL] {
    // SAFETY: single CPU, synchronous traps; no concurrent access.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_GRANTS))[cur] }
}

/// Free the grant slot whose reservation base is `va` (an exact match on a
/// `SYS_GRANT` base), if any - called from `SYS_MUNMAP` so a cell reclaims
/// slots as it drops grants. A `va` that matches no grant (an ordinary anon
/// `SYS_MMAP` region) is a no-op.
fn release_grant_at(cur: usize, va: usize) {
    for slot in cell_grants(cur).iter_mut() {
        if slot.in_use && slot.base == va {
            *slot = EMPTY_GRANT;
            return;
        }
    }
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
    /// The same admission charged to the **system-wide** ledger
    /// (docs/ARCHITECTURE-DEBT.md 2.5), kept so a release frees both. Charging one
    /// and forgetting the other is how a ledger drifts, so they are stored together.
    sys_res: Reservation,
    cap_id: u32,
}

const EMPTY_RES: ResSlot = ResSlot {
    in_use: false,
    res: Reservation::ZERO,
    sys_res: Reservation::ZERO,
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

/// The cell whose trap is being serviced. `crate::uaccess` needs it to know whose
/// address space a supplied pointer belongs to, and whether that cell's mappings are
/// lazy at all (a native cell's are not).
pub fn current_cell() -> usize {
    // SAFETY: single CPU, synchronous traps.
    unsafe { *core::ptr::addr_of!(CURRENT) }
}

/// The syscall ABI cell `idx` speaks - `uaccess` skips its lazy-mapping work entirely
/// for a native cell, whose pages are all committed at load.
pub fn cell_personality(idx: usize) -> Personality {
    cells()[idx].personality
}
static mut EXITED: usize = 0;

/// A kernel-owned capability table per cell slot, for cells the **kernel**
/// creates - a native `SYS_SPAWN` child or a Linux `fork` child
/// (docs/ARCHITECTURE-DEBT.md 2.3).
///
/// Before this, `install_spawned`/`install_forked` copied the parent's `caps`
/// *pointer*, so every descendant shared one table. Three things were wrong with
/// that. `abi.rs`'s claim that spawn authority is "not minted into a spawned
/// child by default" was false - every descendant inherited it. §8.2 property 4
/// (disjoint capability sets) was inapplicable to any parent/child pair, and to
/// a whole service fan-out where four cells shared one table. And what actually
/// isolated memory was the per-cell grant array plus the page tables, so the
/// capability table was not the isolation boundary the design says it is.
///
/// The **top** cell keeps the table its test kernel owns and passes to
/// [`install`]; only kernel-created children come from here. Fixed-size, so the
/// kernel stays allocation-free.
static mut CELL_CAPS: [CapTable; MAX_CELLS] = [const { CapTable::new() }; MAX_CELLS];

/// Cell `idx`'s slot in the kernel-owned table array. Distinct from
/// [`cell_caps`], which reports whatever table is *installed* for the cell -
/// for the top cell that is the one its test kernel owns.
fn owned_caps(idx: usize) -> *mut CapTable {
    // SAFETY: single CPU, synchronous traps; one cell runs at a time.
    unsafe { core::ptr::addr_of_mut!((*core::ptr::addr_of_mut!(CELL_CAPS))[idx]) }
}

/// Create an object and mint a capability for it **into cell `idx`'s own
/// table**, returning the 32-bit ABI id.
///
/// The spawn path needs this because the child's queue-pair and channel
/// capabilities have to land in the child's table, not the parent's - which was
/// automatic while the two were the same table and is the whole point now that
/// they are not.
pub fn mint_into(idx: usize, kind: ObjectKind, rights: u32) -> Option<u32> {
    let cell = cells()[idx];
    // SAFETY: single CPU; `objects` is installed `*const` but owned mutably by
    // whoever created it, and creating an object needs `&mut`. `cell.caps` is
    // this cell's table, uniquely owned for the trap.
    unsafe {
        let objects = &mut *(cell.objects as *mut ObjectTable);
        let caps = &mut *cell.caps;
        let obj = objects.create(kind).ok()?;
        caps.mint(objects, obj, rights, BUDGET_UNLIMITED)
            .ok()
            .map(|h| h.raw_low32())
    }
}

/// A native cell's saved U-mode FP/SIMD state, for a cross-cell switch
/// (`SYS_SWITCH` / the `nproc` scheduler). Sized and aligned for the widest
/// per-ISA save format. Kept **out** of `RunCell` on purpose: `RunCell` is
/// `Copy` and copied on every switch (`let peer = cells()[peer]`), so an inline
/// multi-KiB array would make each switch memcpy the whole image. The kernel is
/// soft-float, so at a switch the live registers still hold the outgoing cell's
/// values (docs/LIBRHEO.md; the Linux personality's per-thread FP in
/// `linux::thread` is the analogous mechanism for L4 threads).
#[repr(C, align(64))]
struct FpArea([u8; arch::FP_AREA_LEN]);

static mut CELL_FP: [FpArea; MAX_CELLS] = [const { FpArea([0; arch::FP_AREA_LEN]) }; MAX_CELLS];

/// Pointer to cell `idx`'s FP save area.
fn cell_fp(idx: usize) -> *mut u8 {
    // SAFETY: single CPU, synchronous traps; `idx < MAX_CELLS`.
    unsafe { (*core::ptr::addr_of_mut!(CELL_FP))[idx].0.as_mut_ptr() }
}

/// Count of native FP/SIMD register-file swaps performed by
/// [`switch_native_cell`] (and of the initial loads done by [`run`]). Bumped
/// *only* inside the swap itself, so a test can assert the swap really ran on
/// every switch rather than infer it from the code (docs/ENGINEERING.md 1).
static mut FP_SWAPS: u64 = 0;

/// How many times a native cell's FP/SIMD register file has been swapped
/// (docs/LIBRHEO.md; the `librheoipc` FP regression phase asserts this is at
/// least the number of cross-cell yields it drove).
pub fn fp_swaps() -> u64 {
    // SAFETY: single CPU, synchronous traps.
    unsafe { *core::ptr::addr_of!(FP_SWAPS) }
}

/// Save native cell `idx`'s live U-mode FP/SIMD state into its area (the kernel
/// is soft-float, so the registers hold `idx`'s values at the switch point).
/// Harmless if `idx` is exiting - the saved image is simply never restored.
pub fn save_native_fp(idx: usize) {
    // SAFETY: `cell_fp(idx)` is a valid, sufficiently-aligned area.
    unsafe { arch::save_user_fp(cell_fp(idx)) };
}

/// Restore native cell `idx`'s U-mode FP/SIMD state before resuming it. For a
/// cell that has never run, the area was set to a clean image by `fp_area_init`
/// at install time, so this loads the ABI-default FP state.
pub fn restore_native_fp(idx: usize) {
    // SAFETY: `cell_fp(idx)` holds a valid image (saved, or `fp_area_init`ed).
    unsafe { arch::restore_user_fp(cell_fp(idx)) };
    // SAFETY: single CPU, synchronous traps.
    unsafe { *core::ptr::addr_of_mut!(FP_SWAPS) += 1 };
}

/// **The** native cross-cell switch: make `to` the current cell, activate its
/// address space, and swap the U-mode FP/SIMD register file with it - save
/// `from`'s live registers into `from`'s area, load `to`'s image.
///
/// Every native path that hands the CPU from one cell to another must go through
/// here: `SYS_SWITCH` (the directed `cur^1` hand-off), the `nproc` scheduler's
/// `reschedule` (`SYS_WAIT` / a child's exit or fault) and its round-robin
/// `SYS_YIELD` (rheo-net N4a: a service cell serving N clients, and the
/// reactor's channel idle path). The FP swap is *inside* this function rather
/// than repeated at each call site on purpose (docs/ENGINEERING.md 3, one owner
/// enforced by construction): a hard-float cell that yields with live values in
/// its vector registers would otherwise silently read back the peer's values -
/// no fault, no log, wrong numbers. That is exactly the defect a textual merge
/// of the FP work with `SYS_YIELD` produced.
///
/// The save areas live in kernel memory, so the swap is independent of which
/// address space is active. Cells are single-context here; the **Linux**
/// personality's cross-cell switch keeps its own per-*context* FP handling
/// (`linux::thread::save_current_fp`/`restore_current` around
/// [`switch_to_cell`]), because a Linux cell holds up to 8 contexts with an FP
/// area each.
pub fn switch_native_cell(from: usize, to: usize) {
    save_native_fp(from);
    switch_to_cell(to);
    restore_native_fp(to);
}
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
        *core::ptr::addr_of_mut!(CELL_FRAMES) = [0; MAX_CELLS];
    }
    crate::sched::reset_system();
    // The ready queue holds funded frames and per-CPU state, so it is released here
    // rather than left holding a previous run's vcores - the `linux::reset` /
    // `thread::release` discipline (docs/SUBSTRATE.md pillar 1). Order matters: the
    // seam's record names vcores, so it is cleared before the queue that holds them.
    crate::sched::dispatch::reset();
    crate::sched::preempt::reset();
    crate::sched::reset_run_queue();
    crate::linux::reset();
    crate::nproc::reset();
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

// ======================================================================
// Per-cell frame budget (docs/ENGINEERING.md 12, docs/ARCHITECTURE.md 5)
// ======================================================================
//
// `len` on `SYS_MMAP`/`SYS_COMMIT` (and the Linux `mmap`/`mprotect` path) is
// **cell-supplied**, so without a ceiling one line of unprivileged cell code -
// `mmap(1 << 40)` - drains the whole frame pool. ARCHITECTURE.md 5 forbids an
// OOM *killer*; an OOM *panic* is strictly worse. Two limits make exhaustion a
// clean `-ENOMEM` refusal instead:
//
//  1. a **global reserve** (`frames::USER_RESERVE_FRAMES`, 16 MiB) that no
//     cell-driven allocation may dip into, so the kernel's own allocations - the
//     page tables a mapping needs, a driver ring, a `fork` copy - always succeed;
//  2. a **per-cell budget** below, so one cell cannot starve its siblings even
//     while the reserve is intact.
//
// Both are checked *before* any frame is taken, and a partial failure rolls back
// what it took, so a refused syscall leaves the pool exactly as it found it.

/// Frames one cell may hold at once through the cell-driven mapping paths:
/// **98304 = 384 MiB** of the 512 MiB pool (`frames::POOL_FRAMES`). Raised in
/// proportion to the pool, for the same reason: a ~100 MB binary plus glibc's
/// per-thread arenas did not fit the previous 96 MiB. It is a fairness cap, not
/// the exhaustion guard - the global reserve above is that - so it stays well
/// under the pool, and a cell that takes its whole budget can no longer `fork`
/// (the child's eager copy would need the same again, and is refused cleanly).
pub const MAX_FRAMES_PER_CELL: usize = 98304;

static mut CELL_FRAMES: [usize; MAX_CELLS] = [0; MAX_CELLS];

/// Frames cell `idx` currently holds through the charged paths. A test asserts
/// on this, so the accounting is observed rather than assumed
/// (docs/ENGINEERING.md 1).
pub fn cell_frames_charged(idx: usize) -> usize {
    // SAFETY: single CPU, synchronous traps.
    unsafe { (*core::ptr::addr_of!(CELL_FRAMES))[idx] }
}

/// Reserve `pages` against cell `cur`'s budget and the global pool, or `false`
/// (nothing charged). Both checks happen before any allocation.
fn charge_frames(cur: usize, pages: usize) -> bool {
    // SAFETY: single CPU, synchronous traps.
    let held = unsafe { &mut (*core::ptr::addr_of_mut!(CELL_FRAMES))[cur] };
    let Some(want) = held.checked_add(pages) else {
        return false;
    };
    if want > MAX_FRAMES_PER_CELL || pages > frames::user_available() {
        return false;
    }
    *held = want;
    true
}

/// Allocate one zeroed frame **charged to the current cell's budget**, or `None`
/// if the budget or the pool (less the kernel reserve) cannot cover it. For a
/// path that maps frames one at a time (the Linux personality's file-backed
/// `mmap`); [`free_user_frame`] uncharges.
pub fn alloc_user_frame() -> Option<usize> {
    let cur = current_index();
    if !charge_frames(cur, 1) {
        return None;
    }
    match frames::alloc() {
        Some(pa) => Some(pa),
        None => {
            uncharge_frames(cur, 1);
            None
        }
    }
}

/// Return a frame taken with [`alloc_user_frame`] and uncharge it.
pub fn free_user_frame(pa: usize) {
    if frames::free_if_pool(pa) {
        uncharge_frames(current_index(), 1);
    }
}

/// Return `pages` to cell `cur`'s budget (a free, or a rolled-back charge).
fn uncharge_frames(cur: usize, pages: usize) {
    // SAFETY: single CPU, synchronous traps.
    let held = unsafe { &mut (*core::ptr::addr_of_mut!(CELL_FRAMES))[cur] };
    *held = held.saturating_sub(pages);
}

/// Map fresh zeroed frames for `[va, va+len)` in the current cell with `perm`.
/// `va` and `len` need not be page-aligned; whole overlapping pages are mapped.
/// Returns `false` (having mapped nothing) when the range is outside the cell's
/// user VA range or the cell's frame budget cannot cover it.
pub fn map_anon_at(va: usize, len: usize, perm: MapPerm) -> bool {
    if len == 0 {
        return true;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if !user_write_ok(base as u64, end - base) {
        return false;
    }
    let cur = current_index();
    let pages = (end - base).div_ceil(frames::FRAME_SIZE);
    if !charge_frames(cur, pages) {
        return false;
    }
    let got = with_current_aspace(|aspace| {
        let mut a = base;
        let mut n = 0;
        while a < end {
            let Some(pa) = frames::alloc() else { break };
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
            n += 1;
        }
        n
    });
    if got < pages {
        // Exhausted mid-way despite the reserve: give back what we took, so a
        // failed call leaves the pool as it found it.
        unmap_range(base, got * frames::FRAME_SIZE);
        uncharge_frames(cur, pages - got);
        return false;
    }
    true
}

/// Unmap every whole page in `[va, va+len)` in the current cell and free the
/// frames, returning how many frames were freed. Pages that were not mapped are
/// skipped, and so is any page whose frame is **not one of the allocator's**
/// (the shared `.user` window, an MMIO aperture) - handing such a page to
/// `frames::free` would panic the kernel.
///
/// The range must lie inside the cell's user VA range; anything else unmaps
/// nothing. That bound is what stops an unprivileged `munmap` of a kernel VA -
/// or, on aarch64 where the `.user` window is linked low, of the shared U-mode
/// code - from reaching the allocator at all. **Ownership** of the range is a
/// separate, stronger check the native `SYS_MUNMAP` applies on top
/// (docs/ENGINEERING.md 12).
pub fn unmap_range(va: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let Some(end) = va.checked_add(len) else {
        return 0;
    };
    if !user_write_ok(base as u64, end - base) {
        return 0;
    }
    let freed = with_current_aspace(|aspace| {
        let mut a = base;
        let mut n = 0;
        while a < end {
            // Route each frame back to the pool it came from: a `Pmem` grant's
            // frames belong to `frames_pmem`, and `frames::free` asserts on a
            // non-pool address, so getting this wrong is a kernel panic from a
            // cell's `SYS_DECOMMIT`.
            if let Some(pa) = aspace.unmap(a) {
                if crate::mm::frames_pmem::contains(pa) {
                    crate::mm::frames_pmem::free(pa);
                    n += 1;
                } else if frames::free_if_pool(pa) {
                    n += 1;
                }
            }
            a += frames::FRAME_SIZE;
        }
        n
    });
    uncharge_frames(current_index(), freed);
    freed
}

/// Commit every whole page in `[va, va+len)` in the current cell with `perm`:
/// pages already mapped are reprotected in place (their frame and contents
/// kept); pages not yet mapped get a fresh zeroed frame. This is the
/// demand-commit path for `mprotect` making a reserved region accessible
/// (docs/LINUX-COMPAT.md L4) - glibc reserves large `PROT_NONE` regions
/// (per-thread malloc arenas, thread-stack guards) and commits sub-ranges as it
/// grows, so eager backing on `mmap` would exhaust the frame pool.
/// Returns `false` when the range is outside the cell's user VA range or the
/// cell's frame budget cannot cover the uncommitted pages. A budget refusal
/// commits nothing; if the pool runs out *mid-range* (only reachable past the
/// kernel reserve) the pages already committed **stay** committed and only the
/// charge is trued up - unlike a fresh `mmap`, a reprotect cannot be rolled back
/// without discarding page contents the cell already had (docs/ENGINEERING.md 12).
pub fn commit_range(va: usize, len: usize, perm: MapPerm) -> bool {
    commit_range_from(va, len, perm, Backing::Ddr)
}

/// Which physical allocator a commit draws its fresh frames from.
///
/// `SYS_GRANT` takes a typed [`abi`-level kind](crate::mm::grant::MemKind) and
/// records it, but every commit used to call `frames::alloc` regardless - so a
/// cell asking for `Pmem` got DDR **with nothing said**, which is exactly what
/// docs/ENGINEERING.md 7 forbids and what docs/MEMORY.md 2.1 claims is real
/// (docs/ARCHITECTURE-DEBT.md 3.6, "object 5 implemented twice"). The kind now
/// reaches the allocator.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Backing {
    /// The DDR frame pool (`mm::frames`) - the default for every path that has
    /// no typed grant behind it.
    Ddr,
    /// The persistent-memory pool (`mm::frames_pmem`), backed by a real nvdimm
    /// region discovered from firmware. Falls back to DDR **with a printed
    /// reason** where no such region exists (arm/riscv `virt` expose no nvdimm).
    Pmem,
}

impl Backing {
    /// The backing an `abi` `MemKind` discriminant selects.
    ///
    /// HBM (1), CXL (2) and Remote (5) are **emulated as DDR** - QEMU models no
    /// such memory and the tree says so (docs/MEMORY.md 2.1). They are reported
    /// once each rather than silently aliased, so a cell's log shows what it
    /// actually got. DeviceBar (4) never reaches here: `grant_create` refuses it.
    fn from_kind(kind: u8) -> Backing {
        match kind {
            3 => Backing::Pmem,
            _ => Backing::Ddr,
        }
    }
}

/// One-shot "you asked for X and got DDR" notices, so the fallback is visible
/// exactly once per kind instead of per page (docs/ENGINEERING.md 7).
static mut BACKING_NOTICE: [bool; 6] = [false; 6];

/// Report, once, that `kind` is not backed by its own memory here and why.
fn note_backing_fallback(kind: u8, reason: &str) {
    // SAFETY: single CPU, synchronous trap.
    let seen = unsafe { &mut *core::ptr::addr_of_mut!(BACKING_NOTICE) };
    let k = (kind as usize).min(5);
    if seen[k] {
        return;
    }
    seen[k] = true;
    let name = match kind {
        1 => "Hbm",
        2 => "Cxl",
        3 => "Pmem",
        5 => "Remote",
        _ => "Ddr",
    };
    crate::println!("mm: grant kind {name} backed by DDR ({reason})");
}

/// [`commit_range`] with an explicit backing store - the path a typed memory
/// grant takes, so `MemKind::Pmem` genuinely lands on the nvdimm pool.
pub fn commit_range_from(va: usize, len: usize, perm: MapPerm, backing: Backing) -> bool {
    if len == 0 {
        return true;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if !user_write_ok(base as u64, end - base) {
        return false;
    }
    // Charge the whole span up front - the pessimistic case, every page fresh -
    // then hand back what turned out to be already committed. A cell cannot
    // commit more than its budget even if it asks page by page.
    let cur = current_index();
    let pages = (end - base).div_ceil(frames::FRAME_SIZE);
    if !charge_frames(cur, pages) {
        return false;
    }
    let (fresh, done) = with_current_aspace(|aspace| {
        let mut a = base;
        let (mut fresh, mut done) = (0usize, 0usize);
        while a < end {
            // Unmap returns the existing frame (if any) so a reprotect keeps
            // the page's contents; otherwise allocate a fresh zeroed frame.
            let pa = match aspace.unmap(a) {
                Some(pa) => pa,
                None => {
                    // A `Pmem` grant draws from the nvdimm pool; if the pool is
                    // absent or exhausted the commit falls back to DDR, and the
                    // frame is still charged to the cell either way (the budget
                    // is about the cell, not about which pool paid).
                    let got = match backing {
                        Backing::Pmem => crate::mm::frames_pmem::alloc().or_else(frames::alloc),
                        Backing::Ddr => frames::alloc(),
                    };
                    match got {
                        Some(pa) => {
                            fresh += 1;
                            pa
                        }
                        None => break,
                    }
                }
            };
            aspace.map_user_frame(a, pa, perm);
            a += frames::FRAME_SIZE;
            done += 1;
        }
        (fresh, done)
    });
    // Only the newly-allocated pages stay charged.
    uncharge_frames(cur, pages - fresh);
    done == pages
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
/// Returns 0 when the request is refused: an empty `len`, a `len` the cell's
/// frame budget or the pool (less the kernel reserve) cannot cover, or a span
/// that would run past the cell's user VA range. Refusing costs no frames
/// (docs/ENGINEERING.md 12) - before this check, `SYS_MMAP(1 << 40)` from an
/// unprivileged cell panicked the kernel with "frame pool exhausted".
fn mmap_anon(cur: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let pages = len.div_ceil(frames::FRAME_SIZE);
    let base = unsafe { *core::ptr::addr_of!(MMAP_NEXT) };
    let Some(top) = pages
        .checked_mul(frames::FRAME_SIZE)
        .and_then(|b| base.checked_add(b))
    else {
        return 0;
    };
    if !user_write_ok(base as u64, top - base) || !charge_frames(cur, pages) {
        return 0;
    }
    let cell = cells()[cur];
    // SAFETY: single CPU; the running cell's address space is uniquely owned
    // for the duration of the run, and adding not-present -> present leaf
    // entries to the active root is safe (re-activated below to publish them).
    let aspace = unsafe { &mut *(cell.aspace as *mut AddressSpace) };
    let mut got = 0;
    for i in 0..pages {
        let Some(pa) = frames::alloc() else { break };
        aspace.map_user_frame(base + i * frames::FRAME_SIZE, pa, MapPerm::UserRw);
        got += 1;
    }
    // Publish the new mappings on the active root (fence / tlbi / cr3 reload).
    aspace.activate();
    if got < pages {
        // Exhausted despite the reserve: roll the whole request back.
        unmap_range(base, got * frames::FRAME_SIZE);
        uncharge_frames(cur, pages - got);
        return 0;
    }
    unsafe {
        *core::ptr::addr_of_mut!(MMAP_NEXT) = top;
    }
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
    // Validate the out-parameter BEFORE minting anything: a refused call must
    // consume no capability and no grant slot (docs/ENGINEERING.md 12).
    let Some(out) = user_out::<GrantInfo>(out_va) else {
        return u64::MAX;
    };
    let bytes = page_up(len);
    let cell = cells()[cur];
    if cell.caps.is_null() || cell.objects.is_null() {
        return u64::MAX;
    }
    // Mint a real MemoryGrant capability (READ|WRITE|MAP|DELEGATE) so commit/
    // decommit/seal are capability-gated and the grant can be *delegated* to a
    // peer once sealed (`SYS_GRANT_SHARE`, docs/LIBRHEO.md Phase E). SAFETY:
    // single CPU, synchronous trap; the cell's tables are uniquely owned for the
    // trap. `objects` is installed as `*const` but the test kernel owns it as a
    // mutable static; creating a new object here needs `&mut`, which this cast
    // recovers.
    let cap_id = unsafe {
        let objects = &mut *(cell.objects as *mut ObjectTable);
        let caps = &mut *cell.caps;
        let Ok(obj) = objects.create(ObjectKind::MemoryGrant) else {
            return u64::MAX;
        };
        // The **creator** gets REVOKE as well (docs/ARCHITECTURE-DEBT.md 2.1):
        // it made the object, so invalidating it for every holder is its call.
        // A derivation does not inherit it unless it asks and the parent has
        // it, which is what stops `SYS_GRANT_SHARE`ing a buffer read-only from
        // also handing over the power to pull it out from under everyone.
        match caps.mint(
            objects,
            obj,
            READ | WRITE | MAP | DELEGATE | capability::REVOKE,
            BUDGET_UNLIMITED,
        ) {
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
    // SAFETY: `out` was checked by `user_out` to be a non-null, `GrantInfo`-
    // aligned address wholly inside this cell's user VA range, and the cell's
    // address space is active for the duration of the trap.
    unsafe {
        out.write(GrantInfo {
            base: base as u64,
            cap_id: cap_id as u64,
        });
    }
    0
}

/// `SYS_MUNMAP`: tear down `[va, va+len)` in the calling cell and return its
/// frames, **only if the cell owns them**. Returns 0, or `u64::MAX` refused
/// (nothing unmapped, nothing freed).
///
/// Three sets of frames reachable in a cell's address space are *not* its own,
/// and freeing any of them is a cross-cell use-after-free plus a reachable
/// "double free" kernel panic (docs/ENGINEERING.md 12):
///
///  - the **shared channel** ring, one region mapped RW into two cells
///    (`nproc::spawn`, `load::map_channel_into`);
///  - a **peer's shared sealed grant**, mapped RO by `SYS_GRANT_SHARE`, whose
///    frames belong to the client that sealed them;
///  - the cell's **own queue-pair region**, which the kernel still holds a raw
///    `QueuePair` overlay onto.
///
/// So authority comes from the same place `SYS_COMMIT`/`DECOMMIT`/`SEAL` get it -
/// [`grant_resolve`], i.e. a live MemoryGrant capability carrying MAP - for a
/// typed grant, and from the cell's own bump regions otherwise. A peer's shared
/// grant *does* have a slot in the peer's table, but its capability was minted
/// READ-only, so `grant_resolve`'s MAP check refuses it: that is the existing
/// pattern doing the work, extended rather than duplicated.
fn sys_munmap(cur: usize, va: usize, len: usize) -> u64 {
    if len == 0 {
        return u64::MAX;
    }
    let base = va & !(frames::FRAME_SIZE - 1);
    let Some(end) = va.checked_add(len) else {
        return u64::MAX;
    };

    // (1) A typed memory grant of this cell, addressed by its reservation VA.
    if base >= GRANT_BASE {
        let slot = cell_grants(cur)
            .iter()
            .copied()
            .find(|s| s.in_use && base >= s.base && end <= s.base + s.len);
        let Some(slot) = slot else {
            return u64::MAX; // no such reservation in this cell
        };
        if grant_resolve(cur, slot.cap_id).is_none() {
            return u64::MAX; // revoked, exhausted, or (a peer's grant) no MAP
        }
        unmap_range(base, end - base);
        // Releasing the whole reservation frees its slot, so a cell that churns
        // typed grants does not leak the fixed per-cell slot table.
        if base == slot.base {
            release_grant_at(cur, base);
        }
        return 0;
    }

    // (2) The cell's own anonymous-mmap region, and (3) its file-mmap region -
    // both bump allocators whose frames this cell alone holds. Everything else,
    // notably the queue-pair region (16 GiB) and the shared channel slots
    // (24 GiB), is refused.
    let anon_top = unsafe { *core::ptr::addr_of!(MMAP_NEXT) };
    let file_top = cells()[cur].filemmap_next;
    let owned = (base >= MMAP_BASE && end <= anon_top)
        || (base >= FILEMMAP_BASE && end <= file_top && FILEMMAP_BASE < file_top);
    if !owned {
        return u64::MAX;
    }
    unmap_range(base, end - base);
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
    // The grant's typed kind decides the physical pool. Before this, every
    // commit went to DDR and a `Pmem` grant was a silent lie
    // (docs/ARCHITECTURE-DEBT.md 3.6).
    let kind = cell_grants(cur)
        .iter()
        .find(|s| s.in_use && s.cap_id == cap_id)
        .map(|s| s.kind)
        .unwrap_or(0);
    let backing = Backing::from_kind(kind);
    if backing == Backing::Pmem && crate::mm::frames_pmem::region().is_none() {
        note_backing_fallback(kind, "no nvdimm region on this machine");
    } else if kind == 1 || kind == 2 || kind == 5 {
        note_backing_fallback(kind, "emulated - QEMU models no such memory");
    }
    // Mirrors the pre-existing behaviour: the commit result is not reported to
    // the cell by this verb (a partial commit leaves the pages it did map). The
    // return value is consumed to keep that explicit rather than accidental.
    let _ = commit_range_from(base + offset, len, MapPerm::UserRw, backing);
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

/// `SYS_GRANT_SHARE`: delegate a **sealed** memory grant to the peer cell
/// (`cur ^ 1`) - zero-copy cross-cell buffer passing, the dmabuf equivalent
/// (docs/LIBRHEO.md Phase E, docs/ARCHITECTURE.md 3 objects 2/5). The client's
/// grant capability must carry DELEGATE (object 2 delegate/revoke) and the grant
/// must be sealed (object 5 seal -> immutable = shareable). The kernel maps the
/// grant's frames into the peer **read-only** at the peer's next grant VA, mints
/// a MemoryGrant capability there referencing the **same** kernel object (so an
/// epoch revoke on the client's object kills the peer's copy too - revocable),
/// records a peer grant slot (its frames owned by the client, so never freed
/// twice), and writes `ShareInfo { peer_va, peer_cap_id }` at `out_va`. Returns
/// 0 or `u64::MAX`.
fn grant_share(cur: usize, cap_id: u32, out_va: u64) -> u64 {
    let peer = cur ^ 1;
    // Validate the out-parameter before mapping anything into the peer: a
    // refused call must leave both cells untouched (docs/ENGINEERING.md 12).
    let Some(out) = user_out::<ShareInfo>(out_va) else {
        return u64::MAX;
    };
    let cell = cells()[cur];
    let peer_cell = cells()[peer];
    if cell.caps.is_null()
        || cell.objects.is_null()
        || !peer_cell.present
        || peer_cell.caps.is_null()
        || peer_cell.aspace.is_null()
    {
        return u64::MAX;
    }
    // Grant-check the client's capability for the DELEGATE right, recovering the
    // kernel object id (revocation / budget enforced here). SAFETY: single CPU,
    // synchronous trap; tables uniquely owned.
    let obj = unsafe {
        let objects = &*cell.objects;
        let caps = &mut *cell.caps;
        match caps.grant_check_low32(objects, cap_id, DELEGATE) {
            Ok(o) => o,
            Err(_) => return u64::MAX,
        }
    };
    // Only a *sealed* (immutable) grant is shareable - the object-5 doctrine.
    let Some(slot) = cell_grants(cur)
        .iter()
        .find(|s| s.in_use && s.cap_id == cap_id)
        .copied()
    else {
        return u64::MAX;
    };
    if !slot.sealed {
        return u64::MAX;
    }
    // Map the grant's frames into the peer read-only at its next grant VA.
    let peer_base = peer_cell.grant_next;
    // SAFETY: single CPU; the client's address space is read (page-table walk,
    // no active requirement) and the peer's is edited (published when the peer is
    // switched to). Both are uniquely owned for the trap.
    let nframes = unsafe {
        let client_aspace = &*cell.aspace;
        let peer_aspace = &mut *(peer_cell.aspace as *mut AddressSpace);
        client_aspace.share_ro_into(peer_aspace, slot.base, slot.len, peer_base)
    };
    if nframes == 0 {
        return u64::MAX;
    }
    cells()[peer].grant_next = peer_base + slot.len;
    // Mint a READ capability in the peer referencing the SAME object (so revoke
    // by epoch kills it too). SAFETY: as above.
    let peer_cap = unsafe {
        let objects = &*cell.objects;
        let caps = &mut *peer_cell.caps;
        match caps.mint(objects, obj, READ, BUDGET_UNLIMITED) {
            Ok(h) => h.raw_low32(),
            Err(_) => return u64::MAX,
        }
    };
    // Record a peer grant slot: sealed (read-only) and its frames owned by the
    // client, so the peer never decommits/frees them (no double free).
    if let Some(pslot) = cell_grants(peer).iter_mut().find(|s| !s.in_use) {
        *pslot = GrantSlot {
            in_use: true,
            base: peer_base,
            len: slot.len,
            kind: slot.kind,
            sealed: true,
            cap_id: peer_cap,
        };
    }
    // SAFETY: `out` was checked by `user_out` (non-null, aligned, inside the
    // running client cell's user VA range); its address space is active.
    unsafe {
        out.write(ShareInfo {
            peer_va: peer_base as u64,
            peer_cap_id: peer_cap as u64,
        });
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
    let pages = bytes / frames::FRAME_SIZE;
    // `len` is cell-supplied here too: bound the span and charge the frames
    // before mapping anything (docs/ENGINEERING.md 12).
    let Some(top) = base.checked_add(bytes) else {
        return 0;
    };
    if !user_write_ok(base as u64, bytes) || !charge_frames(cur, pages) {
        return 0;
    }
    cells()[cur].filemmap_next = top;
    let got = with_current_aspace(|aspace| {
        let mut got = 0;
        for i in 0..pages {
            let va = base + i * frames::FRAME_SIZE;
            let Some(pa) = frames::alloc() else { break };
            got += 1;
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
        got
    });
    if got < pages {
        unmap_range(base, got * frames::FRAME_SIZE);
        uncharge_frames(cur, pages - got);
        return 0;
    }
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
    // Validate the out-parameter before admitting anything: a refused call must
    // not move the admission total (docs/ENGINEERING.md 12). A bad out-pointer
    // is reported as BadParams (1) - it is exactly that.
    let Some(out) = user_out::<ReserveInfo>(out_va) else {
        return 1;
    };
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
    // Charge the **machine** first, then the cell (docs/ARCHITECTURE-DEBT.md 2.5).
    // Both must accept: the per-cell controller is what makes a cell's own set
    // schedulable, the system ledger is what stops N cells each admitting 90% of one
    // CPU - which used to all succeed, because nothing above the per-cell controller
    // existed. On either refusal nothing is left charged.
    let sys_res = match crate::sched::system().admit(budget, period, deadline) {
        Ok(r) => r,
        Err(AdmitError::BadParams) => return 1,
        Err(AdmitError::Overcommit) => return 2,
    };
    let res = match cell_admission(cur).admit(budget, period, deadline) {
        Ok(r) => r,
        Err(AdmitError::BadParams) => {
            crate::sched::system().release(&sys_res);
            return 1;
        }
        Err(AdmitError::Overcommit) => {
            crate::sched::system().release(&sys_res);
            return 2;
        }
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
            crate::sched::system().release(&sys_res);
            return 1;
        };
        match caps.mint(objects, obj, READ, BUDGET_UNLIMITED) {
            Ok(h) => h.raw_low32(),
            Err(_) => {
                cell_admission(cur).release(&res);
                crate::sched::system().release(&sys_res);
                return 1;
            }
        }
    };
    let slot = cell_res(cur).iter_mut().find(|s| !s.in_use).unwrap();
    *slot = ResSlot {
        in_use: true,
        res,
        sys_res,
        cap_id,
    };
    let committed = cell_admission(cur).committed_ppm();
    // SAFETY: `out` was checked by `user_out` (non-null, aligned, inside the
    // running cell's user VA range); its address space is active.
    unsafe {
        out.write(ReserveInfo {
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
    let sys_res = slot.sys_res;
    slot.in_use = false;
    cell_admission(cur).release(&res);
    crate::sched::system().release(&sys_res);
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
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
    };
    *cell_grants(idx) = [EMPTY_GRANT; MAX_GRANTS_PER_CELL];
    // SAFETY: single CPU; a fresh cell starts with no frames charged.
    unsafe { (*core::ptr::addr_of_mut!(CELL_FRAMES))[idx] = 0 };
    // Give the cell a fair-class vcore on this CPU's ready queue, so the scheduler
    // has something to order it by (docs/SUBSTRATE.md pillar 3). A top-level cell
    // starts with a fresh burst score; a child inherits its parent's (see
    // `install_spawned`/`install_forked`).
    crate::sched::dispatch::track(idx, 0, None);
    // Clean FP state, so the first cross-cell switch into this cell restores an
    // ABI-default FPU rather than a zeroed area (docs/LIBRHEO.md).
    // SAFETY: `cell_fp(idx)` is a valid, aligned `FP_AREA_LEN` area.
    unsafe { arch::fp_area_init(cell_fp(idx)) };
}

/// Record the mapped queue-pair region VA and capability id for cell `idx`
/// (docs/LIBRHEO.md). `SYS_QUEUE_INFO` reports these so a loaded librheo cell
/// can bind its ring. Call after `install`, before `run`.
pub fn set_queue_info(idx: usize, qp_va: u64, cap_id: u32) {
    assert!(cells()[idx].present, "set_queue_info on empty slot {idx}");
    cells()[idx].qp_va = qp_va;
    cells()[idx].qp_cap_id = cap_id;
}

/// Record the mapped cross-cell shared channel for cell `idx` (docs/LIBRHEO.md
/// Phase E). `SYS_CONNECT` reports `(chan_va, cap_id, role)` so a librheo cell
/// binds its channel end. Call after `install`, before `run`; the two peers of a
/// connection get the *same* frames (see `load::map_channel_into`) at the same
/// VA but opposite roles (0 = client, 1 = server).
pub fn set_channel_info(idx: usize, chan_va: u64, cap_id: u32, role: u64) {
    set_channel_slot(idx, 0, chan_va, cap_id, role);
}

/// Record channel end `slot` for cell `idx` (docs/NETSTACK.md the service-cell
/// section, rheo-net N4a). Slot 0 is [`set_channel_info`]'s Phase E/J channel;
/// slots 1.. give a **service cell** one end per client, each a distinct shared
/// ring region (see `load::map_channel_into_slot`), which is what makes concurrent
/// fan-out possible. Call after `install`, before `run`.
pub fn set_channel_slot(idx: usize, slot: usize, chan_va: u64, cap_id: u32, role: u64) {
    assert!(cells()[idx].present, "set_channel_slot on empty slot {idx}");
    assert!(slot < MAX_CELL_CHANNELS, "channel slot {slot} out of range");
    cells()[idx].chan[slot] = ChanEnd {
        va: chan_va,
        cap_id,
        role,
    };
}

/// This cell's channel end `slot` as `(chan_va, role)` (`(0, 0)` = none)
/// (docs/LIBRHEO.md Phase J, docs/NETSTACK.md rheo-net N4a). `nproc::spawn` reads
/// it to propagate one of the parent's channels to a spawned child (the child
/// gets the opposite role at its own slot 0).
pub fn cell_chan_slot(idx: usize, slot: usize) -> (u64, u64) {
    if slot >= MAX_CELL_CHANNELS {
        return (0, 0);
    }
    let c = cells()[idx].chan[slot];
    (c.va, c.role)
}

/// How many channel ends cell `idx` holds (contiguous from slot 0).
pub fn cell_chan_count(idx: usize) -> usize {
    cells()[idx].chan.iter().take_while(|c| c.va != 0).count()
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
        // Load this cell's own FP/SIMD image before its first instruction: the
        // clean ABI-default one `fp_area_init` wrote at install for a fresh
        // cell, or its saved image if a previous `run` left it mid-flight. A
        // test kernel that runs several cells in sequence would otherwise hand
        // the next one whatever the last left in the vector registers.
        restore_native_fp(idx);
        // The first entry into a cell does not go through either scheduler, so it is
        // the one place a slice has to be armed explicitly (docs/SUBSTRATE.md pillar
        // 3). Without this a boot's *first* cell would be the only one that could
        // never be preempted - the exact shape of bug that makes a preemptive
        // scheduler look like it works while one workload hangs.
        crate::sched::dispatch::running(idx, 0);
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

/// End the whole run because the scheduler is **deadlocked**: nothing runnable and
/// no blocked cell has a wake source left (docs/ARCHITECTURE-DEBT.md 2.4). The
/// scheduler has already printed which cell is blocked on what; this records the
/// outcome as `Exited(DEADLOCK_EXIT)` and returns the null frame the arch
/// trampoline reads as "unwind", so the state is reportable instead of a `panic!`
/// with a kernel stack trace.
pub fn deadlock_finish() -> *mut TrapFrame {
    finish(Outcome::Exited(crate::abi::DEADLOCK_EXIT))
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

/// Whether cell `idx` speaks the native ABI (docs/NETSTACK.md rheo-net N4a).
/// `nproc::yield_cell` only ever hands the CPU to a native sibling; a Linux cell
/// is scheduled by `linux::proc`.
pub fn cell_is_native(idx: usize) -> bool {
    cells()[idx].personality == Personality::Native
}

/// The address-space pointer installed for cell `idx` (the Linux process
/// scheduler's `fork`/`execve` build on it).
pub fn cell_aspace(idx: usize) -> *const AddressSpace {
    cells()[idx].aspace
}

/// [`cell_aspace`] for the operations that **change** a cell's page tables rather
/// than read them - COW `fork` write-protecting the parent, and teardown freeing its
/// frames. The installed pointer names either a caller-owned space (a test kernel's)
/// or `linux::proc`'s `ASPACE` slot; both are mutable storage, and the `*const` in
/// the cell table records where it is, not that it is frozen.
///
/// # Safety
/// The caller must not hold another reference to the same address space while it
/// mutates through this one, which on a single CPU inside one trap means: do not call
/// it twice and keep both.
pub fn cell_aspace_mut(idx: usize) -> *mut AddressSpace {
    cells()[idx].aspace as *mut AddressSpace
}

/// Repoint cell `idx` at a new address space (after `execve` replaces the image,
/// docs/LINUX-COMPAT.md L6).
pub fn set_cell_aspace(idx: usize, aspace: *const AddressSpace) {
    cells()[idx].aspace = aspace;
}

/// The capability table installed for cell `idx`. The top cell's is owned by
/// whoever created it; a spawned or forked child's is the kernel-owned
/// per-cell table (docs/ARCHITECTURE-DEBT.md 2.3).
pub fn cell_caps(idx: usize) -> *mut CapTable {
    cells()[idx].caps
}

/// The object table pointer installed for cell `idx` (paired with `cell_caps`).
pub fn cell_objects(idx: usize) -> *const ObjectTable {
    cells()[idx].objects
}

/// Install a **spawned** native child in slot `idx` (docs/LIBRHEO.md Phase F).
/// Like `install_forked`, the child shares the parent's capability bundle
/// (`caps`/`objects`), but it gets its **own** address space, queue pair, and
/// frame, and stays `Personality::Native` so it runs librheo. The queue info
/// (`qp_va`, `qp_cap_id`) is recorded so `SYS_QUEUE_INFO` binds the child's ring.
///
/// # Safety
/// `aspace`/`qp`/`frame` must outlive the child's run; `parent` must be present.
#[allow(clippy::too_many_arguments)]
pub unsafe fn install_spawned(
    idx: usize,
    aspace: *const AddressSpace,
    qp: *const QueuePair,
    frame: *mut TrapFrame,
    parent: usize,
    qp_va: u64,
    qp_cap_id: u32,
) {
    let p = cells()[parent];
    // The child gets its **own**, empty capability table - not the parent's
    // (docs/ARCHITECTURE-DEBT.md 2.3). This is what makes `abi.rs`'s claim that
    // spawn authority is "not minted into a spawned child by default" true: the
    // parent's `ObjectKind::Cell` capability is simply not in here, so the child
    // cannot spawn. Whatever the child legitimately needs - its queue pair, an
    // inherited channel - is minted into this table explicitly by the spawn
    // path, which is the point: it is a list, not an inheritance.
    // SAFETY: single CPU; slot `idx` is free, so nothing else holds this table.
    unsafe { (*owned_caps(idx)).clear() };
    cells()[idx] = RunCell {
        aspace,
        caps: owned_caps(idx),
        // The **object** table is one per system (ARCHITECTURE.md 3) and is
        // shared on purpose: it is the registry objects live in, not an
        // authority. Reaching an object still needs a capability in this cell's
        // own table.
        objects: p.objects,
        qp,
        frame,
        outcome: None,
        present: true,
        personality: Personality::Native,
        qp_va,
        qp_cap_id,
        grant_next: GRANT_BASE,
        filemmap_next: FILEMMAP_BASE,
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
    };
    *cell_grants(idx) = [EMPTY_GRANT; MAX_GRANTS_PER_CELL];
    // SAFETY: single CPU; a fresh cell starts with no frames charged.
    unsafe { (*core::ptr::addr_of_mut!(CELL_FRAMES))[idx] = 0 };
    // The child inherits the parent's burst state rather than arriving with a fresh
    // interactive weight it did not earn (docs/SUBSTRATE.md pillar 3, BORE's fork
    // inheritance): a burst of short-lived children would otherwise each be treated
    // as maximally interactive.
    crate::sched::dispatch::track(idx, 0, Some(parent));
    // Clean FP state for the spawned child's first entry (see `install`).
    // SAFETY: `cell_fp(idx)` is a valid, aligned `FP_AREA_LEN` area.
    unsafe { arch::fp_area_init(cell_fp(idx)) };
}

/// Repoint cell `idx` at a new context-0 frame (after `execve`).
pub fn set_cell_frame(idx: usize, frame: *mut TrapFrame) {
    cells()[idx].frame = frame;
}

/// Make cell `idx` the current cell and activate its address space - the
/// address-space half of a cross-cell switch, and nothing else.
///
/// This is the **Linux** personality's cross-cell switch (docs/LINUX-COMPAT.md
/// L6), driven from `crate::linux::proc`, which brackets it with its own
/// per-context FP/TLS swap (`linux::thread::save_current_fp` /
/// `restore_current`). A **native** caller must use [`switch_native_cell`]
/// instead, which also swaps the FP/SIMD register file; calling this directly
/// from a native path leaks one cell's vector registers into another.
pub fn switch_to_cell(idx: usize) {
    unsafe {
        *core::ptr::addr_of_mut!(CURRENT) = idx;
        (*cells()[idx].aspace).activate();
    }
}

/// Install a **forked** child in slot `idx` with a **copy** of the parent's
/// capability table (POSIX fork = clone-cell-within-capability-bundle,
/// docs/POSIX-PERSONALITY.md 2, docs/ARCHITECTURE-DEBT.md 2.3). The address
/// space and frame are kernel-owned by `crate::linux::proc`; personality is
/// Linux.
///
/// A copy rather than the shared pointer this used to install, for the same
/// reason POSIX copies the descriptor table: the child holds what the parent
/// held *at the fork*, and neither can change the other's holdings afterwards.
/// Epoch revocation still reaches both, because that lives on the object.
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
    // SAFETY: single CPU; slot `idx` is free and `parent` is present, so the two
    // tables are distinct and uniquely owned for the trap.
    unsafe { (*owned_caps(idx)).copy_from(&*p.caps) };
    cells()[idx] = RunCell {
        aspace,
        caps: owned_caps(idx),
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
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
    };
    // As `install_spawned`: a forked child inherits its parent's burst score
    // (docs/SUBSTRATE.md pillar 3).
    crate::sched::dispatch::track(idx, 0, Some(parent));
}

/// Free cell slot `idx` (a reaped zombie). The slot becomes reusable by a
/// future `fork`. Its frame charge is cleared: the reaper has already returned
/// the frames (`AddressSpace::free_user_frames`), which bypasses the per-cell
/// accounting because the cell is gone.
pub fn free_cell(idx: usize) {
    // The single choke point where a cell slot is handed back, so it is where the
    // scheduler stops knowing about it. A run-queue entry naming a cell that no
    // longer exists would be asked about that cell forever and never be runnable -
    // a permanently blocked vcore holding a slot (docs/SUBSTRATE.md pillar 3).
    crate::sched::dispatch::untrack(idx);
    cells()[idx] = EMPTY;
    // SAFETY: single CPU, synchronous traps.
    unsafe {
        (*core::ptr::addr_of_mut!(CELL_FRAMES))[idx] = 0;
        // Empty the slot's capability table too, so the next cell to land here
        // cannot inherit a dead one's capabilities (docs/ARCHITECTURE-DEBT.md
        // 2.3). Both install paths already overwrite it, but leaving a reaped
        // cell's authority lying in a static until something happens to
        // overwrite it is the kind of thing that becomes true by accident.
        (*owned_caps(idx)).clear();
    }
}

/// A device or timer **interrupt** was taken while a cell was running in user mode.
/// Returns the frame to resume - the interrupted one, unless a preemption is due.
///
/// This is the portable half of timer preemption (docs/SUBSTRATE.md pillar 3,
/// `crate::sched::preempt`). Each ISA's user-trap entry already services the
/// interrupt and resumes the cell at exactly the instruction it was interrupted at
/// (the path rheo-net N2d added so a NIC frame arriving mid-cell is not read as a
/// fault); it now asks here first, so the *decision* to switch lives in one portable
/// place while the three arch entries keep only "service the device".
///
/// Called in ordinary kernel context on the way out of the trap, never from inside
/// the interrupt handler - the handler does one store ([`crate::sched::preempt::note`])
/// and returns. That split is what keeps the scheduler non-reentrant: an interrupt
/// can land while the kernel holds a reference into a funded table, and invoking a
/// scheduler from there would be a use of that table from two places at once.
///
/// Preemption prefers a **sibling context of the same cell** over another cell,
/// because that is both the cheaper switch (one address space, one register file)
/// and the case the evidence demanded: Bun's main thread waits for a worker context
/// of the same cell to make progress (docs/LINUX-COMPAT.md, GOAL-BUN).
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn on_user_interrupt(frame: *mut TrapFrame) -> *mut TrapFrame {
    if !crate::sched::preempt::take() {
        return frame;
    }
    let cur = current_index();
    if !cell_present(cur) {
        return frame;
    }
    // **Save the interrupted vector-register file before any other kernel work.**
    //
    // The kernel is soft-float, which is what makes the ordinary syscall path safe
    // to leave until the switch site - but "soft-float" bounds the *floating-point*
    // it emits, not the vector registers `compiler_builtins`' `mem*` routines and
    // ordinary struct moves use on x86-64. So anything running between the interrupt
    // and the save can clobber exactly what is about to be saved, and a preemption
    // arrives at an arbitrary instruction in the middle of the cell's own vector
    // code. The symptom is not a fault at the switch: it is the *resumed* context
    // computing with someone else's registers, which showed up as Bun dying with
    // `Illegal instruction` at a nonsense address (docs/LIBRHEO.md, the `SYS_YIELD`
    // FP scar - a fourth path into the same invariant).
    //
    // So the switch functions below do the save as their own first action, and the
    // charging - which reads a clock, walks a funded table and records a histogram -
    // is deliberately moved to *after* the switch. Nothing about the charge depends
    // on running first: it names the outgoing vcore through the scheduler's own
    // per-CPU record, not through `current_index()`.
    let resumed = match cells()[cur].personality {
        // A Linux cell: try its own ready contexts first (this is the fix for "a
        // spinning thread starves its siblings"), then other cells.
        Personality::Linux => match crate::linux::thread::preempt_context(cur) {
            Some(f) => {
                crate::sched::preempt::took(true);
                f
            }
            None => match crate::linux::proc::preempt_cell(cur) {
                Some(f) => {
                    crate::sched::preempt::took(false);
                    f
                }
                None => frame,
            },
        },
        // A native cell has one context, so the only move is to another cell.
        Personality::Native => match crate::nproc::preempt_cell(cur) {
            Some(f) => {
                crate::sched::preempt::took(false);
                f
            }
            None => frame,
        },
    };
    // Charge the interrupted vcore for the slice it just ran and record the stop as
    // **involuntary** - the distinction the burst score depends on, and the reason a
    // compute-bound cell does not earn interactive weight by being preempted. After
    // the switch, per the FP note above.
    crate::sched::dispatch::preempted();
    // Whoever runs next gets a fresh slice. Re-arming here rather than at the switch
    // sites keeps "every entry into a cell is under a slice" true of the preemption
    // path as well as the syscall path.
    let slice = crate::sched::dispatch::running(current_index(), 0);
    crate::sched::preempt::arm(slice);
    resumed
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
            // **Demand paging first** (docs/ARCHITECTURE-DEBT.md 4.0, blocker 2).
            // A fault on a page that is mapped-but-not-yet-populated is not an
            // error at all: fill it and re-execute the instruction. The frame is
            // returned unchanged, which *is* the retry - the faulting PC was never
            // advanced.
            //
            // It has to come before signal delivery, and only a *memory* fault can
            // be a missing page: an illegal instruction or an FP exception is never
            // one, and asking the VMA list about them would be answering a question
            // it was not asked.
            if cause == FaultCause::Segv && crate::linux::fill_fault(cur, fault_addr) {
                return frame;
            }
            // A fault that demand paging could not satisfy is about to become a signal
            // or kill the process, so this is the last point at which the *cause* is
            // still visible. Report it with the scheduler state alongside, because the
            // two questions a reader has are "where did it fault" and "was it
            // preempted" - and correlating those from separate logs is exactly what
            // made the intermittent Node segfault under preemption unattributable
            // (docs/LINUX-COMPAT.md). One line, only on the path that is already
            // failing, so it costs nothing in the common case.
            let (_, taken, _, to_sib, to_cell) = crate::sched::preempt::counters();
            crate::println!(
                "linux: unhandled {:?} fault in cell {cur} ctx {} at {fault_addr:#x}                  (preemptions taken {taken}: {to_sib} sibling, {to_cell} cell)",
                cause,
                crate::linux::thread::current_context(cur)
            );
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
        // A native cell fault stays terminal (no signals). But a *spawned*
        // native child (docs/LIBRHEO.md Phase F) does not end the whole run: it
        // becomes a zombie its parent reaps with `FAULT_EXIT`, and the CPU is
        // handed to the next runnable cell - mirroring the Linux child path.
        if let Some(f) = crate::nproc::on_fault(cur) {
            return f;
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
            let ret = match (cell.qp_va, user_out::<QueueInfo>(arg)) {
                // SAFETY: `out` was checked by `user_out` (non-null, aligned,
                // inside the running cell's user VA range) and the cell's
                // address space is active for the trap.
                (qp_va, Some(out)) if qp_va != 0 => {
                    unsafe {
                        out.write(QueueInfo {
                            qp_va,
                            cap_id: cell.qp_cap_id as u64,
                        });
                    }
                    0
                }
                // No mapped queue, or a rejected out-parameter: refuse without
                // writing (docs/ENGINEERING.md 12).
                _ => u64::MAX,
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret);
            frame
        }
        // ---- the capability verbs (docs/ARCHITECTURE-DEBT.md 2.1) ----------
        //
        // `ARCHITECTURE.md` 3 has named mint/delegate/revoke since the first
        // draft, but none was reachable from a cell: `derive_subset`,
        // `delegate` and `revoke_epoch` had zero production callers, so object
        // 2's claim to be "the security model, the audit log, and the metering
        // system" rested on a primitive nothing but a test had ever called.
        // These four make it reachable. They add no object and no verb the
        // design had not already admitted.
        SYS_CAP_DERIVE => {
            let cell = cells()[cur];
            let (parent, rights, budget) = (arg as u32, args[1] as u32, args[2]);
            // SAFETY: the cell's tables were validated at install time and its
            // address space is active for the trap.
            let caps = unsafe { &mut *cell.caps };
            let objects = unsafe { &*cell.objects };
            let ret = match user_out::<u32>(args[3]) {
                None => -(EFAULT as i64),
                // A right this build does not define cannot be granted -
                // otherwise a bit that means nothing today would be derivable
                // today and mean something else tomorrow.
                Some(_) if rights & !capability::ALL != 0 => -(EINVAL as i64),
                Some(out) => match caps.derive_subset_low32(objects, parent, rights, budget) {
                    // SAFETY: `out` was checked by `user_out` (non-null,
                    // aligned, inside this cell's user VA range).
                    Ok(child) => {
                        unsafe { out.write(child) };
                        0
                    }
                    Err(e) => -(cap_errno(e) as i64),
                },
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret as u64);
            frame
        }
        SYS_CAP_REVOKE => {
            let cell = cells()[cur];
            // SAFETY: tables validated at install time; address space active.
            // `objects` is installed as `*const`; the owning test kernel holds
            // it as a mutable static, and bumping an epoch needs `&mut`.
            let caps = unsafe { &*cell.caps };
            let objects = unsafe { &mut *(cell.objects as *mut ObjectTable) };
            // REVOKE is its own right, not something holding the capability
            // implies: delegating read access to a buffer must not also hand
            // over the power to pull it out from under every other holder.
            // `inspect_low32` rather than a grant check, so the rights test
            // does not spend a metered capability's budget.
            let ret = match caps.inspect_low32(objects, arg as u32) {
                Ok((object, rights, _)) if rights & capability::REVOKE != 0 => {
                    objects.revoke_epoch(object);
                    0
                }
                Ok(_) => -(EPERM as i64),
                Err(e) => -(cap_errno(e) as i64),
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret as u64);
            frame
        }
        SYS_CAP_INFO => {
            let cell = cells()[cur];
            // SAFETY: tables validated at install time; address space active.
            let caps = unsafe { &*cell.caps };
            let objects = unsafe { &*cell.objects };
            let ret = match (
                user_out::<CapInfo>(args[1]),
                caps.inspect_low32(objects, arg as u32),
            ) {
                (None, _) => -(EFAULT as i64),
                (Some(out), Ok((object, rights, budget))) => {
                    // SAFETY: `out` was checked by `user_out`.
                    unsafe {
                        out.write(CapInfo {
                            object: object.0,
                            kind: objects.kind(object).abi_code(),
                            rights,
                            _pad: 0,
                            budget,
                        });
                    }
                    0
                }
                (Some(_), Err(e)) => -(cap_errno(e) as i64),
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret as u64);
            frame
        }
        SYS_CAP_DROP => {
            let cell = cells()[cur];
            // SAFETY: tables validated at install time; address space active.
            let caps = unsafe { &mut *cell.caps };
            let objects = unsafe { &*cell.objects };
            // Reports whether anything was actually released, so a double drop
            // is visible instead of a silent success (docs/ENGINEERING.md 7).
            let ret = match caps.free_low32(objects, arg as u32) {
                Ok(()) => 0,
                Err(e) => -(cap_errno(e) as i64),
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret as u64);
            frame
        }

        // Cross-cell connect: report this cell's shared-channel end for the
        // requested slot (docs/LIBRHEO.md Phase E; multi-slot fan-out is
        // docs/NETSTACK.md rheo-net N4a). Each end is one ring region shared with
        // one peer; the kernel never drains it - the two cells drive the SPSC
        // rings directly over the frames.
        SYS_CONNECT => {
            let cell = cells()[cur];
            let slot = args[1] as usize;
            let end = if slot < MAX_CELL_CHANNELS {
                cell.chan[slot]
            } else {
                EMPTY_CHAN
            };
            let ret = match user_out::<ChannelInfo>(arg) {
                // SAFETY: `out` was checked by `user_out`; the cell's address
                // space is active for the trap.
                Some(out) if end.va != 0 => {
                    unsafe {
                        out.write(ChannelInfo {
                            chan_va: end.va,
                            cap_id: end.cap_id as u64,
                            role: end.role,
                            count: cell_chan_count(cur) as u64,
                        });
                    }
                    0
                }
                // No such channel end, or a rejected out-parameter.
                _ => u64::MAX,
            };
            arch::set_syscall_ret(unsafe { &mut *frame }, ret);
            frame
        }
        // Delegate a sealed grant to the peer cell (zero-copy buffer passing).
        SYS_GRANT_SHARE => {
            let r = grant_share(cur, args[0] as u32, args[1]);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
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
            // The one native cross-cell switch: address space **and** FP/SIMD
            // register file (docs/LIBRHEO.md).
            switch_native_cell(cur, peer);
            peer_cell.frame
        }
        // A spawned native child's exit makes it a zombie and reschedules
        // (docs/LIBRHEO.md Phase F); the top cell's exit unwinds `run`.
        SYS_EXIT | SYS_EXIT_GROUP => match crate::nproc::on_exit(cur, arg) {
            Some(f) => f,
            None => finish(Outcome::Exited(arg)),
        },
        // Native process model (docs/LIBRHEO.md Phase F): create a cell / reap a
        // child, gated by the cell-spawn capability. A cross-cell scheduler in
        // `crate::nproc` generalizes the Linux L6 run loop for native cells.
        SYS_SPAWN => {
            let r = crate::nproc::spawn(cur, args[0], args[1], args[2], args[3], args[4], frame);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
            frame
        }
        SYS_WAIT => match crate::nproc::wait(cur, args[0], frame) {
            crate::nproc::Sched::Ret(v) => {
                arch::set_syscall_ret(unsafe { &mut *frame }, v);
                frame
            }
            crate::nproc::Sched::Switch(f) => f,
        },
        // Cooperative round-robin yield to the next runnable native cell
        // (docs/NETSTACK.md the service-cell section, rheo-net N4a). The N-cell
        // generalisation of `SYS_SWITCH`'s `cur^1` hand-off, needed because a
        // service serving N clients cannot reach client 3 from client 2 with an
        // XOR. Falls back to `cur^1` where the caller has no native process tree,
        // so the Phase E/J two-cell path is unchanged.
        SYS_YIELD => match crate::nproc::yield_cell(cur) {
            crate::nproc::Sched::Ret(v) => {
                arch::set_syscall_ret(unsafe { &mut *frame }, v);
                frame
            }
            crate::nproc::Sched::Switch(f) => f,
        },
        // Arm a one-shot deadline; block until it elapses (docs/LIBRHEO.md Phase
        // F). The deadline goes to the timer arbiter, in the slot `args[1]` names:
        // 0 = the cell's sleep (the pre-N2e shape), 1 = the transport **pacer**'s
        // continuously re-armed send deadline (docs/NETSTACK.md 21). An unknown
        // value falls back to the sleep slot rather than failing the call.
        // A sleep is a *registration*, not an in-kernel wait, whenever some other
        // cell can run: the deadline goes to the arbiter, the caller parks, and a
        // sibling gets the CPU (docs/ARCHITECTURE-DEBT.md 2.4). With no sibling to
        // hand it to, the pre-existing in-trap wait is kept byte for byte - it *is*
        // the idle in that case.
        SYS_ARM_TIMER => {
            let client = if args[1] == crate::abi::TIMER_CLIENT_PACER {
                crate::ktimer::TimerClient::Pacer
            } else {
                crate::ktimer::TimerClient::CellSleep
            };
            match crate::nproc::block_timer(cur, args[0], client) {
                Some(f) => f,
                None => {
                    crate::time::arm_timer_as(args[0], client);
                    arch::set_syscall_ret(unsafe { &mut *frame }, 0);
                    frame
                }
            }
        }
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
        // Ownership-checked teardown: only frames the cell holds through a live
        // MemoryGrant capability or its own bump regions (docs/ENGINEERING.md 12).
        SYS_MUNMAP => {
            let r = sys_munmap(cur, args[0] as usize, args[1] as usize);
            arch::set_syscall_ret(unsafe { &mut *frame }, r);
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
            // The destination is a cell-supplied address, and since the wait may now
            // complete *later* (in the scheduler, after siblings ran) it is bounded
            // here rather than at the write (docs/ENGINEERING.md 12). A rejected
            // buffer reports 0 bytes and never writes.
            let len = args[1] as usize;
            if user_buf_mut(args[0], len).is_none() {
                arch::set_syscall_ret(unsafe { &mut *frame }, 0);
                return frame;
            }
            // SAFETY: `[args[0], args[0]+len)` was just bounded to this cell's user
            // VA range, and the cell's address space is active whenever the block is
            // completed (`nproc::complete_block` switches to it first).
            match unsafe { crate::nproc::block_console(cur, args[0], len) } {
                Some(f) => f,
                None => {
                    let n = crate::input::wait_input(args[0], len);
                    arch::set_syscall_ret(unsafe { &mut *frame }, n as u64);
                    frame
                }
            }
        }
        // Block until a received Ethernet frame is available (docs/NETSTACK.md,
        // rheo-net N2d) - the network twin of SYS_WAIT_INPUT. The kernel idles at
        // WFI where the NIC's RX interrupt is wired, and polls (bounded) otherwise.
        SYS_WAIT_NET => {
            let len = args[1] as usize;
            if user_buf_mut(args[0], len).is_none() {
                arch::set_syscall_ret(unsafe { &mut *frame }, 0);
                return frame;
            }
            // SAFETY: as SYS_WAIT_INPUT - bounded just above, and the cell's address
            // space is active when the block completes.
            match unsafe { crate::nproc::block_net(cur, args[0], len, args[2]) } {
                Some(f) => f,
                None => {
                    let n = crate::net_rx::wait_frame(args[0], len, args[2]);
                    arch::set_syscall_ret(unsafe { &mut *frame }, n as u64);
                    frame
                }
            }
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
