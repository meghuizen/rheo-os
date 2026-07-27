//! The **scheduler idle state** (docs/ARCHITECTURE-DEBT.md 2.4, docs/CONCURRENCY.md
//! 1): what the kernel does when no cell is runnable but at least one is blocked
//! on a wake source.
//!
//! ## The defect this exists to remove
//!
//! `IO.md` 1 and `CONCURRENCY.md` 1 state that blocking does not exist below the
//! library level, and `SCHEDULING.md` 3 says scheduler activations are viable here
//! *because* no blocking syscall exists. Three verbs contradicted that: a cell's
//! `SYS_ARM_TIMER`, `SYS_WAIT_NET` and `SYS_WAIT_INPUT` each **waited inside the
//! trap**, in kernel context, without ever consulting the scheduler. One cell's
//! `sleep(1 s)` therefore idled the whole machine while its siblings sat runnable -
//! precisely the "hidden blocking syscall silently eating a core" the design says
//! cannot happen here.
//!
//! The other half of the same defect: `reschedule` **panicked** when nothing was
//! runnable, so "every cell is waiting for the outside world" - which is the normal
//! steady state of any server - was not an expressible state at all.
//!
//! ## What this module is
//!
//! The waits are now *registrations*: a cell that waits records what it is waiting
//! for, returns to the scheduler, and some sibling runs. When (and only when) no
//! cell is runnable, the scheduler calls [`wait`] with the union of the wake sources
//! the blocked cells named, and this halts the CPU until one of them can fire.
//!
//! It **composes the three primitives that already existed** and adds no kernel
//! object and no new verb:
//!
//! - [`crate::ktimer`] - the timer arbiter, the single owner of the per-ISA hardware
//!   one-shot. This module never touches `arch::timer_*`; it asks the arbiter to
//!   park, exactly like `net_rx` and `time` do (docs/ENGINEERING.md 3).
//! - [`crate::net_rx`] - the NIC RX interrupt and its receive queue.
//! - [`crate::input`] - the UART RX interrupt and the console byte ring.
//!
//! ## How it halts, per ISA
//!
//! There is no per-ISA code here (the portability rule): the decision is made from
//! the `arch` predicates `net_irq_enabled()` / `uart_irq_enabled()` /
//! `timer_irq_enabled()`, which are what each ISA's bring-up **validated** rather
//! than requested.
//!
//! - A **device** wake source that is genuinely wired (the NIC's RX line, the
//!   UART's) lets the arbiter halt indefinitely: `ktimer::park(other_source = true)`
//!   stops the CPU until any enabled interrupt fires. A real 0%-CPU park.
//! - A **timer** deadline halts on the one-shot the arbiter armed for the nearest
//!   outstanding deadline.
//! - Where a source exists but its interrupt does not, the wait is bounded by a
//!   deadline in the arbiter's own slot and re-checked - honest low-duty-cycle
//!   polling, named as such, never described as an idle.
//! - Where nothing at all can wake the CPU, this advances the software clock by a
//!   short spin and returns `false`. Deadlines are still honoured (the arbiter
//!   compares against its monotonic clock); the CPU is not idle and does not claim
//!   to be.
//!
//! As of docs/SMP.md 5 all three ISAs have a verified timer, UART RX and NIC RX
//! interrupt, so the bounded-poll tiers are a correctness backstop rather than the
//! shipped behaviour - but they are kept and reported, because a build without a
//! NIC, or a future ISA, lands there.
//!
//! ## SMP
//!
//! Single-CPU today (task #27). The natural SMP shape is per-CPU: each CPU idles on
//! its own arbiter slot table and a cross-CPU wake is an IPI. Nothing here assumes a
//! global view beyond the statics.

use crate::arch;
use crate::ktimer::{self, TimerClient};
use core::ptr::{addr_of, addr_of_mut};

/// A blocked cell's wake sources, as a bitmask. A block declares the sources that
/// can make it satisfiable; the scheduler ORs them across every blocked cell.
pub type Sources = u32;

/// The hardware/one-shot timer deadline (a cell `sleep`, a `poll` timeout, a
/// receive deadline).
pub const TIMER: Sources = 1 << 0;
/// A received Ethernet frame.
pub const NET: Sources = 1 << 1;
/// A console input byte.
pub const CONSOLE: Sources = 1 << 2;
/// Another **cell** will make this block satisfiable (a pipe peer, a child exit).
/// Never a reason to idle on its own - if this is the only source and nothing is
/// runnable, the wait can never end, which is the deadlock condition.
pub const PEER: Sources = 1 << 3;

/// Sources this module can actually wait on (`PEER` deliberately excluded).
pub const WAITABLE: Sources = TIMER | NET | CONSOLE;

/// Halts performed by [`wait`] (the CPU genuinely stopped).
static mut HALTS: u64 = 0;
/// Bounded-poll / software-advance iterations performed by [`wait`] because no
/// wake source on this machine could halt the CPU. Non-zero means the idle spun.
static mut SPINS: u64 = 0;
/// Whether any [`wait`] genuinely halted since the last [`reset`].
static mut IDLED: bool = false;

/// Clear the idle counters (call before installing a fresh set of cells).
pub fn reset() {
    // SAFETY: single CPU, between runs.
    unsafe {
        *addr_of_mut!(HALTS) = 0;
        *addr_of_mut!(SPINS) = 0;
        *addr_of_mut!(IDLED) = false;
    }
}

/// Halts performed by the scheduler idle state.
pub fn halts() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(HALTS) }
}

/// Bounded-poll iterations performed because nothing could halt the CPU. The
/// honesty counter: a non-zero value with `halts() == 0` says the "idle" was a spin
/// (docs/ENGINEERING.md 7).
pub fn spins() -> u64 {
    // SAFETY: single CPU.
    unsafe { *addr_of!(SPINS) }
}

/// Whether the scheduler idle state genuinely halted the CPU at least once. Set
/// only from inside a park that really stopped (docs/ENGINEERING.md 1) - never on
/// intent.
pub fn did_idle() -> bool {
    // SAFETY: single CPU.
    unsafe { *addr_of!(IDLED) }
}

/// Whether the machine has *any* interrupt that can wake a halt for `src`.
/// Reported honestly per source: a timer deadline counts only if the timer
/// interrupt is wired.
pub fn interrupt_driven(src: Sources) -> bool {
    (src & NET != 0 && arch::net_irq_enabled())
        || (src & CONSOLE != 0 && arch::uart_irq_enabled())
        || (src & TIMER != 0 && arch::timer_irq_enabled())
}

/// Wait until one of `src`'s wake sources can have fired, then return. Returns
/// whether the CPU genuinely halted.
///
/// This never decides *whether* a block became satisfiable - the caller re-checks
/// that after every return, so a spurious wake costs one extra loop and never a
/// missed wake. `src` must be non-zero and must contain at least one [`WAITABLE`]
/// bit; a caller with only [`PEER`] left and nothing runnable is deadlocked and must
/// report that instead of calling this.
pub fn wait(src: Sources) -> bool {
    // The console's byte source is asked first, because on this OS it is not purely
    // an interrupt: a scripted (headless) source *produces* the next byte on demand,
    // delivering it through the real UART RX interrupt where that is wired. So the
    // console "wake" may be immediate work rather than a halt, and the cell blocked
    // on it becomes satisfiable without the CPU stopping.
    if src & CONSOLE != 0 {
        match crate::input::pump() {
            // A byte is now in the ring (possibly delivered through a genuine
            // interrupt, which `input` records itself), or input has ended - either
            // way the blocked reader is now satisfiable.
            crate::input::Pump::Data | crate::input::Pump::Eof => return false,
            crate::input::Pump::Wait => {}
        }
    }

    // Which device lines can wake a halt for the sources we were given.
    let net_irq = src & NET != 0 && arch::net_irq_enabled();
    let uart_irq = src & CONSOLE != 0 && arch::uart_irq_enabled();

    // A network wait with no NIC interrupt has to be re-checked periodically; bound
    // the halt with a poll slice in the arbiter's own `RxPoll` slot, which is
    // exactly the slot `net_rx` uses for the same purpose - so N waiters share one
    // deadline and can never multiply into N wakeups.
    let sliced = src & NET != 0 && !net_irq;
    if sliced {
        ktimer::register(TimerClient::RxPoll, crate::net_rx::poll_slice_ns());
    }

    let halted = ktimer::park(net_irq || uart_irq);

    if sliced {
        // Release our slice; the arbiter re-arms whatever else is outstanding (a
        // cell sleep, an RTO, the pacer) instead of disarming it.
        ktimer::cancel(TimerClient::RxPoll);
    }

    if halted {
        // SAFETY: single CPU.
        unsafe {
            *addr_of_mut!(HALTS) = (*addr_of!(HALTS)).wrapping_add(1);
            *addr_of_mut!(IDLED) = true;
        }
        // Credit the halt to the subsystem whose honesty counter the proofs read, so
        // a park that moved from the syscall into the scheduler is still visible as
        // the same genuine idle (docs/LIBRHEO.md Phase D/F, docs/NETSTACK.md 16).
        if src & TIMER != 0 && arch::timer_irq_enabled() {
            crate::time::mark_timer_idle();
        }
        if uart_irq {
            crate::input::mark_idle();
        }
        if net_irq || sliced {
            crate::net_rx::mark_idle();
        }
    } else {
        // Nothing halted: either no interrupt on this machine, a deadline already
        // due (the caller re-checks), or a remaining delta below the one-shot's
        // resolution. Advance the software clock a little so a deadline compared
        // against `ktimer::now_ns()` still elapses, and say plainly that we spun.
        // SAFETY: single CPU.
        unsafe {
            *addr_of_mut!(SPINS) = (*addr_of!(SPINS)).wrapping_add(1);
        }
        arch::spin_loop(1);
    }
    halted
}

/// One line naming each waitable source in `src` - used by the deadlock
/// diagnostic and by the idle-mode report. Pure function of the mask, so a test
/// can assert it without reaching a deadlock.
pub fn describe(src: Sources) -> &'static str {
    match src & (WAITABLE | PEER) {
        0 => "nothing",
        TIMER => "timer",
        NET => "net",
        CONSOLE => "console",
        PEER => "peer",
        s if s & WAITABLE == 0 => "peer",
        _ => "several",
    }
}
