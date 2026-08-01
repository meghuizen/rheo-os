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
    SYS_RESERVE_RELEASE, SYS_SEAL, SYS_SPAWN, SYS_SWITCH, SYS_VCORE_INFO, SYS_WAIT, SYS_WAIT_INPUT,
    SYS_WAIT_NET, SYS_YIELD, ShareInfo,
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
/// Each cell's **recorded** address-space layout (docs/SUBSTRATE.md pillar 2, S2').
///
/// A parallel array rather than a field of `RunCell`, because `RunCell` is `Copy` - it
/// is assigned and read wholesale - and a `VaSpace` owns a funded table.
///
/// What this buys before placement moves into the allocator: the map stops being
/// *inferred*. `SYS_MUNMAP` decided what an address was by asking which constant range
/// it fell in - the inference `mm/vaspace.rs`'s own header rules out, and one that is
/// wrong the moment a region moves or a new one is added between two others. Every
/// region a cell actually gets is now recorded as it is established, and an address is
/// classified by looking it up.
static mut CELL_VA: [crate::mm::vaspace::VaSpace; MAX_CELLS] =
    [const { crate::mm::vaspace::VaSpace::new() }; MAX_CELLS];

/// Records pre-funded per cell at boot. Enough for the regions a cell establishes -
/// its queue ring, its channel slots, its grants and its mappings - without growing.
const LAYOUT_RECORDS: usize = 64;

/// Fund every cell's layout table **once, at boot**.
///
/// The frames have to be taken here and not on first use. A funded table grows lazily,
/// so its first frame is charged to whichever operation happens to be first - and every
/// per-operation frame-cost oracle in the suite then moves by that frame. A first
/// attempt at recording layouts did exactly that and broke the `security` kernel; this
/// is the S1' answer (docs/SUBSTRATE.md 15) applied per cell: pay at a reset point, so
/// the cost is a boot cost.
///
/// Charged to `Owner::KERNEL` rather than to each cell, which is the honest consequence
/// of making it a boot cost: an exhaustion here names the kernel, not the cell that
/// would have grown the table. That is the same trade the mapped-file registry makes.
pub fn init_layouts() {
    for i in 0..MAX_CELLS {
        let va = cell_va(i);
        va.init(crate::mm::kmeta::Owner::KERNEL);
        va.fund(LAYOUT_RECORDS);
    }
}

/// Cell `idx`'s recorded layout.
#[allow(clippy::mut_from_ref)]
fn cell_va(idx: usize) -> &'static mut crate::mm::vaspace::VaSpace {
    // SAFETY: a cell belongs to one core (`cell_on_this_cpu`), and its layout is only
    // touched while servicing that cell's own trap or while installing it.
    unsafe { &mut (*core::ptr::addr_of_mut!(CELL_VA))[idx] }
}

/// Whether `[base, base+len)` in cell `idx` overlaps a region the **kernel** owns,
/// and which one - the question a caller-chosen `MAP_FIXED` has to be asked.
///
/// Asked of the cell's recorded layout rather than answered from a list of
/// constants. The constants were the previous answer, and restating spans that are
/// already recorded is the inference `mm/vaspace.rs`'s own header rules out: the
/// list goes stale the moment a region moves, and it is one edit away from being
/// short an entry.
///
/// Written as an **allow-list over `RegionKind` with no `_` arm**, which is the part
/// that matters. A deny-list ("refuse queue and channel") answers today's question
/// and defaults a *new* kernel-owned kind to permitted - silently, at whatever
/// commit adds it. This form defaults it to refused and makes adding a variant a
/// compile error, so the decision is forced where the knowledge is.
///
/// The cell's *own* regions - its image, its interpreter, its stack, its anonymous
/// and file mappings - are permitted: `ld.so` legitimately `MAP_FIXED`es over its
/// own reservations, and refusing that would break every dynamically linked binary
/// (docs/LINUX-COMPAT.md L7).
///
/// Honest about reach: the only caller is the Linux `mmap`'s `MAP_FIXED` path, and a
/// Linux cell holds no typed grant and no device BAR (both are native verbs), so
/// today this refuses exactly the two spans the constant list refused. What changed
/// is the *rule*, not the set of refusals - said plainly rather than sold as a new
/// capability (docs/ENGINEERING.md 7).
pub fn kernel_owned_overlap(idx: usize, base: usize, len: usize) -> Option<&'static str> {
    use crate::mm::vaspace::RegionKind;
    let end = base.saturating_add(len);
    cell_va(idx)
        .iter()
        .filter(|r| base < r.end() && r.base < end)
        .find_map(|r| match r.kind {
            // The cell's own mappings: a caller may replace these.
            RegionKind::Image
            | RegionKind::Interp
            | RegionKind::Stack
            | RegionKind::Anon
            | RegionKind::File => None,
            // The kernel's: it holds frames, a raw overlay, or a peer's view here.
            RegionKind::Queue => Some("the cell's queue-pair region"),
            RegionKind::Channel => Some("the cell's cross-cell channel region"),
            RegionKind::Grant => Some("a typed memory grant of the cell"),
            RegionKind::Fixed => Some("a kernel-placed fixed mapping"),
            RegionKind::DeviceBar => Some("a device BAR window"),
        })
}

/// Record a region a cell has been given, ignoring a refusal.
///
/// Best-effort on purpose. A `.user`-window cell's code and stack are linked beside the
/// kernel on two of the three ISAs, i.e. **above** the allocator's ceiling, so recording
/// them is refused `OutOfRange` - and correctly so: an address the allocator can never
/// hand out cannot collide with one it does. This is a *description* of what a cell
/// holds until placement moves into the allocator, and a description that cannot be
/// stored is not a failure of the operation being described.
fn record_region(idx: usize, base: usize, len: usize, kind: crate::mm::vaspace::RegionKind) {
    let _ = cell_va(idx).reserve_fixed(base, len, kind, 0);
}

const MMAP_BASE: usize = 0x3_0000_0000;
/// Exclusive top of the anonymous-`mmap` window - **the queue region's base**.
///
/// The same unbounded-cursor defect as the file-mmap window, and worse in one respect:
/// this cursor is **global**, not per cell, so the 4 GiB between the window and the
/// queue ring at 16 GiB is consumed by every cell in the boot together. Past it the
/// next anonymous mapping would be placed on top of a cell's own queue-pair ring - the
/// one the kernel still holds a raw `QueuePair` overlay onto (docs/SUBSTRATE.md pillar
/// 2). Bounded here; placing the regions with the allocator is the rest of S2'.
///
/// **Unproven, and deliberately said so.** Unlike the grant ceiling, this one has no
/// direct test, because it cannot be reached in a single call: an anonymous mapping is
/// frame-backed, so any span large enough to cross the 4 GiB window is refused first by
/// the per-cell frame budget (`MAX_FRAMES_PER_CELL`, 384 MiB), and a span large enough
/// to overflow the arithmetic is refused before that. Reaching it needs ~4 GiB of
/// *successful* mappings accumulated across several cells - which is precisely the
/// hazard of the cursor being global rather than per cell, and precisely what the
/// allocator removes. A first version of the proof asserted a refusal that the existing
/// overflow check was already producing, i.e. it passed with this ceiling deleted; it
/// was removed rather than kept as decoration (docs/ENGINEERING.md 1).
const MMAP_TOP: usize = crate::load::USER_QUEUE_VA;

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

/// Exclusive upper bound of a cell's low-half user VA range - **each ISA's own**
/// ([`arch::USER_VA_TOP`]): RISC-V Sv39 `2^38` = 256 GiB, x86-64 `2^47` = 128 TiB,
/// ARM64 TTBR0 `2^48` = 256 TiB.
///
/// This was `2^38` on all three, because Sv39 has the narrowest user half and one
/// portable number is simpler than three. It was also the wrong number twice over,
/// and both ways cost something real (docs/SUBSTRATE.md pillar 2):
///
/// - **It held the two wide ISAs to the narrow one.** A modern runtime reserves
///   address space by the hundred gigabyte - JavaScriptCore's Gigacage is a single
///   128 GiB `PROT_NONE` reservation, half of the whole Sv39 half - so the Linux
///   `mmap` window had to be squeezed into the 172 GiB left over
///   (`linux::mem::MMAP_BASE`), and a *second* cage would not have fit at all. On
///   x86-64 and ARM64 the hardware was never the constraint; this constant was.
/// - **It is a property of the page-table format, so it belongs in `arch`.** Sv39
///   is the floor *profile*, not a ceiling the other two must accept - the same
///   distinction `arch::USER_VA_TOP` already draws for [`crate::mm::vaspace`].
///
/// Everything the loader places is far below even the floor: image 1-4 GiB, stack
/// 8 GiB ([`crate::load::USER_STACK_TOP`]), anon mmap 12 GiB ([`MMAP_BASE`]), queue
/// 16 GiB ([`crate::load::USER_QUEUE_VA`]), file mmap 20 GiB ([`FILEMMAP_BASE`]),
/// channels 24 GiB ([`crate::load::USER_CHANNEL_VA`]), grants 32 GiB
/// ([`GRANT_BASE`]), and the Linux ELF interpreter 64 GiB
/// ([`crate::load::LINUX_INTERP_BASE`]) - asserted below, so the widening cannot
/// have moved anything. What grows is only what a cell *asks* for at run time.
pub const USER_VA_MAX: u64 = arch::USER_VA_TOP as u64;

// The narrowest ISA is still the floor, so a region that fit before still fits.
const _: () = assert!(USER_VA_MAX >= (1 << 38));

// The layout above is asserted at compile time, so moving a region without
// revisiting this bound cannot compile.
const _: () = assert!((crate::load::LINUX_INTERP_BASE as u64) < USER_VA_MAX);
const _: () = assert!((GRANT_BASE as u64) < USER_VA_MAX);
const _: () = assert!((crate::load::USER_CHANNEL_VA as u64) < USER_VA_MAX);
const _: () = assert!((FILEMMAP_BASE as u64) < USER_VA_MAX);
const _: () = assert!((crate::load::USER_QUEUE_VA as u64) < USER_VA_MAX);
const _: () = assert!((MMAP_BASE as u64) < USER_VA_MAX);

// ...and so is the layout's **internal order**, which was previously only a comment.
//
// Every growing region now ends where the next one begins, and each of those bounds is
// a second hand-written number that has to agree with the first. Writing the agreement
// down as an assertion is what turns "these constants happen to be ordered" into
// something a change cannot quietly break: move a base without moving the ceiling that
// names it and the kernel does not compile. That is the part of S2' available without
// the allocator; with it, the ordering is a *result* and none of this is needed
// (docs/SUBSTRATE.md pillar 2).
const _: () = assert!(crate::load::USER_STACK_TOP < MMAP_BASE);
const _: () = assert!(MMAP_BASE < MMAP_TOP);
const _: () = assert!(MMAP_TOP == crate::load::USER_QUEUE_VA);
const _: () = assert!(crate::load::USER_QUEUE_VA < FILEMMAP_BASE);
const _: () = assert!(FILEMMAP_BASE < FILEMMAP_TOP);
const _: () = assert!(FILEMMAP_TOP == crate::load::USER_CHANNEL_VA);
// The channel window holds every slot a cell can own, and must end below the grants.
const _: () = assert!(
    crate::load::USER_CHANNEL_VA + MAX_CELL_CHANNELS * crate::queue::QueuePair::REGION_SIZE
        <= GRANT_BASE
);
const _: () = assert!(GRANT_BASE < GRANT_TOP);
// Grants run to the top of the user range. The Linux ELF interpreter's base sits inside
// that span, and does not conflict: a cell has one personality, and a `Personality::
// Linux` cell has no typed grants while a native cell has no interpreter. Stated rather
// than asserted, because the assertion that looks right here - that they are disjoint -
// would be false and would only be satisfiable by capping native grants for a region
// they can never meet.
const _: () = assert!(GRANT_TOP == USER_VA_MAX as usize);

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
    /// One **queue pair per vcore** (docs/SUBSTRATE.md S5). Slot 0 is the ring `install`
    /// was handed; a vcore added by [`install_vcore`] brings its own.
    ///
    /// Per vcore because a ring is a single-producer structure: two contexts submitting
    /// into one would have to serialise, and once they run on two cores that serialisation
    /// is a cross-core write to shared indices - the cost the io_uring-per-thread shape
    /// exists to avoid. With one ring each, a submission never leaves its own core.
    vqp: [*const QueuePair; MAX_VCORES],
    /// One execution context per vcore (docs/SUBSTRATE.md pillar 3, [`MAX_VCORES`]).
    ///
    /// Slot 0 is the cell's original context - the frame `install` was handed, and the
    /// one the Linux personality, `nproc` and `SYS_SWITCH` all mean when they say "the
    /// cell's frame". Slots `1..nvcores` exist only for a cell some launcher gave them
    /// to with [`install_vcore`].
    ///
    /// **This is what makes one cell runnable on two cores.** Two cores in one cell
    /// would otherwise share one trap frame, one kernel stack and one FP save area,
    /// none of which is locked - which is why the claim used to be per *cell*
    /// (docs/SMP.md 10.0). Per vcore the three are disjoint again, and the claim moves
    /// down with them.
    vframe: [*mut TrapFrame; MAX_VCORES],
    /// How each vcore's run ended, or `None` while it is live or handing off.
    voutcome: [Option<Outcome>; MAX_VCORES],
    /// How many vcores this cell holds. 1 unless [`install_vcore`] added more.
    nvcores: usize,
    present: bool,
    personality: Personality,
    /// Base VA of the cell's mapped queue-pair region, reported by
    /// `SYS_QUEUE_INFO` (docs/LIBRHEO.md), **per vcore**: the calling context is told
    /// about its own ring. 0 = that vcore has no mapped queue.
    vqp_va: [u64; MAX_VCORES],
    /// 32-bit ABI id of each vcore's QueuePair capability, reported alongside.
    vqp_cap: [u32; MAX_VCORES],
    /// Next free VA for a typed memory-grant reservation (`SYS_GRANT`,
    /// docs/LIBRHEO.md Phase B). Per-cell so two cells' grants never collide.
    /// Next free VA for a file mmap (`SYS_MMAP_FILE`).
    /// The cell's cross-cell shared-channel ends, reported by `SYS_CONNECT`
    /// (docs/LIBRHEO.md Phase E; the multi-slot table is docs/NETSTACK.md the
    /// service-cell section, rheo-net N4a). Slot 0 is the Phase E/J channel; a
    /// **service cell** holds one slot per client, which is what makes fan-out
    /// possible. Fixed array - the kernel allocates nothing.
    chan: [ChanEnd; MAX_CELL_CHANNELS],
    /// The CPU that **owns** this cell, or [`NO_CPU`] for "not claimed by anyone".
    ///
    /// A cell belongs to one core at a time: that is the partitioning discipline the
    /// multikernel model rests on (docs/SCHEDULING.md 1a, docs/SMP.md 10.0), and it
    /// is what makes running cells on several cores safe without locking the cell
    /// table. Until something claims a cell this is [`NO_CPU`], and an unclaimed cell
    /// is pickable by any core - which is exactly the single-CPU behaviour, so a boot
    /// that never claims anything is unchanged.
    ///
    /// **Per vcore**, so two cores can own two contexts of the same cell. For a
    /// single-vcore cell only slot 0 is ever read, which is the pre-vcore behaviour.
    vcpu: [usize; MAX_VCORES],
    /// The NUMA node this cell's memory is placed on (docs/SUBSTRATE.md pillar 6),
    /// or [`frames::NODE_ANY`] on a machine with one node.
    ///
    /// **A cell's memory is co-located with the cell.** Its page tables and
    /// capability tables (`mm::kmeta`), its typed grants, and every page it commits
    /// all draw from this node, so a cell's data and the kernel's records about it sit
    /// together rather than being scattered by whichever allocation happened first.
    /// A cell that names no node in `SYS_GRANT` gets this one - "no preference" means
    /// "the kernel decides", and the kernel decides locality, which is also what
    /// Linux's default allocation policy does.
    ///
    /// Assigned round-robin across the nodes the frame pool actually holds, so a
    /// multi-cell workload spreads its memory bandwidth instead of piling on node 0.
    /// Not a capability and not the cell's choice: the kernel stamps it, in the shape
    /// of docs/IDENTITY.md's principal.
    node: u8,
}

/// "No CPU owns this cell." The default, and the value that preserves single-CPU
/// behaviour exactly.
pub const NO_CPU: usize = usize::MAX;

/// The next cell's home node, advanced round-robin at each `install`
/// (docs/SUBSTRATE.md pillar 6).
static NEXT_NODE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The NUMA node cell `idx` places its memory on, or [`frames::NODE_ANY`].
pub fn cell_node(idx: usize) -> u8 {
    if idx >= MAX_CELLS {
        return frames::NODE_ANY;
    }
    cells()[idx].node
}

/// The home node for the next cell, round-robin over the nodes the pool holds.
///
/// [`frames::NODE_ANY`] where the machine has fewer than two, which is what keeps a
/// non-NUMA boot's allocation paths byte-for-byte what they were.
fn next_home_node() -> u8 {
    let nodes = frames::nodes_known();
    if nodes < 2 {
        return frames::NODE_ANY;
    }
    let n = NEXT_NODE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    (n % nodes) as u8
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
    vqp: [core::ptr::null(); MAX_VCORES],
    vframe: [core::ptr::null_mut(); MAX_VCORES],
    voutcome: [None; MAX_VCORES],
    nvcores: 0,
    present: false,
    personality: Personality::Native,
    vqp_va: [0; MAX_VCORES],
    vqp_cap: [0; MAX_VCORES],
    chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
    vcpu: [NO_CPU; MAX_VCORES],
    node: frames::NODE_ANY,
};

/// Base VA of a native cell's typed memory-grant reservations (docs/LIBRHEO.md
/// Phase B): 32 GiB, above the image (1-4), stack (8), anon mmap (12+), and
/// queue (16) regions, free in every cell root. Reservations are pure address
/// space (48-bit VA), so multi-GiB grants cost nothing until committed.
const GRANT_BASE: usize = 0x8_0000_0000;
/// Exclusive top of the grant window. The fixed VA map (docs/SUBSTRATE.md pillar 2,
/// migration S2') gives each region a start and, until now, no end: the cursors were
/// bumps with nothing above them. `SYS_GRANT` reserves pure address space, so a cell
/// asking for terabytes of it is cheap and legitimate right up to the point the cursor
/// walks out of the ISA's user range - which is a fault at some unrelated address
/// rather than a refusal. Grants are the topmost region, so their ceiling is the range
/// itself.
const GRANT_TOP: usize = USER_VA_MAX as usize;
/// Base VA of a native cell's file mmaps (`SYS_MMAP_FILE`): 20 GiB.
const FILEMMAP_BASE: usize = 0x5_0000_0000;
/// Exclusive top of the file-mmap window - **the channel region's base**, which is what
/// this cursor was previously free to grow into.
///
/// A real defect, not a tidy-up: `mmap_file` bumped `filemmap_next` with no upper
/// bound, and the shared cross-cell channel rings sit 4 GiB above the window's start
/// (`crate::load::USER_CHANNEL_VA`). A cell that file-mapped 4 GiB in total would have
/// its next mapping placed **on top of its own channel**, silently replacing the ring
/// two cells communicate through. Bounding each region at its neighbour is the part of
/// S2' that can be stated as a constant; giving the regions to a real allocator so the
/// bound is a *result* rather than a second hand-written number is the rest of it, and
/// is not done (docs/SUBSTRATE.md pillar 2).
const FILEMMAP_TOP: usize = crate::load::USER_CHANNEL_VA;
const _: () = assert!(FILEMMAP_BASE < FILEMMAP_TOP);
const _: () = assert!(GRANT_BASE < GRANT_TOP);

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
    /// is refused. The typed *kind* selects the physical pool (`Backing`); the
    /// *node* below selects which part of the DDR pool.
    #[allow(dead_code)]
    kind: u8,
    /// The NUMA node the cell asked its frames to come from, or
    /// [`frames::NODE_ANY`] (docs/SUBSTRATE.md pillar 6). Read from `SYS_GRANT`'s
    /// fourth argument, which librheo's `mem::reserve_on` has always sent and which
    /// the kernel used to drop on the floor - documented as "recorded but
    /// single-node", which was two claims and only the second was true.
    node: u8,
    sealed: bool,
    cap_id: u32,
}

const EMPTY_GRANT: GrantSlot = GrantSlot {
    in_use: false,
    base: 0,
    len: 0,
    kind: 0,
    node: frames::NODE_ANY,
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

/// How many **vcores** one cell may hold (docs/SUBSTRATE.md pillar 3).
///
/// A vcore is one execution context of a cell: its own trap frame, its own kernel
/// stack (carried in that frame), its own FP/SIMD save area and its own owning CPU.
/// Vcore 0 is the context `install` builds, so a cell that is never given a second
/// vcore holds `nvcores == 1` and every path below behaves exactly as it did before
/// vcores existed.
///
/// Four rather than "as many as there are CPUs" because the FP save areas are a fixed
/// static here (`MAX_CELLS * MAX_VCORES` of them, 4 KiB each on x86-64 = 256 KiB of
/// `.bss`). Funding them out of the owning cell's own frame budget - the `mm::kmeta`
/// mechanism S1' already built for the other tables - is what removes the number, and
/// is a named follow-on rather than something this slice needs.
pub const MAX_VCORES: usize = 4;

static mut CELLS: [RunCell; MAX_CELLS] = [EMPTY; MAX_CELLS];

/// The cell each CPU is currently running, the cell each entered `run` with, and the
/// cell whose exit ended each one's run.
///
/// **Per-CPU, because two cores can each be running a cell** (docs/SMP.md 10.0). These
/// were three `static mut usize`s, which was correct while exactly one core ever
/// entered user mode and is the single thing that makes "a cell in user mode on a
/// secondary" produce nonsense rather than a fault: the secondary would overwrite the
/// primary's notion of which cell is running, and `uaccess` would then resolve the
/// primary's pointers against the secondary's address space.
///
/// On the non-`smp` build `cpu_index()` is a compile-time 0, so every access resolves
/// to slot 0 with no indexing at run time and the behaviour is exactly what it was.
static CURRENT: crate::smp::PerCpu<usize> = crate::smp::PerCpu::new(0);

#[inline]
fn cur_cpu_cell() -> usize {
    *CURRENT.this()
}

/// Which **vcore** of [`CURRENT`] this CPU is inside (docs/SUBSTRATE.md pillar 3).
///
/// Per CPU for the same reason `CURRENT` is: two cores inside two vcores of one cell
/// are inside the *same* cell index and different contexts, so the cell alone cannot
/// say which frame or which FP area a trap belongs to. 0 on every path that predates
/// vcores, which is why they are all unchanged.
static CUR_VCORE: crate::smp::PerCpu<usize> = crate::smp::PerCpu::new(0);

/// The vcore of the current cell this CPU is running.
#[inline]
pub fn current_vcore() -> usize {
    *CUR_VCORE.this()
}

/// Record that this CPU is now running vcore `v` of the current cell.
///
/// For the one caller that swaps the register file itself rather than through
/// [`switch_native_cell_vcore`] - `nproc::preempt_cell`, which must save FP *before* the
/// pick runs any kernel code (see its own comment) and so cannot use the packaged switch.
pub fn set_current_vcore(v: usize) {
    // No `enter_vcore` here, for the reason `switch_native_cell` gives: this is a
    // cross-cell move, and marking `INSIDE` outside `run_inner`'s bracket produces a
    // false double-entry when a batch sibling exits without passing back through it.
    // SAFETY: this CPU's own slot.
    unsafe { *CUR_VCORE.this_mut() = v };
}

/// The cell whose trap is being serviced. `crate::uaccess` needs it to know whose
/// address space a supplied pointer belongs to, and whether that cell's mappings are
/// lazy at all (a native cell's are not).
pub fn current_cell() -> usize {
    cur_cpu_cell()
}

/// The syscall ABI cell `idx` speaks - `uaccess` skips its lazy-mapping work entirely
/// for a native cell, whose pages are all committed at load.
pub fn cell_personality(idx: usize) -> Personality {
    cells()[idx].personality
}
static EXITED: crate::smp::PerCpu<usize> = crate::smp::PerCpu::new(0);

/// Which vcore of [`EXITED`] ended this CPU's run.
static EXITED_VCORE: crate::smp::PerCpu<usize> = crate::smp::PerCpu::new(0);

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

/// One area per **vcore**, flat: `cell * MAX_VCORES + vcore`.
///
/// Per vcore rather than per cell because two cores running two contexts of one cell
/// each hold a live register file, and one area between them would have each core save
/// over the other's image - no fault, no log, wrong numbers, the same shape as the
/// `SYS_YIELD` defect (docs/LIBRHEO.md).
static mut CELL_FP: [FpArea; MAX_CELLS * MAX_VCORES] =
    [const { FpArea([0; arch::FP_AREA_LEN]) }; MAX_CELLS * MAX_VCORES];

/// Pointer to vcore `v` of cell `idx`'s FP save area.
fn cell_fp(idx: usize, v: usize) -> *mut u8 {
    // SAFETY: `idx < MAX_CELLS` and `v < MAX_VCORES`, so the index is in bounds; each
    // area belongs to one vcore, which belongs to one CPU at a time.
    unsafe {
        (*core::ptr::addr_of_mut!(CELL_FP))[idx * MAX_VCORES + v]
            .0
            .as_mut_ptr()
    }
}

/// Count of native FP/SIMD register-file swaps performed by
/// [`switch_native_cell`] (and of the initial loads done by [`run`]). Bumped
/// *only* inside the swap itself, so a test can assert the swap really ran on
/// every switch rather than infer it from the code (docs/ENGINEERING.md 1).
/// Relaxed atomic, not a `static mut +=`: two cores now restore FP for two vcores of
/// one cell at the same instant, and a read-modify-write of a plain static loses
/// counts (the fix the `preempt`/`dispatch` counters already took, docs/SMP.md 10.0).
static FP_SWAPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many times a native cell's FP/SIMD register file has been swapped
/// (docs/LIBRHEO.md; the `librheoipc` FP regression phase asserts this is at
/// least the number of cross-cell yields it drove).
pub fn fp_swaps() -> u64 {
    FP_SWAPS.load(core::sync::atomic::Ordering::Acquire)
}

/// Save native cell `idx`'s live U-mode FP/SIMD state into its area (the kernel
/// is soft-float, so the registers hold `idx`'s values at the switch point).
/// Harmless if `idx` is exiting - the saved image is simply never restored.
pub fn save_native_fp(idx: usize) {
    save_native_fp_vcore(idx, 0);
}

/// [`save_native_fp`] for a named vcore of the cell.
pub fn save_native_fp_vcore(idx: usize, v: usize) {
    // SAFETY: `cell_fp(idx, v)` is a valid, sufficiently-aligned area.
    unsafe { arch::save_user_fp(cell_fp(idx, v)) };
}

/// Restore native cell `idx`'s U-mode FP/SIMD state before resuming it. For a
/// cell that has never run, the area was set to a clean image by `fp_area_init`
/// at install time, so this loads the ABI-default FP state.
pub fn restore_native_fp(idx: usize) {
    restore_native_fp_vcore(idx, 0);
}

/// [`restore_native_fp`] for a named vcore of the cell.
pub fn restore_native_fp_vcore(idx: usize, v: usize) {
    // SAFETY: `cell_fp(idx, v)` holds a valid image (saved, or `fp_area_init`ed).
    unsafe { arch::restore_user_fp(cell_fp(idx, v)) };
    FP_SWAPS.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
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
    switch_native_cell_vcore(from, to, 0);
}

/// [`switch_native_cell`], entering a named **vcore** of the target.
///
/// Cross-cell paths that predate vcores enter vcore 0; the native scheduler names the
/// vcore, because a cell that parked one context and left a sibling runnable must be
/// re-entered at the sibling rather than at vcore 0.
pub fn switch_native_cell_vcore(from: usize, to: usize, to_v: usize) {
    // Save the vcore this CPU is actually **inside**, and load the vcore the target is
    // entered at, which for a cross-cell switch is always its vcore 0. Naming both
    // rather than assuming 0 on each side is what lets a multi-vcore cell take this path
    // at all: with `from` fixed at 0, a core inside vcore 1 would write vcore 1's live
    // registers into vcore 0's saved image, which is the corruption per-vcore areas exist
    // to prevent (docs/SUBSTRATE.md pillar 3).
    save_native_fp_vcore(from, current_vcore());
    switch_to_cell(to);
    restore_native_fp_vcore(to, to_v);
    // SAFETY: this CPU's own slot.
    unsafe { *CUR_VCORE.this_mut() = to_v };
    // Deliberately **not** `enter_vcore`. `INSIDE` is written on entry and cleared on
    // return by `run_inner`, and a cross-cell switch chain sits *inside* one such
    // bracket - so making this a second writer of the slot was tried and fires a false
    // double-entry during placement, because a batch sibling can exit without passing
    // back through `run_inner` at all (the same looseness docs/SMP.md 10.0 records for
    // the per-cell guard). The intra-cell `switch_native_vcore` does mark, because its
    // bracket is this core's own run and nothing else moves the slot inside it.
}

/// Hand this CPU from one vcore of the **current** cell to another (docs/SUBSTRATE.md
/// pillar 3). Returns the frame to resume.
///
/// The cheapest context switch in the system, and the reason a vcore is worth having over
/// a second cell: both contexts live in one address space, so there is **no**
/// `activate()` and no TLB consequence - only the FP/SIMD register file and the frame
/// change hands. That is also the whole of what has to be got right, which is why this is
/// one function rather than a pattern repeated at call sites (docs/ENGINEERING.md 3, the
/// `switch_native_cell` precedent).
pub fn switch_native_vcore(cell: usize, from: usize, to: usize) -> *mut TrapFrame {
    assert!(from != to, "vcore self-switch in cell {cell}");
    assert!(
        to < cells()[cell].nvcores,
        "switch to vcore {to} of cell {cell}, which holds {}",
        cells()[cell].nvcores
    );
    save_native_fp_vcore(cell, from);
    restore_native_fp_vcore(cell, to);
    enter_vcore(cell, to);
    // SAFETY: this CPU's own slot. `CURRENT` does not change - same cell.
    unsafe { *CUR_VCORE.this_mut() = to };
    cells()[cell].vframe[to]
}

/// Whether vcore `v` of cell `idx` still exists to be run - it has not exited.
///
/// A vcore's outcome is recorded when it ends, so `voutcome[v].is_none()` *is* liveness.
/// Every path that picks a vcore has to ask, because a cell now outlives its first vcore's
/// exit (docs/SMP.md 10.0a) and entering a dead context resumes it at its own `SYS_EXIT`.
#[inline]
pub fn vcore_live(idx: usize, v: usize) -> bool {
    v < cells()[idx].nvcores && cells()[idx].voutcome[v].is_none()
}

/// How many vcores of cell `idx` have not exited.
pub fn live_vcores(idx: usize) -> usize {
    (0..cells()[idx].nvcores)
        .filter(|&v| cells()[idx].voutcome[v].is_none())
        .count()
}

/// Record that vcore `v` of cell `idx` has ended with `outcome`, without ending the cell.
///
/// The **last vcore out** ends the cell; an earlier one only ends itself. That is the
/// process/thread split the Linux personality already has one level up (`exit` vs
/// `exit_group`), and without it a cell with four vcores dies when the first finishes.
pub fn retire_vcore(idx: usize, v: usize, outcome: Outcome) {
    cells()[idx].voutcome[v] = Some(outcome);
}

/// Whether the calling CPU may enter vcore `v` of cell `idx`.
///
/// The per-vcore form of [`cell_on_this_cpu`], and the predicate that replaced that
/// function's blanket refusal of multi-vcore cells: a vcore belongs to one core, an
/// unclaimed one is enterable by any, and two cores inside two *different* vcores of one
/// cell is the point rather than a hazard.
#[inline]
pub fn vcore_on_this_cpu(idx: usize, v: usize) -> bool {
    if !vcore_live(idx, v) {
        return false;
    }
    let owner = cells()[idx].vcpu[v];
    owner == NO_CPU || owner == crate::smp::cpu_index()
}
/// The cell `run` was entered with (docs/LINUX-COMPAT.md L6): the top of the
/// Linux process tree. Only its exit ends the whole run; a forked child's exit
/// makes it a zombie and reschedules another cell (`linux::proc`).
static TOP_CELL: crate::smp::PerCpu<usize> = crate::smp::PerCpu::new(0);

fn cells() -> &'static mut [RunCell; MAX_CELLS] {
    // SAFETY: single CPU, synchronous traps; no aliasing run concurrently.
    unsafe { &mut *core::ptr::addr_of_mut!(CELLS) }
}

/// Clear the run table (call before installing a fresh set of cells).
pub fn reset() {
    *cells() = [EMPTY; MAX_CELLS];
    // SAFETY: single CPU, between runs.
    unsafe {
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
    cur_cpu_cell()
}

/// The installed frame pointer for cell `idx`. The Linux personality's thread
/// table uses this as its initial (thread 0) execution context
/// (docs/LINUX-COMPAT.md L4); clone-created threads get kernel-owned frames.
pub fn cell_frame(idx: usize) -> *mut TrapFrame {
    cells()[idx].vframe[0]
}

/// The frame of vcore `v` of cell `idx` (docs/SUBSTRATE.md pillar 3). `cell_frame(idx)`
/// is `vcore_frame(idx, 0)`.
pub fn vcore_frame(idx: usize, v: usize) -> *mut TrapFrame {
    cells()[idx].vframe[v]
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
            // Skip a span with no page table under it in one step. Without this the
            // loop is O(range/4KiB) *regardless of what is mapped*, and a program's
            // large reservations - JavaScriptCore's 128 GiB Gigacage, or anything
            // sized against the now-terabyte-wide window - make that a hang rather
            // than a slow unmap (docs/SUBSTRATE.md pillar 2).
            let gap = aspace.unmapped_span(a);
            if gap > 0 {
                a = a.saturating_add(gap);
                continue;
            }
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
    // The **current cell's** node, not `NODE_ANY`. This is the bulk of a cell's
    // memory - anonymous `mmap`, the Linux heap and stack, demand-page fills, COW
    // copies - so placing it anywhere would make "a cell's memory is co-located with
    // the cell" false of almost all of it (docs/SUBSTRATE.md pillar 6).
    commit_range_from(va, len, perm, Backing::Ddr, cell_node(current_index()))
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
///
/// `node` is a **NUMA preference** for the fresh frames: the node the grant asked
/// for, or [`frames::NODE_ANY`]. A preference and not a guarantee - a full node falls
/// back to the pool at large and the fallback is counted
/// (`frames::numa_fallbacks`), because refusing would turn a bandwidth question into
/// an out-of-memory one (docs/SUBSTRATE.md pillar 6).
pub fn commit_range_from(va: usize, len: usize, perm: MapPerm, backing: Backing, node: u8) -> bool {
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
                        // The pmem pool is a single region with no node of its own
                        // here, so a node preference does not apply to it; its DDR
                        // fallback honours the preference like any other DDR frame.
                        Backing::Pmem => {
                            crate::mm::frames_pmem::alloc().or_else(|| frames::alloc_on(node))
                        }
                        Backing::Ddr => frames::alloc_on(node),
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
    let Some(bytes) = pages.checked_mul(frames::FRAME_SIZE) else {
        return 0;
    };
    // Placed by the allocator inside the anonymous-mmap window. This also retires the
    // **global** cursor this path used to share across every cell in a boot - the space
    // is now per cell, which is what a per-cell allocator means.
    let Ok(base) = cell_va(cur).reserve_in(
        MMAP_BASE,
        MMAP_TOP,
        bytes,
        frames::FRAME_SIZE,
        crate::mm::vaspace::RegionKind::Anon,
        0,
    ) else {
        return 0;
    };
    let top = base + bytes;
    if !user_write_ok(base as u64, bytes) || !charge_frames(cur, pages) {
        cell_va(cur).release_at(base);
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
        // Exhausted despite the reserve: roll the whole request back, the reservation
        // included.
        unmap_range(base, got * frames::FRAME_SIZE);
        uncharge_frames(cur, pages - got);
        cell_va(cur).release_at(base);
        return 0;
    }
    let _ = top;
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
fn grant_create(cur: usize, out_va: u64, len: usize, kind: u64, node_hint: u64) -> u64 {
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
    // **Placed, not bumped.** The address is now the allocator's answer: first-fit
    // inside the grant window, around what this cell already holds, with guard gaps and
    // overlap refused (docs/SUBSTRATE.md pillar 2). The window's ceiling is enforced by
    // the allocator rather than by a comparison against a second constant, and a
    // refusal consumes nothing because there is no cursor left to advance.
    let Ok(base) = cell_va(cur).reserve_in(
        GRANT_BASE,
        GRANT_TOP,
        bytes,
        frames::FRAME_SIZE,
        crate::mm::vaspace::RegionKind::Grant,
        0,
    ) else {
        return u64::MAX;
    };
    *slot = GrantSlot {
        in_use: true,
        base,
        len: bytes,
        kind: kind as u8,
        // The cell's NUMA preference, clamped to a node the machine actually has.
        // An out-of-range ask becomes "no preference" rather than a refusal: the
        // node count is a property of the machine the cell was placed on, which the
        // cell does not choose, so asking for node 3 on a two-node box is a hint
        // that cannot be honoured rather than an error the cell made.
        // An in-range ask is honoured; anything else - including the `NODE_ANY`
        // librheo's `Grant::reserve` sends - resolves to the **cell's own** node.
        // "No preference" means "the kernel decides", and the kernel decides
        // locality; a node this machine does not have is a hint it cannot honour
        // rather than an error the cell made, since the cell did not choose the
        // machine it was placed on.
        node: if (node_hint as usize) < frames::nodes_known() {
            node_hint as u8
        } else {
            cells()[cur].node
        },
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

    // Which region is this? **Asked, not inferred.** The classification below used to
    // be "which constant range does the address fall in", which is wrong the moment a
    // region moves or a new one is added between two others - the inference
    // `mm/vaspace.rs`'s own header rules out. The cell's recorded layout answers it
    // directly; the range tests remain the authority on *extent* until placement moves
    // into the allocator too (docs/SUBSTRATE.md pillar 2).

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
            cell_va(cur).release_at(base);
        }
        return 0;
    }

    // (2) The cell's own anonymous-mmap region, and (3) its file-mmap region - the two
    // whose frames this cell alone holds. Everything else is refused, notably the
    // queue-pair region the kernel still has a raw overlay onto and the shared channel
    // slots mapped into two cells.
    //
    // **Which one this is, is asked rather than inferred.** It used to be decided by
    // which constant range the address fell in - the inference `mm/vaspace.rs`'s own
    // header rules out, and one that is wrong the moment a region moves or a new one is
    // added between two others. The cell's recorded layout answers it directly
    // (docs/SUBSTRATE.md pillar 2); the extent test that follows is still the
    // authority on *how much* of the region the call may unmap.
    let region = cell_va(cur).find(base);
    let owned = matches!(
        region.map(|r| r.kind),
        Some(crate::mm::vaspace::RegionKind::Anon | crate::mm::vaspace::RegionKind::File)
    ) && region.map(|r| end <= r.end()).unwrap_or(false);
    if !owned {
        return u64::MAX;
    }
    unmap_range(base, end - base);
    // Give the record back when the whole region goes, so a cell that churns mappings
    // does not exhaust its layout table - the same reasoning that frees a grant slot on
    // a full-reservation unmap just above.
    if let Some(r) = region
        && base == r.base
        && end == r.end()
    {
        cell_va(cur).release_at(base);
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
    // The grant's typed kind decides the physical pool. Before this, every
    // commit went to DDR and a `Pmem` grant was a silent lie
    // (docs/ARCHITECTURE-DEBT.md 3.6).
    let (kind, node) = cell_grants(cur)
        .iter()
        .find(|s| s.in_use && s.cap_id == cap_id)
        .map(|s| (s.kind, s.node))
        .unwrap_or((0, frames::NODE_ANY));
    let backing = Backing::from_kind(kind);
    if backing == Backing::Pmem && crate::mm::frames_pmem::region().is_none() {
        note_backing_fallback(kind, "no nvdimm region on this machine");
    } else if kind == 1 || kind == 2 || kind == 5 {
        note_backing_fallback(kind, "emulated - QEMU models no such memory");
    }
    // Mirrors the pre-existing behaviour: the commit result is not reported to
    // the cell by this verb (a partial commit leaves the pages it did map). The
    // return value is consumed to keep that explicit rather than accidental.
    let _ = commit_range_from(base + offset, len, MapPerm::UserRw, backing, node);
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
    // Map the grant's frames into the peer read-only, at an address the **peer's** own
    // allocator picks - so a delegated mapping is placed by the same rule and around
    // the same regions as one the peer reserved itself, and cannot be dropped on top of
    // something it already holds.
    let Ok(peer_base) = cell_va(peer).reserve_in(
        GRANT_BASE,
        GRANT_TOP,
        slot.len,
        frames::FRAME_SIZE,
        crate::mm::vaspace::RegionKind::Grant,
        0,
    ) else {
        return u64::MAX;
    };
    // SAFETY: single CPU; the client's address space is read (page-table walk,
    // no active requirement) and the peer's is edited (published when the peer is
    // switched to). Both are uniquely owned for the trap.
    let nframes = unsafe {
        let client_aspace = &*cell.aspace;
        let peer_aspace = &mut *(peer_cell.aspace as *mut AddressSpace);
        client_aspace.share_ro_into(peer_aspace, slot.base, slot.len, peer_base)
    };
    if nframes == 0 {
        cell_va(peer).release_at(peer_base);
        return u64::MAX;
    }
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
            // The frames are the client's and already placed; the peer only maps
            // them read-only, so there is nothing here for a node preference to
            // decide. Carried over as a description of where they are.
            node: slot.node,
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
    let pages = bytes / frames::FRAME_SIZE;
    // Placed by the allocator inside the file-mmap window, as grants are.
    let Ok(base) = cell_va(cur).reserve_in(
        FILEMMAP_BASE,
        FILEMMAP_TOP,
        bytes,
        frames::FRAME_SIZE,
        crate::mm::vaspace::RegionKind::File,
        0,
    ) else {
        return 0;
    };
    // `len` is cell-supplied here too: bound the span and charge the frames
    // before mapping anything (docs/ENGINEERING.md 12).
    if !user_write_ok(base as u64, bytes) || !charge_frames(cur, pages) {
        cell_va(cur).release_at(base);
        return 0;
    }
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
        vqp: {
            let mut q = [core::ptr::null(); MAX_VCORES];
            q[0] = qp;
            q
        },
        // Vcore 0 is the frame the caller built; a fresh cell holds exactly one vcore,
        // and a launcher that wants more adds them with `install_vcore`.
        vframe: {
            let mut f = [core::ptr::null_mut(); MAX_VCORES];
            f[0] = frame;
            f
        },
        voutcome: [None; MAX_VCORES],
        nvcores: 1,
        present: true,
        personality: Personality::Native,
        vqp_va: [0; MAX_VCORES],
        vqp_cap: [0; MAX_VCORES],
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
        vcpu: [NO_CPU; MAX_VCORES],
        // Round-robin across the nodes the pool holds, so cells spread their
        // memory bandwidth (docs/SUBSTRATE.md pillar 6). `NODE_ANY` on a
        // single-node machine, which is every allocation path unchanged.
        node: next_home_node(),
    };
    *cell_grants(idx) = [EMPTY_GRANT; MAX_GRANTS_PER_CELL];
    // A fresh cell starts with an empty recorded layout. `clear`, not `init`: the
    // table's frames are a boot cost this slot reuses (see `init_layouts`).
    // Place this cell's kernel metadata - page tables, capability tables, its VA
    // record - on the cell's own node, before anything funds a frame for it
    // (docs/SUBSTRATE.md pillar 6). `kmeta` holds the mapping rather than reading it
    // back out of the cell, so `mm` never depends on `user`.
    crate::mm::kmeta::set_owner_node(crate::mm::kmeta::Owner::cell(idx), cells()[idx].node);
    cell_va(idx).clear();
    // **Reserve the kernel-owned windows for every cell, mapped or not.** The queue
    // ring and the cross-cell channel slots live at fixed addresses the kernel may
    // map into this cell at any time, so those VAs are *reserved* whether or not
    // anything is there yet - and a caller-chosen `MAP_FIXED` over them must be
    // refused either way (`kernel_owned_overlap`).
    //
    // Recorded here rather than only where the ring is actually mapped, because a
    // Linux cell never maps one: leaving it unrecorded made the record *look*
    // complete while quietly permitting a `MAP_FIXED` at 16 GiB in exactly the
    // cells that run unmodified binaries. Caught by `mmapx`, which asserts that
    // refusal.
    record_region(
        idx,
        crate::load::USER_QUEUE_VA,
        QueuePair::REGION_SIZE,
        crate::mm::vaspace::RegionKind::Queue,
    );
    record_region(
        idx,
        crate::load::channel_slot_va(0),
        MAX_CELL_CHANNELS * QueuePair::REGION_SIZE,
        crate::mm::vaspace::RegionKind::Channel,
    );

    // SAFETY: single CPU; a fresh cell starts with no frames charged.
    unsafe { (*core::ptr::addr_of_mut!(CELL_FRAMES))[idx] = 0 };
    // Give the cell a fair-class vcore on this CPU's ready queue, so the scheduler
    // has something to order it by (docs/SUBSTRATE.md pillar 3). A top-level cell
    // starts with a fresh burst score; a child inherits its parent's (see
    // `install_spawned`/`install_forked`).
    crate::sched::dispatch::track(idx, 0, None);
    // Clean FP state, so the first cross-cell switch into this cell restores an
    // ABI-default FPU rather than a zeroed area (docs/LIBRHEO.md).
    // SAFETY: `cell_fp(idx, 0)` is a valid, aligned `FP_AREA_LEN` area.
    unsafe { arch::fp_area_init(cell_fp(idx, 0)) };
}

/// Give installed cell `idx` **another vcore** - a second (third, fourth) execution
/// context in the same address space (docs/SUBSTRATE.md pillar 3).
///
/// Returns the new vcore's index. `frame` must be a fresh trap frame whose entry point,
/// user stack and **kernel stack** are all this vcore's own: a vcore's kernel stack is
/// carried in its frame on ARM64 and RISC-V, so two vcores sharing one would have two
/// cores trapping onto the same stack. (On x86-64 the kernel stack is per-CPU rather
/// than per-frame, so it is already disjoint there; supplying a distinct one anyway is
/// what keeps the three ISAs one contract.)
///
/// Why this is a launcher verb and not a syscall: creating a context is creating
/// something the *scheduler* must own, and the cell-facing spelling of it is the strand
/// runtime asking for a vcore, which is a librheo/`SYS_SPAWN`-shaped question that
/// belongs with the rest of the process model. What is being built here is the kernel
/// mechanism underneath, proven before anything is exposed - the order every capability
/// in this tree landed in.
///
/// `qp` is this vcore's **own** ring (docs/SUBSTRATE.md S5) - a ring is
/// single-producer, so two contexts sharing one would have to serialise, and once they
/// run on two cores that serialisation is a cross-core write to shared indices.
///
/// # Safety
/// `frame` must outlive the cell's run, and no other vcore may share its user stack,
/// kernel stack or queue ring.
pub unsafe fn install_vcore(idx: usize, frame: *mut TrapFrame, qp: *const QueuePair) -> usize {
    let c = &mut cells()[idx];
    assert!(c.present, "install_vcore on empty slot {idx}");
    let v = c.nvcores;
    assert!(v < MAX_VCORES, "cell {idx} already holds {v} vcore(s)");
    c.vframe[v] = frame;
    c.vqp[v] = qp;
    c.voutcome[v] = None;
    c.vcpu[v] = NO_CPU;
    c.nvcores = v + 1;
    // Clean FP state for this vcore's first entry, exactly as `install` does for vcore
    // 0 - a zeroed area is not an ABI-default FPU (docs/LIBRHEO.md).
    // SAFETY: `cell_fp(idx, v)` is a valid, aligned `FP_AREA_LEN` area.
    unsafe { arch::fp_area_init(cell_fp(idx, v)) };
    v
}

/// Record the mapped queue-pair region VA and capability id for cell `idx`
/// (docs/LIBRHEO.md). `SYS_QUEUE_INFO` reports these so a loaded librheo cell
/// can bind its ring. Call after `install`, before `run`.
pub fn set_queue_info(idx: usize, qp_va: u64, cap_id: u32) {
    set_vcore_queue_info(idx, 0, qp_va, cap_id);
}

/// [`set_queue_info`] for a named vcore (docs/SUBSTRATE.md S5): the ring that vcore's
/// own `SYS_QUEUE_INFO` reports and its own `SYS_DOORBELL` drains.
pub fn set_vcore_queue_info(idx: usize, v: usize, qp_va: u64, cap_id: u32) {
    assert!(cells()[idx].present, "set_queue_info on empty slot {idx}");
    cells()[idx].vqp_va[v] = qp_va;
    cells()[idx].vqp_cap[v] = cap_id;
    if v > 0 {
        // Vcore 0's region is recorded by `install`; a later vcore's is recorded here.
        // Recording matters: `SYS_MUNMAP` classifies an address by asking the cell's
        // recorded layout, so an unrecorded ring is one a cell could free (docs/SMP.md,
        // the kernel-owned-overlap allow-list).
        record_region(
            idx,
            qp_va as usize,
            QueuePair::REGION_SIZE,
            crate::mm::vaspace::RegionKind::Queue,
        );
        return;
    }
    record_region(
        idx,
        qp_va as usize,
        QueuePair::REGION_SIZE,
        crate::mm::vaspace::RegionKind::Queue,
    );
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
    record_region(
        idx,
        chan_va as usize,
        QueuePair::REGION_SIZE,
        crate::mm::vaspace::RegionKind::Channel,
    );
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
    let (cell, _vcore, out) = run_vcore(idx, 0);
    (cell, out)
}

/// [`run`], entering a named **vcore** of the cell (docs/SUBSTRATE.md pillar 3).
///
/// Returns which `(cell, vcore)` ended the run and how. `run(idx)` is exactly
/// `run_vcore(idx, 0)`, so every pre-vcore caller is unchanged.
pub fn run_vcore(idx: usize, v: usize) -> (usize, usize, Outcome) {
    run_inner(idx, v);
    let exited = *EXITED.this();
    let ev = *EXITED_VCORE.this();
    (
        exited,
        ev,
        cells()[exited].voutcome[ev].expect("no outcome recorded"),
    )
}

/// Which **vcore** each CPU is inside (`cell * MAX_VCORES + vcore`), or [`NO_CPU`].
///
/// **Per CPU, not per cell**, and the difference is the whole point: a first attempt
/// counted entries per cell and produced false positives, because under preemption a
/// batch sibling exits without ever passing through `run_inner`, so the exit decrements
/// a cell the counter never incremented. Asking "does another CPU already report this
/// cell" is immune to that - it names two cores or it names none.
///
/// Checked rather than assumed because every multi-core defect on this path has
/// presented as corruption somewhere else entirely (a core executing a data symbol, an
/// instruction fetch at 0), which says nothing about where the second entry happened.
/// One store per cell dispatch; nothing on a syscall path.
static INSIDE: crate::smp::PerCpu<usize> =
    crate::smp::PerCpu::from_array([NO_CPU; crate::smp::MAX_CPUS]);

/// Mark this CPU as inside vcore `v` of cell `idx`, refusing if a peer already is.
///
/// The guard is on the **vcore**, not the cell: two cores inside two vcores of one cell is
/// now the point, while two cores inside the *same* vcore is still the corruption this
/// catches.
///
/// **Both** paths that make a CPU inside a vcore go through here - `run_inner`, which
/// enters one from the scheduler, and `switch_native_vcore`, which hands the CPU from one
/// vcore of a cell to another. One owner for the invariant rather than two copies of the
/// check (docs/ENGINEERING.md 3): the second path arrived with vcores, and a new path is
/// exactly where a guard gets forgotten.
///
/// The **cross-cell** switch is deliberately not a third caller - see the note in
/// `switch_native_cell` for why marking there produces a false positive.
fn enter_vcore(idx: usize, v: usize) {
    let vid = idx * MAX_VCORES + v;
    let me = crate::smp::cpu_index();
    for cpu in 0..crate::smp::MAX_CPUS {
        // SAFETY: a plain read of another CPU's slot.
        if cpu != me && unsafe { *INSIDE.get(cpu) } == vid {
            DOUBLE_ENTRY.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
            panic!("cell {idx} vcore {v} entered by CPU {me} while CPU {cpu} is already inside it");
        }
    }
    // SAFETY: this CPU's own slot.
    unsafe { *INSIDE.this_mut() = vid };
}

/// Cores observed inside one cell at once, and the pair that did it. Recorded rather
/// than only panicked so the report names both.
static DOUBLE_ENTRY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// `(count, cell, cpu)` of the most recent double entry, for a test to assert on.
pub fn double_entries() -> u32 {
    DOUBLE_ENTRY.load(core::sync::atomic::Ordering::Acquire)
}

/// Enter cell `idx` and return when some cell's run on this CPU ends. The reason is in
/// `cells()[EXITED].voutcome[EXITED_VCORE]`: `Some` for an exit or fault, `None` for a
/// hand-off.
fn run_inner(idx: usize, v: usize) {
    let cell = cells()[idx];
    assert!(cell.present, "run of empty cell slot {idx}");
    assert!(
        v < cell.nvcores,
        "run of vcore {v} of cell {idx}, which holds {}",
        cell.nvcores
    );
    enter_vcore(idx, v);
    unsafe {
        *CURRENT.this_mut() = idx;
        *CUR_VCORE.this_mut() = v;
        *TOP_CELL.this_mut() = idx;
        (*cell.aspace).activate();
        // Load this vcore's own FP/SIMD image before its first instruction: the clean
        // ABI-default one `fp_area_init` wrote at install for a fresh cell, or its
        // saved image if a previous `run` left it mid-flight.
        restore_native_fp_vcore(idx, v);
        // The first entry into a cell does not go through either scheduler, so it is
        // the one place a slice has to be armed explicitly (docs/SUBSTRATE.md pillar 3).
        crate::sched::dispatch::running(idx, 0);
        arch::enter_user_first(cell.vframe[v]);
    }
    // enter_user_first returns via return_to_kernel after an exit, a fault, or a
    // hand-off. Restore the kernel address space so setup code can again reach all of
    // RAM (a cell root only maps that cell's user pages).
    arch::paging_activate_kernel();
    // This CPU is now inside no cell, whichever of its batch ended the run.
    // SAFETY: this CPU's own slot.
    unsafe { *INSIDE.this_mut() = NO_CPU };
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
    let cur = cur_cpu_cell();
    let v = current_vcore();
    cells()[cur].voutcome[v] = Some(outcome);
    unsafe {
        *EXITED.this_mut() = cur;
        *EXITED_VCORE.this_mut() = v;
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
    *TOP_CELL.this()
}

/// Whether cell `idx` currently holds a runnable/live cell.
pub fn cell_present(idx: usize) -> bool {
    cells()[idx].present
}

/// Give **every vcore** of cell `idx` to CPU `cpu`: from now on only that core may
/// pick it.
///
/// The claim is the whole of the multi-core safety argument for the native path
/// (docs/SMP.md 10.0). Two cores running one *vcore* would share its trap frame, its
/// kernel stack and its FP save area, none of which is locked - so instead of locking
/// them, a vcore belongs to one core and the scheduler on every *other* core refuses to
/// see the cell ([`cell_on_this_cpu`]).
///
/// For a single-vcore cell - every cell before vcores existed - this is exactly the
/// old one-field store. To give two cores two contexts of one cell, claim each vcore
/// separately with [`claim_vcore`].
pub fn claim_cell(idx: usize, cpu: usize) {
    let n = cells()[idx].nvcores;
    for v in 0..n {
        cells()[idx].vcpu[v] = cpu;
    }
}

/// Give vcore `v` of cell `idx` to CPU `cpu`, leaving its siblings' owners alone.
///
/// This is what lets one cell occupy several cores at once: each vcore is claimed by
/// the core that will run it, and the three things a context cannot share - frame,
/// kernel stack, FP area - are per vcore, so the partitioning discipline holds one
/// level down (docs/SUBSTRATE.md pillar 3).
pub fn claim_vcore(idx: usize, v: usize, cpu: usize) {
    cells()[idx].vcpu[v] = cpu;
}

/// The CPU that owns vcore 0 of cell `idx`, or [`NO_CPU`].
pub fn cell_cpu(idx: usize) -> usize {
    cells()[idx].vcpu[0]
}

/// The CPU that owns vcore `v` of cell `idx`, or [`NO_CPU`].
pub fn vcore_cpu(idx: usize, v: usize) -> usize {
    cells()[idx].vcpu[v]
}

/// How many vcores cell `idx` holds (1 unless a launcher added more).
pub fn cell_vcores(idx: usize) -> usize {
    cells()[idx].nvcores
}

/// Whether the calling CPU may schedule cell `idx`.
///
/// True for a cell this core claimed, and true for an **unclaimed** cell - which is
/// what keeps every single-core boot byte-identical, since nothing there ever calls
/// [`claim_cell`] and the predicate is then constant.
///
/// A cell *created* by a claimed cell (a `fork`, a `SYS_SPAWN`) **inherits its parent's
/// owner**, so it is not visible to any other core. That was the honest limitation
/// recorded here while no boot forked off the boot CPU; the `linuxfork` phase does, and
/// the fix is the one this note predicted - inheriting the owner, not a wider lock.
///
/// The question is about **vcore 0**, because that is the context every cell-level path
/// enters. A multi-vcore cell is answerable here for exactly that reason: entering its
/// vcore 0 is safe whenever this core owns vcore 0, whatever core is inside a sibling.
/// A path that enters a *named* vcore asks [`vcore_on_this_cpu`] instead.
#[inline]
pub fn cell_on_this_cpu(idx: usize) -> bool {
    let owner = cells()[idx].vcpu[0];
    let mine = owner == NO_CPU || owner == crate::smp::cpu_index();
    if !mine {
        // Counted so a test can see the affinity test **refuse**, rather than only
        // observe that nothing went wrong. An absence is weak evidence: the window in
        // which a scheduler could pick another core's cell is narrow, so "no double
        // entry" can mean the check worked or that the race never came up. A nonzero
        // count says the check was consulted and said no (docs/ENGINEERING.md 1).
        AFFINITY_SKIPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    mine
}

/// Times a scheduler was offered a cell belonging to another core and declined it.
static AFFINITY_SKIPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many times [`cell_on_this_cpu`] has refused a cell this core does not own.
pub fn affinity_skips() -> u64 {
    AFFINITY_SKIPS.load(core::sync::atomic::Ordering::Acquire)
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
    // Same as `install`: the child's metadata follows the child's node, set before
    // its tables are funded. Needed on this path too because a slot is reused - a
    // stale entry would place this cell's tables on the previous occupant's node.
    crate::mm::kmeta::set_owner_node(crate::mm::kmeta::Owner::cell(idx), p.node);
    cells()[idx] = RunCell {
        aspace,
        caps: owned_caps(idx),
        // The **object** table is one per system (ARCHITECTURE.md 3) and is
        // shared on purpose: it is the registry objects live in, not an
        // authority. Reaching an object still needs a capability in this cell's
        // own table.
        objects: p.objects,
        vqp: {
            let mut q = [core::ptr::null(); MAX_VCORES];
            q[0] = qp;
            q
        },
        // Vcore 0 is the frame the caller built; a fresh cell holds exactly one vcore,
        // and a launcher that wants more adds them with `install_vcore`.
        vframe: {
            let mut f = [core::ptr::null_mut(); MAX_VCORES];
            f[0] = frame;
            f
        },
        voutcome: [None; MAX_VCORES],
        nvcores: 1,
        present: true,
        personality: Personality::Native,
        vqp_va: {
            let mut a = [0; MAX_VCORES];
            a[0] = qp_va;
            a
        },
        vqp_cap: {
            let mut a = [0; MAX_VCORES];
            a[0] = qp_cap_id;
            a
        },
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
        // **The parent's core, not unclaimed.** A child left `NO_CPU` is visible to every
        // core's scheduler (`cell_on_this_cpu` treats unclaimed as pickable, which is what
        // keeps single-core boots unchanged), so a cell spawned *on a secondary* could be
        // entered by the primary at the same time - two cores in one cell, which is the
        // corruption `user::double_entries` exists to name. Inheriting the owner is the fix
        // this very predicate's doc named; it is not a wider lock, it is the same
        // partitioning applied to a cell that did not exist when the round started.
        vcpu: p.vcpu,
        // The parent's node, not a fresh one: a spawned child shares the parent's
        // capability bundle and usually a channel with it, so they are one working
        // set and splitting them across nodes would cost on every message.
        node: p.node,
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
    // SAFETY: `cell_fp(idx, 0)` is a valid, aligned `FP_AREA_LEN` area.
    unsafe { arch::fp_area_init(cell_fp(idx, 0)) };
}

/// Repoint cell `idx` at a new context-0 frame (after `execve`).
pub fn set_cell_frame(idx: usize, frame: *mut TrapFrame) {
    cells()[idx].vframe[0] = frame;
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
        *CURRENT.this_mut() = idx;
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
    // Same as `install`: the child's metadata follows the child's node, set before
    // its tables are funded. Needed on this path too because a slot is reused - a
    // stale entry would place this cell's tables on the previous occupant's node.
    crate::mm::kmeta::set_owner_node(crate::mm::kmeta::Owner::cell(idx), p.node);
    cells()[idx] = RunCell {
        aspace,
        caps: owned_caps(idx),
        objects: p.objects,
        vqp: p.vqp,
        // Vcore 0 is the frame the caller built; a fresh cell holds exactly one vcore,
        // and a launcher that wants more adds them with `install_vcore`.
        vframe: {
            let mut f = [core::ptr::null_mut(); MAX_VCORES];
            f[0] = frame;
            f
        },
        voutcome: [None; MAX_VCORES],
        nvcores: 1,
        present: true,
        personality: Personality::Linux,
        vqp_va: [0; MAX_VCORES],
        vqp_cap: [0; MAX_VCORES],
        chan: [EMPTY_CHAN; MAX_CELL_CHANNELS],
        // The parent's core - see `install_spawned` for why an unclaimed child is a
        // two-cores-one-cell hazard the moment a fork happens off the boot CPU.
        vcpu: p.vcpu,
        // The parent's node, and here it is not a preference but a fact: `fork` is
        // copy-on-write, so the child starts out mapping the parent's frames, which
        // are already placed. Anything else would be a claim the pages do not
        // support.
        node: p.node,
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
        //
        // Under the **personality lock**, because both arms write personality state that
        // is not this cell's alone: `preempt_cell` picks *another* cell's context and
        // touches its row, and both walk the funded per-cell tables a peer core's syscall
        // may be growing. The syscall path takes the same lock (`linux::handle`), and it
        // is recursive per CPU, so a nested acquire from anything these call is free
        // (docs/SMP.md 10.2a - this site was named there as outside the bracket).
        Personality::Linux => {
            let _g = crate::linux::plock();
            match crate::linux::thread::preempt_context(cur) {
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
            }
        }
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
        let cur = cur_cpu_cell();
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
            // From here to the end of this arm is personality state: the context
            // read, the signal-frame build, and - on the terminate path - the reap
            // that touches the *global* pid and pipe registries. Under the same
            // recursive lock the syscall path holds, for the same reason
            // (docs/SMP.md 10.2a). `fill_fault` above took and released it already;
            // taking it again here rather than widening one bracket over both keeps
            // the demand-paging path, which runs on every fault, out of the lock on
            // the common path where it succeeds.
            let _g = crate::linux::plock();
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
    let cur = cur_cpu_cell();

    // Linux cells never reach native dispatch: the personality tag decides
    // the syscall table before the number means anything (the ABIs collide).
    if cells()[cur].personality == Personality::Linux {
        return linux_ctl(crate::linux::handle(cur, nr, &args, frame), frame);
    }

    match nr {
        SYS_DOORBELL => {
            let cell = cells()[cur];
            // **This vcore's own ring** (docs/SUBSTRATE.md S5): a doorbell drains the ring
            // the caller submitted into, which is the caller's, not the cell's first.
            let qp = cell.vqp[current_vcore()];
            // SAFETY: the pointers were validated at install time. During the
            // trap the cell's address space is active, so a queue mapped at a
            // user VA (a loaded librheo cell) is reachable here.
            let n = unsafe { queue::kernel_process(&*qp, &mut *cell.caps, &*cell.objects) };
            arch::set_syscall_ret(unsafe { &mut *frame }, n as u64);
            frame
        }
        SYS_QUEUE_INFO => {
            let cell = cells()[cur];
            let v = current_vcore();
            let ret = match (cell.vqp_va[v], user_out::<QueueInfo>(arg)) {
                // SAFETY: `out` was checked by `user_out` (non-null, aligned,
                // inside the running cell's user VA range) and the cell's
                // address space is active for the trap.
                (qp_va, Some(out)) if qp_va != 0 => {
                    unsafe {
                        out.write(QueueInfo {
                            qp_va,
                            cap_id: cell.vqp_cap[v] as u64,
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

        // Report which vcore this is and how many the cell holds
        // (docs/SUBSTRATE.md pillar 3). The answer the runtime cannot work out for
        // itself: nothing in userspace says "you are context 1 of your cell", because
        // the kernel is what decided.
        SYS_VCORE_INFO => {
            let ret = match user_out::<crate::abi::VcoreInfo>(arg) {
                // SAFETY: `out` was checked by `user_out` (non-null, aligned, inside the
                // running cell's user VA range), and the cell's address space is active
                // for the trap.
                Some(out) => {
                    unsafe {
                        out.write(crate::abi::VcoreInfo {
                            index: current_vcore() as u64,
                            count: cells()[cur].nvcores as u64,
                        });
                    }
                    0
                }
                None => -(EFAULT as i64),
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
            peer_cell.vframe[0]
        }
        // A spawned native child's exit makes it a zombie and reschedules
        // (docs/LIBRHEO.md Phase F); the top cell's exit unwinds `run`.
        // `SYS_EXIT` ends the calling **vcore**; the cell ends when its last one does
        // (docs/SMP.md 10.0a). `SYS_EXIT_GROUP` ends the cell whatever its siblings are
        // doing - the process/thread split the Linux personality already has. A cell with
        // one vcore has no live sibling either way, so both are the pre-vcore behaviour.
        SYS_EXIT => match crate::nproc::retire_vcore(cur, arg) {
            // This vcore ended and a live sibling took the CPU.
            Some(f) => f,
            // No live sibling: this was the cell's last vcore, so end the cell exactly as
            // before vcores existed.
            None => match crate::nproc::on_exit(cur, arg) {
                Some(f) => f,
                None => finish(Outcome::Exited(arg)),
            },
        },
        SYS_EXIT_GROUP => match crate::nproc::on_exit(cur, arg) {
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
    let cur = cur_cpu_cell();
    let cell = cells()[cur];
    if cell.caps.is_null() {
        0
    } else {
        // SAFETY: caps was validated at install time.
        unsafe { (*cell.caps).live_count() }
    }
}
