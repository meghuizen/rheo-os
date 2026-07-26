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

use librheo::println;
use rheo_net::cc::{Cubic, Reno};
use rheo_net::ip::Ipv4Addr;
use rheo_net::tcp::{CongestionControl, Connection, VirtualLink};

/// Exit code on full success (the `nettcpcc` kernel asserts exactly this).
const OK_CODE: i32 = 0x42;

/// The MSS the proofs use (the rheo-net default).
const MSS: u32 = 1460;

/// A named proof step: a check returning `Ok` or an `Err(code)` exit code.
type Step = (fn() -> Result<(), i32>, &'static str);

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let steps: [Step; 8] = [
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
    ];
    for (f, name) in steps {
        if let Err(c) = f() {
            println!("nettcpcc-demo: {name} FAILED (code {c})");
            return c;
        }
        println!("nettcpcc-demo: {name} OK");
    }

    // Bonus: a live TCP handshake to a SLIRP peer - skipped with reason (honest).
    println!(
        "nettcpcc-demo: live TCP handshake SKIPPED - SLIRP user-net has no TCP \
         echo/responder, so no deterministic live peer exists (like the N2a note); \
         a live TCP echo/HTTP GET is an N2c/hardware-lab deliverable. The \
         deterministic cwnd-trajectory proof above is the real deliverable."
    );
    println!("nettcpcc-demo: all congestion-control trajectories match the oracles OK");
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
