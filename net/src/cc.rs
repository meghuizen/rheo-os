//! `net::cc` - real TCP **congestion control** over the N2a
//! [`CongestionControl`](crate::tcp::CongestionControl) seam (docs/NETSTACK.md §11,
//! rheo-net Phase N2b). Two controllers, both **from scratch**, both **integer /
//! fixed-point** (no floating point in the cwnd math - matching the kernel's
//! no-FPU discipline even though these are soft-float U-mode cells), both portable
//! (**no `cfg(target_arch)`**):
//!
//! - [`Reno`] - RFC 5681: slow-start, AIMD congestion avoidance, fast retransmit on
//!   3 duplicate ACKs, fast recovery (inflate/deflate), and RTO slow-start restart.
//! - [`Cubic`] - RFC 8312: the cubic window growth `W(t) = C*(t-K)^3 + W_max` with
//!   the TCP-friendly region, computed in **fixed point** with an integer cube root.
//!
//! Both plug into [`Connection<C>`](crate::tcp::Connection) unchanged: the send
//! window is `min(peer_window, cwnd())`, and the connection calls
//! [`tick`](crate::tcp::CongestionControl::tick) /
//! [`on_ack`](crate::tcp::CongestionControl::on_ack) /
//! [`on_dup_ack`](crate::tcp::CongestionControl::on_dup_ack) /
//! [`on_loss`](crate::tcp::CongestionControl::on_loss) as segments and the RTO
//! drive it.
//!
//! ## Simplifications (honest, documented)
//! - **AIMD is per-RTT byte-counted**: congestion avoidance adds one MSS per cwnd
//!   worth of newly-acked bytes (a byte accumulator), which is deterministic and
//!   independent of ACK coalescing - the clean form of `cwnd += MSS*MSS/cwnd`.
//! - **Reno, not full NewReno**: fast recovery exits on the first new ACK (partial-
//!   ACK deflation without full recovery is deferred). Good enough for the single-
//!   loss proof; multi-loss selective repair wants SACK (also deferred).
//! - **CUBIC is time-clocked, not ack-clocked**: `on_ack` sets cwnd to `W(t)` for
//!   the elapsed time `t` since the last congestion event, rather than the
//!   incremental `cwnd += (W(t+RTT) - cwnd)/cwnd` per ACK. Same trajectory, cleaner
//!   to pin against an oracle; the per-ack increment is the optimization.
//! - **HyStart and CUBIC fast-convergence are deferred** (documented, not built):
//!   pre-loss CUBIC stays in slow start until a loss, and a loss always saves
//!   `W_max = cwnd` (no fast-convergence discount). **BBR is a later phase.**

use crate::tcp::CongestionControl;

/// The default initial window (RFC 6928 allows up to 10 MSS; we use 10 MSS so a
/// fresh connection ramps quickly). The deterministic proofs construct controllers
/// with explicit small windows so the trajectory is crisp.
pub const INIT_WINDOW_SEGMENTS: u32 = 10;

/// Floor integer cube root of `n` (`floor(n^(1/3))`), by binary search. Used by
/// [`Cubic`] to compute `K` in fixed point. `n` here is bounded (a window in bytes
/// times a small constant), so the doubling search never overflows `u128`.
fn icbrt(n: u128) -> u128 {
    if n < 8 {
        return u128::from(n >= 1);
    }
    let mut hi = 1u128;
    while hi.saturating_mul(hi).saturating_mul(hi) <= n && hi < (1u128 << 43) {
        hi <<= 1;
    }
    let mut lo = hi >> 1;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if mid * mid * mid <= n {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

// ---------------------------------------------------------------------------
// Reno (RFC 5681)
// ---------------------------------------------------------------------------

/// TCP Reno congestion control (RFC 5681): slow start, AIMD, fast retransmit / fast
/// recovery on 3 duplicate ACKs, and an RTO slow-start restart. All integer math.
///
/// State machine, in bytes:
/// - **Slow start** (`cwnd < ssthresh`): `cwnd += bytes_acked` per ACK - exponential
///   growth, one doubling per RTT.
/// - **Congestion avoidance** (`cwnd >= ssthresh`): `cwnd += MSS` per cwnd worth of
///   bytes acked (a byte accumulator) - linear, one MSS per RTT.
/// - **Fast retransmit / recovery** (3rd dup ACK): `ssthresh = max(cwnd/2, 2*MSS)`,
///   `cwnd = ssthresh + 3*MSS` (inflate); each further dup ACK inflates by one MSS;
///   the first new ACK deflates to `cwnd = ssthresh` and exits recovery.
/// - **RTO** (`on_loss`): `ssthresh = max(cwnd/2, 2*MSS)`, `cwnd = 1*MSS`
///   (slow-start restart).
#[derive(Copy, Clone, Debug)]
pub struct Reno {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,
    /// Byte accumulator for the per-RTT congestion-avoidance increment.
    ca_acked: u32,
    /// Consecutive duplicate-ACK count (reset by any new cumulative ACK).
    dup_acks: u32,
    recovery: bool,
}

impl Reno {
    /// A Reno controller for the given MSS, at the default initial window and an
    /// effectively unbounded initial slow-start threshold (the first loss sets it).
    pub fn new(mss: u16) -> Reno {
        let mss = mss as u32;
        Reno {
            cwnd: INIT_WINDOW_SEGMENTS * mss,
            ssthresh: u32::MAX,
            mss,
            ca_acked: 0,
            dup_acks: 0,
            recovery: false,
        }
    }

    /// Construct with explicit `cwnd` / `ssthresh` (bytes) - the deterministic
    /// proofs use this to pin the slow-start -> AIMD transition point.
    pub fn with_params(mss: u16, cwnd: u32, ssthresh: u32) -> Reno {
        Reno {
            cwnd,
            ssthresh,
            mss: mss as u32,
            ca_acked: 0,
            dup_acks: 0,
            recovery: false,
        }
    }

    /// Override the congestion window (bytes) - for tests / a warm start.
    pub fn set_cwnd(&mut self, cwnd: u32) {
        self.cwnd = cwnd;
    }
    /// Override the slow-start threshold (bytes).
    pub fn set_ssthresh(&mut self, ssthresh: u32) {
        self.ssthresh = ssthresh;
    }

    /// `ssthresh = max(cwnd/2, 2*MSS)` - the multiplicative-decrease target shared by
    /// the fast-recovery and RTO paths.
    fn halved_ssthresh(&self) -> u32 {
        (self.cwnd / 2).max(2 * self.mss)
    }
}

impl Default for Reno {
    fn default() -> Reno {
        Reno::new(crate::tcp::DEFAULT_MSS)
    }
}

impl CongestionControl for Reno {
    fn on_ack(&mut self, bytes_acked: u32, _rtt_ns: Option<u64>) {
        if bytes_acked == 0 {
            return;
        }
        // A new cumulative ACK: recovery (if any) ends, dup counter resets.
        if self.recovery {
            self.cwnd = self.ssthresh; // deflate
            self.recovery = false;
            self.dup_acks = 0;
            return;
        }
        self.dup_acks = 0;
        if self.cwnd < self.ssthresh {
            // Slow start: exponential (one doubling per RTT).
            self.cwnd = self.cwnd.saturating_add(bytes_acked);
        } else {
            // Congestion avoidance: +1 MSS per cwnd worth of acked bytes (AIMD).
            self.ca_acked = self.ca_acked.saturating_add(bytes_acked);
            while self.ca_acked >= self.cwnd {
                self.ca_acked -= self.cwnd;
                self.cwnd = self.cwnd.saturating_add(self.mss);
            }
        }
    }

    fn on_dup_ack(&mut self) -> bool {
        if self.recovery {
            // Fast recovery: each additional dup ACK inflates cwnd by one MSS
            // (a segment left the network, so one more may be sent).
            self.cwnd = self.cwnd.saturating_add(self.mss);
            return false;
        }
        self.dup_acks += 1;
        if self.dup_acks == 3 {
            self.ssthresh = self.halved_ssthresh();
            self.cwnd = self.ssthresh + 3 * self.mss; // inflate by the 3 dup ACKs
            self.recovery = true;
            return true; // fast-retransmit the lost segment now, before the RTO
        }
        false
    }

    fn on_loss(&mut self) {
        // RTO: collapse to one MSS and slow-start restart.
        self.ssthresh = self.halved_ssthresh();
        self.cwnd = self.mss;
        self.ca_acked = 0;
        self.dup_acks = 0;
        self.recovery = false;
    }

    fn cwnd(&self) -> u32 {
        self.cwnd
    }
    fn ssthresh(&self) -> u32 {
        self.ssthresh
    }
    fn in_recovery(&self) -> bool {
        self.recovery
    }
    fn set_mss(&mut self, mss: u16) {
        self.mss = mss as u32;
    }
}

// ---------------------------------------------------------------------------
// CUBIC (RFC 8312)
// ---------------------------------------------------------------------------

/// TCP CUBIC congestion control (RFC 8312) in **fixed point**. The window follows a
/// cubic function of the time since the last congestion event:
///
/// ```text
///   W(t) = C * (t - K)^3 + W_max ,   K = cbrt( W_max * (1 - beta) / C )
/// ```
///
/// with `beta = 0.7` (the window-decrease factor) and `C = 0.4`. It is **concave**
/// as it approaches `W_max` (the pre-loss window) and **convex** past it - probing
/// for more bandwidth aggressively only once it is confident the old operating point
/// is safe. The **TCP-friendly region** `W_est(t) = W_max*beta + (3*(1-beta)/(1+beta))
/// * t/RTT` guards a lower bound so CUBIC never underperforms Reno.
///
/// ## The fixed-point scheme (correctness-critical)
/// No floats. Windows are in **bytes**; time in the cubic term is in **milliseconds**.
/// With `C = 0.4 = 2/5` and `beta = 0.7 = 7/10`:
/// - `K` (ms) `= cbrt( W_max_segments * (1-beta)/C * 1e9 ) = cbrt( 3 * W_max_seg *
///   250_000_000 )`, via the integer [`icbrt`].
/// - the cubic term (bytes) `= MSS * C * (dt_ms/1000)^3 = (2 * MSS * dt_ms^3) /
///   5_000_000_000`, computed in `i128` so `dt_ms^3` never overflows.
/// - `W_est` base `= (7*W_max)/10`, slope-per-RTT `= (9*MSS)/17` (since
///   `3*(1-beta)/(1+beta) = 9/17`).
///
/// The proof pins `W(t)` at sampled times against a precomputed integer oracle and
/// checks it stays within a few bytes of the real-valued cubic.
#[derive(Copy, Clone, Debug)]
pub struct Cubic {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,
    /// The window at the last congestion event (bytes) - the cubic origin.
    w_max: u32,
    /// `K` in milliseconds (the inflection offset), recomputed on each loss.
    k_ms: u64,
    /// The logical time of the last congestion event (nanoseconds).
    epoch_ns: u64,
    /// The controller's current time (nanoseconds), set by [`tick`].
    now_ns: u64,
    /// The current RTT estimate (nanoseconds) for the TCP-friendly region.
    rtt_ns: u64,
    dup_acks: u32,
    recovery: bool,
}

impl Cubic {
    /// A CUBIC controller for the given MSS, at the default initial window, in slow
    /// start (no congestion event yet).
    pub fn new(mss: u16) -> Cubic {
        let mss = mss as u32;
        Cubic {
            cwnd: INIT_WINDOW_SEGMENTS * mss,
            ssthresh: u32::MAX,
            mss,
            w_max: 0,
            k_ms: 0,
            epoch_ns: 0,
            now_ns: 0,
            rtt_ns: 100_000_000, // 100 ms default until an RTT sample lands
            dup_acks: 0,
            recovery: false,
        }
    }

    /// Set the RTT estimate (nanoseconds) used by the TCP-friendly region.
    pub fn set_rtt(&mut self, rtt_ns: u64) {
        self.rtt_ns = rtt_ns.max(1);
    }

    /// Enter the cubic region as if a loss had just reduced the window from
    /// `w_max` (bytes) at time `epoch_ns`: `cwnd = ssthresh = w_max*beta`, `K`
    /// recomputed. The deterministic shape proof uses this to pin a known `W_max`.
    pub fn set_epoch(&mut self, w_max: u32, epoch_ns: u64) {
        self.w_max = w_max;
        self.ssthresh = ((w_max as u64 * 7) / 10).max(2 * self.mss as u64) as u32;
        self.cwnd = self.ssthresh;
        self.epoch_ns = epoch_ns;
        self.now_ns = epoch_ns;
        self.recovery = false;
        self.dup_acks = 0;
        self.recompute_k();
    }

    /// `K = cbrt( 3 * W_max_segments * 250_000_000 )` milliseconds (fixed point).
    fn recompute_k(&mut self) {
        let w_max_seg = (self.w_max / self.mss.max(1)) as u128;
        self.k_ms = icbrt(3 * w_max_seg * 250_000_000) as u64;
    }

    /// `W_max*beta` - the multiplicative-decrease target (bytes).
    fn beta_window(&self) -> u32 {
        ((self.cwnd as u64 * 7) / 10).max(2 * self.mss as u64) as u32
    }

    /// The cubic window `W(t) = C*(t-K)^3 + W_max` in bytes, `t` = ns since epoch.
    fn w_cubic(&self, t_ns: u64) -> u32 {
        let t_ms = (t_ns / 1_000_000) as i128;
        let dt = t_ms - self.k_ms as i128;
        // (2 * MSS * dt^3) / 5e9  ==  MSS * 0.4 * (dt_ms/1000)^3   (bytes)
        let term = (2 * self.mss as i128 * dt * dt * dt) / 5_000_000_000;
        let w = self.w_max as i128 + term;
        w.clamp(self.mss as i128, u32::MAX as i128) as u32
    }

    /// The TCP-friendly estimate `W_est(t)` in bytes (the lower bound CUBIC honors).
    fn w_est(&self, t_ns: u64) -> u32 {
        let base = (self.w_max as u64 * 7) / 10; // W_max * beta
        let per_rtt = (9 * self.mss as u64) / 17; // 3*(1-beta)/(1+beta) segments
        let rtts = t_ns / self.rtt_ns.max(1);
        (base + per_rtt * rtts).min(u32::MAX as u64) as u32
    }

    /// The current target window `max(W_cubic(t), W_est(t))` at the controller's
    /// present time (exposed for the deterministic oracle).
    pub fn target(&self) -> u32 {
        let t = self.now_ns.saturating_sub(self.epoch_ns);
        self.w_cubic(t).max(self.w_est(t))
    }
}

impl Default for Cubic {
    fn default() -> Cubic {
        Cubic::new(crate::tcp::DEFAULT_MSS)
    }
}

impl CongestionControl for Cubic {
    fn tick(&mut self, now_ns: u64) {
        self.now_ns = now_ns;
    }

    fn on_ack(&mut self, bytes_acked: u32, rtt_ns: Option<u64>) {
        if let Some(r) = rtt_ns {
            self.rtt_ns = r.clamp(1_000_000, 1_000_000_000); // 1 ms .. 1 s
        }
        if bytes_acked == 0 {
            return;
        }
        if self.recovery {
            self.cwnd = self.ssthresh; // deflate on the first new ACK
            self.recovery = false;
            self.dup_acks = 0;
            return;
        }
        self.dup_acks = 0;
        if self.cwnd < self.ssthresh {
            // Slow start (pre-loss, or an RTO restart): exponential.
            self.cwnd = self.cwnd.saturating_add(bytes_acked);
        } else {
            // Cubic region: grow toward W(t) (monotonically between losses).
            let target = self.target();
            if target > self.cwnd {
                self.cwnd = target;
            }
        }
    }

    fn on_dup_ack(&mut self) -> bool {
        if self.recovery {
            self.cwnd = self.cwnd.saturating_add(self.mss);
            return false;
        }
        self.dup_acks += 1;
        if self.dup_acks == 3 {
            // Multiplicative decrease by beta (0.7) - gentler than Reno's 0.5.
            self.ssthresh = self.beta_window();
            self.w_max = self.cwnd;
            self.cwnd = self.ssthresh + 3 * self.mss; // inflate
            self.epoch_ns = self.now_ns;
            self.recompute_k();
            self.recovery = true;
            return true;
        }
        false
    }

    fn on_loss(&mut self) {
        // RTO: save W_max, slow-start restart to one MSS.
        self.ssthresh = self.beta_window();
        self.w_max = self.cwnd;
        self.cwnd = self.mss;
        self.epoch_ns = self.now_ns;
        self.recovery = false;
        self.dup_acks = 0;
        self.recompute_k();
    }

    fn cwnd(&self) -> u32 {
        self.cwnd
    }
    fn ssthresh(&self) -> u32 {
        self.ssthresh
    }
    fn in_recovery(&self) -> bool {
        self.recovery
    }
    fn set_mss(&mut self, mss: u16) {
        self.mss = mss as u32;
    }
}
