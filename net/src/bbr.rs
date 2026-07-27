//! `net::bbr` - **BBRv3**, rheo-net's *default* congestion control
//! (docs/NETSTACK.md §21, rheo-net Phase N2e). From scratch, integer / fixed-point
//! (no floats - these are soft-float cells), portable (**no `cfg(target_arch)`**),
//! and in the crate's always-compiled half: it is a synchronous state machine, like
//! [`crate::tcp`] itself.
//!
//! ## Why the default is not loss-based
//!
//! Reno and CUBIC infer congestion from **loss**. That inference is wrong in three
//! places this stack is aimed at:
//!
//! - **High-BDP paths.** On a 100 ms, 10 Gb/s path a window is ~125 MB. A single
//!   loss halves (Reno) or 0.7-scales (CUBIC) it, and rebuilding takes hundreds of
//!   round trips - minutes of degraded throughput for one corrupted packet.
//! - **Lossy links.** On wireless, loss is mostly radio, not queueing. A loss-based
//!   controller reads interference as congestion and gives up bandwidth that was
//!   never contended: **loss != congestion**.
//! - **Bufferbloat.** A loss-based controller only backs off once a buffer has
//!   *overflowed*, so it deliberately keeps deep queues full - latency is the cost.
//!
//! BBR instead builds a **model** of the path - a maximum delivery rate
//! (`max_bw`) and a minimum RTT (`min_rtt`) - and sends at that rate with about one
//! BDP in flight. Loss is not the signal; it only caps in flight.
//!
//! ## Native to this OS, not a Linux transplant
//!
//! - **The ACK clock is the completion clock.** A delivery-rate sample
//!   ([`crate::tcp::RateSample`]) falls out of the send/ack bookkeeping in
//!   [`crate::tcp::Connection`]; in a hosted cell those ACKs arrive as queue
//!   completions (the CQ entry carries the flow id), so BBR's clock *is* the
//!   completion clock, with no side-channel instrumentation.
//! - **Pacing is a kernel-arbiter deadline.** The pacer ([`crate::pacer`]) registers
//!   its release deadline in the timer arbiter's dedicated pacer slot - the first
//!   continuously re-armed client in the system.
//! - **Per-cell, shared-nothing.** There is no global CC state: a controller lives
//!   inside its `Connection`, inside its shard, inside its cell (N2c). A flood or a
//!   misbehaving peer cannot perturb another tenant's model.
//!
//! ## Reference parameters
//!
//! The gain and window figures below follow **CloudBridge's published BBRv3
//! findings** (Habr article 964556) - *their* measurements, not ours: BBRv3 as
//! production-ready, roughly 1:2 throughput versus CUBIC and 1:3 versus Reno, an
//! initial window of **10 x MSS**, a startup pacing gain of **2.77** (about 90% of
//! path bandwidth within 10 ms), a drain gain of **0.75**, a fairness-mode pacing
//! gain of **0.95**, and a **10 s** min-RTT window. We reproduce the parameters and
//! the mechanism; we do not reproduce their throughput numbers, which are wall-clock
//! measurements on real links (see §21 on what QEMU can and cannot show).
//!
//! ## Simplifications versus the IETF BBR draft (honest, complete)
//!
//! 1. **No ECN.** BBRv3 responds to ECN marks as a congestion signal with its own
//!    `ecn_thresh`/`ecn_alpha`. rheo-net's IPv4 layer does not negotiate ECN, so
//!    only loss is modelled. The seam is the same
//!    ([`Bbr::on_loss`]/[`Bbr::on_dup_ack`]).
//! 2. **Loss accounting is per-signal, not per-byte.** Without SACK (deferred since
//!    N2a) the stack cannot count exactly which bytes were lost, so each loss signal
//!    is charged as **one MSS** in the per-round loss ratio.
//! 3. **`inflight_hi` / `inflight_lo` are one variable.** The draft keeps a
//!    short-term (`lo`) and long-term (`hi`) in-flight bound with their own probing
//!    rules; here a single [`Bbr::inflight_hi`] is trimmed by `beta` on loss (at most
//!    once per round) and raised when a probe refills.
//! 4. **ProbeBW phase durations are deterministic, not randomised.** The draft
//!    randomises the cruise duration so competing flows desynchronise; a fixed
//!    duration ([`profile::CRUISE_NS`]) makes the cycle provable. Real deployments
//!    want the randomisation - it is a one-line change over the per-cell DRBG and is
//!    named as future work.
//! 5. **ProbeRTT starts its dwell at entry.** The draft first waits for in-flight to
//!    fall to the ProbeRTT cap and *then* holds for 200 ms + one round; here the
//!    cap applies immediately and the dwell (`max(duration, one round)`) starts at
//!    entry.
//! 6. **ProbeBW-Up exits on an in-flight bound or two rounds**, rather than the
//!    draft's full bandwidth-growth/queue-inference test.
//! 7. **No cwnd-bound "packet conservation" during recovery**: rheo-net has no
//!    SACK-driven recovery state machine to conserve against. Loss keeps the model
//!    and caps in-flight, which is the property that matters here.
//! 8. **Startup exits on a bandwidth plateau or an excessive-loss round**; the
//!    draft's full startup exit also weighs ECN and a queue estimate.
//!
//! Nothing above changes the property the phase exists to prove: a loss episode with
//! no queue build-up leaves the delivery rate intact.

use crate::tcp::{CongestionControl, DEFAULT_MSS, RateSample};

/// Fixed-point denominator for every gain in this module: gains are hundredths, so
/// `277` is 2.77x. Integer only - no floats anywhere in the model.
pub const UNIT: u64 = 100;

/// Startup pacing gain: **2.77x** the measured rate, so the send rate doubles
/// roughly every round trip until the path's bandwidth is found (CloudBridge report
/// ~90% of bandwidth within 10 ms with this gain).
pub const STARTUP_PACING_GAIN: u64 = 277;
/// Startup cwnd gain: 2 BDP in flight while probing.
pub const STARTUP_CWND_GAIN: u64 = 200;
/// Drain pacing gain: **0.75x**, to empty the queue Startup's overshoot created.
pub const DRAIN_PACING_GAIN: u64 = 75;
/// ProbeBW *down* pacing gain: 0.9x, easing in-flight back toward the BDP.
pub const PROBE_DOWN_GAIN: u64 = 90;
/// ProbeBW *cruise* pacing gain: **0.95x** - the fairness-mode gain (CloudBridge),
/// sending slightly under the estimate so a competing flow can take its share.
pub const CRUISE_GAIN: u64 = 95;
/// ProbeBW *refill* pacing gain: 1.0x, one round at the estimate before probing up.
pub const REFILL_GAIN: u64 = 100;
/// ProbeBW *probe-up* pacing gain: 1.25x, briefly exceeding the estimate to find new
/// bandwidth.
pub const PROBE_UP_GAIN: u64 = 125;
/// ProbeRTT cwnd gain: 0.5 BDP (floored at [`MIN_CWND_SEGMENTS`]) - drain the queue
/// far enough to measure the true propagation delay.
pub const PROBE_RTT_CWND_GAIN: u64 = 50;
/// Loss response: trim the in-flight cap by 30% (**not** the bandwidth estimate, and
/// **not** a window halving) - at most once per round.
pub const LOSS_BETA_PCT: u64 = 30;
/// Startup's bandwidth-plateau test: a round counts as "still growing" if the
/// estimate reached 1.25x the plateau mark.
pub const FULL_BW_THRESH: u64 = 125;
/// Consecutive non-growing rounds that end Startup.
pub const FULL_BW_ROUNDS: u32 = 3;
/// Per-round loss ratio (percent) that also ends Startup - the path is clearly full.
pub const STARTUP_LOSS_THRESH_PCT: u64 = 2;
/// Floor on the congestion window, in segments (BBR always allows a little in
/// flight, even in ProbeRTT).
pub const MIN_CWND_SEGMENTS: u32 = 4;
/// Initial window in segments: **10 x MSS** (CloudBridge's figure, and RFC 6928's
/// allowance) - the same constant the N2b controllers use, so a connection's initial
/// window does not depend on which controller it picked.
pub const INIT_WINDOW_SEGMENTS: u32 = crate::cc::INIT_WINDOW_SEGMENTS;
/// The RTT assumed before any sample, for the initial pacing rate and BDP (the
/// initial window over this RTT). 100 ms is deliberately conservative.
pub const INIT_RTT_NS: u64 = 100_000_000;

/// Profile tunings (docs/NETSTACK.md §21). One arm is compiled, chosen by the
/// crate's profile features with the precedence **hft > warehouse > edge/embedded**:
///
/// - **hft** - latency first: a short min-RTT window and frequent, short ProbeRTT
///   (so the model can never sit on a stale, queue-inflated RTT), a tight in-flight
///   cap, a short bandwidth window, and a short cruise so pacing stays strict.
/// - **warehouse** - throughput first: a long bandwidth window (a jumbo-framed bulk
///   flow's rate must survive a slow round), a large in-flight allowance, and a long
///   cruise so the flow spends most of its time at the estimate.
/// - **edge / embedded (the default)** - the balanced draft-shaped values.
///
/// Only the **edge** arm is exercised in QEMU (the test kernels build with default
/// features); the others are compile-selected and checked to build. Honest.
pub mod profile {
    /// hft: latency-first tunings.
    #[cfg(feature = "hft")]
    mod sel {
        pub const NAME: &str = "hft";
        pub const MIN_RTT_WINDOW_NS: u64 = 2_000_000_000;
        pub const BW_WINDOW_ROUNDS: usize = 6;
        pub const PROBE_RTT_DURATION_NS: u64 = 50_000_000;
        pub const CRUISE_NS: u64 = 500_000_000;
        pub const PROBE_BW_CWND_GAIN: u64 = 150;
    }
    /// warehouse: throughput-first tunings.
    #[cfg(all(feature = "warehouse", not(feature = "hft")))]
    mod sel {
        pub const NAME: &str = "warehouse";
        pub const MIN_RTT_WINDOW_NS: u64 = 10_000_000_000;
        pub const BW_WINDOW_ROUNDS: usize = 20;
        pub const PROBE_RTT_DURATION_NS: u64 = 200_000_000;
        pub const CRUISE_NS: u64 = 4_000_000_000;
        pub const PROBE_BW_CWND_GAIN: u64 = 250;
    }
    /// edge / embedded: the balanced default.
    #[cfg(all(not(feature = "hft"), not(feature = "warehouse")))]
    mod sel {
        pub const NAME: &str = "edge";
        pub const MIN_RTT_WINDOW_NS: u64 = 10_000_000_000;
        pub const BW_WINDOW_ROUNDS: usize = 10;
        pub const PROBE_RTT_DURATION_NS: u64 = 200_000_000;
        pub const CRUISE_NS: u64 = 2_000_000_000;
        pub const PROBE_BW_CWND_GAIN: u64 = 200;
    }

    /// Rounds the max-bandwidth filter remembers.
    pub use sel::BW_WINDOW_ROUNDS;
    /// ProbeBW cruise duration before the next bandwidth probe.
    pub use sel::CRUISE_NS;
    /// The min-RTT filter window: a stale min-RTT triggers ProbeRTT.
    pub use sel::MIN_RTT_WINDOW_NS;
    /// The compiled profile's name (`"edge"`, `"hft"`, `"warehouse"`).
    pub use sel::NAME;
    /// ProbeBW in-flight allowance, as a gain on the BDP.
    pub use sel::PROBE_BW_CWND_GAIN;
    /// How long ProbeRTT holds the reduced in-flight cap.
    pub use sel::PROBE_RTT_DURATION_NS;
}

/// The BBR state machine (docs/NETSTACK.md §21).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BbrState {
    /// Ramp: pace at 2.77x the estimate, doubling per round, until the bandwidth
    /// estimate stops growing (or a round loses too much).
    Startup,
    /// Give back Startup's overshoot: pace at 0.75x until in-flight is one BDP.
    Drain,
    /// Steady state: cruise near the estimate, probing periodically for more.
    ProbeBw(ProbePhase),
    /// Periodically drain the queue right down to re-measure the propagation RTT.
    ProbeRtt,
}

/// The four phases of the ProbeBW cycle.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ProbePhase {
    /// 0.9x: ease in-flight back down to the BDP after a probe.
    Down,
    /// 0.95x: the fairness-mode steady state.
    Cruise,
    /// 1.0x for one round: refill the pipe before probing up.
    Refill,
    /// 1.25x: probe for more bandwidth.
    Up,
}

/// BBRv3 congestion control: a rate-based controller with a max-bandwidth filter, a
/// windowed min-RTT filter, round-trip counting, a pacing rate, and an in-flight cap.
/// See the module docs for the model, the gains and every simplification.
#[derive(Copy, Clone, Debug)]
pub struct Bbr {
    mss: u32,
    state: BbrState,

    // --- the model ---
    /// Per-round maximum delivery rate (bytes/s); the filter's value is the max over
    /// the ring, so a sample **expires** after `BW_WINDOW_ROUNDS` rounds.
    bw_ring: [u64; profile::BW_WINDOW_ROUNDS],
    /// Windowed minimum RTT (ns) and when it was taken.
    min_rtt_ns: u64,
    min_rtt_stamp_ns: u64,
    has_min_rtt: bool,

    // --- round-trip counting ---
    rounds: u64,
    next_round_delivered: u64,
    round_start: bool,

    // --- startup plateau detection ---
    full_bw: u64,
    full_bw_count: u32,
    full_bw_reached: bool,

    // --- ProbeBW cycle ---
    cycle_stamp_ns: u64,
    phase_round: u64,

    // --- ProbeRTT ---
    probe_rtt_done_ns: u64,
    probe_rtt_round: u64,
    probe_rtt_entries: u64,

    // --- windows / rates ---
    cwnd: u32,
    inflight_hi: u32,
    inflight: u32,
    pacing_rate: u64,
    pacing_gain: u64,
    cwnd_gain: u64,

    // --- loss accounting (per round) ---
    lost_in_round: u32,
    delivered_in_round: u32,
    last_loss_round: u64,
    loss_events: u64,
    dup_acks: u32,

    now_ns: u64,
}

impl Bbr {
    /// A fresh controller for `mss`: Startup, the 10 x MSS initial window, no model
    /// yet (the pacing rate is the initial window over [`INIT_RTT_NS`] until the first
    /// delivery-rate sample lands).
    pub fn new(mss: u16) -> Bbr {
        let mss = mss as u32;
        let mut b = Bbr {
            mss,
            state: BbrState::Startup,
            bw_ring: [0; profile::BW_WINDOW_ROUNDS],
            min_rtt_ns: 0,
            min_rtt_stamp_ns: 0,
            has_min_rtt: false,
            rounds: 0,
            next_round_delivered: 0,
            round_start: false,
            full_bw: 0,
            full_bw_count: 0,
            full_bw_reached: false,
            cycle_stamp_ns: 0,
            phase_round: 0,
            probe_rtt_done_ns: 0,
            probe_rtt_round: 0,
            probe_rtt_entries: 0,
            cwnd: INIT_WINDOW_SEGMENTS * mss,
            inflight_hi: u32::MAX,
            inflight: 0,
            pacing_rate: 0,
            pacing_gain: STARTUP_PACING_GAIN,
            cwnd_gain: STARTUP_CWND_GAIN,
            lost_in_round: 0,
            delivered_in_round: 0,
            last_loss_round: u64::MAX,
            loss_events: 0,
            dup_acks: 0,
            now_ns: 0,
        };
        b.update_pacing_rate();
        b
    }

    // ---- inspection (the deterministic proofs read these) ----

    /// The current state (and ProbeBW phase).
    pub fn state(&self) -> BbrState {
        self.state
    }
    /// The current pacing gain, in hundredths (`277` = 2.77x).
    pub fn pacing_gain(&self) -> u64 {
        self.pacing_gain
    }
    /// The current cwnd gain, in hundredths.
    pub fn cwnd_gain(&self) -> u64 {
        self.cwnd_gain
    }
    /// The max-bandwidth filter's value (bytes/s): the maximum over the last
    /// [`profile::BW_WINDOW_ROUNDS`] rounds.
    pub fn max_bw(&self) -> u64 {
        self.bw_ring.iter().copied().max().unwrap_or(0)
    }
    /// The bandwidth-delay product implied by the model, in bytes.
    pub fn bdp_bytes(&self) -> u32 {
        let bw = self.max_bw();
        let rtt = if self.has_min_rtt {
            self.min_rtt_ns
        } else {
            INIT_RTT_NS
        };
        if bw == 0 {
            return INIT_WINDOW_SEGMENTS * self.mss;
        }
        let bdp = (bw as u128 * rtt as u128) / 1_000_000_000;
        bdp.min(u32::MAX as u128) as u32
    }
    /// The in-flight ceiling (`u32::MAX` until a loss trims it).
    pub fn inflight_hi(&self) -> u32 {
        self.inflight_hi
    }
    /// Whether Startup's bandwidth plateau has been detected.
    pub fn full_bw_reached(&self) -> bool {
        self.full_bw_reached
    }
    /// Times ProbeRTT has been entered.
    pub fn probe_rtt_entries(&self) -> u64 {
        self.probe_rtt_entries
    }
    /// Loss signals seen (RTOs plus fast-retransmit triggers).
    pub fn loss_events(&self) -> u64 {
        self.loss_events
    }
    /// Whether the last sample started a new round trip.
    pub fn round_started(&self) -> bool {
        self.round_start
    }
    /// The controller's current time (nanoseconds).
    pub fn now_ns(&self) -> u64 {
        self.now_ns
    }
    /// The floor on the congestion window, in bytes.
    pub fn min_cwnd(&self) -> u32 {
        MIN_CWND_SEGMENTS * self.mss
    }

    // ---- the model ----

    /// Round-trip counting: a round ends when an ACK arrives for data that was sent
    /// after the previous round's mark (BBR's `round_start`).
    fn update_round(&mut self, rs: &RateSample) {
        self.round_start = false;
        if rs.prior_delivered >= self.next_round_delivered {
            self.next_round_delivered = rs.delivered;
            self.rounds += 1;
            self.round_start = true;
            // The new round starts with a clean bandwidth slot: the oldest slot in the
            // ring is dropped, which is how the max filter *expires* samples.
            let idx = (self.rounds as usize) % profile::BW_WINDOW_ROUNDS;
            self.bw_ring[idx] = 0;
            self.lost_in_round = 0;
            self.delivered_in_round = 0;
        }
    }

    /// The windowed min-RTT filter: take any lower sample, and take a *higher* one
    /// once the window has elapsed (that expiry is what makes it a windowed filter
    /// rather than an all-time minimum, and it is what ProbeRTT exists to refresh).
    fn update_min_rtt(&mut self, rs: &RateSample) {
        let Some(r) = rs.rtt_ns else { return };
        if r == 0 {
            return;
        }
        let stale = self.now_ns.saturating_sub(self.min_rtt_stamp_ns) > profile::MIN_RTT_WINDOW_NS;
        if !self.has_min_rtt || r <= self.min_rtt_ns || stale {
            self.min_rtt_ns = r;
            self.min_rtt_stamp_ns = self.now_ns;
            self.has_min_rtt = true;
        }
    }

    /// The max-bandwidth filter: record this sample's delivery rate in the current
    /// round's slot. An **application-limited** sample is only taken if it is at least
    /// as high as the current estimate - it measures the sender, not the path.
    fn update_bw(&mut self, rs: &RateSample) {
        let rate = rs.rate_bps();
        if rate == 0 {
            return;
        }
        if rs.app_limited && rate < self.max_bw() {
            return;
        }
        let idx = (self.rounds as usize) % profile::BW_WINDOW_ROUNDS;
        if rate > self.bw_ring[idx] {
            self.bw_ring[idx] = rate;
        }
    }

    /// Startup's exit test: three consecutive rounds without the estimate growing by
    /// 25%, or a round that lost more than [`STARTUP_LOSS_THRESH_PCT`] of what it
    /// delivered.
    fn check_full_bw(&mut self) {
        if self.full_bw_reached {
            return;
        }
        let lossy = self.delivered_in_round > 0
            && (self.lost_in_round as u64) * 100
                > (self.delivered_in_round as u64) * STARTUP_LOSS_THRESH_PCT;
        if lossy {
            self.full_bw_reached = true;
            return;
        }
        let bw = self.max_bw();
        if bw >= self.full_bw.saturating_mul(FULL_BW_THRESH) / UNIT {
            self.full_bw = bw;
            self.full_bw_count = 0;
            return;
        }
        self.full_bw_count += 1;
        if self.full_bw_count >= FULL_BW_ROUNDS {
            self.full_bw_reached = true;
        }
    }

    /// Enter ProbeBW at the start of a Down phase.
    fn enter_probe_bw(&mut self) {
        self.state = BbrState::ProbeBw(ProbePhase::Down);
        self.cycle_stamp_ns = self.now_ns;
        self.phase_round = self.rounds;
    }

    fn set_phase(&mut self, p: ProbePhase) {
        self.state = BbrState::ProbeBw(p);
        self.cycle_stamp_ns = self.now_ns;
        self.phase_round = self.rounds;
        if p == ProbePhase::Refill {
            // A refill re-opens the in-flight allowance a previous loss trimmed: this
            // is the single-variable stand-in for the draft's inflight_hi probing.
            let target = (self.bdp_bytes() as u64 * PROBE_UP_GAIN / UNIT) as u32;
            if self.inflight_hi != u32::MAX {
                self.inflight_hi = self.inflight_hi.max(target);
            }
        }
    }

    /// Enter ProbeRTT: cap in-flight hard and hold for the dwell (the state also
    /// refreshes the min-RTT filter, because a drained queue is the only place the
    /// propagation delay is visible).
    fn enter_probe_rtt(&mut self) {
        self.state = BbrState::ProbeRtt;
        self.probe_rtt_done_ns = self.now_ns + profile::PROBE_RTT_DURATION_NS;
        self.probe_rtt_round = self.rounds;
        self.probe_rtt_entries += 1;
    }

    /// ProbeRTT entry (a stale min-RTT) and exit (the dwell elapsed **and** a round
    /// has passed).
    fn check_probe_rtt(&mut self) {
        if self.state == BbrState::ProbeRtt {
            if self.now_ns >= self.probe_rtt_done_ns && self.rounds > self.probe_rtt_round {
                // Restart the min-RTT window from now: whatever RTT this state
                // measured is the fresh one.
                self.min_rtt_stamp_ns = self.now_ns;
                if self.full_bw_reached {
                    self.enter_probe_bw();
                } else {
                    self.state = BbrState::Startup;
                }
            }
            return;
        }
        if self.has_min_rtt
            && self.now_ns.saturating_sub(self.min_rtt_stamp_ns) > profile::MIN_RTT_WINDOW_NS
        {
            self.enter_probe_rtt();
        }
    }

    /// The Startup -> Drain -> ProbeBW progression and the ProbeBW gain cycle.
    fn advance_state(&mut self) {
        let bdp = self.bdp_bytes();
        match self.state {
            BbrState::Startup => {
                if self.round_start {
                    self.check_full_bw();
                }
                if self.full_bw_reached {
                    self.state = BbrState::Drain;
                    self.cycle_stamp_ns = self.now_ns;
                }
            }
            BbrState::Drain => {
                // The queue Startup built is gone once in-flight is back to one BDP.
                if self.inflight <= bdp {
                    self.enter_probe_bw();
                }
            }
            BbrState::ProbeBw(phase) => {
                let elapsed = self.now_ns.saturating_sub(self.cycle_stamp_ns);
                let rtt = if self.has_min_rtt {
                    self.min_rtt_ns
                } else {
                    INIT_RTT_NS
                };
                match phase {
                    ProbePhase::Down => {
                        if self.inflight <= bdp || elapsed >= rtt {
                            self.set_phase(ProbePhase::Cruise);
                        }
                    }
                    ProbePhase::Cruise => {
                        if elapsed >= profile::CRUISE_NS {
                            self.set_phase(ProbePhase::Refill);
                        }
                    }
                    ProbePhase::Refill => {
                        if self.rounds > self.phase_round {
                            self.set_phase(ProbePhase::Up);
                        }
                    }
                    ProbePhase::Up => {
                        let ceiling = (bdp as u64 * PROBE_UP_GAIN / UNIT) as u32;
                        if self.inflight > ceiling || self.rounds >= self.phase_round + 2 {
                            self.set_phase(ProbePhase::Down);
                        }
                    }
                }
            }
            BbrState::ProbeRtt => {}
        }
    }

    /// Gains follow the state directly (that is the whole of BBR's control law).
    fn update_gains(&mut self) {
        let (p, c) = match self.state {
            BbrState::Startup => (STARTUP_PACING_GAIN, STARTUP_CWND_GAIN),
            BbrState::Drain => (DRAIN_PACING_GAIN, STARTUP_CWND_GAIN),
            BbrState::ProbeBw(ProbePhase::Down) => (PROBE_DOWN_GAIN, profile::PROBE_BW_CWND_GAIN),
            BbrState::ProbeBw(ProbePhase::Cruise) => (CRUISE_GAIN, profile::PROBE_BW_CWND_GAIN),
            BbrState::ProbeBw(ProbePhase::Refill) => (REFILL_GAIN, profile::PROBE_BW_CWND_GAIN),
            BbrState::ProbeBw(ProbePhase::Up) => (PROBE_UP_GAIN, profile::PROBE_BW_CWND_GAIN),
            BbrState::ProbeRtt => (REFILL_GAIN, PROBE_RTT_CWND_GAIN),
        };
        self.pacing_gain = p;
        self.cwnd_gain = c;
    }

    /// `cwnd = gain * BDP`, floored at 4 segments, capped by `inflight_hi`; in
    /// Startup it also grows by the bytes just acked, so the window ramps even before
    /// the bandwidth estimate is meaningful.
    fn update_cwnd(&mut self, acked: u32) {
        let bdp = self.bdp_bytes() as u64;
        let target = ((bdp * self.cwnd_gain) / UNIT) as u32;
        let mut cwnd = match self.state {
            BbrState::Startup => target.max(self.cwnd.saturating_add(acked)),
            BbrState::ProbeRtt => ((bdp * PROBE_RTT_CWND_GAIN) / UNIT) as u32,
            _ => target,
        };
        cwnd = cwnd.max(self.min_cwnd());
        if self.inflight_hi != u32::MAX {
            cwnd = cwnd.min(self.inflight_hi.max(self.min_cwnd()));
        }
        self.cwnd = cwnd;
    }

    /// `pacing_rate = gain * max_bw`, or the initial window over [`INIT_RTT_NS`]
    /// before the first sample.
    fn update_pacing_rate(&mut self) {
        let bw = self.max_bw();
        let base = if bw == 0 {
            (INIT_WINDOW_SEGMENTS as u64 * self.mss as u64) * 1_000_000_000 / INIT_RTT_NS
        } else {
            bw
        };
        self.pacing_rate = base.saturating_mul(self.pacing_gain) / UNIT;
    }

    /// The loss response: trim the in-flight ceiling by `beta`, **at most once per
    /// round**, and leave the bandwidth estimate, the pacing rate and the min-RTT
    /// untouched. This is the whole of "loss is not congestion".
    fn on_loss_signal(&mut self) {
        self.loss_events += 1;
        self.lost_in_round = self.lost_in_round.saturating_add(self.mss);
        if self.last_loss_round == self.rounds {
            return; // one trim per round
        }
        self.last_loss_round = self.rounds;
        let current = self.inflight.max(self.cwnd).max(self.min_cwnd());
        let trimmed = ((current as u64 * (100 - LOSS_BETA_PCT)) / 100) as u32;
        // **Floored at one BDP.** A loss trims the *headroom above* the operating
        // point, never the operating point itself: the model says one BDP in flight
        // is right, so random loss on an unqueued path costs nothing. Genuine
        // congestion still shows up - as a falling delivery rate, which shrinks the
        // BDP through the bandwidth filter, which lowers this floor. That is the
        // difference between reacting to a *signal* and reacting to a *measurement*.
        self.inflight_hi = trimmed.max(self.bdp_bytes()).max(self.min_cwnd());
        self.cwnd = self.cwnd.min(self.inflight_hi).max(self.min_cwnd());
    }
}

impl Default for Bbr {
    fn default() -> Bbr {
        Bbr::new(DEFAULT_MSS)
    }
}

impl CongestionControl for Bbr {
    fn tick(&mut self, now_ns: u64) {
        if now_ns > self.now_ns {
            self.now_ns = now_ns;
        }
        if self.min_rtt_stamp_ns == 0 && !self.has_min_rtt {
            // Anchor the min-RTT window at the first time we see, so a connection
            // that never gets an RTT sample does not look 10 s stale at once.
            self.min_rtt_stamp_ns = self.now_ns;
        }
        // ProbeRTT is time-driven, so it must be reachable without an ACK.
        self.check_probe_rtt();
        self.update_gains();
        self.update_pacing_rate();
    }

    /// BBR does not act on the ack *count* - the model is driven by
    /// [`on_rate_sample`](CongestionControl::on_rate_sample). This only clears the
    /// duplicate-ACK run, since a cumulative ACK ends it.
    fn on_ack(&mut self, bytes_acked: u32, _rtt_ns: Option<u64>) {
        if bytes_acked > 0 {
            self.dup_acks = 0;
        }
    }

    fn on_rate_sample(&mut self, rs: &RateSample) {
        self.update_round(rs);
        self.delivered_in_round = self.delivered_in_round.saturating_add(rs.acked);
        self.inflight = rs.inflight;
        self.update_min_rtt(rs);
        self.update_bw(rs);
        self.check_probe_rtt();
        self.advance_state();
        self.update_gains();
        self.update_cwnd(rs.acked);
        self.update_pacing_rate();
    }

    fn on_dup_ack(&mut self) -> bool {
        self.dup_acks += 1;
        if self.dup_acks == 3 {
            self.dup_acks = 0;
            self.on_loss_signal();
            return true; // fast-retransmit now, without collapsing the model
        }
        false
    }

    fn on_loss(&mut self) {
        self.on_loss_signal();
    }

    fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// BBR keeps **no slow-start threshold** - it is a rate model, not an AIMD
    /// window. Reporting `u32::MAX` says exactly that.
    fn ssthresh(&self) -> u32 {
        u32::MAX
    }

    /// BBR has no inflate/deflate fast-recovery state: a loss trims the in-flight
    /// ceiling and the model carries on.
    fn in_recovery(&self) -> bool {
        false
    }

    fn set_mss(&mut self, mss: u16) {
        self.mss = mss as u32;
    }

    fn pacing_rate_bps(&self) -> u64 {
        self.pacing_rate
    }

    fn inflight_cap(&self) -> u32 {
        self.inflight_hi
    }

    fn min_rtt_ns(&self) -> Option<u64> {
        if self.has_min_rtt {
            Some(self.min_rtt_ns)
        } else {
            None
        }
    }

    fn bw_bps(&self) -> u64 {
        self.max_bw()
    }

    fn rounds(&self) -> u64 {
        self.rounds
    }

    /// BBR is the rate-based controller: it needs every sample the connection can
    /// produce, so the connection keeps its per-transmission send-time bookkeeping.
    fn uses_rate_samples(&self) -> bool {
        true
    }
}
