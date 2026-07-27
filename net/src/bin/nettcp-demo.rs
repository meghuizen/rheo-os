//! `nettcp-demo` - the rheo-net Phase N2a proof cell (docs/NETSTACK.md §11). It
//! proves the **native TCP** state machine and the **timer wheel** deterministically
//! and **network-free** - two TCP endpoints in one cell, connected by an in-cell
//! [`VirtualLink`], driven through their full lifecycle by a logical clock the
//! [`TimerWheel`] advances (the same "deterministic core, thin live driver"
//! philosophy as the traceroute/DNS proofs). No NIC, no SLIRP, no live peer.
//!
//! 1. **Checksum + segment-encode oracles** (in memory, no network): a fixed
//!    SYN-ACK-with-MSS segment must encode to a known-good byte string and
//!    checksum (over the IPv4 pseudo-header) to the independently-computed
//!    `0x613C`; it must decode back and self-verify; and the RFC 1323 wrapping
//!    sequence comparisons are pinned by a wrap oracle (`0xFFFF_FFFF < 0`).
//! 2. **The full TCP lifecycle** over the virtual link: the three-way handshake
//!    completes (both ESTABLISHED); a known payload transfers **both directions**
//!    with the received bytes exactly equal to the sent bytes; a **dropped data
//!    segment is retransmitted after the RTO** and delivered (the link drops one
//!    segment, the wheel advances the clock past the RTO, recovery is asserted);
//!    and a clean FIN/FIN-ACK teardown reaches CLOSE_WAIT -> TIME_WAIT -> CLOSED.
//! 3. **The socket-shaped API**: [`TcpStream`]/[`TcpListener`] drive a second full
//!    handshake + byte + close, proving the connect/accept/read/write/close
//!    vocabulary forwards to the core.
//! 4. **The timer wheel multiplex**: four timers armed out of order fire in
//!    deadline order off the reactor's **single** one-shot deadline (`rt::sleep_ns`
//!    behind `run_once`) - proving many logical timers multiplex onto the one slot.
//!
//! Exit `0x42` only if every step passes. The kernel is untouched - portable
//! userspace over the existing reactor + one-shot-timer ABI, no new kernel object.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use librheo::{println, rt};
use rheo_net::ip::Ipv4Addr;
use rheo_net::tcp::{
    self, ACK, Connection, FixedWindow, SYN, Segment, State, TcpListener, TcpStream, VirtualLink,
    seq,
};
use rheo_net::timer::{TimerId, TimerWheel};

/// Exit code on full success (the `nettcp` kernel asserts exactly this).
const OK_CODE: i32 = 0x42;

/// A concrete connection over the trivial N2a congestion controller.
type Conn = Connection<FixedWindow>;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    if let Err(c) = oracle() {
        println!("nettcp-demo: oracle check failed ({c})");
        return c;
    }
    if let Err(c) = lifecycle() {
        println!("nettcp-demo: lifecycle failed ({c})");
        return c;
    }
    if let Err(c) = socket_api() {
        println!("nettcp-demo: socket API failed ({c})");
        return c;
    }
    if !wheel_multiplex_proof() {
        println!("nettcp-demo: timer wheel multiplex failed");
        return 91;
    }
    println!(
        "nettcp-demo: handshake + bidirectional data + drop/RTO recover + teardown + timer wheel OK"
    );
    OK_CODE
}

/// Known-good SYN-ACK-with-MSS segment (10.0.2.15:0x1234 -> 10.0.2.2:0x0050,
/// seq 0x11223344, ack 0x55667788, window 0xFAF0, MSS 1460) and its independently
/// computed on-wire bytes + checksum (`0x613C`).
fn oracle() -> Result<(), i32> {
    let src = Ipv4Addr::new(10, 0, 2, 15);
    let dst = Ipv4Addr::new(10, 0, 2, 2);
    let seg = Segment {
        src_port: 0x1234,
        dst_port: 0x0050,
        seq: 0x1122_3344,
        ack: 0x5566_7788,
        flags: SYN | ACK,
        window: 0xFAF0,
        mss: Some(1460),
        payload: Vec::new(),
    };
    const EXPECT: [u8; 24] = [
        0x12, 0x34, 0x00, 0x50, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x60, 0x12, 0xfa,
        0xf0, 0x61, 0x3c, 0x00, 0x00, 0x02, 0x04, 0x05, 0xb4,
    ];
    let mut out = [0u8; 24];
    let n = seg.encode(src, dst, &mut out).ok_or(80)?;
    if n != 24 {
        return Err(81);
    }
    if out != EXPECT {
        return Err(82);
    }
    if u16::from_be_bytes([out[16], out[17]]) != 0x613C {
        return Err(83);
    }
    if !tcp::verify_checksum_v4(src, dst, &out) {
        return Err(84);
    }
    let d = Segment::decode(&out).ok_or(85)?;
    if d.seq != 0x1122_3344 || d.ack != 0x5566_7788 || d.flags != (SYN | ACK) || d.mss != Some(1460)
    {
        return Err(86);
    }
    // RFC 1323 wrapping sequence-number arithmetic oracle.
    if !seq::lt(0xFFFF_FFFF, 0) {
        return Err(87);
    }
    if !seq::gt(0, 0xFFFF_FFFF) {
        return Err(88);
    }
    if seq::lt(100, 100) || !seq::leq(100, 100) {
        return Err(89);
    }
    Ok(())
}

/// Drive two connections to quiescence over the virtual link, advancing the timer
/// wheel's logical clock to the next RTO / TIME-WAIT deadline when neither has any
/// immediate output. This is where "advance the timer wheel, assert recovery"
/// happens - the wheel multiplexes both connections' timers onto one deadline.
fn pump(a: &mut Conn, b: &mut Conn, link: &mut VirtualLink, wheel: &mut TimerWheel) {
    for _ in 0..1_000_000 {
        let now = wheel.now();
        let mut progressed = false;
        while let Some(s) = a.poll(now) {
            link.transfer(&s, b, now);
            progressed = true;
        }
        while let Some(s) = b.poll(now) {
            link.transfer(&s, a, now);
            progressed = true;
        }
        if progressed {
            continue;
        }
        // Idle: rebuild the wheel from both connections' deadlines and jump to the
        // nearest (the single reactor slot would arm exactly this one).
        wheel.clear();
        if let Some(d) = a.poll_at() {
            wheel.insert(d);
        }
        if let Some(d) = b.poll_at() {
            wheel.insert(d);
        }
        match wheel.nearest() {
            Some(d) => wheel.set_now(if d > now { d } else { now + 1 }),
            None => return,
        }
    }
}

/// The full TCP lifecycle over the in-cell virtual link.
fn lifecycle() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let (cport, sport) = (40000u16, 80u16);

    let mut client: Conn = Connection::connect(cip, cport, sip, sport, 0x0000_1000);
    let mut server: Conn = Connection::listen(sip, sport, cip, cport, 0x0000_9000);
    let mut link = VirtualLink::new();
    let mut wheel = TimerWheel::new();

    // --- three-way handshake ---
    pump(&mut client, &mut server, &mut link, &mut wheel);
    if !client.is_established() {
        return Err(50);
    }
    if !server.is_established() {
        return Err(51);
    }

    // --- bidirectional data, exact bytes ---
    let c2s = b"hello from the client over rheo-net TCP";
    let s2c = b"the server acknowledges and replies too!";
    if client.write(c2s) != c2s.len() {
        return Err(52);
    }
    if server.write(s2c) != s2c.len() {
        return Err(53);
    }
    pump(&mut client, &mut server, &mut link, &mut wheel);
    let mut buf = [0u8; 256];
    let n = server.read(&mut buf);
    if buf[..n] != c2s[..] {
        return Err(54);
    }
    let n = client.read(&mut buf);
    if buf[..n] != s2c[..] {
        return Err(55);
    }

    // --- drop one segment, recover via RTO retransmission ---
    let payload = b"this segment is dropped once and must be retransmitted after the RTO fires";
    if client.write(payload) != payload.len() {
        return Err(56);
    }
    link.drop_next_data_segment();
    let dropped_before = link.dropped();
    pump(&mut client, &mut server, &mut link, &mut wheel);
    if link.dropped() != dropped_before + 1 {
        return Err(57); // the segment must have been dropped exactly once
    }
    let n = server.read(&mut buf);
    if buf[..n] != payload[..] {
        return Err(58); // recovery: the retransmit delivered the exact bytes
    }

    // --- graceful teardown: active close -> passive close -> TIME-WAIT -> CLOSED ---
    client.close();
    pump(&mut client, &mut server, &mut link, &mut wheel);
    if server.state() != State::CloseWait {
        return Err(59);
    }
    if client.state() != State::FinWait2 {
        return Err(60);
    }
    server.close();
    pump(&mut client, &mut server, &mut link, &mut wheel);
    if server.state() != State::Closed {
        return Err(61);
    }
    if client.state() != State::Closed {
        return Err(62); // client left TIME-WAIT after 2*MSL (wheel-advanced)
    }
    Ok(())
}

/// Drive the socket-shaped [`TcpStream`]/[`TcpListener`] wrappers through a full
/// handshake + one byte + close, proving the vocabulary forwards to the core.
fn socket_api() -> Result<(), i32> {
    let cip = Ipv4Addr::new(10, 0, 0, 1);
    let sip = Ipv4Addr::new(10, 0, 0, 2);
    let (cport, sport) = (40001u16, 80u16);

    let mut s: TcpStream<FixedWindow> = TcpStream::connect(cip, cport, sip, sport, 0x0000_2000);
    if s.state() != State::SynSent || s.is_established() {
        return Err(70);
    }
    let mut l: TcpListener<FixedWindow> = TcpListener::bind(sip, sport, cip, cport, 0x0000_3000);
    if l.state() != State::Listen {
        return Err(71);
    }

    let mut link = VirtualLink::new();
    let mut wheel = TimerWheel::new();
    pump(s.connection(), l.connection(), &mut link, &mut wheel);
    if !s.is_established() || l.state() != State::Established {
        return Err(72);
    }

    let mut srv = l.accept();
    let msg = b"socket-api byte over TcpStream/TcpListener";
    if s.write(msg) != msg.len() {
        return Err(73);
    }
    pump(s.connection(), srv.connection(), &mut link, &mut wheel);
    let mut buf = [0u8; 64];
    let n = srv.read(&mut buf);
    if buf[..n] != msg[..] {
        return Err(74);
    }

    s.close();
    srv.close();
    pump(s.connection(), srv.connection(), &mut link, &mut wheel);
    if s.state() != State::Closed || srv.state() != State::Closed {
        return Err(75);
    }
    Ok(())
}

/// Prove the timer wheel multiplexes many logical timers onto the reactor's single
/// one-shot: four timers armed out of insertion order fire in **deadline order**
/// off `run_once` (`rt::sleep_ns` behind the one slot).
fn wheel_multiplex_proof() -> bool {
    use core::sync::atomic::{AtomicBool, Ordering};
    static OK: AtomicBool = AtomicBool::new(false);

    rt::block_on(async {
        let mut w = TimerWheel::new();
        // Insert out of order; capture the handles in deadline order.
        let t3 = w.insert(3_000_000);
        let t1 = w.insert(1_000_000);
        let t4 = w.insert(4_000_000);
        let t2 = w.insert(2_000_000);
        let expect: [TimerId; 4] = [t1, t2, t3, t4];

        let mut order: Vec<TimerId> = Vec::new();
        while !w.is_empty() {
            let fired = w.run_once().await;
            order.extend_from_slice(&fired);
        }
        OK.store(order.as_slice() == expect, Ordering::Relaxed);
    });

    OK.load(core::sync::atomic::Ordering::Relaxed)
}
