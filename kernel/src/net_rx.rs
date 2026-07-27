//! Kernel-side network **receive wait**: the park-until-frame primitive
//! `SYS_WAIT_NET` plus the NIC RX interrupt plumbing (docs/NETSTACK.md, rheo-net
//! N2d/N2h). The network twin of `input.rs`, and the OS's third interrupt source
//! after the UART RX line and the timer.
//!
//! Before this, `librheo::net::recv` was a **re-poll**: `OP_NET_RX` returned
//! "nothing available" and the cell submitted it again, so a cell waiting for a
//! packet burned a whole core. Now the cell parks (its reactor blocks here) and
//! the kernel **halts** until a frame arrives or the caller's deadline expires.
//!
//! Everything here is portable; the per-ISA interrupt-controller code stays in
//! `kernel/src/arch` (the portability rule), reached through seams:
//! `arch::enable_virtio_net_irq(slot)`, `arch::net_irq_enabled()`,
//! `arch::net_irq_pending()` and `arch::idle_wait()` for the NIC line, plus
//! `arch::timer_irq_enabled()` for the deadline and the timer-backed idle. Which of
//! the two an ISA has is what selects the wait mode below - portable logic over the
//! predicates, no `cfg(target_arch)` here. The hardware one-shot itself is **never**
//! touched from this module: deadlines and poll slices go through the timer arbiter
//! ([`crate::ktimer`]), which owns it (rheo-net N2h - see below).
//!
//! ## Where the received frames are buffered
//!
//! `input.rs` needs its own byte ring because the 16550's 16-byte FIFO is the
//! only other buffer. A NIC does not: the driver pre-posts **16 RX buffers of
//! 2 KiB** on the receive virtqueue (`hw/virtio_net.rs`), the device DMAs each
//! arriving frame straight into one of them, and the used ring records the
//! arrival. That *is* the kernel RX ring - frame-pool memory, written by the
//! device, so a frame arriving while a cell computes is not lost. Adding a second
//! kernel ring would only add a copy, so this module deliberately does not: the
//! interrupt handler records the arrival ([`on_irq`]) and the wait path copies
//! once, from the virtqueue buffer straight into the cell's buffer.
//!
//! ## How the wait idles: three modes, decided portably
//!
//! A wait may only halt the CPU if *something* can wake it. Two independent
//! interrupt sources can, and the choice between them is plain portable logic over
//! the `arch` predicates ([`IdleMode`]):
//!
//! 1. **[`IdleMode::NicInterrupt`]** - the NIC RX interrupt is wired, so the kernel
//!    registers the caller's deadline with the arbiter and halts, waking on either
//!    source. The genuine 0%-CPU park (riscv64, aarch64).
//! 2. **[`IdleMode::TimerIdle`]** - no NIC RX interrupt on this ISA, but the timer
//!    interrupt is. The kernel then polls the receive queue and **halts on a timer
//!    slice** between polls: a real halt, not a spin, at a low duty cycle. This is
//!    x86-64, whose *timer* is genuinely interrupt-driven (the LAPIC one-shot,
//!    reached over the **xAPIC MMIO** page - docs/SMP.md; the x2APIC MSR block it
//!    used before is inert under QEMU TCG, which for a while made this mode a spin
//!    in disguise, docs/ENGINEERING.md 1) while its virtio-*pci* NIC has no usable
//!    interrupt line under QEMU TCG. The
//!    wake comes from the timer, never from the NIC - which is why this is reported
//!    as its own mode and never as "interrupt-driven".
//! 3. **[`IdleMode::Poll`]** - neither interrupt available: the honest last-resort
//!    bounded poll, where the CPU spins.
//!
//! ## The adaptive poll policy (rheo-net N2h)
//!
//! The slice length used to be a single constant (500 us). That is the wrong number
//! twice over: too much added latency for a latency-first deployment, and wasted
//! wakeups for a power-first one. It is now a **NAPI-style escalation** over three
//! tiers ([`RxTier`]), driven by a per-deployment [`RxProfile`]:
//!
//! - **hot** - a frame arrived, was transmitted, or the queue was non-empty within
//!   the last [`RxPolicy::hot_window_ns`]: another frame is likely imminent, so the
//!   wait does a **bounded busy-poll** of at most [`RxPolicy::spin_polls`] receive-queue
//!   checks. Bounded, so it can never wedge; it is the tier that turns a "one slice"
//!   latency into a sub-microsecond one for back-to-back traffic.
//! - **warm** - [`RxPolicy::warm_slices`] short timer slices
//!   ([`RxPolicy::warm_slice_ns`]): still latency-biased, but halting.
//! - **cold** - long slices ([`RxPolicy::cold_slice_ns`]) forever after: an idle link
//!   costs almost nothing. Where the NIC interrupt exists the warm/cold tiers are a
//!   single **indefinite** park instead - the device wakes us, so no slice is needed.
//!
//! The busy-poll tier is deliberately **not** used in [`IdleMode::NicInterrupt`]
//! unless the profile explicitly asks for it ([`RxPolicy::busy_poll_with_irq`], the
//! `hft` profile): where the device can wake a halted CPU, a genuine 0%-CPU park
//! beats burning cycles, so spinning there is a latency-vs-power choice a deployment
//! opts into rather than a default.
//!
//! **One shared poll timer, by construction.** The slice is registered as the single
//! [`crate::ktimer::TimerClient::RxPoll`] deadline, so N waiters can never become N
//! timers and N wakeups (the thundering herd). Under today's single-CPU cooperative
//! model there is at most one waiter at a time, so this is a structural property, not
//! something a test can yet exercise with two concurrent waiters - stated plainly
//! rather than proven by a scenario that cannot happen.
//!
//! ## The deadline is a deadline, not a spin count
//!
//! `timeout_ns` means the same thing in all three modes: a **monotonic deadline**,
//! registered with the arbiter as [`crate::ktimer::TimerClient::RxDeadline`] and
//! compared in the timer's own time domain. [`POLL_BUDGET`] is only a safety backstop
//! for an *indefinite* wait (`timeout_ns == 0`) in poll mode; it can never truncate a
//! caller's deadline. Before N2d the fallback exited after a fixed iteration count, so
//! the same `timeout_ns` meant wildly different things per ISA.
//!
//! ## Honesty
//!
//! [`interrupt_driven`] reports **only** whether this ISA delivers the NIC RX
//! interrupt - it is not widened by the timer-backed mode. [`irq_count`] counts NIC
//! interrupts the kernel actually took (a genuine device interrupt, not a claim),
//! [`did_idle`] records whether the wait halted the CPU at all, and [`idle_mode`]
//! says **how** it halted. The N2h counters ([`spin_polls`], [`timer_slices`],
//! [`halts`], [`escalations`], [`tier`]) make the escalation and the duty cycle
//! *measurable* rather than claimed. docs/NETSTACK.md 16 has the per-ISA table.

use crate::ktimer::{self, TimerClient};
use core::ptr::{addr_of, addr_of_mut};

/// Iterations an **indefinite** poll-mode wait (`timeout_ns == 0`, no NIC and no
/// timer interrupt) spins before giving up. A safety backstop only: a bounded wait
/// exits on its deadline, so this can never cut a caller's timeout short.
const POLL_BUDGET: u64 = 200_000_000;

/// How a receive wait idled - reported so a test and the docs can state the truth
/// per ISA instead of blurring "halted" into "interrupt-driven".
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IdleMode {
    /// No [`wait_frame`] has run yet.
    None,
    /// Halted on the **NIC's RX interrupt** (with the caller's deadline armed
    /// alongside it): one halt, woken by the device. A genuine 0%-CPU park.
    NicInterrupt,
    /// No NIC RX interrupt on this ISA: polled the receive queue and halted on a
    /// timer slice between polls. A real halt woken by the **timer** - low duty
    /// cycle, but not a NIC interrupt.
    TimerIdle,
    /// Neither interrupt available: a bounded poll. The CPU spins.
    Poll,
}

/// The escalation tier a receive wait is in (docs/NETSTACK.md 16). A wait only ever
/// moves **forwards** through these; the next wait starts over.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RxTier {
    /// No [`wait_frame`] has run yet.
    None,
    /// Bounded busy-poll: recent traffic makes another frame likely imminent.
    Hot,
    /// Short timer slices.
    Warm,
    /// Long slices (or, with a NIC interrupt, an indefinite park).
    Cold,
}

/// Deployment profile for the receive poll policy. These mirror the `rheo-net`
/// crate's cargo profile features (`hft` / `edge` / `warehouse` / `embedded`) **by
/// name and intent**; the kernel cannot read a userspace crate's cargo features, so
/// the profile is selected kernel-side with [`set_profile`] and defaults to
/// [`RxProfile::Edge`], the general-purpose profile - exactly like the net crate's
/// `default = ["edge"]`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RxProfile {
    /// Latency first, cost no object: long hot window, big spin budget, very short
    /// slices, and busy-poll even where the NIC interrupt could park the CPU.
    Hft,
    /// The balanced general-purpose default.
    Edge,
    /// Throughput/batching: a longer hot window (a burst is expected to continue),
    /// but fewer, longer slices once idle - fewer wakeups, bigger batches.
    Warehouse,
    /// Power first: never busy-poll, long slices.
    Embedded,
}

/// The tier constants for one [`RxProfile`]. Every number a receive wait uses lives
/// here, so the trade-off is a table to read rather than magic numbers in the loop.
#[derive(Copy, Clone, Debug)]
pub struct RxPolicy {
    /// How long after the last observed activity (a received frame, a NIC interrupt,
    /// a transmit) the **hot** tier applies. 0 disables busy-polling outright.
    pub hot_window_ns: u64,
    /// Receive-queue checks the hot tier performs before escalating.
    pub spin_polls: u32,
    /// The **warm** tier's slice length.
    pub warm_slice_ns: u64,
    /// How many warm slices before escalating to cold.
    pub warm_slices: u32,
    /// The **cold** tier's slice length.
    pub cold_slice_ns: u64,
    /// Whether to busy-poll even in [`IdleMode::NicInterrupt`], where a halt would
    /// otherwise cost 0% CPU. Only the latency-first profile says yes.
    pub busy_poll_with_irq: bool,
}

impl RxPolicy {
    /// The constants for `profile`.
    ///
    /// The trade-off, stated plainly. A slice bounds added receive latency, so short
    /// slices buy latency with wakeups; a wakeup costs an arm + a re-poll of the
    /// device (a handful of MSR/MMIO accesses - microseconds under QEMU TCG), so the
    /// slice must stay comfortably above that for the **halt** to dominate and the
    /// duty cycle to stay near a percent. `hft` spends CPU and power to get
    /// sub-microsecond wake-ups on a busy link; `embedded` gives up milliseconds of
    /// latency to halt nearly all the time; `warehouse` batches; `edge` sits between.
    pub const fn of(profile: RxProfile) -> RxPolicy {
        match profile {
            RxProfile::Hft => RxPolicy {
                hot_window_ns: 500_000, // 500 us: a busy link stays hot
                spin_polls: 4_096,
                warm_slice_ns: 20_000, // 20 us
                warm_slices: 16,
                cold_slice_ns: 100_000, // 100 us
                busy_poll_with_irq: true,
            },
            RxProfile::Edge => RxPolicy {
                hot_window_ns: 100_000, // 100 us
                spin_polls: 256,
                warm_slice_ns: 100_000, // 100 us
                warm_slices: 8,
                cold_slice_ns: 1_000_000, // 1 ms
                busy_poll_with_irq: false,
            },
            RxProfile::Warehouse => RxPolicy {
                hot_window_ns: 250_000, // 250 us: a burst is expected to continue
                spin_polls: 512,
                warm_slice_ns: 250_000, // 250 us
                warm_slices: 4,
                cold_slice_ns: 2_000_000, // 2 ms
                busy_poll_with_irq: false,
            },
            RxProfile::Embedded => RxPolicy {
                hot_window_ns: 0, // never busy-poll
                spin_polls: 0,
                warm_slice_ns: 2_000_000, // 2 ms
                warm_slices: 1,
                cold_slice_ns: 10_000_000, // 10 ms
                busy_poll_with_irq: false,
            },
        }
    }

    /// The tier a wait is in after `spins` hot-tier polls (out of an effective
    /// budget of `spin_budget`) and `slices` timer slices. The escalation law, as a
    /// pure function, so it can be asserted directly.
    pub const fn tier(&self, spin_budget: u32, spins: u32, slices: u32) -> RxTier {
        if spins < spin_budget {
            RxTier::Hot
        } else if slices < self.warm_slices {
            RxTier::Warm
        } else {
            RxTier::Cold
        }
    }

    /// The slice length for the `slices`-th slice of this wait.
    pub const fn slice_ns(&self, slices: u32) -> u64 {
        if slices < self.warm_slices {
            self.warm_slice_ns
        } else {
            self.cold_slice_ns
        }
    }
}

/// Interrupts taken from the NIC (incremented by [`on_irq`]).
static mut IRQS: u64 = 0;
/// Whether a wait halted the CPU (either idle mode).
static mut IDLED: bool = false;
/// The mode the most recent [`wait_frame`] used.
static mut MODE: IdleMode = IdleMode::None;
/// The selected deployment profile.
static mut PROFILE: RxProfile = RxProfile::Edge;
/// Monotonic ns (timer domain) until which the link counts as **hot**.
static mut HOT_UNTIL_NS: u64 = 0;
/// Receive-queue checks done in the hot (busy-poll) tier.
static mut SPIN_POLLS: u64 = 0;
/// Timer slices halted for (warm + cold).
static mut SLICES: u64 = 0;
/// Halts performed inside a receive wait (either idle mode).
static mut HALTS: u64 = 0;
/// Tier transitions observed (hot->warm, warm->cold), summed over all waits.
static mut ESCALATIONS: u64 = 0;
/// The tier the most recent wait ended in.
static mut TIER: RxTier = RxTier::None;

/// Reset the receive-wait state (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(IRQS) = 0;
        *addr_of_mut!(IDLED) = false;
        *addr_of_mut!(MODE) = IdleMode::None;
        *addr_of_mut!(HOT_UNTIL_NS) = 0;
        *addr_of_mut!(SPIN_POLLS) = 0;
        *addr_of_mut!(SLICES) = 0;
        *addr_of_mut!(HALTS) = 0;
        *addr_of_mut!(ESCALATIONS) = 0;
        *addr_of_mut!(TIER) = RxTier::None;
    }
    ktimer::cancel(TimerClient::RxPoll);
    ktimer::cancel(TimerClient::RxDeadline);
}

/// Select the deployment profile for the receive poll policy (see [`RxProfile`]).
/// Kernel-side configuration: a kernel binary picks the profile its workload wants.
pub fn set_profile(profile: RxProfile) {
    // SAFETY: single CPU; set outside a wait.
    unsafe { *addr_of_mut!(PROFILE) = profile };
}

/// The selected deployment profile.
pub fn profile() -> RxProfile {
    // SAFETY: single CPU.
    unsafe { *addr_of!(PROFILE) }
}

/// The tier constants currently in force.
pub fn policy() -> RxPolicy {
    RxPolicy::of(profile())
}

/// Bring up the NIC's RX interrupt for the installed device, if this ISA can
/// deliver it. Opt-in: called only by the test kernel that waits on frames, so
/// every other kernel boots exactly as before. Returns whether the interrupt is
/// now wired (false = the honest bounded-poll fallback).
///
/// The virtio-mmio **slot** the driver bound to is a portable fact (the device's
/// position on the transport bus); turning it into an interrupt id is per-ISA and
/// lives in `arch`. A virtio-pci NIC reports no slot, so this returns false.
pub fn enable_irq() -> bool {
    match crate::hw::virtio_net::mmio_slot() {
        Some(slot) => crate::arch::enable_virtio_net_irq(slot),
        None => false,
    }
}

/// Whether the running ISA delivers **NIC receive interrupts** (a genuine 0%-CPU
/// park woken by the device) rather than polling. False until [`enable_irq`]
/// succeeds.
///
/// Deliberately narrow: the timer-backed [`IdleMode::TimerIdle`] mode also halts the
/// CPU, but the NIC did not wake it, so this stays false there. Ask [`idle_mode`] for
/// what actually happened.
pub fn interrupt_driven() -> bool {
    crate::arch::net_irq_enabled()
}

/// How the most recent [`wait_frame`] idled (see [`IdleMode`]).
pub fn idle_mode() -> IdleMode {
    // SAFETY: single CPU.
    unsafe { *addr_of!(MODE) }
}

/// How many NIC interrupts the kernel has taken. Non-zero is proof a real device
/// interrupt was genuinely delivered and serviced (it cannot be faked: the
/// handler runs from the ISA's interrupt vector).
pub fn irq_count() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(IRQS) }
}

/// Whether a [`wait_frame`] call halted the CPU (at WFI / `hlt`) while waiting for a
/// frame - true in **both** idle modes, since both genuinely stop the CPU. False in
/// [`IdleMode::Poll`], and false when every frame was already queued before the wait
/// began. Pair it with [`interrupt_driven`] / [`idle_mode`] to say *which* interrupt
/// did the waking.
pub fn did_idle() -> bool {
    // SAFETY: single CPU.
    unsafe { *addr_of!(IDLED) }
}

/// Receive-queue checks performed in the **hot** (bounded busy-poll) tier.
pub fn spin_polls() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(SPIN_POLLS) }
}

/// Timer slices a receive wait halted for (warm + cold tiers). With [`halts`] this
/// is the measurable duty cycle: slices * slice length vs the wait's wall time.
pub fn timer_slices() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(SLICES) }
}

/// Halts performed inside a receive wait (a `wfi`/`hlt`, either idle mode).
pub fn halts() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(HALTS) }
}

/// Tier transitions (hot -> warm, warm -> cold) summed over all waits: the
/// observable proof that the escalation actually happened.
pub fn escalations() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(ESCALATIONS) }
}

/// The tier the most recent wait ended in.
pub fn tier() -> RxTier {
    // SAFETY: single CPU.
    unsafe { *addr_of!(TIER) }
}

/// Whether the link is currently **hot** - activity within the profile's hot window.
pub fn is_hot() -> bool {
    let window = policy().hot_window_ns;
    if window == 0 {
        return false;
    }
    // SAFETY: single CPU.
    let until = unsafe { *addr_of!(HOT_UNTIL_NS) };
    until != 0 && ktimer::now_ns() < until
}

/// Record link activity: the hot tier applies for the next
/// [`RxPolicy::hot_window_ns`]. Called on a received frame, on a NIC interrupt, and
/// on a transmit (a request usually means a reply is imminent).
pub fn note_activity() {
    let window = policy().hot_window_ns;
    if window == 0 {
        return;
    }
    let until = ktimer::now_ns().wrapping_add(window);
    // SAFETY: single CPU; the interrupt handler cannot overlap itself.
    unsafe { *addr_of_mut!(HOT_UNTIL_NS) = until };
}

/// The NIC RX interrupt handler's portable sink: acknowledge the device (so its
/// interrupt line drops and the controller does not re-assert) and record the
/// arrival. The frame itself stays in the receive virtqueue, where [`wait_frame`]
/// copies it from - the interrupt only says "look again".
///
/// Called from the per-ISA interrupt vector, which the kernel takes only inside
/// its idle path (cells run with interrupts masked), so the driver is never
/// re-entered mid-operation.
pub fn on_irq() {
    crate::hw::virtio_net::ack_irq();
    // SAFETY: single CPU; the handler cannot overlap itself.
    unsafe {
        *addr_of_mut!(IRQS) = (*addr_of!(IRQS)).wrapping_add(1);
    }
    note_activity();
}

/// `SYS_WAIT_NET`: block until a received frame is available, copy it into the
/// cell buffer at `buf_va` (up to `len` bytes), and return the frame length. The
/// cell's address space is active during the trap, so `buf_va` is written
/// directly (one copy, from the virtqueue buffer the device DMA'd into).
///
/// `timeout_ns` bounds the wait (0 = wait indefinitely) - the primitive a
/// transport needs for a retransmission timeout: "a frame, or the deadline,
/// whichever comes first". It is a **monotonic deadline** in every mode, so the same
/// `timeout_ns` means the same span of time on every ISA (see the module docs).
///
/// Returns 0 if no NIC is installed, if the deadline elapsed with no frame, or - for
/// an *indefinite* wait on the last-resort poll path - if [`POLL_BUDGET`] expires.
pub fn wait_frame(buf_va: u64, len: usize, timeout_ns: u64) -> usize {
    if len == 0 {
        return 0;
    }
    let nic_irq = crate::arch::net_irq_enabled();
    let timer_irq = crate::arch::timer_irq_enabled();
    // Portable idle decision (module docs): halt on the NIC where it can wake us,
    // else halt on timer slices where the timer can, else poll. No per-ISA code -
    // just the two `arch` predicates.
    let mode = if nic_irq && (timeout_ns == 0 || timer_irq) {
        IdleMode::NicInterrupt
    } else if timer_irq {
        IdleMode::TimerIdle
    } else {
        IdleMode::Poll
    };
    // SAFETY: single CPU.
    unsafe {
        *addr_of_mut!(MODE) = mode;
    }
    let policy = policy();
    // The hot tier's effective budget: only when the link is hot, and - where the
    // device itself can wake a halted CPU - only if the profile opts into spinning.
    let spin_budget = if !is_hot() || (mode == IdleMode::NicInterrupt && !policy.busy_poll_with_irq)
    {
        0
    } else {
        policy.spin_polls
    };
    // The caller's deadline goes to the arbiter, which arms the hardware only if it
    // is the nearest outstanding deadline and re-arms whatever survives our exit.
    if timeout_ns > 0 {
        ktimer::register(TimerClient::RxDeadline, timeout_ns);
    }
    let mut spins = 0u32;
    let mut slices = 0u32;
    let mut poll_spins = 0u64;
    let mut cur_tier = RxTier::None;
    let result = loop {
        // Take a device interrupt that is already pending before draining, so a
        // frame that arrived while the cell was computing is still credited to
        // (and acknowledged by) the interrupt path. `idle_wait` returns at once
        // when an interrupt is pending.
        if nic_irq && crate::arch::net_irq_pending() {
            crate::arch::idle_wait();
        }

        match crate::hw::virtio_net::drain_frame(buf_va, len) {
            Some(n) if n > 0 => {
                note_activity();
                break n;
            }
            Some(_) => {}
            None => break 0, // no NIC installed
        }

        if timeout_ns > 0 && ktimer::expired(TimerClient::RxDeadline) {
            break 0;
        }

        // Escalate: hot (bounded busy-poll) -> warm (short slices) -> cold (long
        // slices, or an indefinite park where the NIC can wake us).
        let next = policy.tier(spin_budget, spins, slices);
        if next != cur_tier {
            if cur_tier != RxTier::None {
                // SAFETY: single CPU.
                unsafe {
                    *addr_of_mut!(ESCALATIONS) = (*addr_of!(ESCALATIONS)).wrapping_add(1);
                }
            }
            cur_tier = next;
            // SAFETY: single CPU.
            unsafe { *addr_of_mut!(TIER) = next };
        }

        if next == RxTier::Hot {
            spins += 1;
            // SAFETY: single CPU.
            unsafe {
                *addr_of_mut!(SPIN_POLLS) = (*addr_of!(SPIN_POLLS)).wrapping_add(1);
            }
            core::hint::spin_loop();
            continue;
        }

        match mode {
            IdleMode::NicInterrupt => {
                // Genuine 0%-CPU park: halt until the NIC's RX interrupt fires (or
                // the arbiter's nearest deadline does). No slice needed - the device
                // is the wake source.
                if ktimer::park(true) {
                    mark_halt();
                }
            }
            IdleMode::TimerIdle => {
                // Timer-backed low-duty-cycle polling: halt for one slice (never
                // past the caller's deadline), then re-poll the receive queue. The
                // halt is real - the arbiter arms the per-ISA one-shot and the CPU
                // stops until it fires. The slice is the single shared `RxPoll`
                // deadline, so waiters can never multiply into wakeups.
                let slice = policy.slice_ns(slices);
                ktimer::register(TimerClient::RxPoll, slice.max(1));
                slices += 1;
                // Count the slice - and claim the idle - only when the park really
                // halted the CPU, so `did_idle`/`timer_slices` never overstate.
                if ktimer::park(false) {
                    // SAFETY: single CPU.
                    unsafe {
                        *addr_of_mut!(SLICES) = (*addr_of!(SLICES)).wrapping_add(1);
                    }
                    mark_halt();
                }
                // Release our slice; the arbiter re-arms whatever else is pending
                // (a cell sleep, an RTO, the pacer) instead of disarming it.
                ktimer::cancel(TimerClient::RxPoll);
            }
            IdleMode::None | IdleMode::Poll => {
                // Honest last resort: neither interrupt is available on this ISA, so
                // the kernel polls the receive queue. One park for the cell (no
                // re-submit storm), but the CPU spins. A bounded wait still exits on
                // its deadline above; the budget only stops an *indefinite* wait
                // from wedging the machine.
                poll_spins += 1;
                if timeout_ns == 0 && poll_spins > POLL_BUDGET {
                    break 0;
                }
                core::hint::spin_loop();
            }
        }
    };
    if timeout_ns > 0 {
        ktimer::cancel(TimerClient::RxDeadline);
    }
    result
}

/// Record a halt inside the wait (either idle mode).
fn mark_halt() {
    // SAFETY: single CPU.
    unsafe {
        *addr_of_mut!(IDLED) = true;
        *addr_of_mut!(HALTS) = (*addr_of!(HALTS)).wrapping_add(1);
    }
}

/// Kernel-side twin of [`wait_frame`] over a **kernel-owned** slice: the
/// rheo-net N4b remote-INET bridge (a registered `svc::SocketOps` table) runs its
/// datapath in kernel context, so it parks on frames into its own buffer rather
/// than a cell VA (docs/NETSTACK.md N4b). Same primitive, same interrupt/idle
/// path - only the destination differs.
pub fn wait_frame_slice(out: &mut [u8], timeout_ns: u64) -> usize {
    let len = out.len();
    wait_frame(out.as_mut_ptr() as u64, len, timeout_ns)
}
