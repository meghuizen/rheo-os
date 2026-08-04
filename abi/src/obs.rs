//! The observability plane's on-wire layout, defined once (docs/OBSERVABILITY.md).
//!
//! Everything here is read by three parties that are compiled separately and must
//! agree byte-for-byte: the **kernel**, which writes it; an **in-guest collector
//! cell**, which reads it through a capability; and a **host tool**, which reads it
//! out of guest physical memory with no cooperation from the guest at all. That
//! third reader is why the layout lives in `rheo-abi` rather than in the kernel:
//! `rheo-abi` is `no_std`, zero-dependency and has no lang items, so a host binary
//! links it as an ordinary dependency and there is one definition rather than a
//! parser written twice.
//!
//! # How a reader finds this
//!
//! The kernel exports one page-aligned symbol, `RHEO_OBS_ROOT`, holding an
//! [`ObsRoot`]. A reader with the kernel ELF and the ability to read guest physical
//! memory needs no hardcoded address and nothing ISA-specific:
//!
//! 1. Resolve `RHEO_OBS_ROOT`'s virtual address from the ELF symbol table.
//! 2. Find the `PT_LOAD` segment containing it and compute
//!    `pa = p_paddr + (vma - p_vaddr)`. All three linker scripts carry a real load
//!    address via `AT()`, so `p_paddr` is the truth.
//! 3. Read the page and check [`ObsRoot::magic`] and [`ObsRoot::version`]. A wrong
//!    address gives a wrong magic, so the reader reports "no root" instead of
//!    parsing whatever happened to be there.
//! 4. Check [`ObsRoot::self_pa`] against the address it just read from. A kernel
//!    that relocated, or a stale section table, disagrees here.
//! 5. Every [`ObsSection`] then carries its own physical address, so the rest of the
//!    plane is directly readable. [`ObsRoot::va_base`] is needed only for pointers
//!    stored *inside* published structures, which are kernel virtual addresses in
//!    the high linear map: `pa = va & !va_base`.
//!
//! # Timestamps are raw ticks, not nanoseconds
//!
//! Records carry a raw counter reading, and [`ObsRoot::tick_hz`] plus
//! [`ObsRoot::tick_domain`] say how to convert. This is not a micro-optimisation:
//! `arch::timer_now_ns()` is a 128-bit multiply and a 128-bit divide on all three
//! ISAs - on riscv64 the divide is a call into `__udivti3`, a software loop, and on
//! aarch64 it re-reads `cntfrq_el0` and executes an `isb` every time. Putting that
//! on an event-emit path makes the tracer cost more than the thing it observes,
//! which is the one thing a tracer must not do. Conversion happens at the edge,
//! where there is time and an allocator.
//!
//! A consequence worth stating rather than discovering: QEMU's riscv64 `virt`
//! timebase is 10 MHz, so one tick is 100 ns there and intervals shorter than that
//! are not resolvable. `tick_hz` is published so a reader can decline to print a
//! number below its own resolution instead of inventing one.

use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicU64;

/// `"RHEO_OBS"` as big-endian ASCII. The first thing a reader checks, and the
/// reason a wrong address is reported rather than parsed.
pub const OBS_MAGIC: u64 = 0x5248_454F_5F4F_4253;

/// Layout version. Bumped whenever any structure in this module changes shape.
/// A reader that does not recognise it must refuse, not guess.
pub const OBS_VERSION: u32 = 1;

/// Sections the root can advertise. Fixed because the root is one page and a
/// reader should not have to follow a chain to enumerate the plane.
pub const OBS_MAX_SECTIONS: usize = 32;

// =========================================================================
// Windows - the enable mask, and the event stream's first key
// =========================================================================
//
// A window is both "which subsystem produced this" and "is this being recorded
// at all": bit N of [`ObsRoot::windows`] enables discriminant N. Deliberately
// coarse - a window is only useful if a reader can name it without reading the
// kernel source first, and a taxonomy with fifty entries is a grep by another
// spelling.
//
// Discriminants 0..=5 are `kernel::trace::Subsys`'s original values and must not
// move: the `@E` serial format and `cargo xtask trace`'s parser both depend on
// them, and preserving them is what lets that module become a shim rather than a
// rewrite with a migration.

/// Kernel metadata frames (`mm::kmeta`) - the funded tables.
pub const W_KMETA: u32 = 0;
/// The physical frame allocator (`mm::frames`).
pub const W_FRAMES: u32 = 1;
/// Execution entities: create, claim, park, exit (`sched::entity`).
pub const W_ENTITY: u32 = 2;
/// Cell lifecycle: install, fork, free (`user`).
pub const W_CELL: u32 = 3;
/// Scheduling decisions: dispatch, preempt, yield.
pub const W_SCHED: u32 = 4;
/// The Linux personality's synthesized state.
pub const W_LINUX: u32 = 5;
/// Interrupt arrival and service.
pub const W_IRQ: u32 = 6;
/// Queue-pair submissions, completions and doorbells.
pub const W_QUEUE: u32 = 7;
/// Timer arbiter arms, firings and cancellations.
pub const W_TIMER: u32 = 8;
/// Lock contention. See [`W_LOCK_HOLD`] for why hold time is a separate bit.
pub const W_LOCK: u32 = 9;
/// Network frames in and out, and receive-wait tier changes.
pub const W_NET: u32 = 10;
/// GPU command submission and presentation.
pub const W_GPU: u32 = 11;
/// Address-space events: fault, map, unmap, copy-on-write.
pub const W_MEM: u32 = 12;
/// Syscall entry and exit - the `strace` window.
pub const W_SYSCALL: u32 = 13;

/// How many windows are defined.
pub const OBS_WINDOWS: usize = 14;

/// Lock **hold** time sampling, as a bit of its own rather than part of
/// [`W_LOCK`].
///
/// They are separate because measuring how long a lock is held means reading the
/// clock inside the critical section, which lengthens the very region being
/// measured. With only [`W_LOCK`] on, contention counts and wait times are
/// recorded and the held region runs at its uninstrumented cost, so the two runs
/// can be compared - which is the only thing that makes the hold-time number
/// worth having when it is turned on.
pub const W_LOCK_HOLD: u32 = 14;

/// The **snapshot plane's writers** ([`ObsCpu`]), as a modifier bit like
/// [`W_LOCK_HOLD`] rather than a window: it keys no events and appears in no
/// record - it says whether the kernel is stamping its per-CPU live state
/// (the seqlock'd group, busy/idle time) at the transitions it passes through.
/// A bit of its own because the snapshot writers sit on the context-switch and
/// idle paths, where the disabled cost must stay one load and a not-taken
/// branch whatever event windows are on.
pub const W_SNAPSHOT: u32 = 15;

/// Every bit that is a window (not a modifier like [`W_LOCK_HOLD`]).
pub const WINDOW_MASK_ALL: u32 = (1u32 << OBS_WINDOWS) - 1;

/// Every bit the mask accepts: the windows plus the two modifiers. A set bit
/// outside this is meaningless and is dropped at the enable boundary rather
/// than stored, so a reader never sees a mask it cannot decode.
pub const MASK_VALID: u32 = WINDOW_MASK_ALL | (1u32 << W_LOCK_HOLD) | (1u32 << W_SNAPSHOT);

/// The bit for window `w`.
#[inline(always)]
pub const fn window_bit(w: u32) -> u32 {
    1u32 << w
}

// =========================================================================
// Event kinds
// =========================================================================
//
// Subsystem-local, so `Acquire` under `W_KMETA` and under `W_FRAMES` are
// different events and read as such. Acquire/Release are a pair on purpose: a
// host-side ledger balances them per owner, and an unmatched acquire *is* the
// leak - a line in the ledger rather than a total that failed to return to zero.

/// A resource was taken. `a` = units, `b` = subsystem detail.
pub const K_ACQUIRE: u8 = 0;
/// A resource was given back. Same fields.
pub const K_RELEASE: u8 = 1;
/// A charge moved between owners. `a` = units, `b` = the owner it came from.
pub const K_TRANSFER: u8 = 2;
/// A request was refused. `a` = units wanted.
pub const K_REFUSE: u8 = 3;
/// A state change with no resource attached.
pub const K_NOTE: u8 = 4;
/// Entry into something (a syscall, a critical section). Paired with [`K_EXIT`].
pub const K_ENTER: u8 = 5;
/// Exit from something. `a` = the result.
pub const K_EXIT: u8 = 6;

/// The owner tag for the kernel itself, matching `mm::kmeta::Owner::KERNEL`.
pub const OWNER_KERNEL: u16 = u16::MAX;

// =========================================================================
// The event record
// =========================================================================

/// One traced event: four integers and three small fields, no formatting.
///
/// **Exactly 32 bytes, and the size is the design.** With a page-aligned backing
/// frame, a 32-byte record never straddles a 64-byte cache line, so an emit
/// dirties one line; at 40 bytes - the size this replaced - records straddle
/// roughly 40% of the time and an emit dirties two. The alignment attribute makes
/// that a property of the type rather than of how a caller happens to place it.
///
/// The cost of 32 bytes is that there is no per-record CPU field. That witness
/// moves up to the ring: [`ObsRingHdr::cpu`] is written when the ring is funded
/// and checked by the dumper, which works because a ring is selected by the
/// CPU's own index and by nothing else - unlike `kernel::telemetry`, where the
/// CPU is a *parameter* to the push and so can disagree with reality.
#[repr(C, align(32))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ObsEvent {
    /// Raw counter reading. [`ObsRoot::tick_hz`] converts; see the module note on
    /// why this is not nanoseconds.
    pub tick: u64,
    pub a: u64,
    pub b: u64,
    /// Per-CPU monotone sequence number. A **gap is loss, located** - the reader
    /// knows exactly which events it missed, which a drop counter cannot say.
    pub seq: u32,
    /// Cell index, or [`OWNER_KERNEL`].
    pub owner: u16,
    /// One of the `W_*` window discriminants.
    pub window: u8,
    /// One of the `K_*` kinds.
    pub kind: u8,
}

const _: () = assert!(core::mem::size_of::<ObsEvent>() == 32);
const _: () = assert!(core::mem::align_of::<ObsEvent>() == 32);

// =========================================================================
// Per-CPU event ring header
// =========================================================================

/// The published state of one CPU's event ring.
///
/// One per CPU, in a plain array so a reader takes the whole set in one read. The
/// events are **one physically contiguous block** of `capacity` records at
/// `base_pa`, so a reader's whole job is a linear read - no directory to walk, no
/// page arithmetic to reproduce. Contiguity is also what the *writer's* hot path is
/// built on: `base + slot * 32` with nothing between the slot arithmetic and the
/// store (docs/OBSERVABILITY.md 11.4).
#[repr(C, align(64))]
pub struct ObsRingHdr {
    /// Free-running count of events *written*. The slot for event `n` is
    /// `n & (capacity - 1)`.
    ///
    /// **There is deliberately no `dropped` counter beside it.** Loss is a
    /// property of a reader's cursor, not of the ring: a reader at cursor `c` has
    /// lost `head - capacity - c` events when that is positive, and the missing
    /// range is exactly `[c, head - capacity)`. That locates the loss instead of
    /// counting it, and it removes an atomic read-modify-write from the emit path.
    pub head: AtomicU64,
    /// Emits offered while this CPU held no frames.
    ///
    /// Exists so "the window was on but this CPU never got memory" is
    /// distinguishable from "nothing happened" - the same lesson
    /// `kernel::telemetry`'s bypass counter records. A zero that could mean either
    /// is a zero nobody can act on.
    pub unfunded: AtomicU64,
    /// Events this ring holds, a power of two. Zero means never funded.
    pub capacity: u32,
    /// Which CPU owns this ring. Written at fund time; the dumper checks it
    /// against the slot index, which is the ring-granularity replacement for a
    /// per-record CPU field.
    pub cpu: u32,
    /// Kernel VA of the event block.
    pub base_va: u64,
    /// Physical address of the same block, so a host reader needs no translation.
    pub base_pa: u64,
    pub _rsv: [u64; 3],
}

const _: () = assert!(core::mem::size_of::<ObsRingHdr>() == 64);

impl ObsRingHdr {
    /// An unfunded header.
    ///
    /// A `const fn` rather than a `const` value because this type contains atomics:
    /// a named constant holding one would be *copied* at each use, so two readers of
    /// `EMPTY` would get two unrelated counters. The kernel builds its per-CPU array
    /// as `PerCpu::from_array([const { ObsRingHdr::new() }; MAX_CPUS])`, which is the
    /// same shape `mm::kmeta::Funded` and `metrics::Histogram` already use.
    pub const fn new() -> ObsRingHdr {
        ObsRingHdr {
            head: AtomicU64::new(0),
            unfunded: AtomicU64::new(0),
            capacity: 0,
            cpu: 0,
            base_va: 0,
            base_pa: 0,
            _rsv: [0; 3],
        }
    }
}

impl Default for ObsRingHdr {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Per-CPU live state and counters
// =========================================================================

/// What a CPU is doing right now, plus its monotone counters.
///
/// This is the plane an htop-style view renders, and it is the only one of the
/// five that answers "now" rather than "what happened". It is written by the
/// owning CPU at transitions it already passes through - there is no sampler, no
/// timer and no inter-processor interrupt anywhere in the design.
///
/// # Why only the first line is under the seqlock
///
/// `(state, cur_cell, cur_entity, cur_vcore, since_tick)` is one fact. A reader
/// that catches half of an update sees a cell that never ran an entity that never
/// existed - not a stale reading but a false one, which for a live display is the
/// difference between showing something and lying. So that group is published
/// under [`ObsCpu::seq`], once per context switch.
///
/// The counters are **outside** it, and that is a decision rather than an
/// oversight: each counter is independently meaningful, so a reader that sees
/// `irqs` from one instant and `switches` from the next has two true facts, not
/// one false one. Bringing them inside would force a release fence on every
/// interrupt, on a path that today does a single increment.
///
/// Counters are monotone and the kernel never derives a rate from them. Rates are
/// the reader's job, computed from two samples and their ticks - so the kernel
/// keeps no moving averages, runs no timer to maintain them, and never divides.
#[repr(C, align(64))]
pub struct ObsCpu {
    // ---- line 0: the seqlock'd coupled group ----
    /// Even means stable, odd means a writer is inside.
    pub seq: AtomicU32,
    /// One of the `OBS_CPU_*` states.
    pub state: u32,
    pub cur_cell: u32,
    pub cur_entity: u32,
    pub cur_vcore: u32,
    /// Runnable entities this CPU can pick from.
    pub runq_depth: u32,
    /// When the current state began.
    pub since_tick: u64,
    /// The nearest outstanding deadline in this CPU's timer arbiter, or 0 for
    /// none. **Monotonic nanoseconds in the arbiter's own domain**
    /// (`ktimer::now_ns`), not a tick: that is the domain the arbiter already
    /// holds the value in, and converting per re-arm would put a multiply on the
    /// pacer's continuous re-arm path to make the field prettier.
    pub timer_deadline_ns: u64,
    /// Deadlines outstanding in this CPU's timing wheel.
    pub wheel_occupancy: u32,
    /// The `LockId` currently held, or 0.
    pub lock_held: u32,
    /// The receive-wait escalation tier (`net_rx`).
    pub net_tier: u32,
    /// GPU driver state, or 0 where this CPU drives none.
    pub gpu_state: u32,
    pub _rsv: u64,

    // ---- lines 1..7: monotone counters, no seqlock ----
    pub counters: [u64; OBS_COUNTERS],
}

const _: () = assert!(core::mem::size_of::<ObsCpu>() == 512);

/// Counter slots per CPU. Sized so [`ObsCpu`] is exactly 512 bytes - eight cache
/// lines, and a granularity at which a reader sampling CPU 3 never touches the
/// lines CPU 4 is writing.
///
/// Which slot means what is *runtime* data, published as [`OBS_SEC_NAMES`], not a
/// compile-time contract: a counter can be named without breaking a reader built
/// against an older header, and an unnamed slot reads as unnamed rather than as
/// somebody else's number.
pub const OBS_COUNTERS: usize = 56;

/// The CPU is not online.
pub const OBS_CPU_OFFLINE: u32 = 0;
/// Online with nothing to run.
pub const OBS_CPU_IDLE: u32 = 1;
/// Executing kernel code.
pub const OBS_CPU_KERNEL: u32 = 2;
/// Executing an unprivileged cell.
pub const OBS_CPU_USER: u32 = 3;
/// Halted waiting for a wake source.
pub const OBS_CPU_PARKED: u32 = 4;

impl ObsCpu {
    /// An offline CPU with every counter at zero.
    ///
    /// A `const fn` rather than a `const` value for the reason [`ObsRingHdr::new`]
    /// gives: this type contains an atomic, and a named constant holding one is
    /// copied at each use.
    pub const fn new() -> ObsCpu {
        ObsCpu {
            seq: AtomicU32::new(0),
            state: OBS_CPU_OFFLINE,
            cur_cell: 0,
            cur_entity: 0,
            cur_vcore: 0,
            runq_depth: 0,
            since_tick: 0,
            timer_deadline_ns: 0,
            wheel_occupancy: 0,
            lock_held: 0,
            net_tier: 0,
            gpu_state: 0,
            _rsv: 0,
            counters: [0; OBS_COUNTERS],
        }
    }
}

impl Default for ObsCpu {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Machine-wide memory status
// =========================================================================

/// The machine's memory numbers, filled **on request** rather than maintained:
/// every field is already live in its own subsystem (the frame pool's counters,
/// the pmem pool's, the kmeta ledger, the demand-paging witnesses), and stamping
/// a copy here from `frames::alloc` would put a store on the hottest allocation
/// path to keep a mirror warm. So [`ObsMem::refreshed_tick`] says when the copy
/// was taken, and a reader judges staleness instead of being lied to about it.
/// The same seqlock protocol as [`ObsCpu`] guards the group.
#[repr(C, align(64))]
pub struct ObsMem {
    /// Even means stable, odd means a refresh is inside.
    pub seq: AtomicU32,
    pub _pad: u32,
    /// The `obs_tick()` at which these numbers were copied from the live
    /// subsystems. 0 = never refreshed, and every other field is meaningless.
    pub refreshed_tick: u64,
    /// DDR frame pool: frames free / frames total.
    pub ddr_free: u64,
    pub ddr_total: u64,
    /// The separate persistent-memory pool, 0/0 where no nvdimm exists.
    pub pmem_free: u64,
    pub pmem_total: u64,
    /// Frame allocations that fell back off their preferred NUMA node.
    pub numa_fallbacks: u64,
    /// Demand paging: ELF pages recorded for lazy fill vs copied eagerly.
    pub recorded_pages: u64,
    pub eager_pages: u64,
    /// Block-cache fills - bytes genuinely read off a disk on demand.
    pub block_cache_fills: u64,
    /// Frames the kernel's own funded tables hold, charged to `Owner::KERNEL`.
    pub kmeta_kernel_frames: u64,
    pub _rsv: [u64; 5],
}

const _: () = assert!(core::mem::size_of::<ObsMem>() == 128);

impl ObsMem {
    /// Never refreshed; every field zero. A `const fn` for the reason
    /// [`ObsRingHdr::new`] gives (the atomic must not be copied from a `const`).
    pub const fn new() -> ObsMem {
        ObsMem {
            seq: AtomicU32::new(0),
            _pad: 0,
            refreshed_tick: 0,
            ddr_free: 0,
            ddr_total: 0,
            pmem_free: 0,
            pmem_total: 0,
            numa_fallbacks: 0,
            recorded_pages: 0,
            eager_pages: 0,
            block_cache_fills: 0,
            kmeta_kernel_frames: 0,
            _rsv: [0; 5],
        }
    }
}

impl Default for ObsMem {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Device status: network / storage / gpu (docs/OBSERVABILITY.md 11, S5)
// =========================================================================
//
// All three follow [`ObsMem`]'s discipline: filled **on request**, stamped with
// when, never maintained by the drivers themselves - a mirror kept warm on the
// send/receive/submit hot paths would tax exactly what it watches. The counts
// they copy are live in the drivers or the counter plane already.

/// The NIC pane: frames and bytes each way, the receive wait's escalation
/// counters, and whether the machine parks or polls - stated, not inferred.
#[repr(C, align(64))]
pub struct ObsNet {
    /// Even means stable, odd means a refresh is inside (the [`ObsCpu`] seqlock).
    pub seq: AtomicU32,
    pub _pad: u32,
    /// `obs_tick()` at the copy; 0 = never refreshed, other fields meaningless.
    pub refreshed_tick: u64,
    /// 1 = a NIC is installed (its MAC is readable); 0 = no NIC.
    pub present: u64,
    /// Frames and bytes the driver received / transmitted.
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    /// NIC receive interrupts genuinely taken.
    pub rx_irqs: u64,
    /// The adaptive receive wait's counters (hot spins, timer slices, halts,
    /// tier escalations - docs/NETSTACK.md 16).
    pub spin_polls: u64,
    pub timer_slices: u64,
    pub halts: u64,
    pub escalations: u64,
    /// The most recent wait's `IdleMode` discriminant (0 none, 1 NIC interrupt,
    /// 2 timer-backed idle, 3 poll).
    pub idle_mode: u32,
    /// Whether the NIC RX interrupt is wired on this ISA - deliberately narrow,
    /// never widened by the timer-backed mode.
    pub interrupt_driven: u32,
    pub _rsv: [u64; 3],
}

const _: () = assert!(core::mem::size_of::<ObsNet>() == 128);

impl ObsNet {
    pub const fn new() -> ObsNet {
        ObsNet {
            seq: AtomicU32::new(0),
            _pad: 0,
            refreshed_tick: 0,
            present: 0,
            rx_frames: 0,
            rx_bytes: 0,
            tx_frames: 0,
            tx_bytes: 0,
            rx_irqs: 0,
            spin_polls: 0,
            timer_slices: 0,
            halts: 0,
            escalations: 0,
            idle_mode: 0,
            interrupt_driven: 0,
            _rsv: [0; 3],
        }
    }
}

impl Default for ObsNet {
    fn default() -> Self {
        Self::new()
    }
}

/// The storage pane: the block cache's demand reads and the NVMe driver's own
/// honesty counters (per-core queue discipline, interrupt-vs-poll, reordering).
#[repr(C, align(64))]
pub struct ObsStorage {
    /// Seqlock, as [`ObsNet`].
    pub seq: AtomicU32,
    pub _pad: u32,
    /// `obs_tick()` at the copy; 0 = never refreshed.
    pub refreshed_tick: u64,
    /// Block-cache line fills - device reads performed on demand, any cache.
    pub cache_fills: u64,
    /// NVMe: completion interrupts taken, waits that genuinely halted, waits
    /// that fell back to polling, submissions across every queue, submissions
    /// that crossed cores (0 is the per-core-queue assertion), the deepest
    /// in-flight batch, completions that arrived out of submission order.
    pub nvme_irqs: u64,
    pub nvme_irq_parks: u64,
    pub nvme_poll_fallbacks: u64,
    pub nvme_submits: u64,
    pub nvme_cross_core_submits: u64,
    pub nvme_max_inflight: u64,
    pub nvme_out_of_order: u64,
    pub _rsv: [u64; 6],
}

const _: () = assert!(core::mem::size_of::<ObsStorage>() == 128);

impl ObsStorage {
    pub const fn new() -> ObsStorage {
        ObsStorage {
            seq: AtomicU32::new(0),
            _pad: 0,
            refreshed_tick: 0,
            cache_fills: 0,
            nvme_irqs: 0,
            nvme_irq_parks: 0,
            nvme_poll_fallbacks: 0,
            nvme_submits: 0,
            nvme_cross_core_submits: 0,
            nvme_max_inflight: 0,
            nvme_out_of_order: 0,
            _rsv: [0; 6],
        }
    }
}

impl Default for ObsStorage {
    fn default() -> Self {
        Self::new()
    }
}

/// The GPU pane. **Engine-busy utilisation is reported absent, not estimated**:
/// no device QEMU models exposes a busy signal and there is no vendor driver
/// here to ask, so [`ObsGpu::util_plus_one`] is 0 (unavailable) rather than a
/// number a reader would trust (observe-never-infer, docs/ENGINEERING.md 1).
#[repr(C, align(64))]
pub struct ObsGpu {
    /// Seqlock, as [`ObsNet`].
    pub seq: AtomicU32,
    pub _pad: u32,
    /// `obs_tick()` at the copy; 0 = never refreshed.
    pub refreshed_tick: u64,
    /// GPU-class devices the machine inventory enumerated.
    pub devices: u64,
    /// virtio-gpu 2D presents completed, and the bytes those presents copied
    /// into the device resource.
    pub presents: u64,
    pub present_bytes: u64,
    /// Attach-measured transport throughput (ticks per KiB streamed through the
    /// aperture, `hw::gpu_attach_measure`) - 0 = never measured, since the
    /// measurement is opt-in.
    pub attach_ticks_per_kib: u64,
    /// 0 = utilisation unavailable (the honest value everywhere today);
    /// a future vendor driver reports 1 + percent.
    pub util_plus_one: u64,
    pub _rsv: [u64; 9],
}

const _: () = assert!(core::mem::size_of::<ObsGpu>() == 128);

impl ObsGpu {
    pub const fn new() -> ObsGpu {
        ObsGpu {
            seq: AtomicU32::new(0),
            _pad: 0,
            refreshed_tick: 0,
            devices: 0,
            presents: 0,
            present_bytes: 0,
            attach_ticks_per_kib: 0,
            util_plus_one: 0,
            _rsv: [0; 9],
        }
    }
}

impl Default for ObsGpu {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// Names
// =========================================================================

/// A human-readable name for a numbered thing, published so a reader does not
/// carry a table that can drift from the kernel's.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ObsName {
    /// The number being named: a counter slot, a window, a metric, a lock.
    pub id: u32,
    /// One of the `OBS_NAME_*` kinds.
    pub kind: u32,
    /// NUL-padded ASCII. Not NUL-*terminated* when the name is exactly 24 bytes.
    pub text: [u8; 24],
}

const _: () = assert!(core::mem::size_of::<ObsName>() == 32);

pub const OBS_NAME_COUNTER: u32 = 1;
pub const OBS_NAME_WINDOW: u32 = 2;
pub const OBS_NAME_METRIC: u32 = 3;
pub const OBS_NAME_LOCK: u32 = 4;
pub const OBS_NAME_KIND: u32 = 5;

impl ObsName {
    pub const EMPTY: ObsName = ObsName {
        id: 0,
        kind: 0,
        text: [0; 24],
    };

    /// Build a name from a `&str`, truncating at 24 bytes. `const` so the name
    /// table is a `static` with no initialisation phase to forget.
    pub const fn new(kind: u32, id: u32, s: &str) -> ObsName {
        let b = s.as_bytes();
        let mut text = [0u8; 24];
        let mut i = 0;
        while i < b.len() && i < 24 {
            text[i] = b[i];
            i += 1;
        }
        ObsName { id, kind, text }
    }
}

// =========================================================================
// Sections
// =========================================================================

/// One published region of the telemetry plane.
///
/// Both `va` and `pa` are carried because the two readers need different ones and
/// neither should have to derive the other: an in-guest collector is handed
/// mappings, while a host reader walks physical memory and has no page tables to
/// consult.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ObsSection {
    /// One of the `OBS_SEC_*` kinds.
    pub kind: u32,
    /// Discriminator within a kind - a CPU index for per-CPU sections, else 0.
    pub id: u32,
    /// Kernel virtual address, or 0 if the region is not present.
    pub va: u64,
    /// Guest physical address, or 0 if not present.
    pub pa: u64,
    /// Length in bytes.
    pub len: u64,
    /// Bytes per element, so a reader can stride without knowing the Rust type.
    pub stride: u32,
    /// Elements.
    pub count: u32,
}

const _: () = assert!(core::mem::size_of::<ObsSection>() == 40);

impl ObsSection {
    pub const EMPTY: ObsSection = ObsSection {
        kind: 0,
        id: 0,
        va: 0,
        pa: 0,
        len: 0,
        stride: 0,
        count: 0,
    };
}

/// The per-CPU event rings, one section for the whole array, `count` = CPUs and
/// `stride` = the kernel's per-CPU ring struct.
///
/// Each element **begins** with an [`ObsRingHdr`], so a reader strides by `stride`
/// and reads a header at each step without knowing what the kernel keeps after it.
/// The events are reached from that header's `base_pa` - one contiguous block per
/// ring - rather than being published as sections of their own, which is not just
/// fewer entries: a ring is funded by its own CPU when that CPU is asked to record,
/// so per-ring sections would have cores racing to append to one section table, a
/// hazard with nothing to gain. The header is written by its owning CPU, which is
/// the only writer it can have.
pub const OBS_SEC_RINGS: u32 = 1;
/// The `[ObsCpu; n]` array. One section, `count` = CPUs.
pub const OBS_SEC_CPU: u32 = 3;
/// The `[ObsName; n]` table in `.rodata`.
pub const OBS_SEC_NAMES: u32 = 4;
/// The per-CPU latency histogram sets (`kernel::metrics`).
pub const OBS_SEC_HISTOGRAMS: u32 = 5;
/// The per-CPU text log rings (`kernel::telemetry`).
pub const OBS_SEC_TEXT_RINGS: u32 = 6;
/// The machine-wide [`ObsMem`] block. One element.
pub const OBS_SEC_MEM: u32 = 8;
/// The NIC status pane: one [`ObsNet`].
pub const OBS_SEC_NET: u32 = 9;
/// The storage status pane: one [`ObsStorage`].
pub const OBS_SEC_STORAGE: u32 = 10;
/// The GPU status pane: one [`ObsGpu`].
pub const OBS_SEC_GPU: u32 = 11;
/// A layout witness carrying no data: `stride` = `size_of::<ObsEvent>()`.
///
/// A reader built against a different `rheo-abi` than the kernel would otherwise
/// stride the event frames by the wrong amount and decode plausible nonsense.
/// This makes that a refusal instead.
pub const OBS_SEC_EVENT_LAYOUT: u32 = 7;

// =========================================================================
// The root
// =========================================================================

/// Tick domains, so a reader knows which counter it is looking at rather than
/// assuming every machine has one clock.
pub const OBS_TICK_NONE: u32 = 0;
/// x86-64 `rdtsc`.
pub const OBS_TICK_TSC: u32 = 1;
/// aarch64 `cntvct_el0`.
pub const OBS_TICK_CNTVCT: u32 = 2;
/// riscv64 `rdtime` - the same domain the timer arbiter's deadlines live in, and
/// deliberately not the `cycle` CSR, which is a different counter.
pub const OBS_TICK_RDTIME: u32 = 3;

pub const OBS_ARCH_RISCV64: u32 = 0;
pub const OBS_ARCH_AARCH64: u32 = 1;
pub const OBS_ARCH_X86_64: u32 = 2;

/// The table of contents for everything above.
///
/// Page-aligned for two reasons that both matter: a host reader's physical
/// address arithmetic stays exact, and an in-guest collector can be handed
/// exactly this one page read-only rather than whatever else shares its
/// neighbourhood.
///
/// [`ObsRoot::magic`], [`ObsRoot::version`], [`ObsRoot::abi_hash`],
/// [`ObsRoot::va_base`] and [`ObsRoot::arch`] are filled at compile time, so they
/// are in the kernel image and a reader can validate the file before the guest
/// has executed a single instruction. Everything else is filled at boot.
#[repr(C, align(4096))]
pub struct ObsRoot {
    /// [`OBS_MAGIC`].
    pub magic: u64,
    /// [`OBS_VERSION`].
    pub version: u32,
    /// Byte offset of `sections[0]`, so a reader can find the section table
    /// without recomputing this struct's field offsets.
    pub header_len: u32,
    /// A compile-time hash of every layout constant in this module.
    ///
    /// Not a build id - the kernel has no build-id mechanism and inventing a
    /// constant to sit in that field would be a field that lies. This is the thing
    /// that can honestly be computed here and is what a reader actually needs:
    /// two sides that disagree about any structure's size disagree about this
    /// number and can say so.
    pub abi_hash: u64,
    /// The kernel's high-linear-map base. A kernel VA found inside a published
    /// structure becomes a physical address as `va & !va_base`.
    pub va_base: u64,
    /// This structure's own physical address, written at publish time. A reader
    /// that computed a different address from the ELF knows immediately.
    pub self_pa: u64,
    /// One of the `OBS_ARCH_*` values.
    pub arch: u32,
    /// Slots in the per-CPU arrays.
    pub max_cpus: u32,
    /// CPUs actually brought up.
    pub online_cpus: u32,
    /// Entries used in `sections`.
    pub section_count: u32,
    /// The **live** enable mask - bit N enables window N, plus [`W_LOCK_HOLD`].
    ///
    /// It lives here rather than in a private kernel static so that there is one
    /// copy: a reader sees exactly what is being recorded, and cannot be told one
    /// thing by a mirror while the kernel consults another.
    pub windows: AtomicU32,
    /// One of the `OBS_TICK_*` domains.
    pub tick_domain: u32,
    /// Ticks per second, for converting [`ObsEvent::tick`].
    pub tick_hz: u64,
    /// The tick at which the plane was published, as an origin.
    pub boot_tick: u64,
    pub sections: [ObsSection; OBS_MAX_SECTIONS],
}

/// Byte offset of `ObsRoot::sections`, asserted below against the real layout so
/// the published `header_len` cannot drift from the struct.
pub const OBS_HEADER_LEN: u32 = 80;

const _: () = assert!(core::mem::size_of::<ObsRoot>() == 4096);
const _: () = assert!(core::mem::align_of::<ObsRoot>() == 4096);
const _: () = assert!(core::mem::offset_of!(ObsRoot, sections) == OBS_HEADER_LEN as usize);

/// The compile-time layout hash published as [`ObsRoot::abi_hash`].
///
/// FNV-1a over the sizes and counts that a reader must agree on. Computed rather
/// than written down, so adding a field to any structure changes it without
/// anyone having to remember to bump a constant.
pub const OBS_ABI_HASH: u64 = {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let vals: [u64; 13] = [
        OBS_VERSION as u64,
        core::mem::size_of::<ObsEvent>() as u64,
        core::mem::size_of::<ObsRingHdr>() as u64,
        core::mem::size_of::<ObsCpu>() as u64,
        core::mem::size_of::<ObsSection>() as u64,
        core::mem::size_of::<ObsName>() as u64,
        core::mem::size_of::<ObsRoot>() as u64,
        core::mem::size_of::<ObsMem>() as u64,
        core::mem::size_of::<ObsNet>() as u64,
        core::mem::size_of::<ObsStorage>() as u64,
        core::mem::size_of::<ObsGpu>() as u64,
        OBS_COUNTERS as u64,
        OBS_WINDOWS as u64,
    ];
    let mut i = 0;
    while i < vals.len() {
        let mut b = 0;
        while b < 8 {
            h ^= (vals[i] >> (b * 8)) & 0xff;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
            b += 1;
        }
        i += 1;
    }
    h
};

impl ObsRoot {
    /// The compile-time image: everything a reader can check before the guest
    /// runs. `va_base` and `arch` are supplied by the kernel because this crate is
    /// ISA-neutral and must not guess either.
    pub const fn new(arch: u32, va_base: u64) -> ObsRoot {
        ObsRoot {
            magic: OBS_MAGIC,
            version: OBS_VERSION,
            header_len: OBS_HEADER_LEN,
            abi_hash: OBS_ABI_HASH,
            va_base,
            self_pa: 0,
            arch,
            max_cpus: 0,
            online_cpus: 0,
            section_count: 0,
            windows: AtomicU32::new(0),
            tick_domain: OBS_TICK_NONE,
            tick_hz: 0,
            boot_tick: 0,
            sections: [ObsSection::EMPTY; OBS_MAX_SECTIONS],
        }
    }

    /// Whether this looks like a real root. The check a reader performs first, and
    /// the reason a wrong address produces "no root" instead of decoded garbage.
    pub fn looks_valid(&self) -> bool {
        self.magic == OBS_MAGIC
            && self.version == OBS_VERSION
            && self.abi_hash == OBS_ABI_HASH
            && self.header_len == OBS_HEADER_LEN
    }

    /// The published sections.
    pub fn published(&self) -> &[ObsSection] {
        let n = (self.section_count as usize).min(OBS_MAX_SECTIONS);
        &self.sections[..n]
    }

    /// The first section of `kind` with the given `id`.
    pub fn section(&self, kind: u32, id: u32) -> Option<&ObsSection> {
        self.published()
            .iter()
            .find(|s| s.kind == kind && s.id == id)
    }
}
