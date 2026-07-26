//! Kernel-side network **receive wait**: the park-until-frame primitive
//! `SYS_WAIT_NET` plus the NIC RX interrupt plumbing (docs/NETSTACK.md, rheo-net
//! N2d). The network twin of `input.rs`, and the OS's third interrupt source
//! after the UART RX line and the timer.
//!
//! Before this, `librheo::net::recv` was a **re-poll**: `OP_NET_RX` returned
//! "nothing available" and the cell submitted it again, so a cell waiting for a
//! packet burned a whole core. Now the cell parks (its reactor blocks here) and
//! the kernel idles until the NIC raises its RX interrupt.
//!
//! Everything here is portable; the per-ISA interrupt-controller code stays in
//! `kernel/src/arch` (the portability rule), reached through three seams:
//! `arch::enable_virtio_net_irq(slot)`, `arch::net_irq_enabled()`,
//! `arch::net_irq_pending()`, plus the shared `arch::idle_wait()`.
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
//! ## Honesty
//!
//! [`interrupt_driven`] reports whether this ISA delivers the NIC RX interrupt;
//! [`irq_count`] counts interrupts the kernel actually took (a genuine device
//! interrupt, not a claim), and [`did_idle`] records whether it halted at
//! WFI waiting for one (the 0%-CPU park). Where no interrupt is available the
//! wait falls back to a **bounded kernel poll loop** - still one park instead of
//! a userspace re-submit storm, but the CPU spins; both counters stay false/0 and
//! say so. docs/NETSTACK.md has the per-ISA table.

use core::ptr::{addr_of, addr_of_mut};

/// Iterations the poll fallback spins before giving up (returning 0 frames).
/// Only reached where no NIC RX interrupt is wired; bounded so a packet that
/// never arrives cannot wedge the machine.
const POLL_BUDGET: u64 = 200_000_000;

/// Interrupts taken from the NIC (incremented by [`on_irq`]).
static mut IRQS: u64 = 0;
/// Whether a wait halted the CPU at WFI at least once.
static mut IDLED: bool = false;

/// Reset the receive-wait state (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(IRQS) = 0;
        *addr_of_mut!(IDLED) = false;
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

/// Whether the running ISA delivers NIC receive interrupts (a genuine 0%-CPU
/// park at WFI) rather than polling. False until [`enable_irq`] succeeds.
pub fn interrupt_driven() -> bool {
    crate::arch::net_irq_enabled()
}

/// How many NIC interrupts the kernel has taken. Non-zero is proof a real device
/// interrupt was genuinely delivered and serviced (it cannot be faked: the
/// handler runs from the ISA's interrupt vector).
pub fn irq_count() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(IRQS) }
}

/// Whether a [`wait_frame`] call halted the CPU at WFI waiting for a frame (the
/// 0%-CPU park assertion). False in the poll build, and false when every frame
/// was already queued before the wait began.
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
/// whichever comes first". Where both the NIC and the timer interrupt are wired
/// the kernel arms the deadline and halts **once**, waking on either source; the
/// timer is disarmed on the way out.
///
/// Returns 0 if no NIC is installed, if the timeout elapsed with no frame, or -
/// in the poll fallback - if the spin budget expires.
pub fn wait_frame(buf_va: u64, len: usize, timeout_ns: u64) -> usize {
    if len == 0 {
        return 0;
    }
    // A deadline can be honoured two ways: by arming the hardware timer (so the
    // wait can halt the CPU and still wake), or - where no timer interrupt is
    // wired - by checking the monotonic counter in the poll loop.
    let use_timer = timeout_ns > 0 && crate::arch::timer_irq_enabled();
    // Halting is only safe when *something* can wake us: the NIC interrupt, plus
    // an armed deadline if one was asked for.
    let can_idle = crate::arch::net_irq_enabled() && (timeout_ns == 0 || use_timer);
    if use_timer {
        crate::arch::timer_arm(timeout_ns);
    }
    let start = crate::arch::cycles();
    let mut spins = 0u64;
    let result = loop {
        // Take a device interrupt that is already pending before draining, so a
        // frame that arrived while the cell was computing is still credited to
        // (and acknowledged by) the interrupt path. `idle_wait` returns at once
        // when an interrupt is pending.
        if crate::arch::net_irq_enabled() && crate::arch::net_irq_pending() {
            crate::arch::idle_wait();
        }

        match crate::hw::virtio_net::drain_frame(buf_va, len) {
            Some(n) if n > 0 => break n,
            Some(_) => {}
            None => break 0, // no NIC installed
        }

        if timed_out(timeout_ns, use_timer, start) {
            break 0;
        }

        if can_idle {
            // Genuine 0%-CPU park: halt until the NIC's RX interrupt fires (or the
            // armed deadline does).
            // SAFETY: single CPU.
            unsafe {
                *addr_of_mut!(IDLED) = true;
            }
            crate::arch::idle_wait();
        } else {
            // Honest fallback: no NIC interrupt on this ISA (or a deadline with no
            // timer interrupt to arm), so the kernel polls the receive queue. One
            // park for the cell (no re-submit storm), but the CPU spins - bounded,
            // so a lost packet cannot wedge the machine.
            spins += 1;
            if spins > POLL_BUDGET {
                break 0;
            }
            core::hint::spin_loop();
        }
    };
    if use_timer {
        crate::arch::timer_disarm();
    }
    result
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

/// Whether the requested deadline has passed: the armed hardware timer where one
/// is available, else the monotonic cycle counter.
fn timed_out(timeout_ns: u64, use_timer: bool, start: u64) -> bool {
    if timeout_ns == 0 {
        return false;
    }
    if use_timer {
        return crate::arch::timer_expired();
    }
    let elapsed = crate::arch::cycles().wrapping_sub(start);
    crate::arch::ticks_to_ns(elapsed) >= timeout_ns
}
