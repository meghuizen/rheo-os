//! `nettcpcc-demo` - the rheo-net Phase N2b proof cell (docs/NETSTACK.md §11
//! congestion control). It proves the two **from-scratch congestion controllers**,
//! [`Reno`] and [`Cubic`], **deterministically and network-free** - integer cwnd
//! trajectories pinned against precomputed oracles, plus a real
//! [`Connection`]-level fast-retransmit-before-RTO scenario over the in-cell
//! [`VirtualLink`]. It exits `0x42` only if every trajectory matches, so the exit
//! code is the proof.
//!
//! The scenarios (each asserts an exact integer trajectory):
//! 1. **Reno slow start** - `cwnd` doubles per ACK round from 1 MSS up to `ssthresh`
//!    (1->2->4->8->16 MSS).
//! 2. **Reno AIMD** - past `ssthresh`, `cwnd` grows by exactly one MSS per round
//!    (linear, +1 MSS/RTT).
//! 3. **Reno fast retransmit / fast recovery** - a scripted dup-ACK sequence: the
//!    3rd dup returns the fast-retransmit trigger, `ssthresh = cwnd/2`, `cwnd`
//!    inflates to `ssthresh + 3*MSS`, each further dup inflates by one MSS, and the
//!    first new ACK deflates to `ssthresh` (**not** a collapse to 1 MSS).
//! 4. **Reno RTO collapse** - a timeout: `ssthresh = cwnd/2`, `cwnd = 1 MSS`.
//! 5. **CUBIC shape** - after a loss, `W(t)` sampled at seven times matches an
//!    integer oracle **and** stays within a few bytes of the real-valued cubic; the
//!    increments are concave (decreasing) before `K` and convex (increasing) after.
//! 6. **CUBIC vs Reno** - at a matched late checkpoint CUBIC's `cwnd` (convex growth
//!    from a gentler 0.7 decrease) exceeds Reno's (linear growth from a 0.5 decrease).
//! 7. **Reno integration** - a real `Connection<Reno>` over the `VirtualLink`: three
//!    duplicate ACKs (a held-back segment makes the receiver re-ack) fast-retransmit
//!    the lost segment **before the RTO deadline**, `cwnd` halves (fast recovery,
//!    not RTO collapse), and the full payload still transfers (received == sent).
//! 8. **Bulk transfer** - a real `Connection<Reno>` and `Connection<Cubic>` each
//!    carry a multi-segment payload to completion (received == sent) with `cwnd`
//!    having grown from slow start.
//!
//! The kernel is untouched - portable userspace over `net::tcp` + `net::cc`, no new
//! kernel object, no per-ISA code. A live TCP handshake to SLIRP is **skipped with
//! reason** (SLIRP has no TCP responder; printed at the end).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{println, rt, sched};
use rheo_net::bbr::{self, Bbr, BbrState, ProbePhase, profile};
use rheo_net::cc::{Cubic, Reno};
use rheo_net::ip::Ipv4Addr;
use rheo_net::pacer;
use rheo_net::tcp::{CongestionControl, Connection, FixedWindow, RateSample, Segment, VirtualLink};

/// Exit code on full success (the `nettcpcc` kernel asserts exactly this).
const OK_CODE: i32 = 0x42;

/// The MSS the proofs use (the rheo-net default).
const MSS: u32 = 1460;

/// A named proof step: a check returning `Ok` or an `Err(code)` exit code.
type Step = (fn() -> Result<(), i32>, &'static str);

/// The failure code of the one async step (the reactor's `block_on` root returns
/// `()`, so a failure is reported through here - the `netl4-demo` pattern).
static LIVE_CODE: AtomicI32 = AtomicI32::new(0);

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let steps: [Step; 19] = [
        // N2b: the window-based controllers, unchanged by N2e's trait extension.
        (reno_slow_start, "reno slow start"),
        (reno_aimd, "reno AIMD"),
        (reno_fast_retransmit, "reno fast retransmit/recovery"),
        (reno_rto, "reno RTO collapse"),
        (cubic_shape, "cubic W(t) shape"),
        (cubic_vs_reno, "cubic vs reno"),
        (
            integration_fast_retransmit,
            "reno integration (dup-ACK/RTO)",
        ),
        (bulk_transfer, "bulk transfer reno+cubic"),
        // N2e: BBRv3, the pacer, and the reservation evaluation.
        (bbr_startup, "bbr startup (2.77x pacing, plateau exit)"),
        (bbr_drain, "bbr drain (inflight -> BDP)"),
        (bbr_probe_bw, "bbr probe-bw gain cycle"),
        (bbr_probe_rtt, "bbr probe-rtt (stale min-RTT)"),
        (bbr_filters, "bbr max-bw + min-rtt filters"),
        (loss_not_congestion, "loss != congestion (bbr vs cubic)"),
        (bbr_pacing, "bbr pacing (paced release intervals)"),
        (bbr_loss_recovery, "bbr loss recovery vs reno collapse"),
        (window_cc_unchanged, "window CC unpaced + uncapped"),
        (pacing_reservation_math, "pacing -> CPU reservation math"),
        (pacing_reservation_admit, "pacing CPU reservation admission"),
    ];
    for (f, name) in steps {
        if let Err(c) = f() {
            println!("nettcpcc-demo: {name} FAILED (code {c})");
            return c;
        }
        println!("nettcpcc-demo: {name} OK");
    }

    // The one step that must run on the reactor: the pacer's release deadlines go
    // through the kernel timer arbiter's **pacer** slot, re-armed after every
    // release (docs/NETSTACK.md 21). The `nettcpcc` kernel checks the arbiter's own
    // registration counter for that slot afterwards.
    rt::block_on(live_pacer_parks());
    let c = LIVE_CODE.load(Ordering::Relaxed);
    if c != 0 {
        println!("nettcpcc-demo: live pacer parks FAILED (code {c})");
        return c;
    }
    println!("nettcpcc-demo: live pacer parks OK");

    // Bonus: a live TCP handshake to a SLIRP peer - skipped with reason (honest).
    println!(
        "nettcpcc-demo: live TCP handshake SKIPPED - SLIRP user-net has no TCP \
         echo/responder, so no deterministic live peer exists (like the N2a note); \
         a live TCP echo/HTTP GET is an N2c/hardware-lab deliverable. The \
         deterministic cwnd-trajectory proof above is the real deliverable."
    );
    println!(
        "nettcpcc-demo: all congestion-control trajectories match the oracles OK \
         (Reno + CUBIC unchanged, BBRv3 default, {} profile)",
        profile::NAME
    );
    OK_CODE
}

// ---------------------------------------------------------------------------
// 1. Reno slow start: exponential growth to ssthresh.
// ---------------------------------------------------------------------------

fn reno_slow_start() -> Result<(), i32> {
    // Start at 1 MSS with ssthresh = 16 MSS; feed a full-cwnd ACK each round.
    let mut r = Reno::with_params(MSS as u16, MSS, 16 * MSS);
    let expect: [u32; 4] = [2 * MSS, 4 * MSS, 8 * MSS, 16 * MSS];
    for &e in &expect {
        let acked = r.cwnd();
        r.on_ack(acked, None); // slow start: cwnd += bytes_acked (doubles per round)
        if r.cwnd() != e {
            return Err(10);
        }
    }
    // Landed exactly on ssthresh, so the next round is congestion avoidance.
    if r.cwnd() != 16 * MSS {
        return Err(11);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Reno AIMD: linear growth past ssthresh (+1 MSS per round).
// ---------------------------------------------------------------------------

fn reno_aimd() -> Result<(), i32> {
    // cwnd == ssthresh == 16 MSS, so we are in congestion avoidance.
    let mut r = Reno::with_params(MSS as u16, 16 * MSS, 16 * MSS);
    let mut expect = 16 * MSS;
    for _ in 0..6 {
        let acked = r.cwnd(); // one cwnd worth acked per RTT
        r.on_ack(acked, None);
        expect += MSS; // AIMD adds exactly one MSS per round
        if r.cwnd() != expect {
            return Err(20);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Reno fast retransmit / fast recovery (scripted dup ACKs).
// ---------------------------------------------------------------------------

fn reno_fast_retransmit() -> Result<(), i32> {
    let mut r = Reno::with_params(MSS as u16, 16 * MSS, u32::MAX);
    // First two dup ACKs do not trigger.
    if r.on_dup_ack() || r.on_dup_ack() {
        return Err(30);
    }
    if r.in_recovery() {
        return Err(31);
    }
    // The 3rd dup ACK triggers fast retransmit and enters fast recovery.
    if !r.on_dup_ack() {
        return Err(32);
    }
    if !r.in_recovery() {
        return Err(33);
    }
    if r.ssthresh() != 8 * MSS {
        return Err(34); // ssthresh = cwnd/2 = 8 MSS
    }
    if r.cwnd() != 11 * MSS {
        return Err(35); // cwnd = ssthresh + 3*MSS = 8+3 = 11 MSS (inflated)
    }
    // Each further dup ACK inflates cwnd by one MSS.
    r.on_dup_ack();
    if r.cwnd() != 12 * MSS {
        return Err(36);
    }
    r.on_dup_ack();
    if r.cwnd() != 13 * MSS {
        return Err(37);
    }
    // The first new ACK deflates to ssthresh and exits recovery (NOT 1 MSS).
    r.on_ack(MSS, None);
    if r.in_recovery() {
        return Err(38);
    }
    if r.cwnd() != 8 * MSS {
        return Err(39);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 4. Reno RTO collapse.
// ---------------------------------------------------------------------------

fn reno_rto() -> Result<(), i32> {
    let mut r = Reno::with_params(MSS as u16, 16 * MSS, u32::MAX);
    r.on_loss(); // RTO
    if r.ssthresh() != 8 * MSS {
        return Err(40); // ssthresh = cwnd/2 = 8 MSS
    }
    if r.cwnd() != MSS {
        return Err(41); // cwnd = 1 MSS (slow-start restart)
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 5. CUBIC W(t) shape against the integer oracle + the real-valued cubic.
// ---------------------------------------------------------------------------

fn cubic_shape() -> Result<(), i32> {
    // W_max = 32 MSS. beta = 0.7, C = 0.4 -> K ~= 2.8845 s (integer K_ms = 2884).
    let mut c = Cubic::new(MSS as u16);
    c.set_rtt(1_000_000_000); // 1 s RTT keeps W_est below W_cubic at the samples
    c.set_epoch(32 * MSS, 0); // cwnd = ssthresh = 0.7*W_max, epoch = 0

    // (t_ns, expected integer W(t) in bytes) - pinned to the fixed-point impl.
    let samples: [(u64, u32); 7] = [
        (0, 32712),
        (1_000_000_000, 42815),
        (2_000_000_000, 46317),
        (2_884_000_000, 46720), // t = K: W == W_max
        (4_000_000_000, 47531),
        (5_000_000_000, 52252),
        (6_000_000_000, 64388),
    ];
    // The real-valued cubic W(t)*MSS, rounded - the fixed-point impl must stay
    // within a few bytes of this (correctness, not just self-consistency).
    let real: [u32; 7] = [32704, 42812, 46316, 46720, 47531, 52249, 64380];

    let mut vals: Vec<u32> = Vec::new();
    for (i, &(t, want)) in samples.iter().enumerate() {
        c.tick(t);
        c.on_ack(MSS, None); // grow toward W(t)
        let cw = c.cwnd();
        if cw != want {
            return Err(50); // integer-oracle mismatch
        }
        if (cw as i64 - real[i] as i64).unsigned_abs() > 16 {
            return Err(51); // too far from the real cubic
        }
        vals.push(cw);
    }

    // Concave before K (increments decreasing), convex after K (increments
    // increasing). K is sample index 3.
    let inc: Vec<i64> = (1..vals.len())
        .map(|i| vals[i] as i64 - vals[i - 1] as i64)
        .collect();
    // inc[0..3] correspond to (0->1000->2000->2884): decelerating (concave).
    if !(inc[0] > inc[1] && inc[1] > inc[2]) {
        return Err(52);
    }
    // inc[3..6] correspond to (2884->4000->5000->6000): accelerating (convex).
    if !(inc[3] < inc[4] && inc[4] < inc[5]) {
        return Err(53);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. CUBIC vs Reno at a matched late checkpoint.
// ---------------------------------------------------------------------------

fn cubic_vs_reno() -> Result<(), i32> {
    // Both start from the same pre-loss window (32 MSS) and take a fast-retransmit
    // loss. Reno halves to 16 MSS then grows linearly; CUBIC drops to 0.7*W_max and
    // grows convexly - so at a late point CUBIC is ahead.
    let mut r = Reno::with_params(MSS as u16, 32 * MSS, u32::MAX);
    r.on_dup_ack();
    r.on_dup_ack();
    r.on_dup_ack(); // enter recovery: ssthresh = 16 MSS
    r.on_ack(MSS, None); // deflate to 16 MSS
    for _ in 0..6 {
        let a = r.cwnd();
        r.on_ack(a, None); // +1 MSS per round -> 22 MSS
    }
    let reno_cwnd = r.cwnd();

    let mut c = Cubic::new(MSS as u16);
    c.set_rtt(1_000_000_000);
    c.set_epoch(32 * MSS, 0);
    c.tick(6_000_000_000);
    c.on_ack(MSS, None); // W(6 s) ~= 44 MSS
    let cubic_cwnd = c.cwnd();

    if cubic_cwnd < reno_cwnd {
        return Err(60);
    }
    // Sanity: Reno is the linear 22 MSS, CUBIC the convex ~44 MSS.
    if reno_cwnd != 22 * MSS {
        return Err(61);
    }
    if cubic_cwnd != 64388 {
        return Err(62);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Integration helpers (real Connections over the in-cell VirtualLink).
// ---------------------------------------------------------------------------

/// One greedy exchange step at fixed `now`: drain both endpoints' immediate output
/// into the peer. Returns whether either produced a segment.
fn step<C: CongestionControl>(
    a: &mut Connection<C>,
    b: &mut Connection<C>,
    link: &mut VirtualLink,
    now: u64,
) -> bool {
    let mut progressed = false;
    while let Some(s) = a.poll(now) {
        link.transfer(&s, b, now);
        progressed = true;
    }
    while let Some(s) = b.poll(now) {
        link.transfer(&s, a, now);
        progressed = true;
    }
    progressed
}

/// Drive to quiescence, advancing the logical clock to the next RTO/TIME-WAIT
/// deadline when neither endpoint has immediate output, while draining `b`'s
/// received bytes into `got` (so the receive window never closes).
fn run_to_done<C: CongestionControl>(
    a: &mut Connection<C>,
    b: &mut Connection<C>,
    link: &mut VirtualLink,
    start: u64,
    got: &mut Vec<u8>,
) {
    let mut now = start;
    let mut buf = [0u8; 4096];
    for _ in 0..2_000_000 {
        if step(a, b, link, now) {
            loop {
                let n = b.read(&mut buf);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            continue;
        }
        let next = [a.poll_at(), b.poll_at()].into_iter().flatten().min();
        match next {
            Some(d) => now = if d > now { d } else { now + 1 },
            None => break,
        }
    }
    loop {
        let n = b.read(&mut buf);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
}

fn handshake<C: CongestionControl>(
    a: &mut Connection<C>,
    b: &mut Connection<C>,
    link: &mut VirtualLink,
    now: u64,
) -> bool {
    for _ in 0..64 {
        if !step(a, b, link, now) {
            break;
        }
    }
    a.is_established() && b.is_established()
}

// ---------------------------------------------------------------------------
// 7. Integration: 3 dup ACKs fast-retransmit before the RTO, cwnd halves,
//    full payload still transfers.
// ---------------------------------------------------------------------------

fn integration_fast_retransmit() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let now = 1_000_000u64; // fixed small logical time (well before any RTO)

    let mut client: Connection<Reno> = Connection::connect(cip, 40000, sip, 80, 0x0000_1000);
    let mut server: Connection<Reno> = Connection::listen(sip, 80, cip, 40000, 0x0000_9000);
    let mut link = VirtualLink::new();
    if !handshake(&mut client, &mut server, &mut link, now) {
        return Err(70);
    }

    // Warm-start the client to 8 MSS so it bursts >= 4 segments at once.
    client.congestion_mut().set_cwnd(8 * MSS);
    client.congestion_mut().set_ssthresh(u32::MAX);

    let data: Vec<u8> = (0..6 * MSS).map(|i| (i % 251) as u8).collect();
    if client.write(&data) != data.len() {
        return Err(71);
    }

    // Collect the client's data segments WITHOUT delivering them.
    let mut segs: Vec<Vec<u8>> = Vec::new();
    while let Some(s) = client.poll(now) {
        segs.push(s);
    }
    if segs.len() < 4 {
        return Err(72);
    }

    // Hold segs[0]; deliver segs[1..4] out of order - each makes the server owe a
    // duplicate ACK (no out-of-order buffering in N2a).
    let mut dup_acks: Vec<Vec<u8>> = Vec::new();
    for s in &segs[1..4] {
        server.on_wire_segment(now, s);
        while let Some(a) = server.poll(now) {
            dup_acks.push(a);
        }
    }
    if dup_acks.len() < 3 {
        return Err(73);
    }

    // Deliver exactly three duplicate ACKs to the client.
    for a in dup_acks.iter().take(3) {
        client.on_wire_segment(now, a);
    }
    // Fast recovery entered: cwnd halved to ssthresh (not collapsed to 1 MSS).
    if !client.congestion().in_recovery() {
        return Err(74);
    }
    if client.congestion().ssthresh() != 4 * MSS {
        return Err(75); // 8 MSS / 2
    }
    if client.congestion().cwnd() != 7 * MSS {
        return Err(76); // ssthresh(4) + 3*MSS, inflated - NOT 1 MSS
    }
    // The retransmit must beat the RTO: now is still before the RTO deadline.
    match client.poll_at() {
        Some(dl) if now < dl => {}
        _ => return Err(77),
    }
    // The next poll fast-retransmits the lost segment (segs[0]).
    let rexmit = client.poll(now).ok_or(78)?;
    server.on_wire_segment(now, &rexmit);

    // Run to completion; the payload must arrive intact (received == sent).
    let mut got: Vec<u8> = Vec::new();
    run_to_done(&mut client, &mut server, &mut link, now, &mut got);
    if got != data {
        return Err(79);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 8. Bulk transfer under Reno and CUBIC: cwnd grows from slow start, data intact.
// ---------------------------------------------------------------------------

fn bulk_transfer() -> Result<(), i32> {
    bulk_one::<Reno>(80)?;
    bulk_one::<Cubic>(84)?;
    Ok(())
}

fn bulk_one<C: CongestionControl + Default>(base: i32) -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let now = 1_000_000u64;

    let mut client: Connection<C> = Connection::connect(cip, 40010, sip, 80, 0x0000_4000);
    let mut server: Connection<C> = Connection::listen(sip, 80, cip, 40010, 0x0000_5000);
    let mut link = VirtualLink::new();
    if !handshake(&mut client, &mut server, &mut link, now) {
        return Err(base);
    }

    let init_cwnd = client.congestion().cwnd();
    let data: Vec<u8> = (0..20 * MSS).map(|i| (i % 253) as u8).collect();
    if client.write(&data) != data.len() {
        return Err(base + 1);
    }

    let mut got: Vec<u8> = Vec::new();
    run_to_done(&mut client, &mut server, &mut link, now, &mut got);
    if got != data {
        return Err(base + 2);
    }
    // Slow start must have grown the window over the transfer.
    if client.congestion().cwnd() <= init_cwnd {
        return Err(base + 3);
    }
    Ok(())
}

/// The scripted round-trip time for the BBR scenarios (50 ms).
const BBR_RTT: u64 = 50_000_000;

/// A scripted delivery-rate feed: one [`RateSample`] per round trip, at a chosen
/// delivery rate, RTT and in-flight level. This is the BBR equivalent of the N2b
/// "feed a full-cwnd ACK each round" driver - the controller sees exactly the ACK
/// stream a link of that rate would produce.
struct Feed {
    delivered: u64,
    now: u64,
}

impl Feed {
    fn new() -> Feed {
        Feed {
            delivered: 0,
            now: 1_000_000,
        }
    }

    /// Deliver one round: `rate` bytes/s sustained for one `rtt`, leaving `inflight`
    /// bytes in flight.
    fn round(&mut self, b: &mut Bbr, rate: u64, rtt: u64, inflight: u32) {
        let prior = self.delivered;
        let acked = (rate * rtt / 1_000_000_000) as u32;
        self.delivered += acked as u64;
        self.now += rtt;
        b.tick(self.now);
        b.on_ack(acked, Some(rtt));
        let rs = RateSample {
            delivered: self.delivered,
            prior_delivered: prior,
            acked,
            interval_ns: rtt,
            rtt_ns: Some(rtt),
            prior_inflight: inflight + acked,
            inflight,
            app_limited: false,
        };
        b.on_rate_sample(&rs);
    }
}

// ---------------------------------------------------------------------------
// 9-16 + the rate-based interface: BBRv3 (rheo-net N2e, docs/NETSTACK.md 21).
// ---------------------------------------------------------------------------

// 9. Startup
fn bbr_startup() -> Result<(), i32> {
    let mut b = Bbr::new(MSS as u16);
    let mut f = Feed::new();
    // Before any sample: the 10 x MSS initial window, paced at the initial-rate
    // estimate (10 MSS per 100 ms) times the 2.77 startup gain.
    if b.cwnd() != 10 * MSS {
        return Err(90);
    }
    if b.pacing_rate_bps() != 404_420 {
        return Err(91);
    }
    if b.state() != BbrState::Startup {
        return Err(92);
    }
    // Five rounds of doubling delivery rate: the pacing rate is 2.77x the estimate
    // and doubles with it - exponential growth, ~90% of the path in a handful of RTTs.
    let want_pacing: [u64; 5] = [2_770_000, 5_540_000, 11_080_000, 22_160_000, 44_320_000];
    let want_cwnd: [u32; 5] = [100_000, 200_000, 400_000, 800_000, 1_600_000];
    let mut rate = 1_000_000u64;
    for i in 0..5 {
        let inflight = 2 * (rate * BBR_RTT / 1_000_000_000) as u32;
        f.round(&mut b, rate, BBR_RTT, inflight);
        if b.pacing_rate_bps() != want_pacing[i] {
            return Err(93);
        }
        if b.cwnd() != want_cwnd[i] {
            return Err(94);
        }
        if b.state() != BbrState::Startup {
            return Err(95);
        }
        rate *= 2;
    }
    // The plateau: three rounds without 25% growth end Startup (and only the third).
    let plateau = 16_000_000u64;
    let inflight = 2 * (plateau * BBR_RTT / 1_000_000_000) as u32;
    for i in 0..3 {
        f.round(&mut b, plateau, BBR_RTT, inflight);
        let last = i == 2;
        if b.full_bw_reached() != last {
            return Err(96);
        }
        if last {
            if b.state() != BbrState::Drain {
                return Err(97);
            }
            // Drain pacing gain 0.75 on a 16 MB/s estimate.
            if b.pacing_gain() != bbr::DRAIN_PACING_GAIN || b.pacing_rate_bps() != 12_000_000 {
                return Err(98);
            }
        } else if b.state() != BbrState::Startup {
            return Err(99);
        }
    }
    Ok(())
}

// 10. Drain
fn bbr_drain() -> Result<(), i32> {
    let (mut b, mut f) = warm_to_drain()?;
    let bdp = b.bdp_bytes();
    if bdp != 800_000 {
        return Err(100);
    }
    // While in-flight is above the BDP, Drain holds at the 0.75 gain.
    f.round(&mut b, 16_000_000, BBR_RTT, 2 * bdp);
    if b.state() != BbrState::Drain || b.pacing_gain() != bbr::DRAIN_PACING_GAIN {
        return Err(101);
    }
    // The queue is gone once in-flight is one BDP: enter ProbeBW.
    f.round(&mut b, 16_000_000, BBR_RTT, bdp);
    if b.state() != BbrState::ProbeBw(ProbePhase::Down) {
        return Err(102);
    }
    if b.pacing_gain() != bbr::PROBE_DOWN_GAIN {
        return Err(103);
    }
    Ok(())
}

/// Warm a controller through Startup to Drain (shared by the drain/cycle scenarios).
fn warm_to_drain() -> Result<(Bbr, Feed), i32> {
    let mut b = Bbr::new(MSS as u16);
    let mut f = Feed::new();
    let mut rate = 1_000_000u64;
    for _ in 0..5 {
        let inflight = 2 * (rate * BBR_RTT / 1_000_000_000) as u32;
        f.round(&mut b, rate, BBR_RTT, inflight);
        rate *= 2;
    }
    let inflight = 2 * (16_000_000 * BBR_RTT / 1_000_000_000) as u32;
    for _ in 0..3 {
        f.round(&mut b, 16_000_000, BBR_RTT, inflight);
    }
    if b.state() != BbrState::Drain {
        return Err(110);
    }
    Ok((b, f))
}

// 11. ProbeBW gain cycle
fn bbr_probe_bw() -> Result<(), i32> {
    let (mut b, mut f) = warm_to_drain()?;
    let bdp = b.bdp_bytes();
    f.round(&mut b, 16_000_000, BBR_RTT, bdp); // Drain -> ProbeBW(Down)
    if b.state() != BbrState::ProbeBw(ProbePhase::Down) {
        return Err(120);
    }
    // Walk the cycle, recording every phase change and the gain in force.
    let mut seen: Vec<(ProbePhase, u64)> = Vec::new();
    let mut cruise_start = f.now;
    let mut cruise_len = 0u64;
    let mut prev = ProbePhase::Down;
    for _ in 0..120 {
        f.round(&mut b, 16_000_000, BBR_RTT, bdp - 10_000);
        if let BbrState::ProbeBw(p) = b.state() {
            if p != prev {
                if prev == ProbePhase::Cruise {
                    cruise_len = f.now - cruise_start;
                }
                if p == ProbePhase::Cruise {
                    cruise_start = f.now;
                }
                seen.push((p, b.pacing_gain()));
                prev = p;
            }
        } else {
            return Err(121);
        }
        if seen.len() == 4 {
            break;
        }
    }
    // Down -> Cruise -> Refill -> Up -> Down, with the gains 0.95, 1.0, 1.25, 0.9.
    let want = [
        (ProbePhase::Cruise, bbr::CRUISE_GAIN),
        (ProbePhase::Refill, bbr::REFILL_GAIN),
        (ProbePhase::Up, bbr::PROBE_UP_GAIN),
        (ProbePhase::Down, bbr::PROBE_DOWN_GAIN),
    ];
    if seen.len() != 4 {
        return Err(122);
    }
    for i in 0..4 {
        if seen[i] != want[i] {
            return Err(123);
        }
    }
    // Cruise lasted the profile's cruise time (to within one round trip).
    if !(profile::CRUISE_NS..profile::CRUISE_NS + BBR_RTT).contains(&cruise_len) {
        return Err(124);
    }
    // Back in Down, the pacing rate follows the gain exactly: 0.9 x 16 MB/s.
    if b.pacing_rate_bps() != 16_000_000 * bbr::PROBE_DOWN_GAIN / bbr::UNIT {
        return Err(125);
    }
    Ok(())
}

// 12. ProbeRTT
fn bbr_probe_rtt() -> Result<(), i32> {
    let mut b = Bbr::new(MSS as u16);
    let mut f = Feed::new();
    // Warm up on a 50 ms path.
    for _ in 0..12 {
        f.round(&mut b, 10_000_000, BBR_RTT, 400_000);
    }
    if b.min_rtt_ns() != Some(BBR_RTT) {
        return Err(130);
    }
    if b.probe_rtt_entries() != 0 {
        return Err(131);
    }
    // Now the path queues: every RTT sample is 60 ms, so the 50 ms minimum is never
    // refreshed and goes stale after the 10 s window - which is exactly when BBR must
    // drain the queue to re-measure it.
    let stale_at = f.now + profile::MIN_RTT_WINDOW_NS;
    let mut entry: Option<(u64, u32, u32)> = None;
    let mut exit: Option<(u64, BbrState)> = None;
    for _ in 0..400 {
        f.round(&mut b, 10_000_000, 60_000_000, 400_000);
        if b.state() == BbrState::ProbeRtt && entry.is_none() {
            entry = Some((f.now, b.cwnd(), b.bdp_bytes()));
        }
        if entry.is_some() && b.state() != BbrState::ProbeRtt {
            exit = Some((f.now, b.state()));
            break;
        }
    }
    let Some((t_in, cwnd_in, bdp_in)) = entry else {
        return Err(132);
    };
    let Some((t_out, state_out)) = exit else {
        return Err(133);
    };
    if b.probe_rtt_entries() != 1 {
        return Err(134);
    }
    // Entered only once the min-RTT was genuinely stale (never before).
    if t_in < stale_at || t_in > stale_at + 2 * 60_000_000 {
        return Err(135);
    }
    // The reduced in-flight cap: half a BDP (the ProbeRTT cwnd gain), not the 2 BDP
    // ProbeBW would allow.
    if cwnd_in != bdp_in * bbr::PROBE_RTT_CWND_GAIN as u32 / bbr::UNIT as u32 {
        return Err(136);
    }
    if cwnd_in >= bdp_in {
        return Err(137);
    }
    // Held for at least the dwell, then back to ProbeBW with a fresh min-RTT.
    if t_out - t_in < profile::PROBE_RTT_DURATION_NS {
        return Err(138);
    }
    if !matches!(state_out, BbrState::ProbeBw(_)) {
        return Err(139);
    }
    if b.min_rtt_ns() != Some(60_000_000) {
        return Err(140);
    }
    // And it does not immediately re-enter: the window restarted at the exit.
    for _ in 0..20 {
        f.round(&mut b, 10_000_000, 60_000_000, 400_000);
    }
    if b.probe_rtt_entries() != 1 {
        return Err(141);
    }
    Ok(())
}

// 13. The two filters
fn bbr_filters() -> Result<(), i32> {
    // Max-bandwidth filter: a 20 MB/s round is remembered for exactly
    // BW_WINDOW_ROUNDS rounds and then expires, leaving the 5 MB/s truth.
    let mut b = Bbr::new(MSS as u16);
    let mut f = Feed::new();
    f.round(&mut b, 20_000_000, BBR_RTT, 400_000);
    if b.bw_bps() != 20_000_000 {
        return Err(150);
    }
    for i in 0..profile::BW_WINDOW_ROUNDS {
        f.round(&mut b, 5_000_000, BBR_RTT, 400_000);
        let last = i == profile::BW_WINDOW_ROUNDS - 1;
        let want = if last { 5_000_000 } else { 20_000_000 };
        if b.bw_bps() != want {
            return Err(151);
        }
    }
    // Min-RTT filter: a lower sample is taken at once, a higher one is ignored while
    // the window holds (that is what makes it a *minimum* filter).
    let mut b2 = Bbr::new(MSS as u16);
    let mut f2 = Feed::new();
    f2.round(&mut b2, 10_000_000, 60_000_000, 400_000);
    if b2.min_rtt_ns() != Some(60_000_000) {
        return Err(152);
    }
    f2.round(&mut b2, 10_000_000, 30_000_000, 400_000);
    if b2.min_rtt_ns() != Some(30_000_000) {
        return Err(153);
    }
    f2.round(&mut b2, 10_000_000, 90_000_000, 400_000);
    if b2.min_rtt_ns() != Some(30_000_000) {
        return Err(154);
    }
    Ok(())
}

// 14. Loss is not congestion - the headline property.
fn loss_not_congestion() -> Result<(), i32> {
    const LINK: u64 = 10_000_000; // 10 MB/s bottleneck
    let bdp = (LINK * BBR_RTT / 1_000_000_000) as u32; // 500_000 bytes
    let mut b = Bbr::new(MSS as u16);
    let mut f = Feed::new();
    // Warm BBR to its steady state on the 10 MB/s path.
    let mut r = 1_250_000u64;
    for _ in 0..4 {
        f.round(&mut b, r, BBR_RTT, bdp);
        r *= 2;
    }
    for _ in 0..4 {
        f.round(&mut b, LINK, BBR_RTT, bdp - 10_000);
    }
    if b.bw_bps() != LINK {
        return Err(160);
    }
    // CUBIC at the same operating point (cwnd == one BDP, in the cubic region).
    let mut c = Cubic::new(MSS as u16);
    c.set_rtt(BBR_RTT);
    c.tick(f.now);
    c.set_epoch(bdp * 10 / 7, f.now);
    let bbr_cwnd0 = b.cwnd();
    let cubic_cwnd0 = c.cwnd();

    // Twelve rounds at exactly the link rate, with a **random-loss** episode every
    // fourth round: three duplicate ACKs, no queue growth, no rate change. This is
    // lossy-wireless, not congestion.
    let mut t = f.now;
    for i in 0..12 {
        let lossy = i % 4 == 3;
        if lossy && !b.on_dup_ack() && !b.on_dup_ack() && !b.on_dup_ack() {
            return Err(161); // the third duplicate must fast-retransmit
        }
        f.round(&mut b, LINK, BBR_RTT, bdp - 10_000);
        t += BBR_RTT;
        c.tick(t);
        if lossy {
            c.on_dup_ack();
            c.on_dup_ack();
            c.on_dup_ack();
            c.on_ack(MSS, Some(BBR_RTT));
        } else {
            let a = c.cwnd();
            c.on_ack(a, Some(BBR_RTT));
        }
    }
    // BBR: the model is untouched. The bandwidth estimate is still the link rate, the
    // pacing rate is still the fairness-mode 0.95x of it, and in-flight is one BDP -
    // it gave up *queue*, not *throughput*.
    if b.bw_bps() != LINK {
        return Err(162);
    }
    if b.pacing_rate_bps() != LINK * bbr::CRUISE_GAIN / bbr::UNIT {
        return Err(163);
    }
    if b.cwnd() != bdp {
        return Err(164);
    }
    if b.loss_events() != 3 {
        return Err(165);
    }
    // CUBIC: three multiplicative decreases, cubic regrowth too slow to recover.
    if c.cwnd() != 187_534 {
        return Err(166);
    }
    let bbr_rate = b.cwnd() as u64 * 1_000_000_000 / BBR_RTT;
    let cubic_rate = c.cwnd() as u64 * 1_000_000_000 / BBR_RTT;
    if bbr_rate < LINK {
        return Err(167); // BBR still sends at the full link rate
    }
    if cubic_rate * 100 / LINK > 40 {
        return Err(168); // CUBIC dropped materially (to ~37%)
    }
    println!(
        "nettcpcc-demo: loss != congestion - over 3 random-loss episodes on a {} MB/s \
         unqueued path: BBR cwnd {} -> {} ({}% of link rate, bw estimate {} MB/s intact, \
         pacing {}% of link), CUBIC cwnd {} -> {} ({}% of link rate) - {}.{}x the \
         sending rate",
        LINK / 1_000_000,
        bbr_cwnd0,
        b.cwnd(),
        bbr_rate * 100 / LINK,
        b.bw_bps() / 1_000_000,
        b.pacing_rate_bps() * 100 / LINK,
        cubic_cwnd0,
        c.cwnd(),
        cubic_rate * 100 / LINK,
        bbr_rate * 10 / cubic_rate.max(1) / 10,
        bbr_rate * 10 / cubic_rate.max(1) % 10
    );

    // And the converse: BBR is not loss-blind, it is *measurement*-driven. When the
    // delivery rate itself halves - real congestion - the estimate follows it once the
    // filter window turns over, and the pacing rate halves with it.
    for i in 0..profile::BW_WINDOW_ROUNDS {
        f.round(&mut b, LINK / 2, BBR_RTT, bdp / 2);
        let last = i == profile::BW_WINDOW_ROUNDS - 1;
        if last {
            if b.bw_bps() != LINK / 2 {
                return Err(169);
            }
            if b.pacing_rate_bps() != (LINK / 2) * bbr::CRUISE_GAIN / bbr::UNIT {
                return Err(170);
            }
        } else if b.bw_bps() != LINK {
            return Err(171);
        }
    }
    Ok(())
}

// ---- connection-level helpers ----

fn hs<C: CongestionControl>(
    a: &mut Connection<C>,
    b: &mut Connection<C>,
    link: &mut VirtualLink,
    now: u64,
) -> bool {
    for _ in 0..64 {
        let mut prog = false;
        while let Some(s) = a.poll(now) {
            link.transfer(&s, b, now);
            prog = true;
        }
        while let Some(s) = b.poll(now) {
            link.transfer(&s, a, now);
            prog = true;
        }
        if !prog {
            break;
        }
    }
    a.is_established() && b.is_established()
}

// 15. Pacing at the connection level.
fn bbr_pacing() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let start = 1_000_000u64;
    let mut client: Connection<Bbr> = Connection::connect(cip, 40020, sip, 80, 0x0000_6000);
    let mut server: Connection<Bbr> = Connection::listen(sip, 80, cip, 40020, 0x0000_7000);
    let mut link = VirtualLink::new();
    if !hs(&mut client, &mut server, &mut link, start) {
        return Err(180);
    }
    // The initial pacing rate and its burst allowance are exact integers.
    let rate = client.congestion().pacing_rate_bps();
    if rate != 404_420 {
        return Err(181);
    }
    if client.pacer().burst_bytes() != 2 * MSS as u64 {
        return Err(182);
    }
    let interval = client.pacer().interval_ns(MSS);
    if interval != 3_610_108 {
        return Err(183);
    }

    let data: Vec<u8> = (0..20 * MSS).map(|i| (i % 251) as u8).collect();
    if client.write(&data) != data.len() {
        return Err(184);
    }
    // Drive to completion, recording when each data segment was released.
    let mut sends: Vec<u64> = Vec::new();
    let mut got: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut now = start;
    for _ in 0..200_000 {
        let mut prog = false;
        while let Some(s) = client.poll(now) {
            if let Some(seg) = Segment::decode(&s)
                && !seg.payload.is_empty()
            {
                sends.push(now);
            }
            link.transfer(&s, &mut server, now);
            prog = true;
        }
        while let Some(s) = server.poll(now) {
            link.transfer(&s, &mut client, now);
            prog = true;
        }
        loop {
            let n = server.read(&mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        if prog {
            continue;
        }
        let next = [client.poll_at(), server.poll_at()]
            .into_iter()
            .flatten()
            .min();
        match next {
            Some(d) => now = if d > now { d } else { now + 1 },
            None => break,
        }
    }
    if got != data {
        return Err(185);
    }
    if sends.len() != 20 {
        return Err(186);
    }
    // The shape of a paced flow: the burst allowance (2 MSS) leaves back to back, and
    // every segment after it is spaced by exactly one segment-time at the pacing rate.
    // An unpaced sender would have emitted all 20 at the same instant.
    let mut zero = 0;
    for w in sends.windows(2) {
        let gap = w[1] - w[0];
        if gap == 0 {
            zero += 1;
        } else if gap != 3_610_109 {
            return Err(187);
        }
    }
    if zero != 1 {
        return Err(188);
    }
    // The pacer really deferred sends (a pass-through would never defer).
    if client.pacer().defers() == 0 {
        return Err(189);
    }
    let span = sends[sends.len() - 1] - sends[0];
    println!(
        "nettcpcc-demo: pacing - 20 segments released over {} us at {} B/s: 2 in the \
         {}-byte burst, then one every {} us (the exact segment-time), {} deferrals",
        span / 1_000,
        rate,
        client.pacer().burst_bytes(),
        3_610_109 / 1_000,
        client.pacer().defers()
    );
    Ok(())
}

// 16. Loss recovery at the connection level: recover the data, keep the model.
fn bbr_loss_recovery() -> Result<(), i32> {
    let (bbr_cwnd, bbr_ok, bbr_ss) = loss_run::<Bbr>()?;
    let (reno_cwnd, reno_ok, reno_ss) = loss_run::<Reno>()?;
    if !bbr_ok || !reno_ok {
        return Err(190);
    }
    // BBR keeps no slow-start threshold at all; Reno's records the halving.
    if bbr_ss != u32::MAX {
        return Err(191);
    }
    if reno_ss == u32::MAX {
        return Err(192);
    }
    // BBR's window is trimmed by beta and floored at the BDP; Reno's collapsed to one
    // MSS and is still rebuilding.
    if bbr_cwnd <= 4 * MSS {
        return Err(193);
    }
    if bbr_cwnd <= reno_cwnd {
        return Err(194);
    }
    println!(
        "nettcpcc-demo: after one lost segment recovered by RTO - BBR cwnd {} \
         (no ssthresh, model intact), Reno cwnd {} (ssthresh {}, slow-start restart)",
        bbr_cwnd, reno_cwnd, reno_ss
    );
    Ok(())
}

fn loss_run<C: CongestionControl + Default>() -> Result<(u32, bool, u32), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let start = 1_000_000u64;
    let mut client: Connection<C> = Connection::connect(cip, 40021, sip, 80, 0x0000_8000);
    let mut server: Connection<C> = Connection::listen(sip, 80, cip, 40021, 0x0000_9000);
    let mut link = VirtualLink::new();
    if !hs(&mut client, &mut server, &mut link, start) {
        return Err(195);
    }
    let data: Vec<u8> = (0..10 * MSS).map(|i| (i % 251) as u8).collect();
    if client.write(&data) != data.len() {
        return Err(196);
    }
    link.drop_next_data_segment();
    let mut got: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut now = start;
    for _ in 0..500_000 {
        let mut prog = false;
        while let Some(s) = client.poll(now) {
            link.transfer(&s, &mut server, now);
            prog = true;
        }
        while let Some(s) = server.poll(now) {
            link.transfer(&s, &mut client, now);
            prog = true;
        }
        loop {
            let n = server.read(&mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        if prog {
            continue;
        }
        let next = [client.poll_at(), server.poll_at()]
            .into_iter()
            .flatten()
            .min();
        match next {
            Some(d) => now = if d > now { d } else { now + 1 },
            None => break,
        }
    }
    if link.dropped() != 1 {
        return Err(197);
    }
    Ok((
        client.congestion().cwnd(),
        got == data,
        client.congestion().ssthresh(),
    ))
}

// 17. The window-based controllers are untouched by the rate-based interface.
fn window_cc_unchanged() -> Result<(), i32> {
    fn check<C: CongestionControl + Default>(code: i32) -> Result<(), i32> {
        let c = C::default();
        if c.pacing_rate_bps() != 0 {
            return Err(code); // unpaced: the pacer is never engaged
        }
        if c.inflight_cap() != u32::MAX {
            return Err(code + 1); // no in-flight cap: the min() is a no-op
        }
        if c.min_rtt_ns().is_some() || c.bw_bps() != 0 || c.rounds() != 0 {
            return Err(code + 2); // no path model
        }
        Ok(())
    }
    check::<FixedWindow>(200)?;
    check::<Reno>(210)?;
    check::<Cubic>(220)?;
    // A Reno connection with data queued has no pacing deadline: `poll_at` is exactly
    // what it was before N2e.
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let mut client: Connection<Reno> = Connection::connect(cip, 40022, sip, 80, 0x0000_b000);
    let mut server: Connection<Reno> = Connection::listen(sip, 80, cip, 40022, 0x0000_c000);
    let mut link = VirtualLink::new();
    if !hs(&mut client, &mut server, &mut link, 1_000_000) {
        return Err(230);
    }
    client.write(&[7u8; 4096]);
    if client.pacing_deadline().is_some() {
        return Err(231);
    }
    if client.pacer().is_paced() {
        return Err(232);
    }
    // ... and a BBR connection does have one.
    let mut bc: Connection<Bbr> = Connection::connect(cip, 40023, sip, 80, 0x0000_d000);
    let mut bs: Connection<Bbr> = Connection::listen(sip, 80, cip, 40023, 0x0000_e000);
    let mut blink = VirtualLink::new();
    if !hs(&mut bc, &mut bs, &mut blink, 1_000_000) {
        return Err(233);
    }
    bc.write(&[7u8; 4096]);
    while bc.poll(1_000_000).is_some() {}
    if bc.pacing_deadline().is_none() {
        return Err(234);
    }
    Ok(())
}

// 18a. The pacing-rate -> CPU-reservation arithmetic (the syscall half needs a cell).
fn pacing_reservation_math() -> Result<(), i32> {
    // A 12 MB/s pace on 1460-byte segments wakes every ~121 us: 2 us of work in a
    // 121 us period is ~1.6% of a core.
    let (budget, period) =
        pacer::cpu_reservation_for(12_000_000, MSS as u16, pacer::PACER_WAKEUP_NS);
    if budget != 2_000 || period != 121_666 {
        return Err(240);
    }
    if pacer::cpu_utilization_ppm(12_000_000, MSS as u16) != 16_438 {
        return Err(241);
    }
    // A 100 Gb/s pace wants a wake every 116 ns - less than the wake itself costs, so
    // the request is not admissible at all (budget > period).
    let (b2, p2) = pacer::cpu_reservation_for(12_500_000_000, MSS as u16, pacer::PACER_WAKEUP_NS);
    if p2 >= b2 {
        return Err(242);
    }
    if pacer::cpu_utilization_ppm(12_500_000_000, MSS as u16) != 1_000_000 {
        return Err(243);
    }
    println!(
        "nettcpcc-demo: pacing CPU cost - 12 MB/s => {} ns every {} ns ({} ppm of a \
         core); 100 Gb/s => {} ns every {} ns, i.e. not admissible",
        budget,
        period,
        pacer::cpu_utilization_ppm(12_000_000, MSS as u16),
        b2,
        p2
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// 18b. The pacing CPU reservation, admitted by the kernel (object 7).
// ---------------------------------------------------------------------------

/// A pacing rate is **not** a bandwidth reservation - the kernel holds no authority
/// over link capacity - but the pacer's own wake-up cost **is** a periodic CPU task,
/// which is exactly object 7's shape (`net::pacer` documents the full evaluation).
/// So this asks the real admission controller "can this cell afford to pace at this
/// rate?", and shows it refusing what it cannot guarantee instead of pretending.
fn pacing_reservation_admit() -> Result<(), i32> {
    // ~292 MB/s on 1460-byte segments: a wake every 5 us, 2 us of work - 40% of a
    // core. Admitted, and the kernel reports the cell's committed utilization.
    const RATE_40PCT: u64 = 292_000_000;
    let r1 = pacer::admit_pacing_cpu(RATE_40PCT, MSS as u16).map_err(|_| 250)?;
    if r1.committed_ppm() != 400_000 {
        return Err(251);
    }
    let r2 = pacer::admit_pacing_cpu(RATE_40PCT, MSS as u16).map_err(|_| 252)?;
    if r2.committed_ppm() != 800_000 {
        return Err(253);
    }
    // A third would need 120% of the core: refused, cleanly, by the EDF test.
    match pacer::admit_pacing_cpu(RATE_40PCT, MSS as u16) {
        Err(sched::ReserveError::Overcommit) => {}
        _ => return Err(254),
    }
    // And pacing at 100 Gb/s wants a wake every 116 ns, less than one wake costs:
    // not schedulable at any utilization, so it is refused as bad parameters.
    match pacer::admit_pacing_cpu(12_500_000_000, MSS as u16) {
        Err(sched::ReserveError::BadParams) => {}
        _ => return Err(255),
    }
    println!(
        "nettcpcc-demo: pacing CPU reservations - 2 x 292 MB/s admitted ({} ppm \
         committed), a 3rd refused Overcommit, 100 Gb/s refused BadParams",
        r2.committed_ppm()
    );
    // Both handles drop here, returning their utilization (RAII release).
    Ok(())
}

// ---------------------------------------------------------------------------
// 19. The live pacer: release deadlines on the kernel timer arbiter's pacer slot.
// ---------------------------------------------------------------------------

/// Drive the pacer against the **real** kernel timer: after the burst allowance is
/// spent, every release waits on a deadline registered in the arbiter's
/// [`Pacer`](kernel timer client) slot, re-armed each time - the arbiter's first
/// continuously re-armed client. `rt::pacing_parks()` counts the reactor services
/// that armed that slot, and the `nettcpcc` kernel independently checks the
/// arbiter's own registration counter.
///
/// **Two clocks, honestly** (the `net::timer` wheel's rule): a cell has no
/// nanosecond clock reading - `librheo::time::now()` is raw ticks - so the pacer
/// driver keeps its own logical nanosecond clock, parks for the *delta* to the next
/// deadline, and advances the clock to it. The kernel's one-shot is what makes the
/// delay real.
async fn live_pacer_parks() {
    const RATE: u64 = 1_200_000; // 1.2 MB/s -> ~1.2 ms per segment
    const RELEASES: usize = 16;
    let mut p = pacer::Pacer::new(RATE, MSS as u16);
    // The burst allowance is 2 MSS at this rate, so 2 releases go immediately and the
    // remaining 14 each wait on the arbiter.
    let want_parks = RELEASES as u64 - 2;
    let before = rt::pacing_parks();
    let mut now = 0u64;
    let mut released = 0usize;
    let mut parks = 0u64;
    while released < RELEASES {
        if p.ready(now, MSS) {
            p.on_sent(now, MSS);
            released += 1;
            continue;
        }
        let Some(deadline) = p.next_send_at(MSS) else {
            LIVE_CODE.store(260, Ordering::Relaxed);
            return;
        };
        pacer::park_for(deadline.saturating_sub(now)).await;
        now = deadline;
        parks += 1;
        if parks > RELEASES as u64 * 4 {
            LIVE_CODE.store(261, Ordering::Relaxed); // pacing made no progress
            return;
        }
    }
    let serviced = rt::pacing_parks() - before;
    if parks != want_parks {
        LIVE_CODE.store(262, Ordering::Relaxed);
        return;
    }
    // Every wait was a genuine reactor park that armed the arbiter's pacer slot -
    // never a spin, and never the cell-sleep slot.
    if serviced != want_parks {
        LIVE_CODE.store(263, Ordering::Relaxed);
        return;
    }
    if p.sends() != RELEASES as u64 || p.bytes() != RELEASES as u64 * MSS as u64 {
        LIVE_CODE.store(264, Ordering::Relaxed);
        return;
    }
    println!(
        "nettcpcc-demo: live pacing - {} segments at {} B/s: 2 in the burst, {} \
         released on a kernel timer-arbiter pacer deadline (re-armed every time), \
         {} logical us of paced span",
        RELEASES,
        RATE,
        serviced,
        now / 1_000
    );
}
