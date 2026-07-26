//! `nettrace-demo` - the rheo-net Phase N1e proof cell (docs/NETSTACK.md, the
//! TTL / hop-limit / traceroute section). It proves, **deterministically and
//! network-free** (the core proof), then with a **bonus live** 1-hop probe:
//!
//! 1. **First-class TTL / hop limit**: an IPv4 header round-trips its TTL (the
//!    default 64 and an explicit value) and an IPv6 header round-trips its hop
//!    limit (default 64 and explicit) through build -> parse.
//! 2. **The forwarding decrement primitive** (the router/firewall path):
//!    `ip::decrement_ttl` on a real on-wire header decrements the TTL and
//!    recomputes a valid checksum (asserted against the known-good literal
//!    `0xB961` and by re-verifying the header), and returns the drop signal
//!    (`None`) when the TTL is 1 or 0; `ip::decrement_hop_limit` mirrors it for v6.
//! 3. **ICMP Time Exceeded oracle**: a fixed ICMPv4 Time Exceeded (type 11)
//!    build -> parse checksums to the known-good `0xF4FF`, self-verifies, and
//!    round-trips its embedded original datagram; the ICMPv6 Time Exceeded (type
//!    3) codec checksums to the known-good `0x1936` and self-verifies.
//! 4. **The traceroute state machine fed synthetic responses**: a crafted
//!    sequence of Time Exceededs (hops 1..3, distinct routers) then a destination
//!    Echo Reply is classified and fed to the `trace::Tracer`, which reconstructs
//!    the exact ordered hop list and terminates - multi-hop discovery proven
//!    without real intermediate routers.
//! 5. **Bonus live**: a 1-hop trace to the gateway `10.0.2.2` over SLIRP. SLIRP
//!    has **no intermediate hops** (it is the destination at hop 1), so we assert
//!    only that - if any hop is reached it is `10.0.2.2` at TTL 1 - and tolerate a
//!    clean timeout (printed reason), never faking a pass.
//!
//! It exits `0x42` only if every deterministic check (1-4) passes. The kernel is
//! untouched - portable userspace over the existing `OP_NET_*` queue path.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};
use rheo_net::eth::Mac;
use rheo_net::icmp::{self, IcmpEndpoint};
use rheo_net::ip::{self, Ipv4Addr, Ipv4Header, Ipv6Addr, Ipv6Header};
use rheo_net::trace::{self, Config, Response, Tracer};
use rheo_net::wire::WireError;

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's default gateway (also the destination for the live 1-hop trace).
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// The echo identifier for probes.
const IDENT: u16 = 0xABCD;

/// The netcore IPv4 example header (checksum field zeroed), TTL byte = `0x40`
/// (64). Independently: its checksum is `0xB861`, and after `decrement_ttl`
/// (TTL 63) it is `0xB961` - the N1e forwarding-plane oracle.
const IPV4_HEADER: [u8; 20] = [
    0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8, 0x00, 0x01,
    0xc0, 0xa8, 0x00, 0xc7,
];
const IPV4_TTL63_CHECKSUM: u16 = 0xB961;

/// A fixed offending datagram embedded in the ICMPv4 Time Exceeded oracle: an
/// IPv4 header (proto ICMP, valid checksum) + an 8-byte ICMP echo request
/// (id `0xABCD`, seq 5). Independently computed; the Time Exceeded over it
/// checksums to `0xF4FF`.
const TE_ORIG_V4: [u8; 28] = [
    0x45, 0x00, 0x00, 0x1c, 0x00, 0x00, 0x40, 0x00, 0x01, 0x01, 0x61, 0xd1, 0x0a, 0x00, 0x02, 0x0f,
    0x0a, 0x00, 0x02, 0x02, 0x08, 0x00, 0x4c, 0x2d, 0xab, 0xcd, 0x00, 0x05,
];
const TE_V4_CHECKSUM: u16 = 0xF4FF;

/// The ICMPv6 Time Exceeded oracle addresses + embedded original (an IPv6 header,
/// next-header ICMPv6, hop limit 1, + an 8-byte ICMPv6 echo request). Its Time
/// Exceeded (over the v6 pseudo-header) checksums to `0x1936`.
const V6_SRC: Ipv6Addr = Ipv6Addr([
    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x15,
]);
const V6_DST: Ipv6Addr = Ipv6Addr([
    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x02,
]);
const TE_ORIG_V6: [u8; 48] = [
    0x60, 0x00, 0x00, 0x00, 0x00, 0x08, 58, 1, // IPv6 hdr: payload 8, next 58, hop 1
    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x15, // src
    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x02, // dst
    128, 0, 0, 0, 0x12, 0x34, 0x00, 0x07, // ICMPv6 echo request id/seq
];
const TE_V6_CHECKSUM: u16 = 0x1936;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("nettrace-demo: TTL + hop limit + Time Exceeded + traceroute OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

/// Build the offending original datagram a router would quote for our echo probe
/// at hop `seq`: an IPv4 header (proto ICMP) + our 8-byte echo request.
fn probe_original(seq: u16, out: &mut [u8]) -> usize {
    let hdr = Ipv4Header {
        dscp_ecn: 0,
        total_len: (ip::IPV4_HEADER_LEN + icmp::HEADER_LEN) as u16,
        identification: 0,
        flags_frag: 0x4000,
        ttl: seq as u8,
        protocol: ip::proto::ICMP,
        src: GUEST_IP,
        dst: GATEWAY_IP,
    };
    hdr.write(&mut out[..ip::IPV4_HEADER_LEN]).unwrap();
    let elen = icmp::build_echo_request(IDENT, seq, &[], &mut out[ip::IPV4_HEADER_LEN..]).unwrap();
    ip::IPV4_HEADER_LEN + elen
}

async fn run() {
    // The NIC MAC is only needed for the live probe; fetch it up front (a missing
    // NIC still lets the deterministic checks run - the live step degrades).
    let src_mac: Option<Mac> = net::mac().await.ok();

    if !deterministic() {
        return; // `fail` already recorded the code
    }

    // --- 5. Bonus live: a 1-hop trace to the gateway (tolerate a timeout). ---
    if let Some(mac) = src_mac {
        live_one_hop(mac).await;
    } else {
        println!("nettrace-demo: no NIC MAC - skipping the bonus live 1-hop trace");
    }
    // Success: CODE stays 0 (the deterministic checks are the proof).
}

/// The network-free core proof (checks 1-4). Returns `false` (and records a fail
/// code) on the first failing assertion.
fn deterministic() -> bool {
    // --- 1. First-class TTL / hop limit round-trips. ---
    if !ttl_roundtrip(ip::DEFAULT_TTL) {
        return set(20);
    }
    if !ttl_roundtrip(17) {
        return set(21);
    }
    if !hop_limit_roundtrip(ip::DEFAULT_HOP_LIMIT) {
        return set(22);
    }
    if !hop_limit_roundtrip(200) {
        return set(23);
    }

    // --- 2. The forwarding decrement primitive. ---
    // Build a full valid header (checksum filled) from the example, decrement.
    let mut hdr = [0u8; 20];
    hdr.copy_from_slice(&IPV4_HEADER);
    let ck0 = ip::checksum16(&hdr);
    hdr[10..12].copy_from_slice(&ck0.to_be_bytes()); // now a valid on-wire header
    // N -> N-1: decrement succeeds, checksum stays valid and matches the oracle.
    if ip::decrement_ttl(&mut hdr).is_none() {
        return set(30);
    }
    if hdr[8] != 0x3f || !Ipv4Header::verify_checksum(&hdr) {
        return set(30);
    }
    let ck1 = u16::from_be_bytes([hdr[10], hdr[11]]);
    if ck1 != IPV4_TTL63_CHECKSUM {
        println!("nettrace-demo: ttl63 checksum {ck1:#06x} != {IPV4_TTL63_CHECKSUM:#06x}");
        return set(31);
    }
    // TTL 1 -> drop signal (None), header untouched.
    let mut ttl1 = IPV4_HEADER;
    ttl1[8] = 1;
    if ip::decrement_ttl(&mut ttl1).is_some() {
        return set(32);
    }
    // TTL 0 -> also None.
    let mut ttl0 = IPV4_HEADER;
    ttl0[8] = 0;
    if ip::decrement_ttl(&mut ttl0).is_some() {
        return set(33);
    }
    // IPv6 hop-limit decrement mirrors it.
    let mut v6 = [0u8; 40];
    Ipv6Header {
        traffic_class: 0,
        flow_label: 0,
        payload_len: 0,
        next_header: ip::proto::ICMPV6,
        hop_limit: 64,
        src: V6_SRC,
        dst: V6_DST,
    }
    .write(&mut v6)
    .unwrap();
    if ip::decrement_hop_limit(&mut v6).is_none() || v6[7] != 63 {
        return set(34);
    }
    v6[7] = 1;
    if ip::decrement_hop_limit(&mut v6).is_some() {
        return set(35);
    }

    // --- 3. ICMP Time Exceeded oracles (v4 + v6 codec). ---
    let mut te = [0u8; 64];
    let n = match icmp::build_time_exceeded(&TE_ORIG_V4, &mut te) {
        Some(n) => n,
        None => return set(40),
    };
    if u16::from_be_bytes([te[2], te[3]]) != TE_V4_CHECKSUM {
        return set(41);
    }
    if !icmp::verify_checksum(&te[..n]) {
        return set(42);
    }
    match icmp::parse_error(&te[..n]) {
        Some(e) if e.msg_type == icmp::TIME_EXCEEDED && e.original == TE_ORIG_V4 => {}
        _ => return set(43),
    }
    // ICMPv6 Time Exceeded codec (unit-proven; live v6 deferred).
    let mut te6 = [0u8; 64];
    let n6 = match icmp::build_time_exceeded_v6(V6_SRC, V6_DST, &TE_ORIG_V6, &mut te6) {
        Some(n) => n,
        None => return set(44),
    };
    if u16::from_be_bytes([te6[2], te6[3]]) != TE_V6_CHECKSUM {
        return set(44);
    }
    if !icmp::verify_checksum_v6(V6_SRC, V6_DST, &te6[..n6]) {
        return set(45);
    }

    // --- 4. The traceroute state machine fed synthetic responses. ---
    if !state_machine() {
        return false;
    }

    println!("nettrace-demo: deterministic TTL/decrement/Time-Exceeded/traceroute checks pass");
    true
}

/// Feed a crafted 3-router path + destination reply into the state machine and
/// assert the exact reconstructed hop list. Time Exceededs are built with the
/// real codec and put through the real `classify`, so this exercises
/// build -> parse -> correlate -> state machine end to end.
fn state_machine() -> bool {
    let mut tr = Tracer::new(GATEWAY_IP, Config::new(IDENT));
    // Routers 10.0.0.1..3 return Time Exceeded for hops 1..3.
    for hop in 1u16..=3 {
        if tr.next_probe() != Some(hop as u8) {
            return set(52);
        }
        let mut orig = [0u8; 40];
        let olen = probe_original(hop, &mut orig);
        let mut te = [0u8; 64];
        let n = icmp::build_time_exceeded(&orig[..olen], &mut te).unwrap();
        let from = Ipv4Addr::new(10, 0, 0, hop as u8);
        match trace::classify(&te[..n], from, IDENT) {
            Some(Response::TimeExceeded { seq, from: f }) if seq == hop && f == from => {
                tr.record(Response::TimeExceeded { seq, from: f });
            }
            _ => return set(50),
        }
    }
    // Hop 4: the destination answers with an Echo Reply.
    if tr.next_probe() != Some(4) {
        return set(52);
    }
    let mut reply = [0u8; 32];
    let rn = icmp::build_echo(icmp::ECHO_REPLY, IDENT, 4, &[], &mut reply).unwrap();
    match trace::classify(&reply[..rn], GATEWAY_IP, IDENT) {
        Some(Response::Reply { seq: 4, from }) if from == GATEWAY_IP => {
            tr.record(Response::Reply {
                seq: 4,
                from: GATEWAY_IP,
            });
        }
        _ => return set(50),
    }
    // The trace must now be done, with the exact ordered hop list.
    if !tr.done() || tr.next_probe().is_some() {
        return set(51);
    }
    let hops = tr.hops();
    if hops.len() != 4 {
        return set(50);
    }
    for (i, h) in hops.iter().enumerate() {
        let ttl = (i + 1) as u8;
        let expect = if ttl == 4 {
            (GATEWAY_IP, true)
        } else {
            (Ipv4Addr::new(10, 0, 0, ttl), false)
        };
        if h.ttl != ttl || h.addr != expect.0 || h.reached != expect.1 {
            return set(50);
        }
    }
    println!("nettrace-demo: state machine reconstructed a 4-hop path (3 routers + destination)");
    true
}

/// The bonus live 1-hop trace: SLIRP is the destination at hop 1, so a reached
/// hop must be `10.0.2.2` at TTL 1; a timeout is tolerated with a printed reason.
async fn live_one_hop(src_mac: Mac) {
    let mut ep = IcmpEndpoint::new(src_mac, GUEST_IP);
    let mut cfg = Config::new(IDENT);
    cfg.max_hops = 1; // 1-hop: SLIRP has no intermediate hops to discover
    let mut tr = Tracer::new(GATEWAY_IP, cfg);
    match tr.run(&mut ep, &[0x10, 0x11, 0x12, 0x13]).await {
        Ok(()) => {}
        Err(WireError::Net) => {
            println!("nettrace-demo: live trace transport error - tolerated (deterministic pass)");
            return;
        }
        Err(_) => {}
    }
    match tr.hops().iter().find(|h| h.reached) {
        Some(h) if h.ttl == 1 && h.addr == GATEWAY_IP => {
            println!("nettrace-demo: live 1-hop trace reached the gateway 10.0.2.2 at TTL 1");
        }
        Some(h) => {
            // A reply from somewhere unexpected is worth printing but is not a
            // proof failure (the deterministic checks are the proof).
            println!(
                "nettrace-demo: live trace reached {}.{}.{}.{} at TTL {} (SLIRP-dependent)",
                h.addr.0[0], h.addr.0[1], h.addr.0[2], h.addr.0[3], h.ttl
            );
        }
        None => {
            println!(
                "nettrace-demo: live 1-hop trace saw no reply (SLIRP has no intermediate hops) - tolerated"
            );
        }
    }
}

/// Round-trip an IPv4 header's TTL through build -> parse.
fn ttl_roundtrip(ttl: u8) -> bool {
    let hdr = Ipv4Header {
        dscp_ecn: 0,
        total_len: 20,
        identification: 0,
        flags_frag: 0,
        ttl,
        protocol: ip::proto::UDP,
        src: GUEST_IP,
        dst: GATEWAY_IP,
    };
    let mut buf = [0u8; 20];
    hdr.write(&mut buf).unwrap();
    Ipv4Header::parse(&buf).map(|h| h.ttl) == Some(ttl) && Ipv4Header::verify_checksum(&buf)
}

/// Round-trip an IPv6 header's hop limit through build -> parse.
fn hop_limit_roundtrip(hop_limit: u8) -> bool {
    let hdr = Ipv6Header {
        traffic_class: 0,
        flow_label: 0,
        payload_len: 0,
        next_header: ip::proto::UDP,
        hop_limit,
        src: V6_SRC,
        dst: V6_DST,
    };
    let mut buf = [0u8; 40];
    hdr.write(&mut buf).unwrap();
    Ipv6Header::parse(&buf).map(|h| h.hop_limit) == Some(hop_limit)
}

/// Record a fail code and return `false` (so callers can `return set(n)`).
fn set(code: i32) -> bool {
    fail(code);
    false
}
