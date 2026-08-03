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

pub mod dump;
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
/// Expanding to `if on(w) { emit_now(...) }` puts the mask test outside the argument
/// expressions, and the measured delta went back to **zero**.
#[macro_export]
macro_rules! obs_event {
    ($w:expr, $k:expr, $owner:expr, $a:expr, $b:expr) => {
        if $crate::obs::on($w) {
            $crate::obs::emit_now($w, $k, $owner, $a, $b);
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
    // is never told a window is recording into a ring that does not exist.
    set_windows(if ok { mask & WINDOW_MASK_ALL } else { 0 });
    ok
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
    r.fund(cpu as u32, crate::arch::virt_to_phys)
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
        if r.funded() {
            r.release();
        }
    }
}

/// Whether any window is recording.
#[inline]
pub fn enabled() -> bool {
    windows() != 0
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
    emit_now(window, kind, owner, a, b);
}

/// Record one event **without** testing the mask - [`crate::obs_event!`]'s body.
///
/// `#[inline(never)]` so a call site is a test and a call rather than the whole ring
/// path inlined into it, and `#[cold]` because the two are not the same instruction:
/// without it the compiler is free to place this call's register setup *before* the
/// branch that skips it, and measurably did - a queue round trip cost **+8
/// instructions** with the window off, against the 3 a load-and-test should be.
/// `#[cold]` moves the whole path out of line and the delta went to **+1**.
#[inline(never)]
#[cold]
pub fn emit_now(window: Window, kind: Kind, owner: u16, a: u64, b: u64) {
    // SAFETY: this CPU's own ring; single producer by partitioning, and `push`
    // re-enters nothing.
    let r = unsafe { RINGS.this_mut() };
    r.push(
        crate::arch::obs_tick(),
        window as u8,
        kind as u8,
        owner,
        a,
        b,
    );
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
