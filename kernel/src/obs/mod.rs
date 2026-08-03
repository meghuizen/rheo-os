//! **Observability**: the spine that indexes what the kernel knows about itself
//! (docs/OBSERVABILITY.md).
//!
//! # What this is, and what it is not
//!
//! Five planes answer five different questions, and each one already has - or
//! gets - the cheapest structure that answers it:
//!
//! | Plane | Question | Where it lives |
//! |---|---|---|
//! | Text | what did it say | [`crate::telemetry`] |
//! | Event | what happened, in order | this module ([`ring`]) |
//! | Distribution | how long did it take | [`crate::metrics`] |
//! | Counter | how many | this module (not built yet) |
//! | Snapshot | what is it doing **now** | this module (not built yet) |
//!
//! Collapsing them is what makes observability expensive: a histogram stored as
//! events costs a record per sample, and a live gauge stored as a log line costs a
//! parse per read. So this module does not absorb the two that already exist - it
//! *indexes* them, by publishing their addresses in one root a reader can find.
//!
//! # Why the root, rather than a syscall
//!
//! The most useful moments to inspect a kernel are the ones where it is least able
//! to answer a question: wedged, faulting, or halfway through bringing a core up.
//! A plane that is plain memory behind one exported symbol is readable in all of
//! them - by a host debugger, by a hypervisor, out of a crash dump - and readable
//! with **zero** guest instructions, so watching does not perturb what is being
//! watched. A syscall could do none of that. The syscall surface exists too, for a
//! collector cell that has no other way in, but it is the second reader, not the
//! first.
//!
//! # Cost
//!
//! Nothing here runs until a boot turns recording on. The published root is one
//! page of `.data`; the rings take frames only from a CPU that has been told to
//! fund one. That is not a politeness - it is what lets the existing kernels stay
//! byte-for-byte what they were, which is the only way a framework this size lands
//! without invalidating every proof already in the tree.

pub mod cpu;
pub mod dump;
pub mod lock;
pub mod ring;
pub mod root;

/// Record one event, evaluating its arguments **only if** the window is on.
///
/// The macro rather than a function call, and the reason is measured rather than
/// stylistic. Rust evaluates a call's arguments before the call, so
/// `obs::emit(W, K, owner, expensive(), other())` pays for `expensive()` even when the
/// window is off. The first version of the queue window did precisely that - one shift,
/// one or, one truncation - and `cargo xtask bench` reported **+9 instructions per
/// queue round trip** while nothing was being recorded, on a path whose whole disabled
/// cost is supposed to be a load and a branch. The plan had named that as a control to
/// run; writing it as a function call committed the defect instead.
///
/// Expanding to `if on(w) { emit_packed(...) }` puts the mask test outside the argument
/// expressions, and the measured delta went back to **zero**.
/// Expanding to `if on(w) { emit_packed(pack_meta(...), a, b) }` also lets the
/// window/kind constants **fold**: `pack_meta` is `const fn`, so a site with a
/// constant owner passes the whole metadata word as one immediate - the ftrace
/// `TRACE_EVENT` trick of assembling the header word at compile time.
#[macro_export]
macro_rules! obs_event {
    ($w:expr, $k:expr, $owner:expr, $a:expr, $b:expr) => {
        if $crate::obs::on($w) {
            $crate::obs::emit_packed(
                $crate::obs::ring::pack_meta($w as u8, $k as u8, $owner),
                $a,
                $b,
            );
        }
    };
}

pub use dump::dump;
pub use root::{publish, refresh_online, root_va};

use ring::ObsRing;

/// Which subsystem produced an event - the **window key**, and from
/// [`crate::abi::obs`] so that a reader outside the guest names the same numbers.
///
/// Deliberately coarse: a window is only useful if a reader can name it without
/// reading the kernel source first, and a taxonomy with fifty entries is a grep by
/// another spelling.
///
/// Discriminants 0..=5 are [`crate::trace::Subsys`]'s original values and must not
/// move - the `@E` serial format and `cargo xtask trace` both depend on them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Window {
    Kmeta = 0,
    Frames = 1,
    Entity = 2,
    Cell = 3,
    Sched = 4,
    Linux = 5,
    Irq = 6,
    Queue = 7,
    Timer = 8,
    Lock = 9,
    Net = 10,
    Gpu = 11,
    Mem = 12,
    Syscall = 13,
}

impl Window {
    /// Every window, for a reader that enumerates them.
    pub const ALL: [Window; 14] = [
        Window::Kmeta,
        Window::Frames,
        Window::Entity,
        Window::Cell,
        Window::Sched,
        Window::Linux,
        Window::Irq,
        Window::Queue,
        Window::Timer,
        Window::Lock,
        Window::Net,
        Window::Gpu,
        Window::Mem,
        Window::Syscall,
    ];

    /// The short name the `@E` line carries. Stable: `cargo xtask trace` groups on
    /// it, so these strings are part of the format.
    pub fn name(self) -> &'static str {
        match self {
            Window::Kmeta => "kmeta",
            Window::Frames => "frames",
            Window::Entity => "entity",
            Window::Cell => "cell",
            Window::Sched => "sched",
            Window::Linux => "linux",
            Window::Irq => "irq",
            Window::Queue => "queue",
            Window::Timer => "timer",
            Window::Lock => "lock",
            Window::Net => "net",
            Window::Gpu => "gpu",
            Window::Mem => "mem",
            Window::Syscall => "syscall",
        }
    }

    /// The window a raw discriminant names, or `None` - so a record read back from
    /// the plane cannot be decoded into a variant that does not exist.
    pub fn from_u8(v: u8) -> Option<Window> {
        Window::ALL.into_iter().find(|w| *w as u8 == v)
    }

    /// This window's bit in the enable mask.
    #[inline(always)]
    pub const fn bit(self) -> u32 {
        1u32 << (self as u32)
    }
}

/// What happened. Subsystem-local, so [`Kind::Acquire`] under [`Window::Kmeta`] and
/// under [`Window::Frames`] are different events and read as such.
///
/// **Acquire/Release are a pair on purpose**: the host-side ledger balances them per
/// owner, and an unmatched acquire *is* the leak - visible as a line in the ledger
/// rather than inferred from a total that did not return to zero.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// A resource was taken. `a` = units, `b` = subsystem detail.
    Acquire = 0,
    /// A resource was given back. Same fields.
    Release = 1,
    /// A charge moved between owners. `a` = units, `b` = the owner it came from.
    Transfer = 2,
    /// A request was refused. `a` = units wanted.
    Refuse = 3,
    /// A state change with no resource attached.
    Note = 4,
    /// Entry into something - a syscall, a critical section. Paired with
    /// [`Kind::Exit`].
    Enter = 5,
    /// Exit from something. `a` = the result.
    Exit = 6,
}

/// The owner tag for the kernel itself, matching `mm::kmeta::Owner::KERNEL`.
pub const OWNER_KERNEL: u16 = crate::abi::obs::OWNER_KERNEL;

/// Every window's bit set - what the root advertises while recording is on.
pub const WINDOW_MASK_ALL: u32 = crate::abi::obs::WINDOW_MASK_ALL;

/// One ring per CPU, written only by its owner.
///
/// `from_array` rather than `PerCpu::new`, because a ring owns frames and so must
/// not be `Copy`: a duplicated value would duplicate the claim on them. Safe across
/// cores by **partitioning** - a core touches only its own slot - which is the
/// argument `crate::telemetry` already makes and the one `crate::trace` could not,
/// having a single shared buffer and a single shared counter.
static RINGS: crate::smp::PerCpu<ObsRing> =
    crate::smp::PerCpu::from_array([const { ObsRing::new() }; crate::smp::MAX_CPUS]);

/// Start recording **every** window, funding this CPU's ring.
///
/// Returns false when the pool refuses the frames, which is a clean "tracing is off"
/// rather than a boot failure: an observability facility that can take a machine
/// down is worse than one that is absent.
///
/// Each secondary funds its own ring when it is asked to record
/// ([`fund_this_cpu`]). A CPU that never funds one still records nothing and holds no
/// frames, and its offered emits are counted rather than lost silently.
pub fn enable() -> bool {
    enable_windows(WINDOW_MASK_ALL)
}

/// Start recording just the named windows.
///
/// The reason the mask is per window rather than one flag: the useful question is
/// almost never "narrate everything". A boot chasing a leak wants
/// [`Window::Kmeta`] and [`Window::Frames`] and nothing else, and turning on
/// [`Window::Syscall`] beside them would bury the six lines that matter under
/// thousands - the same reason `cargo xtask trace` groups by window on the way out.
/// Selecting at the *source* also means the events never cost anything to produce.
pub fn enable_windows(mask: u32) -> bool {
    let ok = fund_this_cpu();
    // Only advertise windows if there is somewhere to put their events, so a reader
    // is never told a window is recording into a ring that does not exist. The
    // modifier bits (`W_LOCK_HOLD`, `W_SNAPSHOT`) pass through: the snapshot plane
    // writes fixed `.bss`, so it needs no funding to be true.
    set_windows(if ok {
        mask & crate::abi::obs::MASK_VALID
    } else {
        0
    });
    ok
}

/// Turn the snapshot writers on without recording any events - fixed `.bss`, so
/// nothing to fund and nothing that can refuse.
pub fn enable_snapshots() {
    set_windows(windows() | SNAPSHOT_BIT);
}

/// Change which windows record, without funding anything.
///
/// One store. The mask lives in the published root rather than in a private static,
/// so a reader sees exactly what the kernel consults - there is no mirror that could
/// disagree with it.
pub fn set_windows(mask: u32) {
    root::republish_windows(mask);
}

/// Which windows are recording.
#[inline(always)]
pub fn windows() -> u32 {
    root::windows()
}

/// Whether `window` is recording.
///
/// **The hot path's whole cost when off**: one relaxed load of a fixed address, one
/// `and` with an immediate, one not-taken branch - 3-4 instructions with no barrier,
/// since a relaxed atomic load lowers to a plain load on all three ISAs. The mask is
/// never written while a workload runs, so its line stays shared-clean in every L1
/// and never causes a coherence event.
///
/// Honest about the one cost that is not free: *changing* the mask moves that line to
/// Modified on the writer, so every other core takes one coherence miss on its next
/// check. Enabling is a boot-time or operator act rather than something a workload
/// does, so it is a one-off - said rather than claimed to be nothing.
#[inline(always)]
pub fn on(window: Window) -> bool {
    windows() & window.bit() != 0
}

/// Take frames for this CPU's ring, if it has none.
///
/// **Called from bring-up, never from [`emit`]**, and that is a correctness
/// requirement rather than a preference. Funding allocates, allocation takes
/// `mm::frames`' pool lock, and one of the windows being recorded is
/// [`Window::Frames`] - so an emit that funded on demand could re-enter the frame
/// allocator from inside the allocator, on a lock that is not recursive. That is a
/// deadlock, not a slow path, and it would appear only on the first event a boot
/// ever recorded from that window.
pub fn fund_this_cpu() -> bool {
    let cpu = crate::smp::cpu_index();
    // SAFETY: this CPU's own slot, and nothing else holds a reference to it across
    // this call - funding is a bring-up act, not something an emit re-enters.
    let r = unsafe { RINGS.this_mut() };
    if r.funded() {
        return true;
    }
    // One contiguous block, so the emit path is `base + slot * 32` and a reader's
    // job is a linear read - the allocation the ring's whole layout is built on
    // (docs/OBSERVABILITY.md 11.4). The pool zeroes it, which the ring requires: an
    // untouched slot is recognised by its zero sequence number. Allocated here
    // rather than inside the ring so `obs/ring.rs` names nothing and stays
    // host-drivable; freed in [`reset`], the matching release path.
    let Some(pa) = crate::mm::frames::alloc_contig(ring::RING_PAGES) else {
        return false;
    };
    r.fund(
        cpu as u32,
        crate::arch::phys_to_virt(pa),
        crate::arch::virt_to_phys,
    );
    true
}

/// Stop recording and give every ring's frames back.
///
/// Called between runs, single-threaded, which is why it may touch other CPUs'
/// slots.
pub fn reset() {
    set_windows(0);
    for i in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs, no secondary is executing.
        let r = unsafe { RINGS.get_mut(i) };
        let va = r.release();
        if va == 0 {
            continue;
        }
        // The block [`fund_this_cpu`] allocated, frame by frame: a contiguous run is
        // not an object, just frames that happen to be adjacent, so the ordinary
        // per-frame free is the whole teardown.
        let pa = crate::arch::virt_to_phys(va);
        for p in 0..ring::RING_PAGES {
            crate::mm::frames::free(pa + p * crate::mm::frames::FRAME_SIZE);
        }
    }
    for i in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs, no secondary is executing and no reader is live, so
        // replacing the block wholesale (its seqlock included) races nothing.
        unsafe { *CPUS.get_mut(i) = crate::abi::obs::ObsCpu::new() };
    }
}

/// Whether any window is recording.
#[inline]
pub fn enabled() -> bool {
    // The *event* windows only: the modifier bits (`W_SNAPSHOT`, `W_LOCK_HOLD`)
    // do not put records in the rings, and the one caller of this predicate
    // (the `trace` shim) is asking exactly "are events being recorded".
    windows() & WINDOW_MASK_ALL != 0
}

/// Record one event, with the arguments already computed.
///
/// **Prefer [`crate::obs_event!`] at a call site.** This function cannot avoid
/// evaluating its arguments, because Rust evaluates them before the call - so a site
/// whose `a` or `b` costs anything to compute pays that cost even with the window
/// off. That is not hypothetical: the first version of the queue window did exactly
/// this and `cargo xtask bench` showed **+9 instructions per queue round trip** with
/// nothing being recorded. The macro puts the mask test *outside* the argument
/// expressions and the delta went back to zero.
///
/// This entry point remains for callers whose arguments are already in registers -
/// the [`crate::trace`] shim, which is handed them by its own caller.
#[inline]
pub fn emit(window: Window, kind: Kind, owner: u16, a: u64, b: u64) {
    if !on(window) {
        return;
    }
    emit_packed(ring::pack_meta(window as u8, kind as u8, owner), a, b);
}

/// Record one event **without** testing the mask - [`crate::obs_event!`]'s body.
///
/// Takes the metadata half pre-packed ([`ring::pack_meta`]) rather than as three
/// fields, because at every call site the window and kind are compile-time
/// constants: packed there, the constant half folds into one immediate and this
/// function neither marshals three small arguments nor stores four sub-word fields.
///
/// `#[inline(never)]` so a call site is a test and a call rather than the whole ring
/// path inlined into it, and `#[cold]` because the two are not the same instruction:
/// without it the compiler is free to place this call's register setup *before* the
/// branch that skips it, and measurably did - a queue round trip cost **+8
/// instructions** with the window off, against the 3 a load-and-test should be.
/// `#[cold]` moves the whole path out of line and the delta went to **+1**.
#[inline(never)]
#[cold]
pub fn emit_packed(meta: u64, a: u64, b: u64) {
    // SAFETY: this CPU's own ring; single producer by partitioning, and
    // `push_packed` re-enters nothing.
    let r = unsafe { RINGS.this_mut() };
    r.push_packed(crate::arch::obs_tick(), meta, a, b);
}

/// `(events written, emits offered to an unfunded ring)`, summed over every CPU.
///
/// The second number is not a curiosity: it is what distinguishes "nothing happened"
/// from "a CPU was asked to record and had no memory", which a single total cannot
/// say. Loss to the ring wrapping is deliberately **not** here - see
/// [`ring::ObsRing::get`] - because that is a property of a reader's cursor and is
/// reported as the range of sequence numbers it missed.
pub fn counters() -> (u64, u64) {
    let mut written = 0u64;
    let mut unfunded = 0u64;
    for i in 0..crate::smp::MAX_CPUS {
        // SAFETY: a cross-core read of counters. A concurrent producer can only make
        // them larger, which is the contract `PerCpu::get` states.
        let r = unsafe { RINGS.get(i) };
        written += r.written();
        unfunded += r.unfunded_emits();
    }
    (written, unfunded)
}

/// Events written past what the rings can hold, summed over every CPU.
///
/// **Derived, not counted**: a ring holds `RING_EVENTS`, so everything beyond that
/// has been overwritten, and no counter on the emit path is needed to know it. That
/// matters twice - it removes an atomic read-modify-write from the hot path, and it
/// stops conflating "the ring is a ring" with "a reader lost data", which a counter
/// incremented on every event after the first wrap does.
///
/// What it is *for*: deciding whether a total computed from a dump can be trusted. A
/// nonzero value means the stream is incomplete, so a balance taken from it would
/// lie - which is precisely the assertion the `smp` kernel's trace phase makes.
pub fn overwritten() -> u64 {
    let mut over = 0u64;
    for i in 0..crate::smp::MAX_CPUS {
        // SAFETY: a cross-core counter read; see `counters`.
        let r = unsafe { RINGS.get(i) };
        over += r.written().saturating_sub(r.capacity() as u64);
    }
    over
}

/// CPU `i`'s ring, for the dumper and for a test oracle.
///
/// # Safety
/// The caller must accept a torn read of the counters; the records themselves are
/// validated by [`ring::ObsRing::get`].
pub unsafe fn ring_of(i: usize) -> &'static ObsRing {
    // SAFETY: delegated to the caller per the contract above.
    unsafe { RINGS.get(i) }
}

/// Kernel VA of the per-CPU ring array, for the root to publish.
pub fn rings_va() -> usize {
    core::ptr::addr_of!(RINGS) as usize
}

// =========================================================================
// The snapshot plane (docs/OBSERVABILITY.md 11, phase S3)
// =========================================================================

/// One live-state block per CPU, written only by its owner at transitions it
/// already passes through - no sampler, no timer, no IPI anywhere in the design.
/// `from_array` for the reason [`RINGS`] gives; safe across cores by partitioning
/// on the write side, and by [`cpu::read`]'s seqlock on the read side.
static CPUS: crate::smp::PerCpu<crate::abi::obs::ObsCpu> = crate::smp::PerCpu::from_array(
    [const { crate::abi::obs::ObsCpu::new() }; crate::smp::MAX_CPUS],
);

/// The snapshot writers' bit in the mask (a modifier, like `W_LOCK_HOLD`).
pub const SNAPSHOT_BIT: u32 = 1u32 << crate::abi::obs::W_SNAPSHOT;

/// Whether the snapshot writers are stamping. Same cost shape as [`on`]: one
/// relaxed load, one test - the whole price every transition pays when off.
#[inline(always)]
pub fn snapshots_on() -> bool {
    windows() & SNAPSHOT_BIT != 0
}

/// This CPU entered an execution entity: publish the coupled group and count a
/// dispatch. The interval since the last transition is charged busy.
#[inline(always)]
pub fn snap_user(cell: usize, entity: usize, vcore: usize) {
    if snapshots_on() {
        snap_user_cold(cell, entity, vcore);
    }
}

#[inline(never)]
#[cold]
fn snap_user_cold(cell: usize, entity: usize, vcore: usize) {
    // SAFETY: this CPU's own block; `transition` is single-writer by partitioning.
    unsafe {
        let c = CPUS.this_mut() as *mut _;
        cpu::transition(
            c,
            crate::abi::obs::OBS_CPU_USER,
            cell as u32,
            entity as u32,
            vcore as u32,
            crate::arch::obs_tick(),
            false,
        );
        cpu::bump(c, cpu::CTR_DISPATCHES, 1);
    }
}

/// This CPU is back in kernel context (a run ended, a trap will not resume the
/// same entity). The interval since the last transition is charged busy.
#[inline(always)]
pub fn snap_kernel() {
    if snapshots_on() {
        snap_state_cold(crate::abi::obs::OBS_CPU_KERNEL, false);
    }
}

/// This CPU is about to park in the scheduler idle state. The interval **up to
/// here** was execution, so it is charged busy; the park itself is charged by
/// [`snap_unparked`], which knows whether the halt was genuine.
#[inline(always)]
pub fn snap_parked() {
    if snapshots_on() {
        snap_state_cold(crate::abi::obs::OBS_CPU_PARKED, false);
    }
}

/// This CPU came out of a park. `halted` is `idle::wait`'s own answer: a genuine
/// halt charges the interval idle; a park that could not stop the CPU charges it
/// **busy** - a spin is not idle, and recording it as idle would launder the one
/// number this plane exists to make honest (docs/ENGINEERING.md 7).
///
/// The halt/spin **counts** are not bumped here: since S4 they are the
/// unconditional [`cpu::CTR_HALTS`]/[`cpu::CTR_SPINS`] slots, written by
/// `idle::wait` itself whatever the mask says, because `idle::halts()`/`spins()`
/// read them and existing kernels assert those with recording off. Bumping them
/// here too would double-count every halt taken while snapshots are on.
#[inline(always)]
pub fn snap_unparked(halted: bool) {
    if snapshots_on() {
        snap_state_cold(crate::abi::obs::OBS_CPU_KERNEL, halted);
    }
}

#[inline(never)]
#[cold]
fn snap_state_cold(state: u32, charge_idle: bool) {
    // SAFETY: this CPU's own block.
    unsafe {
        cpu::transition(
            CPUS.this_mut() as *mut _,
            state,
            0,
            0,
            0,
            crate::arch::obs_tick(),
            charge_idle,
        );
    }
}

/// Publish an auxiliary field of this CPU's coupled group: the nearest armed
/// timer deadline and/or the receive-wait tier.
#[inline(always)]
pub fn snap_aux(timer_deadline_ns: Option<u64>, net_tier: Option<u32>) {
    if snapshots_on() {
        snap_aux_cold(timer_deadline_ns, net_tier);
    }
}

#[inline(never)]
#[cold]
fn snap_aux_cold(timer_deadline_ns: Option<u64>, net_tier: Option<u32>) {
    // SAFETY: this CPU's own block.
    unsafe { cpu::set_aux(CPUS.this_mut() as *mut _, timer_deadline_ns, net_tier) }
}

/// CPU `i`'s snapshot block, read-only - the test oracle and the dumper's source.
pub fn cpu_block(i: usize) -> *const crate::abi::obs::ObsCpu {
    // SAFETY: a shared pointer to a block whose cross-core reads go through the
    // seqlock (`cpu::read`) or single-copy-atomic counter loads (`cpu::counter`).
    unsafe { CPUS.get(i) as *const _ }
}

/// One coherent reading of CPU `i`'s coupled group.
pub fn cpu_snap(i: usize) -> Option<cpu::CpuSnap> {
    // SAFETY: a live block; racing the owner is what the seqlock is for.
    unsafe { cpu::read(cpu_block(i)) }
}

/// Counter `slot` of CPU `i`.
pub fn cpu_counter(i: usize, slot: usize) -> u64 {
    // SAFETY: a live block; a torn u64 cannot occur on these ISAs.
    unsafe { cpu::counter(cpu_block(i), slot) }
}

/// Add `delta` to counter `slot` on **this CPU's** block, returning the new value.
///
/// **Unconditional** - not behind any mask bit - because the counters migrated
/// here (S4) replace module statics whose accessors existing test kernels assert
/// with recording off; a counter that only counts while observability is enabled
/// is a different quantity wearing the same name. The cost is what the statics
/// cost: one volatile read-add-write on this core's own line, no lock, so an
/// interrupt handler can never wait on it.
#[inline]
pub fn cpu_bump(slot: usize, delta: u64) -> u64 {
    // SAFETY: this CPU's own block; single writer by partitioning.
    unsafe { cpu::bump(CPUS.this_mut() as *mut _, slot, delta) }
}

/// Counter `slot` summed over every CPU - what a module-level accessor reports
/// on a machine where any core may have done the work. On a single-CPU boot this
/// is byte-for-byte the old static (every other block reads zero).
pub fn cpu_counter_sum(slot: usize) -> u64 {
    let mut total = 0u64;
    for i in 0..crate::smp::MAX_CPUS {
        total = total.wrapping_add(cpu_counter(i, slot));
    }
    total
}

/// Zero counter `slot` on every CPU - the module `reset()` paths' half of the
/// migration, under the same contract those resets already state: between runs,
/// no secondary executing.
pub fn cpu_counter_clear(slot: usize) {
    for i in 0..crate::smp::MAX_CPUS {
        // SAFETY: between runs, no secondary is executing (the callers' contract).
        unsafe { cpu::set(CPUS.get_mut(i) as *mut _, slot, 0) };
    }
}

/// Kernel VA of the per-CPU snapshot array, for the root to publish.
pub fn cpus_va() -> usize {
    core::ptr::addr_of!(CPUS) as usize
}

/// The machine-wide memory status block (`abi::obs::ObsMem`), filled by
/// [`mem_refresh`] and stamped with when - never maintained by the allocators
/// themselves, because that would put a mirror-keeping store on the hottest
/// allocation path. `refreshed_tick == 0` means never filled.
static MEM: crate::abi::obs::ObsMem = crate::abi::obs::ObsMem::new();

/// Copy the live memory numbers into the published [`MEM`] block, under the same
/// seqlock protocol the per-CPU group uses, and stamp when.
///
/// Callers: the dump path, a test oracle, and eventually `SYS_OBS_INFO` - anything
/// that is about to *read* the numbers. Cheap enough to call freely (a dozen loads
/// of counters every subsystem already maintains), expensive enough in principle
/// (the pool's lock for `stats`) that it must never sit on an allocation path.
pub fn mem_refresh() {
    use core::ptr::write_volatile;
    use core::sync::atomic::Ordering;

    let (ddr_free, ddr_total) = crate::mm::frames::stats();
    let (pmem_free, pmem_total) = crate::mm::frames_pmem::stats();
    let c = &MEM as *const crate::abi::obs::ObsMem as *mut crate::abi::obs::ObsMem;
    // SAFETY: the block is a static written only here; concurrent readers are
    // defended by the seqlock exactly as for `ObsCpu`. Single writer in practice
    // (the boot CPU's read paths); a future concurrent refresher must serialise.
    unsafe {
        let s = (*c).seq.fetch_add(1, Ordering::Acquire);
        write_volatile(&raw mut (*c).refreshed_tick, crate::arch::obs_tick());
        write_volatile(&raw mut (*c).ddr_free, ddr_free as u64);
        write_volatile(&raw mut (*c).ddr_total, ddr_total as u64);
        write_volatile(&raw mut (*c).pmem_free, pmem_free as u64);
        write_volatile(&raw mut (*c).pmem_total, pmem_total as u64);
        write_volatile(
            &raw mut (*c).numa_fallbacks,
            crate::mm::frames::numa_fallbacks() as u64,
        );
        write_volatile(&raw mut (*c).recorded_pages, crate::load::recorded_pages());
        write_volatile(&raw mut (*c).eager_pages, crate::load::eager_pages());
        write_volatile(
            &raw mut (*c).block_cache_fills,
            crate::hw::block::cache_fills(),
        );
        write_volatile(
            &raw mut (*c).kmeta_kernel_frames,
            crate::mm::kmeta::charged(crate::mm::kmeta::Owner::KERNEL) as u64,
        );
        (*c).seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// The published memory block, read-only.
pub fn mem_block() -> &'static crate::abi::obs::ObsMem {
    &MEM
}

/// Kernel VA of the memory block, for the root.
pub fn mem_va() -> usize {
    core::ptr::addr_of!(MEM) as usize
}

/// The published name table: which counter slot means what, as runtime data
/// rather than an ABI contract, so a reader never guesses at an unnamed slot
/// (`abi::obs::OBS_SEC_NAMES`).
static NAMES: [crate::abi::obs::ObsName; 25] = {
    use crate::abi::obs::{OBS_NAME_COUNTER, ObsName};
    [
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_BUSY_TICKS as u32, "busy_ticks"),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_IDLE_TICKS as u32, "idle_ticks"),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_DISPATCHES as u32, "dispatches"),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_HALTS as u32, "halts"),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_SPINS as u32, "spins"),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_NET_IRQS as u32, "net_rx_irqs"),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_NET_SPIN_POLLS as u32,
            "net_spin_polls",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_NET_SLICES as u32,
            "net_timer_slices",
        ),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_NET_HALTS as u32, "net_halts"),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_NET_ESCALATIONS as u32,
            "net_escalations",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_CONSOLE_BYTES as u32,
            "console_bytes",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PUMP_FIFO_TAKES as u32,
            "pump_fifo_takes",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PUMP_DIRECT_PUSHES as u32,
            "pump_direct_pushes",
        ),
        ObsName::new(OBS_NAME_COUNTER, cpu::CTR_SCHED_PICKS as u32, "sched_picks"),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_SCHED_RR_PICKS as u32,
            "sched_rr_picks",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_SCHED_DIVERGED as u32,
            "sched_diverged",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_SCHED_CHARGED_NS as u32,
            "sched_charged_ns",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_SCHED_REARM_CALLS as u32,
            "sched_rearm_calls",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_SCHED_REARM_NO_RECORD as u32,
            "sched_rearm_no_record",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_ARMED as u32,
            "preempt_armed",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_TAKEN as u32,
            "preempt_taken",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_UNARMABLE as u32,
            "preempt_unarmable",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_TO_SIBLING as u32,
            "preempt_to_sibling",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_TO_CELL as u32,
            "preempt_to_cell",
        ),
        ObsName::new(
            OBS_NAME_COUNTER,
            cpu::CTR_PREEMPT_NOTES as u32,
            "preempt_notes",
        ),
    ]
};

/// The name table's address and entry count, for the root.
pub fn names_va() -> (usize, usize) {
    (core::ptr::addr_of!(NAMES) as usize, NAMES.len())
}
