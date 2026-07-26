//! `net::pacer` - the **send pacer** (docs/NETSTACK.md §21, rheo-net Phase N2e):
//! release bytes at the congestion controller's pacing rate instead of dumping a
//! whole window into the link.
//!
//! ## Pacing is a precondition for BBR, not a tuning knob
//!
//! BBR estimates the bottleneck bandwidth and then *sends at that rate*. If the
//! sender is unpaced, a cwnd-sized burst leaves at line rate of the sending NIC,
//! arrives at the bottleneck far faster than it drains, and builds exactly the
//! standing queue BBR exists to avoid. The measured RTT then rises, the model
//! degrades, and the flow behaves worse than plain CUBIC. Pacing is therefore part
//! of the algorithm: **an unpaced BBR is not a slower BBR, it is a broken one.**
//! [`CongestionControl::pacing_rate_bps`](crate::tcp::CongestionControl::pacing_rate_bps)
//! returning 0 is what keeps every window-based controller unpaced and unchanged.
//!
//! ## The mechanism: a token bucket plus one arbiter deadline
//!
//! [`Pacer`] is a **synchronous** integer token bucket - no I/O, no async, no
//! floating point - so it is as provable as the rest of the stack:
//!
//! - tokens accrue in **bytes** at `rate_bps`, capped at a small **burst** allowance
//!   ([`Pacer::burst_bytes`], `max(2*MSS, rate * BURST_NS)`; a bucket with no burst
//!   allowance cannot send a single segment until a full segment-time has passed,
//!   which would stall the connection at every window opening);
//! - [`Pacer::ready`] refills to `now` and answers "may these bytes go?";
//! - [`Pacer::next_send_at`] is the **deadline** the driver waits on.
//!
//! That deadline is the only new timer in the system, and it goes through the kernel
//! **timer arbiter** ([`park_for`], `librheo::time::sleep_pacing` ->
//! `SYS_ARM_TIMER` with `TIMER_CLIENT_PACER` -> `ktimer::TimerClient::Pacer`) -
//! never `arch::timer_*`, and never the cell-sleep slot. The pacer is the arbiter's
//! first **continuously re-armed** client: a paced flow registers a fresh deadline
//! after every release, for the life of the flow, so any subsystem that armed the
//! hardware directly would have its deadline destroyed within microseconds. That is
//! the conflict N2h's arbiter was built to remove, and N2e is the client that would
//! have made it fatal.
//!
//! ## A paced flow as a reservation (object 7) - what fits and what does not
//!
//! The tempting shape is "a paced flow *is* a reservation": the pacing rate is a
//! promised rate, and object 7 already does EDF admission. Half of that fits, and
//! the other half is a category error - both are stated plainly here and in §21.
//!
//! - **Does not fit: the byte rate.** A reservation admits *CPU time* against one
//!   core's capacity (`sum(budget_i/period_i) <= 1`, `kernel/src/sched.rs`). A
//!   pacing rate is bytes/second against a **link**, and the kernel holds no
//!   authority over link capacity: no attested line rate, no per-flow NIC rate
//!   limiter, no admission dimension for bytes. Admitting "40 Gb/s" would return a
//!   guarantee nothing can keep - the exact dishonesty the admission controller
//!   exists to refuse. It is also the wrong *granularity*: reservations are per
//!   **cell**, while a sharded transport cell owns many flows.
//! - **Does fit: the pacer's own CPU cost.** Pacing at rate R with segment size S
//!   wakes the cell every `S/R` seconds, and each wake costs real CPU. That is a
//!   periodic task with a budget and a period - precisely object 7's shape. So
//!   [`cpu_reservation_for`] converts a pacing rate into `(budget, period)`, and
//!   [`admit_pacing_cpu`] asks the kernel whether the cell **can afford to pace at
//!   that rate at all**. An absurd rate is refused cleanly (a 100 Gb/s pace wants a
//!   wake every 116 ns, which cannot fit a 2 us wake cost) instead of silently
//!   degenerating into a spin. This is a genuine use of object 7, and it is
//!   explicitly *not* a bandwidth reservation.
//!
//! What would make byte-rate admission real: an attested link capacity per NIC queue
//! (an engine-style attest-by-measurement figure), a per-flow rate limiter in the
//! steering table (the deferred N6 socket/steering object), and a per-flow rather
//! than per-cell reservation subject. Named in §21, not built here.
//!
//! ## Honest limits
//! - Enforcement of an admitted reservation is still SMP/preemption work (task #27):
//!   admission is real, scheduling is not. A pacing CPU reservation is an admitted
//!   guarantee, not yet a scheduled one.
//! - The pacer paces **data** segments. Pure ACKs, SYN/FIN control segments and
//!   retransmissions are released immediately: delaying an ACK slows the peer's
//!   clock, and delaying a retransmit extends a stall.
//! - `PACER_WAKEUP_NS` is a **declared** per-wake cost, not a measured one; QEMU
//!   yields instruction path lengths, not wall-clock time.
//! - **Two clocks, as in [`crate::timer`].** A cell has no nanosecond clock reading
//!   (`librheo::time::now()` is raw per-ISA ticks), so a driver keeps its own logical
//!   nanosecond clock, parks for the *delta* to the next deadline, and advances the
//!   clock to it. The kernel's one-shot is what makes the delay real; the logical
//!   clock is what makes it expressible. Deterministic proofs drive that clock by hand.
//! - The reactor still has a **single** timer request slot, so a strand pacing and a
//!   strand sleeping in the same cell interleave rather than wait together (a pre-N2e
//!   limitation - the *kernel* arbiter does keep them in separate slots, so no
//!   deadline is lost or falsely reported).

/// The burst window: the pacer lets `rate * BURST_NS` bytes (at least two segments)
/// leave back to back before it starts spacing them out. Linux's TCP pacing uses the
/// same idea (`sk_pacing_shift`, ~1 ms of bytes); without it a bucket could not
/// release even one MSS until a full segment-time had elapsed.
pub const BURST_NS: u64 = 1_000_000;

/// A declared cost for one pacing wake-up (register a deadline, park, wake, build
/// and submit one segment), in nanoseconds. Used by [`cpu_reservation_for`] to turn a
/// pacing rate into a CPU budget. **Declared, not measured**: under QEMU-TCG there is
/// no meaningful wall clock (docs/TOOLING.md), so this is a deliberately conservative
/// figure a hardware lab replaces.
pub const PACER_WAKEUP_NS: u64 = 2_000;

/// A token-bucket send pacer. Integer arithmetic, no floats, no I/O - see the module
/// docs for how it composes with the kernel timer arbiter.
#[derive(Copy, Clone, Debug, Default)]
pub struct Pacer {
    /// Release rate in bytes/second. **0 disables the pacer entirely** (every
    /// [`ready`](Self::ready) is `true`), which is what a window-based controller
    /// gets.
    rate_bps: u64,
    /// Accrued allowance, in bytes.
    tokens: u64,
    /// Cap on the allowance, in bytes.
    burst: u64,
    /// The last time tokens were refilled.
    last_ns: u64,
    /// Segment size used to size the burst floor.
    mss: u32,
    /// Releases granted (one per paced segment) - a proof counter.
    sends: u64,
    /// Bytes released.
    bytes: u64,
    /// Times a release was **deferred** because the bucket was short - i.e. the
    /// pacer genuinely paced rather than waving everything through.
    defers: u64,
}

impl Pacer {
    /// An unpaced pacer (`rate_bps == 0`): every release is allowed. This is the
    /// state a [`Connection`](crate::tcp::Connection) with a window-based controller
    /// stays in forever.
    pub const fn unpaced(mss: u16) -> Pacer {
        Pacer {
            rate_bps: 0,
            tokens: 0,
            burst: 0,
            last_ns: 0,
            mss: mss as u32,
            sends: 0,
            bytes: 0,
            defers: 0,
        }
    }

    /// A pacer releasing at `rate_bps` bytes/second, starting with a full burst
    /// allowance so the first window opening is not delayed.
    pub fn new(rate_bps: u64, mss: u16) -> Pacer {
        let mut p = Pacer::unpaced(mss);
        p.set_rate(rate_bps);
        p.tokens = p.burst;
        p
    }

    /// Set the release rate (bytes/second); `0` disables pacing. Recomputes the burst
    /// allowance and clamps the accrued tokens to it. Called every poll from the
    /// controller's [`pacing_rate_bps`](crate::tcp::CongestionControl::pacing_rate_bps),
    /// so a rate change (a BBR gain phase change) takes effect immediately.
    pub fn set_rate(&mut self, rate_bps: u64) {
        if rate_bps == 0 {
            self.rate_bps = 0;
            self.burst = 0;
            self.tokens = 0;
            return;
        }
        let first = self.rate_bps == 0;
        self.rate_bps = rate_bps;
        self.burst = Self::burst_for(rate_bps, self.mss);
        if first {
            // Newly paced: start with a full allowance rather than stalling.
            self.tokens = self.burst;
        } else if self.tokens > self.burst {
            self.tokens = self.burst;
        }
    }

    /// The burst allowance for a rate: `max(2*MSS, rate * BURST_NS)` bytes.
    fn burst_for(rate_bps: u64, mss: u32) -> u64 {
        let by_time = rate_bps.saturating_mul(BURST_NS) / 1_000_000_000;
        by_time.max(2 * mss as u64)
    }

    /// The current release rate (bytes/second); 0 = unpaced.
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// Whether pacing is active.
    pub fn is_paced(&self) -> bool {
        self.rate_bps != 0
    }

    /// The burst allowance in bytes (the most that may leave back to back).
    pub fn burst_bytes(&self) -> u64 {
        self.burst
    }

    /// Accrued allowance in bytes, as of the last refill.
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    /// Releases granted.
    pub fn sends(&self) -> u64 {
        self.sends
    }

    /// Bytes released.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Releases deferred for want of tokens - the count that shows the pacer really
    /// spaced sends out instead of passing them straight through.
    pub fn defers(&self) -> u64 {
        self.defers
    }

    /// Time to accrue `bytes` at the current rate, in nanoseconds (0 when unpaced).
    pub fn interval_ns(&self, bytes: u32) -> u64 {
        if self.rate_bps == 0 {
            return 0;
        }
        (bytes as u64).saturating_mul(1_000_000_000) / self.rate_bps.max(1)
    }

    /// Refill to `now` and report whether `bytes` may be released. Unpaced pacers
    /// always say yes. A `false` is counted in [`defers`](Self::defers) and the
    /// caller should wait until [`next_send_at`](Self::next_send_at).
    pub fn ready(&mut self, now_ns: u64, bytes: u32) -> bool {
        if self.rate_bps == 0 {
            return true;
        }
        self.refill(now_ns);
        if self.tokens >= bytes as u64 {
            true
        } else {
            self.defers += 1;
            false
        }
    }

    /// Account for a release of `bytes` at `now_ns` (call only after
    /// [`ready`](Self::ready) allowed it).
    pub fn on_sent(&mut self, now_ns: u64, bytes: u32) {
        if self.rate_bps == 0 {
            return;
        }
        self.refill(now_ns);
        self.tokens = self.tokens.saturating_sub(bytes as u64);
        self.sends += 1;
        self.bytes += bytes as u64;
    }

    /// The earliest time `bytes` may be released, given the allowance as of the last
    /// refill. `None` when unpaced or when it may go now - i.e. `Some` is exactly
    /// "there is a pacing deadline to wait on", which is what the driver registers
    /// with the arbiter's pacer slot.
    pub fn next_send_at(&self, bytes: u32) -> Option<u64> {
        if self.rate_bps == 0 {
            return None;
        }
        let need = bytes as u64;
        if self.tokens >= need {
            return None;
        }
        let deficit = need - self.tokens;
        // **Round the wait up.** Token accrual floors (`refill`), so a floored wait
        // can come up one byte short, and the caller then waits again for the same
        // deficit - a livelock that advances the clock in nanosecond steps and never
        // sends. `ceil` guarantees the refill after this deadline covers the deficit.
        let rate = self.rate_bps.max(1);
        let delay = deficit
            .saturating_mul(1_000_000_000)
            .saturating_add(rate - 1)
            / rate;
        Some(self.last_ns.saturating_add(delay.max(1)))
    }

    /// Inform the pacer of the negotiated MSS (it sizes the burst floor).
    pub fn set_mss(&mut self, mss: u16) {
        self.mss = mss as u32;
        if self.rate_bps != 0 {
            self.burst = Self::burst_for(self.rate_bps, self.mss);
        }
    }

    /// Accrue tokens for the time since the last refill, capped at the burst.
    fn refill(&mut self, now_ns: u64) {
        if now_ns <= self.last_ns {
            self.last_ns = self.last_ns.max(now_ns);
            return;
        }
        let dt = now_ns - self.last_ns;
        let gained = self.rate_bps.saturating_mul(dt) / 1_000_000_000;
        self.tokens = (self.tokens.saturating_add(gained)).min(self.burst);
        self.last_ns = now_ns;
    }
}

/// The `(budget, period)` a pacing rate implies as a **CPU** reservation (object 7):
/// one wake every `mss/rate` seconds, each costing `wakeup_ns`. Both are nanoseconds,
/// and admission is a *ratio* (`budget/period`), so the units only have to agree with
/// each other - see the module docs for why the *byte* rate is deliberately not what
/// gets admitted.
///
/// The period is floored at 1 ns so an absurd rate produces `budget > period`, which
/// the kernel refuses as `BadParams` rather than dividing by zero.
pub fn cpu_reservation_for(rate_bps: u64, mss: u16, wakeup_ns: u64) -> (u64, u64) {
    let period = (mss as u64)
        .saturating_mul(1_000_000_000)
        .checked_div(rate_bps)
        .map_or(0, |p| p.max(1));
    (wakeup_ns, period)
}

/// The CPU utilization (parts per million) pacing at `rate_bps` would cost, given
/// [`PACER_WAKEUP_NS`] per wake. Saturates at 1e6 ("a whole core, or more than one").
pub fn cpu_utilization_ppm(rate_bps: u64, mss: u16) -> u64 {
    let (budget, period) = cpu_reservation_for(rate_bps, mss, PACER_WAKEUP_NS);
    if period == 0 {
        return 0;
    }
    (budget.saturating_mul(1_000_000) / period).min(1_000_000)
}

/// Park until a pacing deadline, on the kernel timer arbiter's **pacer** slot
/// (docs/NETSTACK.md §21). `delay_ns` is the remaining time to the deadline; the
/// strand parks, the vcore runs the cell's other strands, and only when they have all
/// parked does the kernel arm the one-shot - so a paced flow costs no spin.
#[cfg(feature = "hosted")]
pub async fn park_for(delay_ns: u64) {
    if delay_ns == 0 {
        return;
    }
    librheo::rt::sleep_pacing_ns(delay_ns).await;
}

/// Ask the kernel to admit the **CPU** cost of pacing at `rate_bps` (object 7,
/// docs/NETSTACK.md §21). `Ok` means the cell can afford the wake-up rate; an
/// `Err(Overcommit)`/`Err(BadParams)` is a clean refusal, which is the honest answer
/// to "pace at 100 Gb/s from one cooperative vcore".
///
/// This admits CPU time, **not** link bandwidth - see the module docs.
#[cfg(feature = "hosted")]
pub fn admit_pacing_cpu(
    rate_bps: u64,
    mss: u16,
) -> Result<librheo::sched::Reservation, librheo::sched::ReserveError> {
    let (budget, period) = cpu_reservation_for(rate_bps, mss, PACER_WAKEUP_NS);
    librheo::sched::Reservation::request(budget, period, period, 0)
}
