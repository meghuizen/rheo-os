//! Kernel-side network **receive wait**: the park-until-frame primitive
//! `SYS_WAIT_NET` plus the NIC RX interrupt plumbing (docs/NETSTACK.md, rheo-net
//! N2d). The network twin of `input.rs`, and the OS's third interrupt source
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
//! `arch::timer_irq_enabled()`/`timer_arm`/`timer_expired`/`timer_disarm`/`timer_wait`
//! for the deadline and the timer-backed idle. Which of the two an ISA has is what
//! selects the wait mode below - portable logic over the predicates, no
//! `cfg(target_arch)` here.
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
//!    arms the caller's deadline and halts **once**, waking on either source. The
//!    genuine 0%-CPU park (riscv64, aarch64).
//! 2. **[`IdleMode::TimerIdle`]** - no NIC RX interrupt on this ISA, but the timer
//!    interrupt is. The kernel then polls the receive queue and **halts on a short
//!    timer slice** ([`TIMER_SLICE_NS`]) between polls: a real halt, not a spin, at
//!    a low duty cycle. This is x86-64, whose *timer* is genuinely interrupt-driven
//!    (LAPIC one-shot, x2APIC) while its virtio-*pci* NIC has no usable interrupt
//!    line under QEMU TCG. The wake comes from the timer, never from the NIC - which
//!    is why this is reported as its own mode and never as "interrupt-driven".
//! 3. **[`IdleMode::Poll`]** - neither interrupt available: the honest last-resort
//!    bounded poll, where the CPU spins.
//!
//! ## The deadline is a deadline, not a spin count
//!
//! `timeout_ns` means the same thing in all three modes: a **monotonic deadline**,
//! measured with the armed hardware timer where one is armed and with
//! `arch::cycles()` otherwise. [`POLL_BUDGET`] is only a safety backstop for an
//! *indefinite* wait (`timeout_ns == 0`) in poll mode; it can never truncate a
//! caller's deadline. Before this, the fallback exited after a fixed iteration
//! count, so the same `timeout_ns` meant wildly different things per ISA (and a
//! long wait on a slow poll path blew past any test budget).
//!
//! ## Honesty
//!
//! [`interrupt_driven`] reports **only** whether this ISA delivers the NIC RX
//! interrupt - it is not widened by the timer-backed mode. [`irq_count`] counts NIC
//! interrupts the kernel actually took (a genuine device interrupt, not a claim),
//! [`did_idle`] records whether the wait halted the CPU at all, and [`idle_mode`]
//! says **how** it halted. docs/NETSTACK.md 16 has the per-ISA table.

use core::ptr::{addr_of, addr_of_mut};

/// Iterations an **indefinite** poll-mode wait (`timeout_ns == 0`, no NIC and no
/// timer interrupt) spins before giving up. A safety backstop only: a bounded wait
/// exits on its deadline, so this can never cut a caller's timeout short.
const POLL_BUDGET: u64 = 200_000_000;

/// The timer slice [`IdleMode::TimerIdle`] halts for between receive-queue polls:
/// **500 microseconds**.
///
/// The trade-off, stated plainly. Receive latency grows by at most one slice, so
/// 500 us is invisible next to the millisecond-scale round trips this path serves
/// (ARP, DHCP, DNS, a TCP RTO) - and next to QEMU's own emulation jitter. In the
/// other direction the slice must be comfortably larger than the cost of arming the
/// timer and re-polling the device (a handful of MSR / MMIO accesses, microseconds
/// under QEMU TCG) so that the **halt dominates** and the duty cycle stays around a
/// percent instead of 100%. 500 us sits two orders of magnitude above the one and
/// two below the other. Shorter would spend the saving back on bookkeeping; longer
/// would start to show up as receive latency.
pub const TIMER_SLICE_NS: u64 = 500_000;

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
    /// [`TIMER_SLICE_NS`] timer slice between polls. A real halt woken by the
    /// **timer** - low duty cycle, but not a NIC interrupt.
    TimerIdle,
    /// Neither interrupt available: a bounded poll. The CPU spins.
    Poll,
}

/// Interrupts taken from the NIC (incremented by [`on_irq`]).
static mut IRQS: u64 = 0;
/// Whether a wait halted the CPU at least once (either idle mode).
static mut IDLED: bool = false;
/// The mode the most recent [`wait_frame`] used.
static mut MODE: IdleMode = IdleMode::None;

/// Reset the receive-wait state (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(IRQS) = 0;
        *addr_of_mut!(IDLED) = false;
        *addr_of_mut!(MODE) = IdleMode::None;
    }
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
    // Only the NIC mode arms the hardware timer for the *deadline*: the timer-idle
    // mode needs it for its slices, so there the deadline is the cycle counter.
    let armed = mode == IdleMode::NicInterrupt && timeout_ns > 0;
    if armed {
        crate::arch::timer_arm(timeout_ns);
    }
    let start = crate::arch::cycles();
    let mut spins = 0u64;
    let result = loop {
        // Take a device interrupt that is already pending before draining, so a
        // frame that arrived while the cell was computing is still credited to
        // (and acknowledged by) the interrupt path. `idle_wait` returns at once
        // when an interrupt is pending.
        if nic_irq && crate::arch::net_irq_pending() {
            crate::arch::idle_wait();
        }

        match crate::hw::virtio_net::drain_frame(buf_va, len) {
            Some(n) if n > 0 => break n,
            Some(_) => {}
            None => break 0, // no NIC installed
        }

        if timed_out(timeout_ns, armed, start) {
            break 0;
        }

        match mode {
            IdleMode::NicInterrupt => {
                // Genuine 0%-CPU park: halt until the NIC's RX interrupt fires (or
                // the armed deadline does).
                mark_idled();
                crate::arch::idle_wait();
            }
            IdleMode::TimerIdle => {
                // Timer-backed low-duty-cycle polling: halt for one slice (never
                // past the caller's deadline), then re-poll the receive queue. The
                // halt is real - `timer_wait` arms the per-ISA one-shot and stops
                // the CPU until it fires.
                let slice = match timeout_ns {
                    0 => TIMER_SLICE_NS,
                    t => TIMER_SLICE_NS
                        .min(t.saturating_sub(elapsed_ns(start)))
                        .max(1),
                };
                mark_idled();
                crate::arch::timer_wait(slice);
            }
            IdleMode::None | IdleMode::Poll => {
                // Honest last resort: neither interrupt is available on this ISA, so
                // the kernel polls the receive queue. One park for the cell (no
                // re-submit storm), but the CPU spins. A bounded wait still exits on
                // its deadline above; the budget only stops an *indefinite* wait
                // from wedging the machine.
                spins += 1;
                if timeout_ns == 0 && spins > POLL_BUDGET {
                    break 0;
                }
                core::hint::spin_loop();
            }
        }
    };
    if armed {
        crate::arch::timer_disarm();
    }
    result
}

/// Record that the wait halted the CPU (either idle mode).
fn mark_idled() {
    // SAFETY: single CPU.
    unsafe {
        *addr_of_mut!(IDLED) = true;
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

/// Whether the requested deadline has passed: the armed hardware timer where the
/// deadline itself was armed, else the monotonic cycle counter. Either way this is a
/// **time** comparison, never an iteration count.
fn timed_out(timeout_ns: u64, armed: bool, start: u64) -> bool {
    if timeout_ns == 0 {
        return false;
    }
    if armed {
        return crate::arch::timer_expired();
    }
    elapsed_ns(start) >= timeout_ns
}

/// Nanoseconds elapsed since the cycle-counter reading `start`.
fn elapsed_ns(start: u64) -> u64 {
    crate::arch::ticks_to_ns(crate::arch::cycles().wrapping_sub(start))
}
