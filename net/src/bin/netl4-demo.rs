//! `netl4-demo` - the rheo-net Phase N1b proof cell (docs/NETSTACK.md). It proves
//! **UDP** and **ICMP** end-to-end over QEMU SLIRP, deterministically and
//! network-free:
//!
//! 1. **Checksum oracles** (in memory, no network): a fixed DNS-query UDP
//!    datagram must checksum (over the IPv4 pseudo-header) to the known-good
//!    `0x6D45`, and a fixed ICMP echo request to `0xFFE0` - values computed
//!    independently (by hand / a script), so they pin the checksum math the way
//!    N1a's `0xB861` pins the IPv4 header checksum.
//! 2. **UDP round trip**: send a real DNS query (`A example.com`) over UDP to
//!    SLIRP's built-in DNS responder at `10.0.2.3:53` and receive the reply.
//!    `recv_from` validates the UDP checksum; the demo asserts the reply is from
//!    `10.0.2.3:53` and echoes our transaction id `0x1234`. DNS is not parsed
//!    (that is N1c) - this proves the UDP datagram round-tripped and its checksum
//!    validates.
//! 3. **ICMP echo (ping)**: send an ICMP echo request to the gateway `10.0.2.2`
//!    (SLIRP answers echo to the gateway internally - no host network) and assert
//!    the reply is type 0 with the matching id/seq and a valid checksum.
//!
//! Bounded retransmits guard a momentary RX miss; if SLIRP genuinely does not
//! answer, the demo returns a nonzero code and the `netl4` kernel fails loudly.
//! It exits `0x42` only if every step passes. The kernel is untouched - portable
//! userspace over the existing `OP_NET_*` queue path.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};
use rheo_net::eth::Mac;
use rheo_net::icmp::IcmpEndpoint;
use rheo_net::ip::Ipv4Addr;
use rheo_net::udp::{self, UdpEndpoint};
use rheo_net::{icmp, wire::WireError};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's default guest address, gateway, and built-in DNS responder.
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const DNS_PORT: u16 = 53;
/// Our ephemeral UDP source port (fixed so it matches the known-good vector).
const SRC_PORT: u16 = 0x9876;

/// A minimal DNS query: transaction id `0x1234`, RD set, one question
/// `example.com IN A`. 29 bytes. We only send it and check the id echoes back -
/// full DNS parsing is N1c (the caching resolver).
const DNS_TXID: u16 = 0x1234;
const DNS_QUERY: [u8; 29] = [
    0x12, 0x34, // transaction id
    0x01, 0x00, // flags: standard query, recursion desired
    0x00, 0x01, // qdcount = 1
    0x00, 0x00, // ancount = 0
    0x00, 0x00, // nscount = 0
    0x00, 0x00, // arcount = 0
    7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', // "example"
    3, b'c', b'o', b'm', // "com"
    0,    // root label
    0x00, 0x01, // qtype = A
    0x00, 0x01, // qclass = IN
];

/// Known-good UDP checksum of `DNS_QUERY` from `10.0.2.15:0x9876` to
/// `10.0.2.3:53` over the IPv4 pseudo-header (computed independently).
const KNOWN_UDP_CHECKSUM: u16 = 0x6D45;

/// ICMP echo identifier/sequence and payload for the ping.
const ICMP_IDENT: u16 = 0xABCD;
const ICMP_SEQ: u16 = 0x0001;
const ICMP_PAYLOAD: [u8; 8] = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17];
/// Known-good ICMP echo-request checksum of the header (id/seq above) + payload.
const KNOWN_ICMP_CHECKSUM: u16 = 0xFFE0;

/// Retransmit budget: send + poll this many times before giving up (each poll is
/// itself bounded, so a lost first datagram is retried, not a hang).
const ATTEMPTS: u32 = 4;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("netl4-demo: UDP + ICMP round trips OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

async fn run() {
    // The NIC MAC: the L2 source, and what QEMU's rx filter accepts so SLIRP's
    // replies reach our RX queue.
    let src_mac: Mac = match net::mac().await {
        Ok(m) => m,
        Err(_) => return fail(10),
    };

    // --- 1. Checksum oracles (in memory). ---
    let udp_ck = udp::checksum_v4(GUEST_IP, DNS_IP, SRC_PORT, DNS_PORT, &DNS_QUERY);
    if udp_ck != KNOWN_UDP_CHECKSUM {
        println!("netl4-demo: UDP checksum {udp_ck:#06x} != {KNOWN_UDP_CHECKSUM:#06x}");
        return fail(20);
    }
    let mut icmp_buf = [0u8; 64];
    let n = icmp::build_echo_request(ICMP_IDENT, ICMP_SEQ, &ICMP_PAYLOAD, &mut icmp_buf).unwrap();
    // The on-wire checksum field sits at bytes 2..4 of the built message.
    let icmp_ck = u16::from_be_bytes([icmp_buf[2], icmp_buf[3]]);
    if icmp_ck != KNOWN_ICMP_CHECKSUM {
        println!("netl4-demo: ICMP checksum {icmp_ck:#06x} != {KNOWN_ICMP_CHECKSUM:#06x}");
        return fail(21);
    }
    // The built message must self-verify (fold to zero).
    if !icmp::verify_checksum(&icmp_buf[..n]) {
        return fail(22);
    }

    // --- 2. UDP round trip: DNS query to SLIRP's 10.0.2.3:53. ---
    let mut udp = UdpEndpoint::new(src_mac, GUEST_IP);
    let mut reply = [0u8; 1500];
    let mut got = None;
    for _ in 0..ATTEMPTS {
        if let Err(e) = udp.send_to(DNS_IP, DNS_PORT, SRC_PORT, &DNS_QUERY).await {
            if e == WireError::Net {
                return fail(30);
            }
            continue; // ArpTimeout/TooBig on this attempt - retry
        }
        match udp.recv_from(&mut reply).await {
            Ok(r) => {
                got = Some(r);
                break;
            }
            Err(WireError::Net) => return fail(31),
            Err(_) => continue, // no reply this round - retransmit
        }
    }
    let r = match got {
        Some(r) => r,
        None => return fail(32), // SLIRP DNS did not answer
    };
    // The reply must come from 10.0.2.3:53 (recv_from already validated the UDP
    // checksum) and echo our transaction id.
    if r.src_ip != DNS_IP || r.src_port != DNS_PORT {
        println!(
            "netl4-demo: UDP reply from {}.{}.{}.{}:{} unexpected",
            r.src_ip.0[0], r.src_ip.0[1], r.src_ip.0[2], r.src_ip.0[3], r.src_port
        );
        return fail(33);
    }
    if r.len < 2 || u16::from_be_bytes([reply[0], reply[1]]) != DNS_TXID {
        return fail(34);
    }
    println!(
        "netl4-demo: UDP DNS reply from 10.0.2.3:53, {} B, txid {:#06x} echoed",
        r.len, DNS_TXID
    );

    // --- 3. ICMP echo (ping) to the gateway 10.0.2.2. ---
    let mut icmp = IcmpEndpoint::new(src_mac, GUEST_IP);
    let pong = match icmp
        .ping(GATEWAY_IP, ICMP_IDENT, ICMP_SEQ, &ICMP_PAYLOAD, ATTEMPTS)
        .await
    {
        Ok(p) => p,
        Err(WireError::Net) => return fail(40),
        Err(_) => return fail(41), // no echo reply
    };
    if pong.src_ip != GATEWAY_IP || pong.ident != ICMP_IDENT || pong.seq != ICMP_SEQ {
        return fail(42);
    }
    println!(
        "netl4-demo: ICMP echo reply from 10.0.2.2, id {:#06x} seq {}",
        pong.ident, pong.seq
    );

    // Success: CODE stays 0.
}
