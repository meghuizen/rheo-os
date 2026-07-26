//! `netcore-demo` - the rheo-net Phase N1a proof cell (docs/NETSTACK.md,
//! docs/NETWORKING.md). It proves the L2/L3 core end-to-end **through the new
//! `net` crate's abstractions** (not hand-laid bytes):
//!
//! 1. reads the NIC MAC via `librheo::net`, then **resolves the SLIRP gateway
//!    `10.0.2.2` via `net::arp::resolve`** - a real ARP round trip built with
//!    `net::eth`/`net::arp`, replacing `librheo-net`'s hand-built frame - and
//!    populates the ARP cache (a second lookup hits it);
//! 2. runs the ones-complement checksum against a **known-good value**
//!    (`0xB861`, the RFC/Wikipedia IPv4 example), then does an IPv4 header
//!    build -> parse -> **checksum-validate** round trip and asserts a flipped
//!    byte fails validation;
//! 3. does an IPv6 header build -> parse round trip.
//!
//! It exits `0x42` only if every step passes; the `netcore` test kernel asserts
//! that code on all three ISAs. The kernel is untouched - this is portable
//! userspace over the existing `OP_NET_*` queue path.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, Ordering};

use librheo::{net, println, rt};
use rheo_net::arp::{self, ArpCache};
use rheo_net::eth::Mac;
use rheo_net::ip::{self, Ipv4Addr, Ipv4Header, Ipv6Addr, Ipv6Header};

/// Failure code (0 = success), set inside the `'static` async root.
static CODE: AtomicI32 = AtomicI32::new(0);
/// Exit code on full success (the test asserts exactly this).
const OK_CODE: i32 = 0x42;

/// SLIRP's default guest address and gateway (QEMU `-netdev user`).
const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// The RFC 1071 / Wikipedia IPv4 example header with the checksum field zeroed;
/// its Internet checksum is the known constant `0xB861`. This is the checksum
/// oracle - a fixed, independently-verifiable value.
const KNOWN_IPV4_HEADER: [u8; 20] = [
    0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8, 0x00, 0x01,
    0xc0, 0xa8, 0x00, 0xc7,
];
const KNOWN_IPV4_CHECKSUM: u16 = 0xB861;

#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    rt::block_on(run());
    let code = CODE.load(Ordering::Relaxed);
    if code != 0 {
        return code;
    }
    println!("netcore-demo: eth/arp/ip core OK");
    OK_CODE
}

fn fail(code: i32) {
    CODE.store(code, Ordering::Relaxed);
}

async fn run() {
    // 1. The NIC MAC - the ARP source, and what QEMU's rx filter accepts so
    //    SLIRP's reply reaches our RX queue.
    let src_mac: Mac = match net::mac().await {
        Ok(m) => m,
        Err(_) => return fail(10),
    };

    // 1b. Resolve the gateway through net::arp (build_request + send + recv +
    //     parse), the ARP round trip done through the stack.
    let mut cache = ArpCache::new();
    let gw_mac = match arp::resolve(&mut cache, src_mac, GUEST_IP, GATEWAY_IP).await {
        Ok(m) => m,
        Err(arp::ResolveError::Net) => return fail(11),
        Err(arp::ResolveError::Timeout) => return fail(12),
    };
    // The cache is now populated - the second lookup hits it.
    if cache.lookup(GATEWAY_IP) != Some(gw_mac) {
        return fail(13);
    }
    println!(
        "netcore-demo: ARP 10.0.2.2 -> {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (cache {} entry)",
        gw_mac.0[0],
        gw_mac.0[1],
        gw_mac.0[2],
        gw_mac.0[3],
        gw_mac.0[4],
        gw_mac.0[5],
        cache.len()
    );

    // 2. Checksum oracle: the known-good IPv4 example must checksum to 0xB861.
    if ip::checksum16(&KNOWN_IPV4_HEADER) != KNOWN_IPV4_CHECKSUM {
        return fail(20);
    }
    // A header whose checksum field is correct must validate to 0.
    let mut known_full = KNOWN_IPV4_HEADER;
    known_full[10..12].copy_from_slice(&KNOWN_IPV4_CHECKSUM.to_be_bytes());
    if !Ipv4Header::verify_checksum(&known_full) {
        return fail(21);
    }

    // 2b. IPv4 build -> parse -> validate round trip.
    let hdr = Ipv4Header {
        dscp_ecn: 0,
        total_len: 20 + 8,
        identification: 0x1234,
        flags_frag: 0x4000, // Don't Fragment
        ttl: 64,
        protocol: ip::proto::UDP,
        src: GUEST_IP,
        dst: GATEWAY_IP,
    };
    let mut buf = [0u8; 20];
    if hdr.write(&mut buf).is_none() {
        return fail(22);
    }
    if !Ipv4Header::verify_checksum(&buf) {
        return fail(23);
    }
    let parsed = match Ipv4Header::parse(&buf) {
        Some(h) => h,
        None => return fail(24),
    };
    if parsed != hdr {
        return fail(25);
    }
    // Flip a byte: validation must now fail.
    let mut corrupt = buf;
    corrupt[8] ^= 0xFF; // mangle the TTL
    if Ipv4Header::verify_checksum(&corrupt) {
        return fail(26);
    }

    // 3. IPv6 build -> parse round trip (IPv6 has no header checksum).
    let v6 = Ipv6Header {
        traffic_class: 0,
        flow_label: 0x0_ABCD,
        payload_len: 16,
        next_header: ip::proto::UDP,
        hop_limit: 64,
        src: Ipv6Addr([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ]),
        dst: Ipv6Addr([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
        ]),
    };
    let mut buf6 = [0u8; 40];
    if v6.write(&mut buf6).is_none() {
        return fail(30);
    }
    let parsed6 = match Ipv6Header::parse(&buf6) {
        Some(h) => h,
        None => return fail(31),
    };
    if parsed6 != v6 {
        fail(32);
    }

    // Success: CODE stays 0 (nothing follows - the last check needs no return).
}
